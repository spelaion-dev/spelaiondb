//! Running a saved task (ADR-089 points 4 to 9): the connection resolved the same way for the
//! app and the CLI, a run prepared from the functions the dialogs call so it needs a yes
//! exactly where the run by hand would ask, a session of its own, and a record of the run in
//! the store with a redacted, bounded log.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use spelaion_config::{
    Config, Password, ResolveError, ResolvedConnection, Resolver, SystemBackend, TrustStore,
    canonical_folder, load,
};
use spelaion_driver_api::Secret;
use spelaion_ssh::{Interaction, KnownHosts};
use spelaion_store::{Store, TaskEnding, TaskOrigin};

use super::document::{ExportSource, ExportTask, TaskDocument, TaskKind, TaskRequest};
use super::{TaskError, TaskRunState};
use crate::backup::{
    self, BackupEvent, DumpOptions, DumpRequest, EmbeddedDumpRequest, RestoreRequest, ToolSource,
};
use crate::conn_test::{Recording, learn_connection_secrets};
use crate::export_stream::{
    ExportProgress, FileNamePattern, NameParts, TableSource, export_query, export_table,
    export_tables, free_path, rerun_check,
};
use crate::guard::{Risk, script_risks};
use crate::import::{ImportError, ImportProgress, plan_import, run_import};
use crate::job::{JobCancel, JobEnd, Refusal};
use crate::open::{Engines, OpenConnection, OpenError, OpenOptions, RouteOptions, TunnelPool};
use crate::{Redactor, ResultStore};

/// Lines a run's log keeps; the rest are counted.
const LOG_LINES: usize = 2_000;
/// Bytes a run's log keeps.
const LOG_BYTES: usize = spelaion_store::TASK_LOG_MAX;
/// Characters of a query shown in a plan.
const PLAN_SQL_CHARS: usize = 2_000;

/// What a run may ask the person who started it (ADR-089 point 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Questions {
    /// The app: a password or a confirmation is answered as [`TaskOutcome`]'s question, and
    /// nothing is recorded until the run is started again with the answer.
    Ask,
    /// The CLI: nobody is asked; a run that needs a password it cannot find fails, and one
    /// that needs a yes it was not given is refused, both recorded.
    Refuse,
}

/// What a run needs from the program that starts it.
pub struct TaskEnv {
    /// The personal folder: its configuration, trust store, known hosts and store.
    pub user_dir: PathBuf,
    pub engines: Arc<Engines>,
    pub tunnels: Arc<TunnelPool>,
    /// The tunnel's dialogs: the app's windows, or `spelaion_ssh::NonInteractive` in the CLI.
    pub interaction: Arc<dyn Interaction>,
    /// The password typed for this run, if the connection asks for one (the app's dialog).
    pub password: Option<Secret>,
    pub questions: Questions,
    /// Replaces where a dump's or a restore's tools are looked for: the tests' launcher of the
    /// official images. Never read from a task, and adapters leave it `None`.
    pub tools: Option<ToolSource>,
}

/// Progress of a run, as often as the job it runs reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskProgress {
    /// An export: rows and bytes written (of the table at this position, for several).
    Export {
        table: Option<usize>,
        rows: u64,
        bytes: u64,
    },
    Import {
        read: u64,
        committed: u64,
    },
    /// A dump's bytes written, or a restore's bytes read of the file's.
    Bytes {
        bytes: u64,
        total: Option<u64>,
    },
    /// Statements of a `.sql` file run.
    Statements {
        done: u64,
        total: u64,
    },
}

/// What a run tells the program that started it while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    /// A line of the run's log, already redacted (the same line the store keeps).
    Log(String),
    Progress(TaskProgress),
}

/// What a run will do, shown before it does: in the app's confirmation, or printed by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPlan {
    pub kind: TaskKind,
    /// The connection's name.
    pub connection: String,
    /// What runs: the statements, the query run again, the tool, the files.
    pub steps: Vec<String>,
    /// Why it asks: each risk the guard found, or why this kind always asks.
    pub risks: Vec<String>,
    /// Things to know (a tool's notes, a check a tunnel prevents).
    pub notes: Vec<String>,
    /// It runs only with a yes: exactly where the run by hand would ask.
    pub needs_confirmation: bool,
}

