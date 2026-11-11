//! Backup and restore through the tunnel (ADR-049): the vendors' own tools (`pg_dump`,
//! `pg_restore`, `psql`, `mysqldump`, `mysql`), found by the core and pointed at the connection's
//! route (the SSH tunnel's or Kubernetes forward's local end). They run with an argument list,
//! never through a shell, and the password only in their environment; every line they print is
//! redacted before it leaves the core. A dump is written to `<name>.part` and renamed when the
//! tool succeeds; a restore is a write that read-only connections refuse and that runs only
//! confirmed.

mod args;
mod definer;
mod name;
mod options;
mod process;
pub mod tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use spelaion_config::{Engine, Password, ResolvedConnection};
use spelaion_driver_api::{ObjectKind, Secret};

pub use args::split_extra_args;
pub use name::{DEFAULT_NAME_PATTERN, render_file_name};
pub use options::{
    ColumnStatistics, DumpContent, DumpOptions, DumpRequest, MysqlConsistency, MysqlDumpOptions,
    MysqlRestoreOptions, PgDumpOptions, PgFormat, PgRestoreOptions, RestoreFormat, RestoreOptions,
    RestoreRequest,
};
pub use process::{BackupCancel, BackupEvent};
pub use tools::{Tool, ToolError, ToolFlavor, ToolName, ToolSource, ToolVersion, locate};

use crate::conn_test::{Recording, learn_connection_secrets};
use crate::open::{OpenError, RouteOptions, open_route};
use crate::{Redactor, Session};
use args::Endpoint;
use process::{Input, Output, RunEnd, ToolRun, run_tool};

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("{0}")]
    Tool(#[from] ToolError),
    /// Read-only mode (ADR-008) refuses every write, a restore included.
    #[error("this connection is read-only: a restore writes to the database")]
    ReadOnly,
    /// A restore or a file run that was not confirmed.
    #[error("confirm the restore first")]
    NeedsConfirmation,
    #[error("{0}")]
    Open(#[from] OpenError),
    #[error("could not {action} {path}: {message}")]
    Io {
        action: &'static str,
        path: String,
        message: String,
    },
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Failed(String),
    #[error("the run was cancelled")]
    Cancelled,
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> BackupError {
    let path = path.display().to_string();
    move |e| BackupError::Io {
        action,
        path,
        message: e.to_string(),
    }
}

/// Which client tools an engine's backups use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFamily {
    Postgres,
    Mysql,
}

impl ToolFamily {
    /// The family of `engine`, or `None` for engines backed up without a client tool.
    pub fn of(engine: Engine) -> Option<Self> {
        match engine {
            Engine::Postgres => Some(ToolFamily::Postgres),
            Engine::Mysql | Engine::Mariadb => Some(ToolFamily::Mysql),
            Engine::Sqlite | Engine::Duckdb | Engine::Sqlserver => None,
        }
    }

    /// Every tool of the family, in the order the dialog lists them.
    pub fn tools(self) -> &'static [ToolName] {
        match self {
            ToolFamily::Postgres => &[ToolName::PgDump, ToolName::PgRestore, ToolName::Psql],
            ToolFamily::Mysql => &[ToolName::Mysqldump, ToolName::Mysql],
        }
    }
}

fn family(resolved: &ResolvedConnection) -> Result<ToolFamily, BackupError> {
    ToolFamily::of(resolved.engine).ok_or_else(|| {
        BackupError::Invalid(format!(
            "{} databases are not backed up with a client tool",
            resolved.engine.as_str()
        ))
    })
}

/// The flavor of client preferred for a connection: MariaDB's for MariaDB.
fn preferred_flavor(engine: Engine) -> Option<ToolFlavor> {
    match engine {
        Engine::Mariadb => Some(ToolFlavor::Mariadb),
        Engine::Mysql => Some(ToolFlavor::Mysql),
        Engine::Postgres => Some(ToolFlavor::Postgres),
        Engine::Sqlite | Engine::Duckdb | Engine::Sqlserver => None,
    }
}

/// The tools found for a connection's engine, each one or why it is missing.
pub async fn find_tools(
    engine: Engine,
    source: &ToolSource,
) -> Vec<(ToolName, Result<Tool, ToolError>)> {
    let Some(family) = ToolFamily::of(engine) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in family.tools() {
        out.push((*name, locate(*name, source, preferred_flavor(engine)).await));
    }
    out
}

