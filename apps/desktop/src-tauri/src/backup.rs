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