/// How starting a task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    /// It ran (or was refused, or failed before running), recorded as `run`.
    Ran {
        run: i64,
        state: TaskRunState,
        /// The run's summary: rows and bytes, files, statements.
        summary: String,
        /// Why it failed or was refused.
        message: Option<String>,
    },
    /// [`Questions::Ask`]: it needs a yes. Nothing ran and nothing was recorded: start it
    /// again confirmed.
    NeedsConfirmation(TaskPlan),
    /// [`Questions::Ask`]: the connection's password must be asked first. Nothing was
    /// recorded.
    PasswordRequired { can_remember: bool },
}

/// Resolves a task's connection (ADR-089 point 4), with the notes the run's log keeps: a
/// repository connection from its folder alone with the personal layer, only while it is
/// trusted as it is; a personal one with the folders the task recorded, each used only to
/// tighten.
pub fn resolve_connection(
    user_dir: &Path,
    connection: &super::TaskConnection,
) -> Result<(ResolvedConnection, Vec<String>), String> {
    let key = connection.key().map_err(|e| e.to_string())?;
    let id = key.id().clone();
    let trust = TrustStore::load(user_dir).map_err(|e| e.to_string())?;
    let mut notes = Vec::new();
    let resolved = match key.folder() {
        Some(root) => {
            if !root.is_dir() {
                return Err(format!(
                    "the folder of the connection {id}, {}, is not there any more",
                    root.display()
                ));
            }
            let config = load(Some(root), user_dir);
            if config
                .workspace_root
                .as_deref()
                .map(canonical_folder)
                .as_deref()
                != Some(root)
            {
                return Err(format!(
                    "{} is not a repository with .spelaion/ any more",
                    root.display()
                ));
            }
            if config.connection(&id).is_none() {
                return Err(format!(
                    "there is no connection {id} in {} any more: edit the task to choose another",
                    root.display()
                ));
            }
            Resolver::new(&config, &trust, &SystemBackend).resolve(&id)
        }
        None => {
            let mut folders: Vec<Config> = Vec::new();
            for folder in &connection.open_folders {
                if !folder.is_dir() {
                    notes.push(format!(
                        "The folder {} is not there any more: left out.",
                        folder.display()
                    ));
                    continue;
                }
                let config = load(Some(folder), user_dir);
                let Some(root) = config.workspace_root.as_deref().map(canonical_folder) else {
                    notes.push(format!(
                        "{} is not a repository any more: left out.",
                        folder.display()
                    ));
                    continue;
                };
                if !trust.is_trusted(&root) {
                    notes.push(format!(
                        "{} is not trusted: nothing of it applies.",
                        root.display()
                    ));
                }
                folders.push(config);
            }
            let personal;
            let base = match folders.first() {
                Some(config) => config,
                None => {
                    personal = load(None, user_dir);
                    &personal
                }
            };
            if base.connection(&id).is_none() {
                return Err(format!(
                    "there is no personal connection {id} any more: edit the task to choose another"
                ));
            }
            let open: Vec<&Config> = folders.iter().collect();
            Resolver::new(base, &trust, &SystemBackend)
                .with_open_folders(&open)
                .resolve(&id)
        }
    };
    let resolved = resolved.map_err(|e| match e {
        ResolveError::Untrusted { root } => format!(
            "{} is not trusted: trust it in the app or with `spelaiondb trust`",
            root.display()
        ),
        ResolveError::Changed { root, change } => format!(
            "the configuration of {} changed since it was trusted ({change}): review it in the app or with `spelaiondb trust`",
            root.display()
        ),
        other => other.to_string(),
    })?;
    Ok((resolved, notes))
}

/// A run's log: every line redacted, sent to the program that started the run and kept,
/// bounded, for the store.
struct RunLog<'a> {
    redactor: Arc<Redactor>,
    events: &'a (dyn Fn(TaskEvent) + Send + Sync),
    kept: Mutex<Kept>,
}

#[derive(Default)]
struct Kept {
    lines: Vec<String>,
    bytes: usize,
    omitted: u64,
}

impl<'a> RunLog<'a> {
    fn new(events: &'a (dyn Fn(TaskEvent) + Send + Sync)) -> Self {
        Self {
            redactor: Arc::new(Redactor::new()),
            events,
            kept: Mutex::new(Kept::default()),
        }
    }

    fn line(&self, text: &str) {
        for line in text.lines() {
            let line = self.redactor.redact(line);
            (self.events)(TaskEvent::Log(line.clone()));
            let mut kept = self.kept.lock().expect("lock poisoned");
            if kept.lines.len() >= LOG_LINES || kept.bytes + line.len() + 1 > LOG_BYTES {
                kept.omitted += 1;
            } else {
                kept.bytes += line.len() + 1;
                kept.lines.push(line);
            }
        }
    }