/// What the dialog shows before a dump runs.
#[derive(Debug, Clone)]
pub struct DumpPreview {
    pub tool: Tool,
    /// The file (or folder, for PostgreSQL's directory format) the dump will be.
    pub path: PathBuf,
    /// Things to know: a check the tunnel prevents, a tool older than the server.
    pub notes: Vec<String>,
}

/// What a finished dump wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpSummary {
    pub path: PathBuf,
    pub bytes: u64,
    pub elapsed: Duration,
}

/// What the dialog shows before a restore runs (it is the confirmation).
#[derive(Debug, Clone)]
pub struct RestorePreview {
    pub tool: Tool,
    /// The input's format, read from the file when the dialog said "auto".
    pub format: RestoreFormat,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSummary {
    /// Bytes of the file fed to the tool (0 for a directory archive).
    pub bytes: u64,
    pub elapsed: Duration,
}

fn database_of(resolved: &ResolvedConnection, options: &DumpOptions) -> String {
    match options {
        DumpOptions::Postgres(_) => resolved.params.database.clone(),
        DumpOptions::Mysql(m) => match m.databases.as_slice() {
            [one] => one.clone(),
            [] => resolved.params.database.clone(),
            many => many.join("+"),
        },
    }
}

fn extension(options: &DumpOptions) -> &'static str {
    match options {
        DumpOptions::Postgres(pg) => match (pg.format, pg.compression) {
            (PgFormat::Plain, Some(level)) if level > 0 => ".sql.gz",
            (PgFormat::Plain, _) => ".sql",
            (PgFormat::Custom, _) => ".dump",
            (PgFormat::Tar, _) => ".tar",
            (PgFormat::Directory, _) => "",
        },
        DumpOptions::Mysql(_) => ".sql",
    }
}

fn routed(resolved: &ResolvedConnection) -> bool {
    resolved.tunnel.is_some() || resolved.kubernetes.is_some()
}

/// Finds the dump's tool and writes the file's path; `catalog` (a session of the connection)
/// lets it compare `pg_dump`'s version with the server's.
pub async fn preview_dump(
    resolved: &ResolvedConnection,
    request: &DumpRequest,
    catalog: Option<&Session>,
) -> Result<DumpPreview, BackupError> {
    preview_dump_at(resolved, request, catalog, Local::now()).await
}

async fn preview_dump_at(
    resolved: &ResolvedConnection,
    request: &DumpRequest,
    catalog: Option<&Session>,
    now: DateTime<Local>,
) -> Result<DumpPreview, BackupError> {
    let family = family(resolved)?;
    let name = match (&request.options, family) {
        (DumpOptions::Postgres(_), ToolFamily::Postgres) => ToolName::PgDump,
        (DumpOptions::Mysql(_), ToolFamily::Mysql) => ToolName::Mysqldump,
        (DumpOptions::Postgres(_), ToolFamily::Mysql)
        | (DumpOptions::Mysql(_), ToolFamily::Postgres) => {
            return Err(BackupError::Invalid(
                "these options are for another engine".into(),
            ));
        }
    };
    let tool = locate(name, &request.tools, preferred_flavor(resolved.engine)).await?;
    let database = database_of(resolved, &request.options);
    let file = render_file_name(&request.name_pattern, &database, &resolved.name, now)
        .map_err(BackupError::Invalid)?;
    if request.folder.as_os_str().is_empty() {
        return Err(BackupError::Invalid(
            "choose the folder to write the backup to".into(),
        ));
    }
    let path = request
        .folder
        .join(format!("{file}{}", extension(&request.options)));
    let mut notes = Vec::new();
    if let DumpOptions::Mysql(_) = request.options
        && routed(resolved)
        && resolved.params.tls == spelaion_driver_api::TlsMode::VerifyFull
    {
        notes.push(
            "the server's certificate name cannot be checked through the tunnel's local port: \
             only its authority is checked"
                .into(),
        );
    }
    if family == ToolFamily::Postgres
        && let Some(session) = catalog
        && let Ok(server) = session.connection().server_version().await
        && let Some(major) = server
            .split(|c: char| !c.is_ascii_digit())
            .find(|p| !p.is_empty())
            .and_then(|p| p.parse::<u32>().ok())
        && tool.version.number.0 < major
    {
        notes.push(format!(
            "{} is older than the server (PostgreSQL {major}): it will refuse to dump; choose a \
             folder with pg_dump {major} or newer",
            tool.version.text
        ));
    }
    Ok(DumpPreview { tool, path, notes })
}

