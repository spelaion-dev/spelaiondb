//! Saved tasks (ADR-089): a thin adapter. The core writes and checks a task's document, refuses
//! one that holds a secret, resolves its connection, decides whether a run needs a yes, runs it
//! and records it; these commands only carry the dialogs' requests to it, its questions back to
//! the page and its events while it runs. Nothing here starts a task on its own.

use spelaion_desktop_macros::command;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::ImportSource;
use spelaion_core::backup::ToolSource;
use spelaion_core::task::{
    self, ExportSettings, ExportSource, ExportTask, ImportTask, Questions, RestoreTask,
    RunFileTask, Store, TableName, TaskConnection, TaskDocument, TaskDuckdbImport, TaskDumpRequest,
    TaskEmbeddedDump, TaskEnv, TaskError, TaskEvent, TaskKind, TaskOrigin, TaskOutcome, TaskPlan,
    TaskProgress, TaskRequest, TaskRun, TaskRunState, TaskTools,
};
use spelaion_driver_api::Secret;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::backup::{BackupRequestItem, EmbeddedBackupItem, RestoreRequestItem};
use crate::dto::{
    ConnectionKeyItem, ExportFileOptions, FileNamePatternItem, OnErrorItem, RelationRefItem,
    TableExportItem,
};
use crate::import::ImportRequestItem;
use crate::interaction::AppInteraction;
use crate::jobs::{self, JobKind, JobTicket};
use crate::{AppState, CmdResult};

/// Runs listed under each task in the Tasks view.
const RUNS_SHOWN: usize = 50;

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TaskKindItem {
    Export,
    Import,
    Dump,
    Restore,
    EmbeddedDump,
    DuckdbImport,
    RunFile,
}

impl From<TaskKind> for TaskKindItem {
    fn from(kind: TaskKind) -> Self {
        match kind {
            TaskKind::Export => Self::Export,
            TaskKind::Import => Self::Import,
            TaskKind::Dump => Self::Dump,
            TaskKind::Restore => Self::Restore,
            TaskKind::EmbeddedDump => Self::EmbeddedDump,
            TaskKind::DuckdbImport => Self::DuckdbImport,
            TaskKind::RunFile => Self::RunFile,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TaskOriginItem {
    App,
    Cli,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TaskRunStateItem {
    /// No ending was recorded: still running, or its program stopped.
    Unfinished,
    Succeeded,
    Failed,
    Cancelled,
    Refused,
}

impl From<TaskRunState> for TaskRunStateItem {
    fn from(state: TaskRunState) -> Self {
        match state {
            TaskRunState::Unfinished => Self::Unfinished,
            TaskRunState::Succeeded => Self::Succeeded,
            TaskRunState::Failed => Self::Failed,
            TaskRunState::Cancelled => Self::Cancelled,
            TaskRunState::Refused => Self::Refused,
        }
    }
}

/// A recorded run, from the app or the command line.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TaskRunItem {
    pub id: f64,
    pub origin: TaskOriginItem,
    pub started_ms: f64,
    pub ended_ms: Option<f64>,
    pub state: TaskRunStateItem,
    pub summary: String,
}

impl From<TaskRun> for TaskRunItem {
    fn from(run: TaskRun) -> Self {
        Self {
            id: run.id as f64,
            origin: match run.origin {
                TaskOrigin::App => TaskOriginItem::App,
                TaskOrigin::Cli => TaskOriginItem::Cli,
            },
            started_ms: run.started_ms as f64,
            ended_ms: run.ended_ms.map(|ms| ms as f64),
            state: run.state.into(),
            summary: run.summary,
        }
    }
}

/// A saved task as the Tasks view lists it. `unreadable` says why a document cannot be used;
/// such a task has no kind or connection and runs nothing.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TaskItem {
    pub id: f64,
    pub name: String,
    pub folder: String,
    pub kind: Option<TaskKindItem>,
    pub connection: Option<ConnectionKeyItem>,
    /// The file or folder it writes to or reads from, as Edit changes it.
    pub path: Option<String>,
    /// A query export's query, as Edit changes it.
    pub sql: Option<String>,
    pub unreadable: Option<String>,
    pub last_run: Option<TaskRunItem>,
}

/// What a dialog's "Save as task…" saves: the request it would run, as it sends it to run.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TaskRequestItem {
    ExportQuery {
        sql: String,
        folder: String,
        names: FileNamePatternItem,
        options: ExportFileOptions,
    },
    ExportTable {
        table: TableExportItem,
        folder: String,
        names: FileNamePatternItem,
        options: ExportFileOptions,
    },
    ExportTables {
        tables: Vec<RelationRefItem>,
        folder: String,
        names: FileNamePatternItem,
        options: ExportFileOptions,
    },
    Import {
        request: ImportRequestItem,
    },
    Dump {
        request: BackupRequestItem,
    },
    Restore {
        request: RestoreRequestItem,
    },
    EmbeddedDump {
        request: EmbeddedBackupItem,
    },
    DuckdbImport {
        folder: String,
    },
    RunFile {
        path: String,
        on_error: OnErrorItem,
    },
}

/// How saving went: a name another task has is asked about before it is replaced.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SaveTaskOutcome {
    Saved { id: f64 },
    NameTaken { id: f64 },
}

