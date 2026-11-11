//! Backup and restore through the tunnel (ADR-049): a thin adapter. The core finds the tools,
//! opens the route, writes every argument, refuses a read-only connection and an unconfirmed
//! restore, runs the tool and redacts its log; these commands only carry the dialog's choices to
//! it and its events back.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::backup::{
    self, BackupCancel, BackupError, BackupEvent, ColumnStatistics, DumpContent, DumpOptions,
    DumpRequest, MysqlConsistency, MysqlDumpOptions, MysqlRestoreOptions, PgDumpOptions, PgFormat,
    PgRestoreOptions, RestoreFormat, RestoreOptions, RestoreRequest, ToolSource,
};
use spelaion_core::{BackupPreferences, RouteOptions};
use spelaion_ssh::KnownHosts;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::dto::TableRefItem;
use crate::interaction::AppInteraction;
use crate::{AppState, CmdResult};

/// The dialog's remembered choices.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BackupPreferencesItem {
    /// Empty: the tools are found automatically.
    pub postgres_tools: String,
    pub mysql_tools: String,
    pub folder: String,
    /// Empty: the default pattern.
    pub name_pattern: String,
    /// The default pattern, for the field's placeholder.
    pub default_name_pattern: String,
}

#[tauri::command]
#[specta::specta]
pub async fn backup_preferences(state: State<'_, AppState>) -> CmdResult<BackupPreferencesItem> {
    let p = state
        .history
        .backup_preferences()
        .await
        .map_err(|e| e.to_string())?;
    Ok(BackupPreferencesItem {
        postgres_tools: p.postgres_tools,
        mysql_tools: p.mysql_tools,
        folder: p.folder,
        name_pattern: p.name_pattern,
        default_name_pattern: backup::DEFAULT_NAME_PATTERN.to_string(),
    })
}

#[tauri::command]
#[specta::specta]
pub async fn set_backup_preferences(
    preferences: BackupPreferencesItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_backup_preferences(BackupPreferences {
            postgres_tools: preferences.postgres_tools,
            mysql_tools: preferences.mysql_tools,
            folder: preferences.folder,
            name_pattern: preferences.name_pattern,
        })
        .await
        .map_err(|e| e.to_string())
}

fn tool_source(folder: &str) -> ToolSource {
    if folder.trim().is_empty() {
        ToolSource::Detect
    } else {
        ToolSource::Folder(PathBuf::from(folder.trim()))
    }
}

/// A client tool, found or not.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolItem {
    pub name: String,
    /// Where it was found; empty when it was not.
    pub path: String,
    /// Its `--version`'s first line.
    pub version: String,
    /// Why it is missing or broken.
    pub error: Option<String>,
}

/// The connection's database and its engine's client tools.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BackupToolsItem {
    /// The database the connection opens (what a backup dumps by default).
    pub database: String,
    pub tools: Vec<ToolItem>,
}