    fn progress(&self, progress: TaskProgress) {
        (self.events)(TaskEvent::Progress(progress));
    }

    fn backup_event(&self, event: BackupEvent) {
        match event {
            BackupEvent::Started { command_line } => self.line(&format!("Command: {command_line}")),
            BackupEvent::Log { line } => self.line(&line),
            BackupEvent::Progress { bytes, total } => {
                self.progress(TaskProgress::Bytes { bytes, total });
            }
            BackupEvent::Statements { done, total } => {
                self.progress(TaskProgress::Statements { done, total });
            }
        }
    }

    fn text(&self) -> (String, u64) {
        let kept = self.kept.lock().expect("lock poisoned");
        (kept.lines.join("\n"), kept.omitted)
    }
}

/// How a run's work ended, before it is recorded.
enum Step {
    Done(String),
    Failed(String),
    Cancelled(String),
    Refused(String),
    NeedsConfirmation(Box<TaskPlan>),
    PasswordRequired { can_remember: bool },
}

impl Step {
    fn of<T, K>(
        end: JobEnd<T, K>,
        done: impl FnOnce(T) -> String,
        kept: impl FnOnce(K) -> String,
    ) -> Self {
        match end {
            JobEnd::Done(value) | JobEnd::StoppedAtLimit(value) => Step::Done(done(value)),
            JobEnd::Cancelled(left) => Step::Cancelled(kept(left)),
            JobEnd::Failed {
                message,
                kept: left,
            } => {
                let left = kept(left);
                Step::Failed(if left.is_empty() {
                    message
                } else {
                    format!("{message} ({left})")
                })
            }
            JobEnd::Refused(Refusal::NeedsConfirmation) => {
                Step::Refused("it needs a yes that was not given".into())
            }
            JobEnd::Refused(Refusal::PasswordRequired) => Step::PasswordRequired {
                can_remember: false,
            },
            JobEnd::Refused(Refusal::NotARead(message)) => Step::Refused(message),
        }
    }
}

/// Starts the task `task_id`: records the run, resolves its connection, prepares it, asks or
/// refuses where it needs a yes, runs it in a session of its own and records how it ended.
pub async fn run_task(
    task_id: i64,
    origin: TaskOrigin,
    env: &TaskEnv,
    confirmed: bool,
    cancel: &JobCancel,
    events: &(dyn Fn(TaskEvent) + Send + Sync),
) -> Result<TaskOutcome, TaskError> {
    let store = Store::open(&spelaion_config::state_db(&env.user_dir))?;
    let stored = store
        .task(task_id)?
        .ok_or_else(|| TaskError::NotFound(task_id.to_string()))?;
    let run = store.start_task_run(task_id, origin)?;
    let log = RunLog::new(events);
    log.line(&format!(
        "Task {:?} started from {}.",
        stored.name,
        match origin {
            TaskOrigin::App => "the app",
            TaskOrigin::Cli => "the command line",
        }
    ));
    let step = match TaskDocument::from_json(&stored.document) {
        Err(reason) => Step::Failed(format!("this task cannot be read: {reason}")),
        Ok(document) => run_document(&document, env, confirmed, cancel, &log).await,
    };
    let (ending, summary, message) = match step {
        Step::Done(summary) => (TaskEnding::Succeeded, summary, None),
        Step::Failed(message) => (TaskEnding::Failed, String::new(), Some(message)),
        Step::Cancelled(kept) => (TaskEnding::Cancelled, kept, Some("cancelled".into())),
        Step::Refused(message) => (TaskEnding::Refused, String::new(), Some(message)),
        Step::NeedsConfirmation(plan) => match env.questions {
            Questions::Ask => {
                store.forget_task_run(run)?;
                return Ok(TaskOutcome::NeedsConfirmation(*plan));
            }
            Questions::Refuse => (
                TaskEnding::Refused,
                String::new(),
                Some(match origin {
                    TaskOrigin::Cli => {
                        "this task asks before it runs, as the run by hand would: run it again with --yes to confirm".into()
                    }
                    TaskOrigin::App => "this task asks before it runs: confirm it".into(),
                }),
            ),
        },
        Step::PasswordRequired { can_remember } => match env.questions {
            Questions::Ask => {
                store.forget_task_run(run)?;
                return Ok(TaskOutcome::PasswordRequired { can_remember });
            }
            Questions::Refuse => (
                TaskEnding::Failed,
                String::new(),
                Some(
                    "the connection asks for its password interactively: to run this task from the command line, keep the password in the keychain, env: or op://".into(),
                ),
            ),
        },
    };
    let message = message.map(|m| log.redactor.redact(&m));
    // A run that did not succeed is summed up by why.
    let summary = match (&message, summary.is_empty()) {
        (Some(message), true) => message.clone(),
        (Some(_) | None, _) => log.redactor.redact(&summary),
    };
    match &message {
        Some(message) => log.line(&format!("{}: {message}", ending_word(ending))),
        None => log.line(&format!("{}: {summary}", ending_word(ending))),
    }
    let (text, omitted) = log.text();
    store.end_task_run(run, ending, &summary, &text, omitted)?;
    Ok(TaskOutcome::Ran {
        run,
        state: match ending {
            TaskEnding::Succeeded => TaskRunState::Succeeded,
            TaskEnding::Failed => TaskRunState::Failed,
            TaskEnding::Cancelled => TaskRunState::Cancelled,
            TaskEnding::Refused => TaskRunState::Refused,
        },
        summary,
        message,
    })
}