/// What the Tasks view's Edit changes. The other options are changed by saving again from the
/// dialog (ADR-089 point 10).
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TaskEditItem {
    pub name: String,
    pub folder: String,
    pub connection: ConnectionKeyItem,
    pub path: Option<String>,
    pub sql: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TaskLogItem {
    pub log: String,
    pub omitted: f64,
}

/// What a run will do, for the confirmation: written by the core.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TaskPlanItem {
    pub kind: TaskKindItem,
    pub connection: String,
    pub steps: Vec<String>,
    pub risks: Vec<String>,
    pub notes: Vec<String>,
}

impl From<TaskPlan> for TaskPlanItem {
    fn from(plan: TaskPlan) -> Self {
        Self {
            kind: plan.kind.into(),
            connection: plan.connection,
            steps: plan.steps,
            risks: plan.risks,
            notes: plan.notes,
        }
    }
}

/// How starting a task ended: it ran (recorded), or it asks first (nothing recorded).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TaskOutcomeItem {
    Ran {
        run: f64,
        state: TaskRunStateItem,
        summary: String,
        message: Option<String>,
    },
    NeedsConfirmation {
        plan: TaskPlanItem,
    },
    PasswordRequired {
        can_remember: bool,
    },
}

/// A run's live log line or progress.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TaskEventItem {
    Log {
        line: String,
    },
    Rows {
        table: Option<f64>,
        rows: f64,
        bytes: f64,
    },
    Import {
        read: f64,
        committed: f64,
    },
    Bytes {
        bytes: f64,
        total: Option<f64>,
    },
    Statements {
        done: f64,
        total: f64,
    },
}

impl From<TaskEvent> for TaskEventItem {
    fn from(event: TaskEvent) -> Self {
        match event {
            TaskEvent::Log(line) => Self::Log { line },
            TaskEvent::Progress(TaskProgress::Export { table, rows, bytes }) => Self::Rows {
                table: table.map(|t| t as f64),
                rows: rows as f64,
                bytes: bytes as f64,
            },
            TaskEvent::Progress(TaskProgress::Import { read, committed }) => Self::Import {
                read: read as f64,
                committed: committed as f64,
            },
            TaskEvent::Progress(TaskProgress::Bytes { bytes, total }) => Self::Bytes {
                bytes: bytes as f64,
                total: total.map(|t| t as f64),
            },
            TaskEvent::Progress(TaskProgress::Statements { done, total }) => Self::Statements {
                done: done as f64,
                total: total as f64,
            },
        }
    }
}

fn store(state: &AppState) -> CmdResult<Store> {
    Store::open(&spelaion_config::state_db(&state.user_dir)).map_err(|e| e.to_string())
}

fn error(e: TaskError) -> String {
    e.to_string()
}