/// The redactor for a run: the connection's and its tunnel's secrets, the password used, and
/// whatever the tunnel's dialogs are answered with.
fn redactor_for(resolved: &ResolvedConnection, password: Option<&Secret>) -> Arc<Redactor> {
    let redactor = Arc::new(Redactor::new());
    learn_connection_secrets(&redactor, resolved);
    if let Some(password) = password {
        redactor.add(password.expose());
    }
    redactor
}

/// The password the tool gets, or the error that asks for it.
fn password_for(
    resolved: &ResolvedConnection,
    typed: Option<Secret>,
) -> Result<Option<Secret>, BackupError> {
    if let Password::Prompt { save_to } = &resolved.password
        && typed.is_none()
    {
        return Err(OpenError::PasswordRequired {
            save_to: save_to.clone(),
        }
        .into());
    }
    Ok(resolved.connect_params(typed).password)
}

fn endpoint(resolved: &ResolvedConnection, route: &crate::open::Route, tool: &Tool) -> Endpoint {
    let params = &resolved.params;
    Endpoint {
        connect_host: tool
            .reach_host
            .clone()
            .unwrap_or_else(|| route.host.clone()),
        port: route.port,
        server_name: route.forwarded.then(|| params.host.clone()),
        user: params.user.clone(),
        database: params.database.clone(),
        tls: params.tls,
        ca_certificate: params.ca_certificate.clone(),
        client_certificate: params.client_certificate.clone(),
        client_key: params.client_key.clone(),
        connect_timeout_secs: params.connect_timeout().as_secs().max(1),
    }
}

/// The views of the chosen databases, for `mysqldump --ignore-table` when views are left out.
async fn mysql_views(
    session: &Session,
    databases: &[String],
) -> Result<Vec<(String, String)>, BackupError> {
    let mut views = Vec::new();
    for db in databases {
        let objects = session
            .connection()
            .list_objects(db)
            .await
            .map_err(|e| BackupError::Failed(e.to_string()))?;
        views.extend(
            objects
                .into_iter()
                .filter(|o| matches!(o.kind, ObjectKind::View))
                .map(|o| (db.clone(), o.name)),
        );
    }
    Ok(views)
}

fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

async fn remove_path(path: &Path) {
    if tokio::fs::metadata(path).await.is_ok_and(|m| m.is_dir()) {
        let _ = tokio::fs::remove_dir_all(path).await;
    } else {
        let _ = tokio::fs::remove_file(path).await;
    }
}