fn ending_word(ending: TaskEnding) -> &'static str {
    match ending {
        TaskEnding::Succeeded => "Done",
        TaskEnding::Failed => "Failed",
        TaskEnding::Cancelled => "Cancelled",
        TaskEnding::Refused => "Refused",
    }
}

/// Everything a run does after it is recorded as started.
async fn run_document(
    document: &TaskDocument,
    env: &TaskEnv,
    confirmed: bool,
    cancel: &JobCancel,
    log: &RunLog<'_>,
) -> Step {
    let (resolved, notes) = match resolve_connection(&env.user_dir, &document.connection) {
        Ok(resolved) => resolved,
        Err(message) => return Step::Failed(message),
    };
    learn_connection_secrets(&log.redactor, &resolved);
    if let Some(password) = &env.password {
        log.redactor.add(password.expose());
    }
    for note in &notes {
        log.line(note);
    }
    log.line(&format!(
        "Connection: {} ({}).",
        resolved.name, document.connection.id
    ));
    // Asked before anything opens, so a dump or a restore (which opens no session) asks too.
    if let Password::Prompt { save_to } = &resolved.password
        && env.password.is_none()
    {
        return Step::PasswordRequired {
            can_remember: save_to.is_some(),
        };
    }
    let run = Run {
        resolved: &resolved,
        env,
        confirmed,
        cancel,
        log,
    };
    match &document.request {
        TaskRequest::Export(export) => run.export(export).await,
        TaskRequest::Import(import) => run.import(import).await,
        TaskRequest::Dump(dump) => run.dump(dump).await,
        TaskRequest::Restore(restore) => run.restore(restore).await,
        TaskRequest::EmbeddedDump(dump) => run.embedded_dump(dump).await,
        TaskRequest::DuckdbImport(import) => run.duckdb_import(&import.folder).await,
        TaskRequest::RunFile(file) => run.run_file(&file.file, file.on_error).await,
    }
}

/// How each kind of guard risk reads in a plan.
fn risk_text(risk: Risk) -> &'static str {
    match risk {
        Risk::DeleteAll => "a DELETE without WHERE removes every row",
        Risk::UpdateAll => "an UPDATE without WHERE changes every row",
        Risk::Drop => "a DROP removes an object",
        Risk::Truncate => "a TRUNCATE removes every row",
        Risk::UncheckedWrite => "a write whose WHERE could not be checked",
        Risk::DataChange => "a data change, which this connection's type asks about",
        Risk::Execution => "this connection's type asks before anything runs",
        Risk::OtherReading => {
            "text the server may read otherwise than standard SQL (a backslash before a quote, or \
             a dollar quote)"
        }
    }
}

fn shown_sql(sql: &str) -> String {
    let sql = sql.trim();
    if sql.chars().count() <= PLAN_SQL_CHARS {
        sql.to_string()
    } else {
        let cut: String = sql.chars().take(PLAN_SQL_CHARS).collect();
        format!("{cut}…")
    }
}

/// One run of a task, its connection resolved.
struct Run<'a> {
    resolved: &'a ResolvedConnection,
    env: &'a TaskEnv,
    confirmed: bool,
    cancel: &'a JobCancel,
    log: &'a RunLog<'a>,
}