fn id_of(id: f64) -> CmdResult<i64> {
    if id.fract() == 0.0 && id >= 1.0 && id <= i64::MAX as f64 {
        Ok(id as i64)
    } else {
        Err("not a task".into())
    }
}

fn key_item(connection: &TaskConnection) -> ConnectionKeyItem {
    ConnectionKeyItem {
        // As the listed connections name their folder (`connection_in`).
        folder: connection
            .folder
            .as_deref()
            .map(|folder| folder.display().to_string()),
        id: connection.id.clone(),
    }
}

fn item(store: &Store, saved: task::SavedTask) -> CmdResult<TaskItem> {
    let last_run = task::task_runs(store, saved.id, 1)
        .map_err(error)?
        .into_iter()
        .next()
        .map(TaskRunItem::from);
    let (kind, connection, path, sql, unreadable) = match &saved.document {
        Ok(document) => (
            Some(document.request.kind().into()),
            Some(key_item(&document.connection)),
            document.path().map(|p| p.display().to_string()),
            document.query().map(str::to_string),
            None,
        ),
        Err(reason) => (None, None, None, None, Some(reason.clone())),
    };
    Ok(TaskItem {
        id: saved.id as f64,
        name: saved.name,
        folder: saved.folder,
        kind,
        connection,
        path,
        sql,
        unreadable,
        last_run,
    })
}

/// Every saved task, by folder and name, with its last run.
#[command]
pub async fn tasks(state: State<'_, AppState>) -> CmdResult<Vec<TaskItem>> {
    let store = store(&state)?;
    task::list_tasks(&store)
        .map_err(error)?
        .into_iter()
        .map(|saved| item(&store, saved))
        .collect()
}

/// A task's newest runs, newest first.
#[command]
pub async fn task_runs(id: f64, state: State<'_, AppState>) -> CmdResult<Vec<TaskRunItem>> {
    Ok(task::task_runs(&store(&state)?, id_of(id)?, RUNS_SHOWN)
        .map_err(error)?
        .into_iter()
        .map(TaskRunItem::from)
        .collect())
}

/// A run's kept log.
#[command]
pub async fn task_run_log(run: f64, state: State<'_, AppState>) -> CmdResult<TaskLogItem> {
    let (log, omitted) = task::task_run_log(&store(&state)?, id_of(run)?).map_err(error)?;
    Ok(TaskLogItem {
        log,
        omitted: omitted as f64,
    })
}

/// The task's connection as the session `session` has it: its identity, and for a personal
/// connection the repository folders open now (ADR-089 point 4), with its resolved connection
/// and the password typed for it, the judges of what the task may hold.
async fn session_connection(
    state: &AppState,
    session: &str,
) -> CmdResult<(
    TaskConnection,
    spelaion_config::ResolvedConnection,
    Option<Secret>,
)> {
    let (resolved, password, folder) = {
        let sessions = state.sessions.lock().await;
        let source = sessions
            .get(session)
            .ok_or("the connection of this dialog is closed")?;
        (
            source.open.resolved.clone(),
            source.password.clone(),
            source.folder.clone(),
        )
    };
    let open_folders = match folder {
        Some(_) => Vec::new(),
        None => state
            .open_configs()
            .await
            .iter()
            .filter_map(|config| config.workspace_root.as_deref())
            .map(spelaion_config::canonical_folder)
            .collect(),
    };
    Ok((
        TaskConnection {
            folder,
            id: resolved.id.to_string(),
            open_folders,
        },
        resolved,
        password,
    ))
}

fn tools(source: ToolSource) -> CmdResult<TaskTools> {
    match source {
        ToolSource::Detect => Ok(TaskTools::Detect),
        ToolSource::Folder(path) => Ok(TaskTools::Folder { path }),
        ToolSource::Launcher { .. } => {
            Err("a task finds its tools, it never names a program".into())
        }
    }
}

fn export(
    source: ExportSource,
    folder: String,
    names: FileNamePatternItem,
    options: &ExportFileOptions,
    table: &str,
) -> CmdResult<TaskRequest> {
    let settings = crate::stream_export(options, table)?;
    Ok(TaskRequest::Export(ExportTask {
        source,
        settings: ExportSettings::of(&settings),
        folder: PathBuf::from(folder.trim()),
        names: names.into(),
    }))
}