async fn size_of(path: &Path) -> u64 {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let mut total = 0;
    if let Ok(mut entries) = tokio::fs::read_dir(path).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            total += entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// Dumps the connection's database to a file with the vendor's tool, through its route.
/// `catalog` is a session of the connection, used to list the views a MySQL dump leaves out.
pub async fn run_dump(
    resolved: &ResolvedConnection,
    password: Option<Secret>,
    route_options: RouteOptions,
    request: &DumpRequest,
    catalog: Option<&Session>,
    cancel: &BackupCancel,
    events: &(dyn Fn(BackupEvent) + Send + Sync),
) -> Result<DumpSummary, BackupError> {
    let started = Instant::now();
    let preview = preview_dump(resolved, request, catalog).await?;
    let extra = split_extra_args(&request.extra_args).map_err(BackupError::Invalid)?;
    let password = password_for(resolved, password)?;
    let redactor = redactor_for(resolved, password.as_ref());
    let log = |line: String| {
        events(BackupEvent::Log {
            line: redactor.redact(&line),
        })
    };
    for note in &preview.notes {
        log(format!("Note: {note}"));
    }
    tokio::fs::create_dir_all(&request.folder)
        .await
        .map_err(io_error("create the folder", &request.folder))?;
    if tokio::fs::symlink_metadata(&preview.path).await.is_ok() {
        return Err(BackupError::Invalid(format!(
            "{} already exists: change the file name",
            preview.path.display()
        )));
    }
    let part = part_path(&preview.path);
    remove_path(&part).await;

    let views = match &request.options {
        DumpOptions::Mysql(m) if !m.include_views => {
            let session = catalog.ok_or_else(|| {
                BackupError::Invalid("leaving views out needs the connection's catalog".into())
            })?;
            let databases = if m.databases.is_empty() {
                vec![resolved.params.database.clone()]
            } else {
                m.databases.clone()
            };
            mysql_views(session, &databases).await?
        }
        DumpOptions::Mysql(_) | DumpOptions::Postgres(_) => Vec::new(),
    };

    let route = open_with_cancel(resolved, route_options, &redactor, cancel).await?;
    let endpoint = endpoint(resolved, &route, &preview.tool);
    let tool = &preview.tool;
    let run = match dump_run(
        &endpoint,
        tool,
        &request.options,
        &views,
        resolved.engine == Engine::Mariadb,
        &part,
        &extra,
        password,
    ) {
        Ok(run) => run,
        Err(e) => {
            route.close().await;
            return Err(e);
        }
    };
    let ended = run_tool(run, &redactor, cancel, events).await;
    route.close().await;
    let ended = match ended {
        Ok(ended) => ended,
        Err(e) => {
            remove_path(&part).await;
            return Err(BackupError::Failed(format!(
                "{} could not be started: {e}",
                tool.program.display()
            )));
        }
    };
    match ended {
        RunEnd::Done => {
            tokio::fs::rename(&part, &preview.path)
                .await
                .map_err(io_error("rename the backup to", &preview.path))?;
            Ok(DumpSummary {
                bytes: size_of(&preview.path).await,
                path: preview.path,
                elapsed: started.elapsed(),
            })
        }
        RunEnd::Failed { message } => {
            remove_path(&part).await;
            Err(BackupError::Failed(message))
        }
        RunEnd::Cancelled => {
            remove_path(&part).await;
            Err(BackupError::Cancelled)
        }
    }
}

/// The dump's command: arguments without the password, which goes in the tool's environment.
#[allow(clippy::too_many_arguments)]
fn dump_run(
    endpoint: &Endpoint,
    tool: &Tool,
    options: &DumpOptions,
    views: &[(String, String)],
    server_is_mariadb: bool,
    part: &Path,
    extra: &[String],
    password: Option<Secret>,
) -> Result<ToolRun, BackupError> {
    let (args, output) = match options {
        DumpOptions::Postgres(pg) if pg.format == PgFormat::Directory => (
            args::pg_dump_args(endpoint, pg, Some(part), extra),
            Output::Log,
        ),
        DumpOptions::Postgres(pg) => (
            args::pg_dump_args(endpoint, pg, None, extra),
            Output::File {
                path: part.to_path_buf(),
                strip_definer: false,
            },
        ),
        DumpOptions::Mysql(m) => {
            let (args, _) =
                args::mysqldump_args(endpoint, &tool.version, m, views, server_is_mariadb, extra)
                    .map_err(BackupError::Invalid)?;
            (
                args,
                Output::File {
                    path: part.to_path_buf(),
                    strip_definer: m.strip_definer,
                },
            )
        }
    };
    Ok(ToolRun {
        program: tool.program.clone(),
        args: [tool.leading_args.clone(), args].concat(),
        env: password
            .map(|p| (args::password_variable(tool.version.flavor), p))
            .into_iter()
            .collect(),
        input: Input::None,
        output,
    })
}

/// Opens the route unless the run is cancelled first; SSH dialogs' answers are redacted too.
async fn open_with_cancel(
    resolved: &ResolvedConnection,
    options: RouteOptions,
    redactor: &Arc<Redactor>,
    cancel: &BackupCancel,
) -> Result<crate::open::Route, BackupError> {
    let options = RouteOptions {
        interaction: Arc::new(Recording {
            inner: options.interaction,
            redactor: redactor.clone(),
        }),
        ..options
    };
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(BackupError::Cancelled),
        route = open_route(resolved, options) => Ok(route?),
    }
}

/// Reads a restore input's format: a folder with `toc.dat` is a directory archive, `PGDMP` starts
/// a custom archive, `ustar` at byte 257 marks a tar archive, anything else is SQL.
pub fn detect_restore_format(path: &Path) -> Result<RestoreFormat, BackupError> {
    let meta = std::fs::metadata(path).map_err(io_error("read", path))?;
    if meta.is_dir() {
        return if path.join("toc.dat").is_file() {
            Ok(RestoreFormat::Directory)
        } else {
            Err(BackupError::Invalid(format!(
                "{} is a folder without a pg_dump archive (toc.dat)",
                path.display()
            )))
        };
    }
    use std::io::Read;
    let mut head = [0u8; 512];
    let mut file = std::fs::File::open(path).map_err(io_error("read", path))?;
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) => return Err(io_error("read", path)(e)),
        }
    }
    let head = &head[..read];
    Ok(if head.starts_with(b"PGDMP") {
        RestoreFormat::Custom
    } else if head.len() >= 262 && &head[257..262] == b"ustar" {
        RestoreFormat::Tar
    } else {
        RestoreFormat::Plain
    })
}