/// The client tools of the connection's engine, from `folder` (empty: found automatically).
#[tauri::command]
#[specta::specta]
pub async fn backup_tools(
    session: String,
    folder: String,
    state: State<'_, AppState>,
) -> CmdResult<BackupToolsItem> {
    let open = state.session(&session).await?;
    let found = backup::find_tools(open.resolved.engine, &tool_source(&folder)).await;
    let tools = found
        .into_iter()
        .map(|(name, tool)| match tool {
            Ok(tool) => ToolItem {
                name: name.label().to_string(),
                path: tool.program.display().to_string(),
                version: tool.version.text,
                error: None,
            },
            Err(e) => ToolItem {
                name: name.label().to_string(),
                path: String::new(),
                version: String::new(),
                error: Some(e.to_string()),
            },
        })
        .collect();
    Ok(BackupToolsItem {
        database: open.resolved.params.database.clone(),
        tools,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum PgFormatItem {
    Plain,
    Custom,
    Directory,
    Tar,
}

impl From<PgFormatItem> for PgFormat {
    fn from(f: PgFormatItem) -> Self {
        match f {
            PgFormatItem::Plain => PgFormat::Plain,
            PgFormatItem::Custom => PgFormat::Custom,
            PgFormatItem::Directory => PgFormat::Directory,
            PgFormatItem::Tar => PgFormat::Tar,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DumpContentItem {
    SchemaAndData,
    SchemaOnly,
    DataOnly,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum MysqlConsistencyItem {
    SingleTransaction,
    LockAllTables,
    NoLocks,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ColumnStatisticsItem {
    Auto,
    Off,
    On,
}

/// Each tool's options, as the dialog shows them.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BackupOptionsItem {
    Postgres {
        schemas: Vec<String>,
        tables: Vec<TableRefItem>,
        format: PgFormatItem,
        /// 0–9; `None`: the tool's default.
        compression: Option<u8>,
        /// Empty: the database's.
        encoding: String,
        content: DumpContentItem,
        inserts: bool,
        no_privileges: bool,
        no_owner: bool,
        clean: bool,
        create: bool,
    },
    Mysql {
        databases: Vec<String>,
        tables: Vec<String>,
        consistency: MysqlConsistencyItem,
        add_drop: bool,
        no_create: bool,
        no_data: bool,
        extended_insert: bool,
        routines: bool,
        events: bool,
        triggers: bool,
        include_views: bool,
        strip_definer: bool,
        hex_blob: bool,
        column_statistics: ColumnStatisticsItem,
    },
}

impl From<BackupOptionsItem> for DumpOptions {
    fn from(item: BackupOptionsItem) -> Self {
        match item {
            BackupOptionsItem::Postgres {
                schemas,
                tables,
                format,
                compression,
                encoding,
                content,
                inserts,
                no_privileges,
                no_owner,
                clean,
                create,
            } => DumpOptions::Postgres(PgDumpOptions {
                schemas,
                tables: tables.into_iter().map(|t| (t.schema, t.name)).collect(),
                format: format.into(),
                compression,
                encoding: (!encoding.trim().is_empty()).then_some(encoding),
                content: match content {
                    DumpContentItem::SchemaAndData => DumpContent::SchemaAndData,
                    DumpContentItem::SchemaOnly => DumpContent::SchemaOnly,
                    DumpContentItem::DataOnly => DumpContent::DataOnly,
                },
                inserts,
                no_privileges,
                no_owner,
                clean,
                create,
            }),
            BackupOptionsItem::Mysql {
                databases,
                tables,
                consistency,
                add_drop,
                no_create,
                no_data,
                extended_insert,
                routines,
                events,
                triggers,
                include_views,
                strip_definer,
                hex_blob,
                column_statistics,
            } => DumpOptions::Mysql(MysqlDumpOptions {
                databases,
                tables,
                consistency: match consistency {
                    MysqlConsistencyItem::SingleTransaction => MysqlConsistency::SingleTransaction,
                    MysqlConsistencyItem::LockAllTables => MysqlConsistency::LockAllTables,
                    MysqlConsistencyItem::NoLocks => MysqlConsistency::NoLocks,
                },
                add_drop,
                no_create,
                no_data,
                extended_insert,
                routines,
                events,
                triggers,
                include_views,
                strip_definer,
                hex_blob,
                column_statistics: match column_statistics {
                    ColumnStatisticsItem::Auto => ColumnStatistics::Auto,
                    ColumnStatisticsItem::Off => ColumnStatistics::Off,
                    ColumnStatisticsItem::On => ColumnStatistics::On,
                },
            }),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BackupRequestItem {
    /// Empty: the tools are found automatically.
    pub tools_folder: String,
    pub folder: String,
    pub name_pattern: String,
    pub extra_args: String,
    pub options: BackupOptionsItem,
}

impl From<BackupRequestItem> for DumpRequest {
    fn from(item: BackupRequestItem) -> Self {
        DumpRequest {
            tools: tool_source(&item.tools_folder),
            folder: PathBuf::from(item.folder.trim()),
            name_pattern: if item.name_pattern.trim().is_empty() {
                backup::DEFAULT_NAME_PATTERN.to_string()
            } else {
                item.name_pattern
            },
            extra_args: item.extra_args,
            options: item.options.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BackupPreviewItem {
    pub tool: String,
    pub version: String,
    /// The file (or folder) the backup will be.
    pub path: String,
    pub notes: Vec<String>,
}

/// The tool a backup will run and the file it will write, for the dialog to show first.
#[tauri::command]
#[specta::specta]
pub async fn preview_backup(
    session: String,
    request: BackupRequestItem,
    state: State<'_, AppState>,
) -> CmdResult<BackupPreviewItem> {
    let open = state.session(&session).await?;
    let request: DumpRequest = request.into();
    let preview = backup::preview_dump(&open.resolved, &request, Some(&open.session))
        .await
        .map_err(|e| e.to_string())?;
    Ok(BackupPreviewItem {
        tool: preview.tool.program.display().to_string(),
        version: preview.tool.version.text,
        path: preview.path.display().to_string(),
        notes: preview.notes,
    })
}

/// What a run reports as it goes.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BackupEventItem {
    Started { command_line: String },
    Log { line: String },
    Progress { bytes: f64, total: Option<f64> },
    Statements { done: f64, total: f64 },
}

impl From<BackupEvent> for BackupEventItem {
    fn from(event: BackupEvent) -> Self {
        match event {
            BackupEvent::Started { command_line } => Self::Started { command_line },
            BackupEvent::Log { line } => Self::Log { line },
            BackupEvent::Progress { bytes, total } => Self::Progress {
                bytes: bytes as f64,
                total: total.map(|t| t as f64),
            },
            BackupEvent::Statements { done, total } => Self::Statements {
                done: done as f64,
                total: total as f64,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BackupOutcome {
    Done {
        path: String,
        bytes: f64,
        millis: f64,
    },
    /// The password must be asked first.
    PasswordRequired,
    Cancelled,
    Failed {
        message: String,
    },
}

/// Registers a run so `cancel_backup` can stop it.
fn register(state: &AppState, run_id: u32) -> CmdResult<Arc<BackupCancel>> {
    let cancel = Arc::new(BackupCancel::new());
    let mut runs = state.backups.lock().expect("lock poisoned");
    if runs.contains_key(&run_id) {
        return Err("a backup with this id is already running".into());
    }
    runs.insert(run_id, cancel.clone());
    Ok(cancel)
}

fn route_options(state: &AppState, app: &AppHandle) -> RouteOptions {
    RouteOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app.clone(), state.pending.clone())),
    }
}

/// The source session's connection and its in-memory password.
async fn source(
    state: &AppState,
    session: &str,
) -> CmdResult<(
    Arc<spelaion_core::OpenConnection>,
    Option<spelaion_driver_api::Secret>,
)> {
    let sessions = state.sessions.lock().await;
    let source = sessions
        .get(session)
        .ok_or("the connection of this backup is closed")?;
    Ok((source.open.clone(), source.password.clone()))
}

/// Backs up the connection's database with its vendor's tool, through its tunnel; the tool's
/// output goes to `on_event` line by line.
#[tauri::command]
#[specta::specta]
pub async fn run_backup(
    session: String,
    run_id: u32,
    request: BackupRequestItem,
    on_event: Channel<BackupEventItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<BackupOutcome> {
    let (open, password) = source(&state, &session).await?;
    let request: DumpRequest = request.into();
    let cancel = register(&state, run_id)?;
    let events = move |e: BackupEvent| {
        let _ = on_event.send(e.into());
    };
    let outcome = backup::run_dump(
        &open.resolved,
        password,
        route_options(&state, &app),
        &request,
        Some(&open.session),
        &cancel,
        &events,
    )
    .await;
    state.backups.lock().expect("lock poisoned").remove(&run_id);
    Ok(match outcome {
        Ok(summary) => BackupOutcome::Done {
            path: summary.path.display().to_string(),
            bytes: summary.bytes as f64,
            millis: summary.elapsed.as_millis() as f64,
        },
        Err(e) => outcome_error(e),
    })
}

fn outcome_error(e: BackupError) -> BackupOutcome {
    match e {
        BackupError::Cancelled => BackupOutcome::Cancelled,
        BackupError::Open(spelaion_core::OpenError::PasswordRequired { .. }) => {
            BackupOutcome::PasswordRequired
        }
        e @ (BackupError::Tool(_)
        | BackupError::ReadOnly
        | BackupError::NeedsConfirmation
        | BackupError::Open(_)
        | BackupError::Io { .. }
        | BackupError::Invalid(_)
        | BackupError::Failed(_)) => BackupOutcome::Failed {
            message: e.to_string(),
        },
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum RestoreFormatItem {
    Auto,
    Plain,
    Custom,
    Directory,
    Tar,
}

impl From<RestoreFormatItem> for RestoreFormat {
    fn from(f: RestoreFormatItem) -> Self {
        match f {
            RestoreFormatItem::Auto => RestoreFormat::Auto,
            RestoreFormatItem::Plain => RestoreFormat::Plain,
            RestoreFormatItem::Custom => RestoreFormat::Custom,
            RestoreFormatItem::Directory => RestoreFormat::Directory,
            RestoreFormatItem::Tar => RestoreFormat::Tar,
        }
    }
}

impl From<RestoreFormat> for RestoreFormatItem {
    fn from(f: RestoreFormat) -> Self {
        match f {
            RestoreFormat::Auto => RestoreFormatItem::Auto,
            RestoreFormat::Plain => RestoreFormatItem::Plain,
            RestoreFormat::Custom => RestoreFormatItem::Custom,
            RestoreFormat::Directory => RestoreFormatItem::Directory,
            RestoreFormat::Tar => RestoreFormatItem::Tar,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RestoreOptionsItem {
    Postgres {
        clean: bool,
        no_owner: bool,
        create: bool,
    },
    Mysql {
        /// Empty: the connection's.
        database: String,
        disable_foreign_keys: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RestoreRequestItem {
    pub tools_folder: String,
    pub input: String,
    pub format: RestoreFormatItem,
    pub extra_args: String,
    pub options: RestoreOptionsItem,
}

impl From<RestoreRequestItem> for RestoreRequest {
    fn from(item: RestoreRequestItem) -> Self {
        RestoreRequest {
            tools: tool_source(&item.tools_folder),
            input: PathBuf::from(item.input.trim()),
            format: item.format.into(),
            extra_args: item.extra_args,
            options: match item.options {
                RestoreOptionsItem::Postgres {
                    clean,
                    no_owner,
                    create,
                } => RestoreOptions::Postgres(PgRestoreOptions {
                    clean,
                    no_owner,
                    create,
                }),
                RestoreOptionsItem::Mysql {
                    database,
                    disable_foreign_keys,
                } => RestoreOptions::Mysql(MysqlRestoreOptions {
                    database: (!database.trim().is_empty()).then_some(database),
                    disable_foreign_keys,
                }),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RestorePreviewItem {
    pub tool: String,
    pub version: String,
    pub format: RestoreFormatItem,
    pub notes: Vec<String>,
    /// The connection is read-only: the core will refuse the restore.
    pub read_only: bool,
}

/// The tool a restore will run and what its confirmation must say.
#[tauri::command]
#[specta::specta]
pub async fn preview_restore(
    session: String,
    request: RestoreRequestItem,
    state: State<'_, AppState>,
) -> CmdResult<RestorePreviewItem> {
    let open = state.session(&session).await?;
    let request: RestoreRequest = request.into();
    let preview = backup::preview_restore(&open.resolved, &request)
        .await
        .map_err(|e| e.to_string())?;
    Ok(RestorePreviewItem {
        tool: preview.tool.program.display().to_string(),
        version: preview.tool.version.text,
        format: preview.format.into(),
        notes: preview.notes,
        read_only: open.resolved.params.read_only,
    })
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RestoreOutcome {
    Done { bytes: f64, millis: f64 },
    NeedsConfirmation,
    PasswordRequired,
    Cancelled,
    Failed { message: String },
}

/// Restores a file into the connection's database with its vendor's tool, through its tunnel.
/// The core refuses a read-only connection and an unconfirmed restore.
#[tauri::command]
#[specta::specta]
pub async fn run_restore(
    session: String,
    run_id: u32,
    request: RestoreRequestItem,
    confirmed: bool,
    on_event: Channel<BackupEventItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<RestoreOutcome> {
    let (open, password) = source(&state, &session).await?;
    let request: RestoreRequest = request.into();
    let cancel = register(&state, run_id)?;
    let events = move |e: BackupEvent| {
        let _ = on_event.send(e.into());
    };
    let outcome = backup::run_restore(
        &open.resolved,
        password,
        route_options(&state, &app),
        &request,
        confirmed,
        &cancel,
        &events,
    )
    .await;
    state.backups.lock().expect("lock poisoned").remove(&run_id);
    Ok(match outcome {
        Ok(summary) => RestoreOutcome::Done {
            bytes: summary.bytes as f64,
            millis: summary.elapsed.as_millis() as f64,
        },
        Err(BackupError::NeedsConfirmation) => RestoreOutcome::NeedsConfirmation,
        Err(e) => match outcome_error(e) {
            BackupOutcome::Cancelled => RestoreOutcome::Cancelled,
            BackupOutcome::PasswordRequired => RestoreOutcome::PasswordRequired,
            BackupOutcome::Failed { message } => RestoreOutcome::Failed { message },
            BackupOutcome::Done { .. } => RestoreOutcome::Failed {
                message: "the restore ended in an unexpected state".into(),
            },
        },
    })
}

/// Stops a running backup or restore: the tool is killed and a partial backup removed.
#[tauri::command]
#[specta::specta]
pub async fn cancel_backup(run_id: u32, state: State<'_, AppState>) -> CmdResult<()> {
    let cancel = state
        .backups
        .lock()
        .expect("lock poisoned")
        .get(&run_id)
        .cloned();
    if let Some(cancel) = cancel {
        cancel.cancel().await;
    }
    Ok(())
}

// ------------------------------------------------------------------ SQLite, DuckDB, .sql files

/// A new session of the `source` connection for a copy, an import or a file run, never a
/// tab's own (ADR-031). `reads_only`: its session setup goes through the read barrier.
async fn open_run_session(
    state: &AppState,
    app: &AppHandle,
    source: &str,
    reads_only: bool,
) -> CmdResult<spelaion_core::OpenConnection> {
    let (open, password) = self::source(state, source).await?;
    spelaion_core::open_connection(
        open.resolved.clone(),
        &state.engines,
        spelaion_core::OpenOptions {
            tunnels: state.tunnels.clone(),
            known_hosts: KnownHosts::for_user(&state.user_dir),
            interaction: Arc::new(AppInteraction::new(app.clone(), state.pending.clone())),
            password,
            read_only_session: reads_only,
        },
        state.results.clone(),
    )
    .await
    .map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DuckdbFormatItem {
    Parquet,
    Csv,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedBackupItem {
    pub folder: String,
    pub name_pattern: String,
    /// DuckDB only.
    pub duckdb_format: DuckdbFormatItem,
}

impl From<EmbeddedBackupItem> for backup::EmbeddedDumpRequest {
    fn from(item: EmbeddedBackupItem) -> Self {
        backup::EmbeddedDumpRequest {
            folder: PathBuf::from(item.folder.trim()),
            name_pattern: if item.name_pattern.trim().is_empty() {
                backup::DEFAULT_NAME_PATTERN.to_string()
            } else {
                item.name_pattern
            },
            duckdb_format: match item.duckdb_format {
                DuckdbFormatItem::Parquet => backup::DuckdbExportFormat::Parquet,
                DuckdbFormatItem::Csv => backup::DuckdbExportFormat::Csv,
            },
        }
    }
}

/// The file (SQLite) or folder (DuckDB) a copy will be.
#[tauri::command]
#[specta::specta]
pub async fn preview_embedded_backup(
    session: String,
    request: EmbeddedBackupItem,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    backup::embedded_dump_path(&open.resolved, &request.into())
        .map(|p| p.display().to_string())
        .map_err(|e| e.to_string())
}

/// Copies a SQLite database with `VACUUM INTO`, or exports a DuckDB one, in a session of its own.
#[tauri::command]
#[specta::specta]
pub async fn run_embedded_backup(
    session: String,
    run_id: u32,
    request: EmbeddedBackupItem,
    on_event: Channel<BackupEventItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<BackupOutcome> {
    let cancel = register(&state, run_id)?;
    let outcome = match open_run_session(&state, &app, &session, true).await {
        Ok(open) => {
            let events = move |e: BackupEvent| {
                let _ = on_event.send(e.into());
            };
            let result = backup::embedded_dump(
                &open.session,
                &open.resolved,
                &request.into(),
                &cancel,
                &events,
            )
            .await;
            open.close().await;
            match result {
                Ok(summary) => BackupOutcome::Done {
                    path: summary.path.display().to_string(),
                    bytes: summary.bytes as f64,
                    millis: summary.elapsed.as_millis() as f64,
                },
                Err(e) => outcome_error(e),
            }
        }
        Err(message) => BackupOutcome::Failed { message },
    };
    state.backups.lock().expect("lock poisoned").remove(&run_id);
    Ok(outcome)
}

/// Loads a DuckDB export folder into the connection's database. The core refuses a read-only
/// connection and an unconfirmed import.
#[tauri::command]
#[specta::specta]
pub async fn run_duckdb_import(
    session: String,
    run_id: u32,
    folder: String,
    confirmed: bool,
    on_event: Channel<BackupEventItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<RestoreOutcome> {
    let cancel = register(&state, run_id)?;
    let outcome = match open_run_session(&state, &app, &session, false).await {
        Ok(open) => {
            let events = move |e: BackupEvent| {
                let _ = on_event.send(e.into());
            };
            let result = backup::duckdb_import(
                &open.session,
                &open.resolved,
                &PathBuf::from(folder.trim()),
                confirmed,
                &cancel,
                &events,
            )
            .await;
            open.close().await;
            match result {
                Ok(elapsed) => RestoreOutcome::Done {
                    bytes: 0.0,
                    millis: elapsed.as_millis() as f64,
                },
                Err(BackupError::NeedsConfirmation) => RestoreOutcome::NeedsConfirmation,
                Err(BackupError::Cancelled) => RestoreOutcome::Cancelled,
                Err(e) => RestoreOutcome::Failed {
                    message: e.to_string(),
                },
            }
        }
        Err(message) => RestoreOutcome::Failed { message },
    };
    state.backups.lock().expect("lock poisoned").remove(&run_id);
    Ok(outcome)
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileRiskItem {
    pub risk: crate::dto::RiskKind,
    pub count: f64,
}

/// What the confirmation of a file run shows.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FilePlanItem {
    pub bytes: f64,
    pub statements: f64,
    pub risks: Vec<FileRiskItem>,
    /// The connection is read-only: the core will refuse the run.
    pub read_only: bool,
}

/// Reads an `.sql` file and counts its statements and what the guard would ask about.
#[tauri::command]
#[specta::specta]
pub async fn plan_sql_file(
    session: String,
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<FilePlanItem> {
    let open = state.session(&session).await?;
    let path = PathBuf::from(path.trim());
    let safeguards = open.resolved.safeguards();
    let read_only = open.resolved.params.read_only;
    let plan =
        backup::plan_sql_file(&open.session, &path, &safeguards).map_err(|e| e.to_string())?;
    Ok(FilePlanItem {
        bytes: plan.bytes as f64,
        statements: plan.statements as f64,
        risks: plan
            .risks
            .into_iter()
            .map(|(risk, count)| FileRiskItem {
                risk: risk.into(),
                count: count as f64,
            })
            .collect(),
        read_only,
    })
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FileRunOutcome {
    Done {
        done: f64,
        failed: f64,
        total: f64,
        millis: f64,
    },
    NeedsConfirmation,
    Cancelled,
    Failed {
        message: String,
    },
}

/// Runs an `.sql` file in a session of its own, statement by statement. The core refuses a
/// read-only connection and an unconfirmed run.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub async fn run_sql_file(
    session: String,
    run_id: u32,
    path: String,
    on_error: crate::dto::OnErrorItem,
    confirmed: bool,
    on_event: Channel<BackupEventItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<FileRunOutcome> {
    let cancel = register(&state, run_id)?;
    let outcome = match open_run_session(&state, &app, &session, false).await {
        Ok(open) => {
            let events = move |e: BackupEvent| {
                let _ = on_event.send(e.into());
            };
            let result = backup::run_sql_file(
                &open.session,
                &PathBuf::from(path.trim()),
                open.resolved.params.read_only,
                confirmed,
                on_error.into(),
                &cancel,
                &events,
            )
            .await;
            open.close().await;
            match result {
                Ok(summary) => FileRunOutcome::Done {
                    done: summary.done as f64,
                    failed: summary.failed as f64,
                    total: summary.total as f64,
                    millis: summary.elapsed.as_millis() as f64,
                },
                Err(BackupError::NeedsConfirmation) => FileRunOutcome::NeedsConfirmation,
                Err(BackupError::Cancelled) => FileRunOutcome::Cancelled,
                Err(e) => FileRunOutcome::Failed {
                    message: e.to_string(),
                },
            }
        }
        Err(message) => FileRunOutcome::Failed { message },
    };
    state.backups.lock().expect("lock poisoned").remove(&run_id);
    Ok(outcome)
}