fn request(item: TaskRequestItem) -> CmdResult<TaskRequest> {
    match item {
        TaskRequestItem::ExportQuery {
            sql,
            folder,
            names,
            options,
        } => export(
            ExportSource::Query { sql },
            folder,
            names,
            &options,
            crate::INSERT_TABLE,
        ),
        TaskRequestItem::ExportTable {
            table,
            folder,
            names,
            options,
        } => {
            let name = table.name.clone();
            export(
                ExportSource::Table {
                    schema: table.schema,
                    name: table.name,
                    filter: table.filter,
                    order_by: table.order_by,
                },
                folder,
                names,
                &options,
                &name,
            )
        }
        TaskRequestItem::ExportTables {
            tables,
            folder,
            names,
            options,
        } => export(
            ExportSource::Tables {
                tables: tables
                    .into_iter()
                    .map(|t| TableName {
                        schema: t.schema,
                        name: t.name,
                    })
                    .collect(),
            },
            folder,
            names,
            &options,
            "",
        ),
        TaskRequestItem::Import { request } => {
            let request: spelaion_core::ImportRequest = request.try_into()?;
            let ImportSource::File(file) = request.source else {
                return Err("pasted text cannot be saved as a task: import from a file".into());
            };
            Ok(TaskRequest::Import(ImportTask {
                file,
                options: request.options,
                target: request.target,
                columns: request.columns,
                commit_every: request.commit_every,
            }))
        }
        TaskRequestItem::Dump { request } => {
            let request: spelaion_core::backup::DumpRequest = request.into();
            Ok(TaskRequest::Dump(TaskDumpRequest {
                tools: tools(request.tools)?,
                folder: request.folder,
                name_pattern: request.name_pattern,
                extra_args: request.extra_args,
                options: request.options,
            }))
        }
        TaskRequestItem::Restore { request } => {
            let request: spelaion_core::backup::RestoreRequest = request.into();
            Ok(TaskRequest::Restore(RestoreTask {
                tools: tools(request.tools)?,
                input: request.input,
                format: request.format,
                extra_args: request.extra_args,
                options: request.options,
            }))
        }
        TaskRequestItem::EmbeddedDump { request } => {
            let request: spelaion_core::backup::EmbeddedDumpRequest = request.into();
            Ok(TaskRequest::EmbeddedDump(TaskEmbeddedDump {
                folder: request.folder,
                name_pattern: request.name_pattern,
                duckdb_format: request.duckdb_format,
            }))
        }
        TaskRequestItem::DuckdbImport { folder } => {
            Ok(TaskRequest::DuckdbImport(TaskDuckdbImport {
                folder: PathBuf::from(folder.trim()),
            }))
        }
        TaskRequestItem::RunFile { path, on_error } => Ok(TaskRequest::RunFile(RunFileTask {
            file: PathBuf::from(path.trim()),
            on_error: on_error.into(),
        })),
    }
}

/// Saves a dialog's request as the task `name` in `folder`, on the connection of `session`
/// (ADR-089 point 10). A name another task has comes back as `nameTaken` unless `replace` is
/// that task's id, which the page passes once the user agreed to replace it.
#[command]
pub async fn save_task(
    session: String,
    name: String,
    folder: String,
    replace: Option<f64>,
    request: TaskRequestItem,
    state: State<'_, AppState>,
) -> CmdResult<SaveTaskOutcome> {
    let (connection, resolved, password) = session_connection(&state, &session).await?;
    let document = TaskDocument::new(connection, self::request(request)?);
    let secrets = task::secrets_of(&resolved, password.as_ref());
    let store = store(&state)?;
    let replace = replace.map(id_of).transpose()?;
    if replace.is_none()
        && let Some(taken) = store.task_named(name.trim()).map_err(|e| e.to_string())?
    {
        return Ok(SaveTaskOutcome::NameTaken {
            id: taken.id as f64,
        });
    }
    let id =
        task::save_task(&store, replace, &name, &folder, &document, &secrets).map_err(error)?;
    Ok(SaveTaskOutcome::Saved { id: id as f64 })
}