fn resolve_format(
    family: ToolFamily,
    request: &RestoreRequest,
) -> Result<RestoreFormat, BackupError> {
    let detected = match request.format {
        RestoreFormat::Auto => detect_restore_format(&request.input)?,
        chosen => {
            std::fs::metadata(&request.input).map_err(io_error("read", &request.input))?;
            chosen
        }
    };
    match (family, detected) {
        (ToolFamily::Mysql, RestoreFormat::Plain) | (ToolFamily::Postgres, _) => Ok(detected),
        (ToolFamily::Mysql, _) => Err(BackupError::Invalid(
            "this file is a PostgreSQL archive, not a SQL file".into(),
        )),
    }
}

/// Finds the restore's tool for the file's format, and what the confirmation must say.
pub async fn preview_restore(
    resolved: &ResolvedConnection,
    request: &RestoreRequest,
) -> Result<RestorePreview, BackupError> {
    let family = family(resolved)?;
    let format = resolve_format(family, request)?;
    let name = match (family, format) {
        (ToolFamily::Postgres, RestoreFormat::Plain) => ToolName::Psql,
        (ToolFamily::Postgres, _) => ToolName::PgRestore,
        (ToolFamily::Mysql, _) => ToolName::Mysql,
    };
    let tool = locate(name, &request.tools, preferred_flavor(resolved.engine)).await?;
    let mut notes = Vec::new();
    match (&request.options, name) {
        (RestoreOptions::Mysql(m), ToolName::Mysql) => {
            let probe = Endpoint {
                connect_host: "127.0.0.1".into(),
                port: 0,
                server_name: routed(resolved).then(|| resolved.params.host.clone()),
                user: String::new(),
                database: resolved.params.database.clone(),
                tls: resolved.params.tls,
                ca_certificate: None,
                client_certificate: None,
                client_key: None,
                connect_timeout_secs: 1,
            };
            let (_, more) = args::mysql_restore_args(&probe, &tool.version, m, &[])
                .map_err(BackupError::Invalid)?;
            notes.extend(more);
        }
        (RestoreOptions::Postgres(_), ToolName::Psql) => notes.push(
            "psql runs the file's backslash commands, \\! (a shell command) included: restore \
             only a file you trust"
                .into(),
        ),
        (RestoreOptions::Postgres(_), ToolName::PgRestore) => {}
        (
            RestoreOptions::Mysql(_),
            ToolName::Psql | ToolName::PgRestore | ToolName::PgDump | ToolName::Mysqldump,
        )
        | (RestoreOptions::Postgres(_), ToolName::Mysql | ToolName::PgDump | ToolName::Mysqldump) =>
        {
            return Err(BackupError::Invalid(
                "these options are for another engine".into(),
            ));
        }
    }
    Ok(RestorePreview {
        tool,
        format,
        notes,
    })
}

/// Restores a file into the connection's database with the vendor's tool, through its route.
/// Refused on a read-only connection, and unless `confirmed` (the dialog is the confirmation).
pub async fn run_restore(
    resolved: &ResolvedConnection,
    password: Option<Secret>,
    route_options: RouteOptions,
    request: &RestoreRequest,
    confirmed: bool,
    cancel: &BackupCancel,
    events: &(dyn Fn(BackupEvent) + Send + Sync),
) -> Result<RestoreSummary, BackupError> {
    if resolved.params.read_only {
        return Err(BackupError::ReadOnly);
    }
    if !confirmed {
        return Err(BackupError::NeedsConfirmation);
    }
    let started = Instant::now();
    let preview = preview_restore(resolved, request).await?;
    let extra = split_extra_args(&request.extra_args).map_err(BackupError::Invalid)?;
    let password = password_for(resolved, password)?;
    let redactor = redactor_for(resolved, password.as_ref());
    let route = open_with_cancel(resolved, route_options, &redactor, cancel).await?;
    let endpoint = endpoint(resolved, &route, &preview.tool);
    let tool = &preview.tool;
    let fed = preview.format != RestoreFormat::Directory;
    let run = match restore_run(&endpoint, tool, request, preview.format, &extra, password) {
        Ok(run) => run,
        Err(e) => {
            route.close().await;
            return Err(e);
        }
    };
    for note in &preview.notes {
        events(BackupEvent::Log {
            line: redactor.redact(&format!("Note: {note}")),
        });
    }
    let ended = run_tool(run, &redactor, cancel, events).await;
    route.close().await;
    match ended.map_err(|e| {
        BackupError::Failed(format!(
            "{} could not be started: {e}",
            tool.program.display()
        ))
    })? {
        RunEnd::Done => Ok(RestoreSummary {
            bytes: if fed {
                size_of(&request.input).await
            } else {
                0
            },
            elapsed: started.elapsed(),
        }),
        RunEnd::Failed { message } => Err(BackupError::Failed(message)),
        RunEnd::Cancelled => Err(BackupError::Cancelled),
    }
}