impl Run<'_> {
    fn plan(&self, kind: TaskKind) -> TaskPlan {
        TaskPlan {
            kind,
            connection: self.resolved.name.clone(),
            steps: Vec::new(),
            risks: Vec::new(),
            notes: Vec::new(),
            needs_confirmation: false,
        }
    }

    /// The tunnel's dialogs, every answer remembered as a secret of this run.
    fn interaction(&self) -> Arc<dyn Interaction> {
        Arc::new(Recording {
            inner: self.env.interaction.clone(),
            redactor: self.log.redactor.clone(),
        })
    }

    fn route_options(&self) -> RouteOptions {
        RouteOptions {
            tunnels: self.env.tunnels.clone(),
            known_hosts: KnownHosts::for_user(&self.env.user_dir),
            interaction: self.interaction(),
        }
    }

    /// A new session for this run only (never a tab's), opened while a cancel can stop it.
    async fn open(&self, read_only_session: bool) -> Result<OpenConnection, Step> {
        let options = OpenOptions {
            tunnels: self.env.tunnels.clone(),
            known_hosts: KnownHosts::for_user(&self.env.user_dir),
            interaction: self.interaction(),
            password: self.env.password.clone(),
            read_only_session,
        };
        let opening = crate::open::open_connection(
            self.resolved.clone(),
            &self.env.engines,
            options,
            Arc::new(ResultStore::new()),
        );
        let opened = tokio::select! {
            opened = opening => opened,
            () = self.cancel.cancelled() => return Err(Step::Cancelled(String::new())),
        };
        match opened {
            Ok(open) => Ok(open),
            Err(OpenError::PasswordRequired { save_to }) => Err(Step::PasswordRequired {
                can_remember: save_to.is_some(),
            }),
            Err(e) => Err(Step::Failed(e.to_string())),
        }
    }

    /// Where the run stops for a yes: the plan goes back to be asked or refused.
    fn gate(&self, plan: TaskPlan) -> Result<(), Step> {
        for step in &plan.steps {
            self.log.line(&format!("Runs: {step}"));
        }
        for note in &plan.notes {
            self.log.line(&format!("Note: {note}"));
        }
        for risk in &plan.risks {
            self.log.line(&format!("Asks first: {risk}"));
        }
        if plan.needs_confirmation && !self.confirmed {
            return Err(Step::NeedsConfirmation(Box::new(plan)));
        }
        if plan.needs_confirmation {
            self.log.line("Confirmed.");
        }
        Ok(())
    }

    fn read_only(&self) -> bool {
        self.resolved.params.read_only
    }

    fn backup_events(&self) -> impl Fn(BackupEvent) + Send + Sync + '_ {
        |event| self.log.backup_event(event)
    }

    async fn export(&self, export: &ExportTask) -> Step {
        let open = match self.open(true).await {
            Ok(open) => open,
            Err(step) => return step,
        };
        let step = self.export_on(&open, export).await;
        open.close().await;
        step
    }

    async fn export_on(&self, open: &OpenConnection, export: &ExportTask) -> Step {
        let session = &open.session;
        let safeguards = self.resolved.safeguards();
        let settings = export.settings.stream();
        let database = session
            .connection()
            .current_database()
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let mut plan = self.plan(TaskKind::Export);
        let to = |table: &str, schema: &str| -> Result<PathBuf, Step> {
            name_in(
                &export.folder,
                &export.names,
                &settings.format,
                settings.gzip,
                table,
                schema,
                &database,
            )
        };
        if let Err(e) = tokio::fs::create_dir_all(&export.folder).await {
            return Step::Failed(format!("could not create {}: {e}", export.folder.display()));
        }
        match &export.source {
            ExportSource::Query { sql } => {
                if let Some(refused) = rerun_check(session, Some(sql)).refused {
                    return Step::Refused(refused);
                }
                let risks = script_risks(session.dialect().as_ref(), sql, &safeguards);
                plan.steps.push(format!(
                    "the query again, in a session of its own: {}",
                    shown_sql(sql)
                ));
                plan.steps.push(format!(
                    "every row written to a new file in {}",
                    export.folder.display()
                ));
                plan.risks = risks
                    .iter()
                    .map(|r| risk_text(r.risk).to_string())
                    .collect();
                plan.needs_confirmation = !risks.is_empty();
                if let Err(step) = self.gate(plan) {
                    return step;
                }
                let path = match to("result", "") {
                    Ok(path) => path,
                    Err(step) => return step,
                };
                let progress = |p: ExportProgress| self.export_progress(None, p);
                let end: JobEnd<_, ()> = export_query(
                    session,
                    sql,
                    &safeguards,
                    self.confirmed,
                    &path,
                    &settings,
                    self.cancel,
                    &progress,
                )
                .await
                .into();
                Step::of(
                    end,
                    |s| file_summary(s.rows, s.bytes, &path),
                    |()| String::new(),
                )
            }
            ExportSource::Table {
                schema,
                name,
                filter,
                order_by,
            } => {
                plan.steps.push(format!(
                    "every row of {}{name} written to a new file in {}",
                    if schema.is_empty() {
                        String::new()
                    } else {
                        format!("{schema}.")
                    },
                    export.folder.display()
                ));
                if let Err(step) = self.gate(plan) {
                    return step;
                }
                let path = match to(name, schema) {
                    Ok(path) => path,
                    Err(step) => return step,
                };
                let table = TableSource {
                    schema: schema.clone(),
                    name: name.clone(),
                    filter: filter.clone(),
                    order_by: order_by.clone(),
                };
                let progress = |p: ExportProgress| self.export_progress(None, p);
                let end: JobEnd<_, ()> =
                    export_table(session, &table, &path, &settings, self.cancel, &progress)
                        .await
                        .into();
                Step::of(
                    end,
                    |s| file_summary(s.rows, s.bytes, &path),
                    |()| String::new(),
                )
            }
            ExportSource::Tables { tables } => {
                plan.steps.push(format!(
                    "{} tables, a file each, written to {}",
                    tables.len(),
                    export.folder.display()
                ));
                if let Err(step) = self.gate(plan) {
                    return step;
                }
                let list: Vec<(String, String)> = tables
                    .iter()
                    .map(|t| (t.schema.clone(), t.name.clone()))
                    .collect();
                let progress =
                    |table: usize, p: ExportProgress| self.export_progress(Some(table), p);
                let database = (!database.is_empty()).then_some(database.as_str());
                let end: JobEnd<_, _> = export_tables(
                    session,
                    &list,
                    &export.folder,
                    &export.names,
                    database,
                    &settings,
                    self.cancel,
                    &progress,
                )
                .await
                .into();
                Step::of(
                    end,
                    |files| {
                        for file in &files {
                            self.log.line(&format!(
                                "Wrote {} ({} rows, {} bytes).",
                                file.path.display(),
                                file.summary.rows,
                                file.summary.bytes
                            ));
                        }
                        let rows: u64 = files.iter().map(|f| f.summary.rows).sum();
                        let bytes: u64 = files.iter().map(|f| f.summary.bytes).sum();
                        format!(
                            "{} files, {rows} rows, {bytes} bytes written to {}",
                            files.len(),
                            export.folder.display()
                        )
                    },
                    |stopped| {
                        for file in &stopped.files {
                            self.log
                                .line(&format!("Kept {} (finished).", file.path.display()));
                        }
                        format!("{} files finished before it stopped", stopped.files.len())
                    },
                )
            }
        }
    }

    fn export_progress(&self, table: Option<usize>, p: ExportProgress) {
        self.log.progress(TaskProgress::Export {
            table,
            rows: p.rows,
            bytes: p.bytes,
        });
    }

    async fn import(&self, import: &super::ImportTask) -> Step {
        if self.read_only() {
            return Step::Refused(ImportError::ReadOnly.to_string());
        }
        let open = match self.open(false).await {
            Ok(open) => open,
            Err(step) => return step,
        };
        let session = &open.session;
        let safeguards = self.resolved.safeguards();
        let request = import.request();
        let step = 'run: {
            let planned = match plan_import(session, &request, &safeguards, self.read_only()).await
            {
                Ok(planned) => planned,
                Err(e @ ImportError::ReadOnly) => break 'run Step::Refused(e.to_string()),
                Err(e) => break 'run Step::Failed(e.to_string()),
            };
            let mut plan = self.plan(TaskKind::Import);
            plan.steps
                .push(format!("rows read from {}", import.file.display()));
            plan.steps.extend(planned.statements.iter().cloned());
            plan.steps.push(planned.insert.clone());
            plan.steps.extend(planned.finish.iter().cloned());
            plan.notes.extend(
                planned
                    .leftovers
                    .iter()
                    .map(|t| format!("left behind by an earlier import: {t}")),
            );
            plan.risks = planned
                .risks
                .iter()
                .map(|(_, risk)| risk_text(*risk).to_string())
                .collect();
            plan.needs_confirmation = planned.needs_confirmation();
            if let Err(step) = self.gate(plan) {
                break 'run step;
            }
            let progress = |p: ImportProgress| {
                self.log.progress(TaskProgress::Import {
                    read: p.read,
                    committed: p.committed,
                });
            };
            let end: JobEnd<_, u64> = run_import(
                session,
                &request,
                &safeguards,
                self.read_only(),
                self.confirmed,
                self.cancel,
                &progress,
            )
            .await
            .into();
            Step::of(
                end,
                |s| {
                    format!(
                        "{} rows into {}{}{}",
                        s.rows,
                        if s.schema.is_empty() {
                            String::new()
                        } else {
                            format!("{}.", s.schema)
                        },
                        s.table,
                        if s.created_table { " (created)" } else { "" }
                    )
                },
                |committed| format!("{committed} rows committed before it stopped"),
            )
        };
        open.close().await;
        step
    }

    fn tools(&self, task: &super::TaskTools) -> ToolSource {
        self.env.tools.clone().unwrap_or_else(|| task.source())
    }

    async fn dump(&self, dump: &super::TaskDumpRequest) -> Step {
        let request = DumpRequest {
            tools: self.tools(&dump.tools),
            folder: dump.folder.clone(),
            name_pattern: dump.name_pattern.clone(),
            extra_args: dump.extra_args.clone(),
            options: dump.options.clone(),
        };
        // MySQL's dump lists the views it leaves out from the catalog.
        let catalog = match &dump.options {
            DumpOptions::Mysql(m) if !m.include_views => match self.open(true).await {
                Ok(open) => Some(open),
                Err(step) => return step,
            },
            DumpOptions::Mysql(_) | DumpOptions::Postgres(_) => None,
        };
        let session = catalog.as_ref().map(|open| &open.session);
        let step = 'run: {
            let preview = match backup::preview_dump(self.resolved, &request, session).await {
                Ok(preview) => preview,
                Err(e) => break 'run backup_error(e),
            };
            let mut plan = self.plan(TaskKind::Dump);
            plan.steps.push(format!(
                "{} ({}), writing {}",
                preview.tool.program.display(),
                preview.tool.version.text,
                preview.path.display()
            ));
            plan.notes = preview.notes;
            if let Err(step) = self.gate(plan) {
                break 'run step;
            }
            let events = self.backup_events();
            let end: JobEnd<_, ()> = backup::run_dump(
                self.resolved,
                self.env.password.clone(),
                self.route_options(),
                &request,
                session,
                self.cancel,
                &events,
            )
            .await
            .into();
            Step::of(
                end,
                |s| file_summary_bytes(s.bytes, &s.path),
                |()| String::new(),
            )
        };
        if let Some(open) = catalog {
            open.close().await;
        }
        step
    }

    async fn restore(&self, restore: &super::RestoreTask) -> Step {
        if self.read_only() {
            return Step::Refused(backup::BackupError::ReadOnly.to_string());
        }
        let request = RestoreRequest {
            tools: self.tools(&restore.tools),
            input: restore.input.clone(),
            format: restore.format,
            extra_args: restore.extra_args.clone(),
            options: restore.options.clone(),
        };
        let preview = match backup::preview_restore(self.resolved, &request).await {
            Ok(preview) => preview,
            Err(e) => return backup_error(e),
        };
        let mut plan = self.plan(TaskKind::Restore);
        plan.steps.push(format!(
            "{} ({}) restoring {} into the connection's database",
            preview.tool.program.display(),
            preview.tool.version.text,
            restore.input.display()
        ));
        plan.risks
            .push("a restore writes to the database: it always asks".into());
        plan.notes = preview.notes;
        plan.needs_confirmation = true;
        if let Err(step) = self.gate(plan) {
            return step;
        }
        let events = self.backup_events();
        let end: JobEnd<_, ()> = backup::run_restore(
            self.resolved,
            self.env.password.clone(),
            self.route_options(),
            &request,
            self.confirmed,
            self.cancel,
            &events,
        )
        .await
        .into();
        Step::of(
            end,
            |s| {
                format!(
                    "{} restored ({} bytes read)",
                    restore.input.display(),
                    s.bytes
                )
            },
            |()| String::new(),
        )
    }

    async fn embedded_dump(&self, dump: &super::TaskEmbeddedDump) -> Step {
        let request = EmbeddedDumpRequest {
            folder: dump.folder.clone(),
            name_pattern: dump.name_pattern.clone(),
            duckdb_format: dump.duckdb_format,
        };
        let path = match backup::embedded_dump_path(self.resolved, &request) {
            Ok(path) => path,
            Err(e) => return Step::Failed(e.to_string()),
        };
        let mut plan = self.plan(TaskKind::EmbeddedDump);
        plan.steps.push(format!(
            "a copy of the database written to {}",
            path.display()
        ));
        if let Err(step) = self.gate(plan) {
            return step;
        }
        let open = match self.open(true).await {
            Ok(open) => open,
            Err(step) => return step,
        };
        let events = self.backup_events();
        let end: JobEnd<_, ()> =
            backup::embedded_dump(&open.session, self.resolved, &request, self.cancel, &events)
                .await
                .into();
        open.close().await;
        Step::of(
            end,
            |s| file_summary_bytes(s.bytes, &s.path),
            |()| String::new(),
        )
    }

    async fn duckdb_import(&self, folder: &Path) -> Step {
        if self.read_only() {
            return Step::Refused(backup::BackupError::ReadOnly.to_string());
        }
        let mut plan = self.plan(TaskKind::DuckdbImport);
        plan.steps
            .push(format!("IMPORT DATABASE from {}", folder.display()));
        plan.risks
            .push("an import writes to the database: it always asks".into());
        plan.needs_confirmation = true;
        if let Err(step) = self.gate(plan) {
            return step;
        }
        let open = match self.open(false).await {
            Ok(open) => open,
            Err(step) => return step,
        };
        let events = self.backup_events();
        let end: JobEnd<_, ()> = backup::duckdb_import(
            &open.session,
            self.resolved,
            folder,
            self.confirmed,
            self.cancel,
            &events,
        )
        .await
        .into();
        open.close().await;
        Step::of(
            end,
            |elapsed| {
                format!(
                    "{} imported in {} ms",
                    folder.display(),
                    elapsed.as_millis()
                )
            },
            |()| String::new(),
        )
    }

    async fn run_file(&self, file: &Path, on_error: crate::script::OnError) -> Step {
        if self.read_only() {
            return Step::Refused(backup::BackupError::ReadOnly.to_string());
        }
        let open = match self.open(false).await {
            Ok(open) => open,
            Err(step) => return step,
        };
        let session = &open.session;
        let step = 'run: {
            let planned = match backup::plan_sql_file(session, file, &self.resolved.safeguards()) {
                Ok(planned) => planned,
                Err(e) => break 'run Step::Failed(e.to_string()),
            };
            let mut plan = self.plan(TaskKind::RunFile);
            plan.steps.push(format!(
                "{} statements of {} ({} bytes), in a session of their own",
                planned.statements,
                file.display(),
                planned.bytes
            ));
            plan.risks
                .push("running a file writes to the database: it always asks".into());
            plan.risks.extend(
                planned
                    .risks
                    .iter()
                    .map(|(risk, n)| format!("{n} × {}", risk_text(*risk))),
            );
            plan.needs_confirmation = true;
            if let Err(step) = self.gate(plan) {
                break 'run step;
            }
            let events = self.backup_events();
            let end: JobEnd<_, ()> = backup::run_sql_file(
                session,
                file,
                self.read_only(),
                self.confirmed,
                on_error,
                self.cancel,
                &events,
            )
            .await
            .into();
            Step::of(
                end,
                |s| {
                    format!(
                        "{} of {} statements run, {} failed",
                        s.done, s.total, s.failed
                    )
                },
                |()| String::new(),
            )
        };
        open.close().await;
        step
    }
}

/// The file an export of one result or table writes: the pattern rendered, the format's
/// extension, and never over an existing file.
fn name_in(
    folder: &Path,
    names: &FileNamePattern,
    format: &crate::export::ExportFormat,
    gzip: bool,
    table: &str,
    schema: &str,
    database: &str,
) -> Result<PathBuf, Step> {
    let base = names
        .render_now(&NameParts {
            table,
            schema,
            database,
        })
        .map_err(Step::Failed)?;
    Ok(free_path(folder, &base, &format.extension(gzip)))
}

/// A backup step that failed before its tool ran.
fn backup_error(error: backup::BackupError) -> Step {
    let end: JobEnd<(), ()> = Err(error).into();
    Step::of(end, |()| String::new(), |()| String::new())
}

fn file_summary(rows: u64, bytes: u64, path: &Path) -> String {
    format!("{rows} rows, {bytes} bytes written to {}", path.display())
}

fn file_summary_bytes(bytes: u64, path: &Path) -> String {
    format!("{bytes} bytes written to {}", path.display())
}