/// Changes a task's name, folder, connection, path or query (the Tasks view's Edit), checked
/// again as when it was saved, with the new connection's secrets as the judge.
#[command]
pub async fn edit_task(id: f64, edit: TaskEditItem, state: State<'_, AppState>) -> CmdResult<()> {
    let id = id_of(id)?;
    let store = store(&state)?;
    let saved = task::task(&store, id).map_err(error)?;
    let document = saved
        .document
        .map_err(|reason| format!("this task cannot be read: {reason}"))?;
    let (config, connection_id) = state.connection_config(&edit.connection).await?;
    let key = spelaion_config::ConnectionKey::of(&config, &connection_id);
    let open_folders = match key.folder() {
        Some(_) => Vec::new(),
        None => state
            .open_configs()
            .await
            .iter()
            .filter_map(|config| config.workspace_root.as_deref())
            .map(spelaion_config::canonical_folder)
            .collect(),
    };
    let connection = TaskConnection {
        folder: key.folder().map(std::path::Path::to_path_buf),
        id: connection_id.to_string(),
        open_folders,
    };
    let (resolved, _) = task::resolve_connection(&state.user_dir, &connection)?;
    let edited = document
        .edited(
            connection,
            edit.path.as_deref().map(|p| PathBuf::from(p.trim())),
            edit.sql,
        )
        .map_err(error)?;
    task::save_task(
        &store,
        Some(id),
        &edit.name,
        &edit.folder,
        &edited,
        &task::secrets_of(&resolved, None),
    )
    .map_err(error)?;
    Ok(())
}

/// A copy of a task under the next free "(copy)" name, without its runs.
#[command]
pub async fn duplicate_task(id: f64, state: State<'_, AppState>) -> CmdResult<f64> {
    Ok(task::duplicate_task(&store(&state)?, id_of(id)?).map_err(error)? as f64)
}

/// Deletes a task and its runs.
#[command]
pub async fn delete_task(id: f64, state: State<'_, AppState>) -> CmdResult<()> {
    store(&state)?
        .delete_task(id_of(id)?)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Runs a task now, as the Tasks view's Run: the core asks back for a password or a yes
/// (nothing is recorded until the run starts with the answer), runs it in a session of its
/// own and records it. `password` is what the password dialog got, kept for this run only.
#[command]
#[allow(clippy::too_many_arguments)]
pub async fn run_task(
    id: f64,
    job: Channel<JobTicket>,
    password: Option<String>,
    confirmed: bool,
    on_event: Channel<TaskEventItem>,
    window: tauri::Window,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<TaskOutcomeItem> {
    let id = id_of(id)?;
    let running = jobs::start(&state, JobKind::Task, &job);
    let cancel = running.cancel().clone();
    let env = TaskEnv {
        user_dir: state.user_dir.clone(),
        engines: state.engines(),
        tunnels: state.tunnels.clone(),
        // The tunnel's questions go to the window that started the run.
        interaction: Arc::new(AppInteraction::for_window(
            app.clone(),
            state.pending.clone(),
            window.label(),
        )),
        password: password.map(Secret::new),
        questions: Questions::Ask,
        tools: None,
    };
    let events = move |event: TaskEvent| {
        let _ = on_event.send(event.into());
    };
    let outcome = task::run_task(id, TaskOrigin::App, &env, confirmed, &cancel, &events).await;
    drop(running);
    Ok(match outcome.map_err(error)? {
        TaskOutcome::Ran {
            run,
            state,
            summary,
            message,
        } => TaskOutcomeItem::Ran {
            run: run as f64,
            state: state.into(),
            summary,
            message,
        },
        TaskOutcome::NeedsConfirmation(plan) => {
            TaskOutcomeItem::NeedsConfirmation { plan: plan.into() }
        }
        TaskOutcome::PasswordRequired { can_remember } => {
            TaskOutcomeItem::PasswordRequired { can_remember }
        }
    })
}