/// The restore's command: arguments without the password, which goes in the tool's environment.
fn restore_run(
    endpoint: &Endpoint,
    tool: &Tool,
    request: &RestoreRequest,
    format: RestoreFormat,
    extra: &[String],
    password: Option<Secret>,
) -> Result<ToolRun, BackupError> {
    let file = || Input::File(request.input.clone());
    let (args, input) = match (&request.options, format) {
        (RestoreOptions::Postgres(_), RestoreFormat::Plain) => {
            (args::psql_args(endpoint, extra), file())
        }
        (RestoreOptions::Postgres(pg), RestoreFormat::Directory) => (
            args::pg_restore_args(
                endpoint,
                pg,
                PgFormat::Directory,
                Some(&request.input),
                extra,
            ),
            Input::None,
        ),
        (RestoreOptions::Postgres(pg), RestoreFormat::Custom) => (
            args::pg_restore_args(endpoint, pg, PgFormat::Custom, None, extra),
            file(),
        ),
        (RestoreOptions::Postgres(pg), RestoreFormat::Tar) => (
            args::pg_restore_args(endpoint, pg, PgFormat::Tar, None, extra),
            file(),
        ),
        (RestoreOptions::Mysql(m), RestoreFormat::Plain) => {
            let (args, _) = args::mysql_restore_args(endpoint, &tool.version, m, extra)
                .map_err(BackupError::Invalid)?;
            (args, file())
        }
        (RestoreOptions::Postgres(_), RestoreFormat::Auto)
        | (RestoreOptions::Mysql(_), RestoreFormat::Auto)
        | (
            RestoreOptions::Mysql(_),
            RestoreFormat::Custom | RestoreFormat::Directory | RestoreFormat::Tar,
        ) => {
            return Err(BackupError::Invalid(
                "this file cannot be restored with these options".into(),
            ));
        }
    };
    Ok(ToolRun {
        program: tool.program.clone(),
        args: [tool.leading_args.clone(), args].concat(),
        env: password
            .map(|p| (args::password_variable(tool.version.flavor), p))
            .into_iter()
            .collect(),
        input,
        output: Output::Log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_formats_are_read_from_the_file() {
        let dir = std::env::temp_dir().join(format!("spelaion-detect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let custom = dir.join("a.dump");
        std::fs::write(&custom, b"PGDMP\x01\x0f").unwrap();
        assert_eq!(
            detect_restore_format(&custom).unwrap(),
            RestoreFormat::Custom
        );
        let mut tar = vec![0u8; 600];
        tar[257..262].copy_from_slice(b"ustar");
        let tar_path = dir.join("a.tar");
        std::fs::write(&tar_path, &tar).unwrap();
        assert_eq!(
            detect_restore_format(&tar_path).unwrap(),
            RestoreFormat::Tar
        );
        let sql = dir.join("a.sql");
        std::fs::write(&sql, b"-- dump\nCREATE TABLE t (a int);\n").unwrap();
        assert_eq!(detect_restore_format(&sql).unwrap(), RestoreFormat::Plain);
        let folder = dir.join("archive");
        std::fs::create_dir_all(&folder).unwrap();
        assert!(detect_restore_format(&folder).is_err());
        std::fs::write(folder.join("toc.dat"), b"PGDMP").unwrap();
        assert_eq!(
            detect_restore_format(&folder).unwrap(),
            RestoreFormat::Directory
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    const PASSWORD: &str = "pa55-'\"; DROP-s3cret";

    fn tool(name: ToolName, version: &str) -> Tool {
        Tool {
            name,
            program: PathBuf::from("/usr/bin/tool"),
            leading_args: Vec::new(),
            version: ToolVersion::parse(name, version),
            reach_host: None,
        }
    }

    fn endpoint() -> Endpoint {
        Endpoint {
            connect_host: "127.0.0.1".into(),
            port: 41000,
            server_name: Some("db.internal".into()),
            user: "app".into(),
            database: "shop".into(),
            tls: spelaion_driver_api::TlsMode::Prefer,
            ca_certificate: None,
            client_certificate: None,
            client_key: None,
            connect_timeout_secs: 15,
        }
    }

    fn assert_env_only(run: &ToolRun, variable: &str) {
        for arg in &run.args {
            assert!(
                !arg.contains(PASSWORD),
                "the password is in an argument: {arg}"
            );
            assert!(
                !arg.contains("s3cret"),
                "part of the password is in an argument: {arg}"
            );
        }
        assert_eq!(run.env.len(), 1);
        assert_eq!(run.env[0].0, variable);
        assert_eq!(run.env[0].1.expose(), PASSWORD);
        let shown = process::command_line(&run.program, &run.args);
        assert!(!shown.contains("s3cret"), "{shown}");
    }

    #[test]
    fn the_password_goes_in_the_environment_never_in_the_arguments() {
        let secret = || Some(Secret::new(PASSWORD.to_string()));
        let pg = tool(ToolName::PgDump, "pg_dump (PostgreSQL) 17.11");
        let part = Path::new("/backups/shop.dump.part");
        for format in [
            PgFormat::Plain,
            PgFormat::Custom,
            PgFormat::Directory,
            PgFormat::Tar,
        ] {
            let options = DumpOptions::Postgres(PgDumpOptions {
                format,
                ..PgDumpOptions::default()
            });
            let run =
                dump_run(&endpoint(), &pg, &options, &[], false, part, &[], secret()).unwrap();
            assert_env_only(&run, "PGPASSWORD");
        }
        let my = tool(ToolName::Mysqldump, "mysqldump  Ver 8.4.13 for Linux");
        let maria = tool(
            ToolName::Mysqldump,
            "mariadb-dump from 11.4.13-MariaDB, client 10.19",
        );
        for t in [&my, &maria] {
            let options = DumpOptions::Mysql(MysqlDumpOptions {
                strip_definer: true,
                ..MysqlDumpOptions::default()
            });
            let run = dump_run(&endpoint(), t, &options, &[], true, part, &[], secret()).unwrap();
            assert_env_only(&run, "MYSQL_PWD");
        }
        let restore = |options: RestoreOptions| RestoreRequest {
            tools: ToolSource::Detect,
            input: PathBuf::from("/backups/shop.dump"),
            format: RestoreFormat::Auto,
            extra_args: String::new(),
            options,
        };
        let pg_request = restore(RestoreOptions::Postgres(PgRestoreOptions {
            clean: true,
            no_owner: true,
            create: true,
        }));
        let psql = tool(ToolName::Psql, "psql (PostgreSQL) 17.11");
        let pg_restore = tool(ToolName::PgRestore, "pg_restore (PostgreSQL) 17.11");
        for (t, format) in [
            (&psql, RestoreFormat::Plain),
            (&pg_restore, RestoreFormat::Custom),
            (&pg_restore, RestoreFormat::Directory),
            (&pg_restore, RestoreFormat::Tar),
        ] {
            let run = restore_run(&endpoint(), t, &pg_request, format, &[], secret()).unwrap();
            assert_env_only(&run, "PGPASSWORD");
        }
        let my_request = restore(RestoreOptions::Mysql(MysqlRestoreOptions {
            database: Some("shop".into()),
            disable_foreign_keys: true,
        }));
        let mysql = tool(ToolName::Mysql, "mysql  Ver 8.4.13 for Linux");
        let run = restore_run(
            &endpoint(),
            &mysql,
            &my_request,
            RestoreFormat::Plain,
            &[],
            secret(),
        )
        .unwrap();
        assert_env_only(&run, "MYSQL_PWD");
        assert!(run.args.contains(&"--skip-system-command".to_string()));
    }

    #[test]
    fn part_files_sit_next_to_the_backup() {
        assert_eq!(
            part_path(Path::new("/b/shop_1.dump")),
            PathBuf::from("/b/shop_1.dump.part")
        );
    }
}
