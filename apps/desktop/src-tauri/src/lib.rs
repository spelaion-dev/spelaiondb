//! SpelunkDB desktop app: a thin Tauri adapter over `spelunk-core` (ADR-002). There is no
//! database or security logic here: only translation between UI commands and the core.

mod dto;
mod interaction;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use futures_util::StreamExt;
use spelunk_config::{
    Config, ConnectionDraft, ConnectionId, Resolver, Source, SystemBackend, TrustStore,
    default_user_dir, load,
};
use spelunk_core::{
    Engines, OpenConnection, OpenError, OpenOptions, QueryHistory, ResultId, ResultStore,
    TunnelPool, open_connection,
};
use spelunk_driver_api::{DriverError, Secret, Value};
use spelunk_ssh::KnownHosts;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Manager, State};
use tokio::sync::{Mutex, RwLock};

use crate::dto::*;
use crate::interaction::{AppInteraction, Pending};

/// App state. Sessions are identified by a key the UI chooses: one per editor tab and one
/// for metadata per connection (ADR-004).
struct AppState {
    user_dir: PathBuf,
    workspace: RwLock<Option<PathBuf>>,
    workspace_version: AtomicU64,
    config: RwLock<Config>,
    engines: Engines,
    tunnels: Arc<TunnelPool>,
    results: Arc<ResultStore>,
    sessions: Mutex<HashMap<String, DesktopSession>>,
    history: QueryHistory,
    pending: Pending,
}

struct DesktopSession {
    open: Arc<OpenConnection>,
    /// Captured when connecting: changing folders must not reattribute an existing tab.
    history_workspace: String,
}

type CmdResult<T> = Result<T, String>;

impl AppState {
    async fn reload(&self) {
        let version = self.workspace_version.load(Ordering::Acquire);
        let workspace = self.workspace.read().await.clone();
        let loaded = load(workspace.as_deref(), &self.user_dir);
        let mut config = self.config.write().await;
        // A reload started in the previous folder must not overwrite the new one.
        if version == self.workspace_version.load(Ordering::Acquire) {
            *config = loaded;
        }
    }

    async fn info(&self) -> CmdResult<WorkspaceInfo> {
        let config = self.config.read().await;
        let trust = TrustStore::load(&self.user_dir).map_err(|e| e.to_string())?;
        Ok(WorkspaceInfo {
            workspace: config
                .workspace_root
                .as_ref()
                .map(|p| p.display().to_string()),
            trusted: config
                .workspace_root
                .as_deref()
                .is_some_and(|r| trust.is_trusted(r)),
            has_workspace_connections: config
                .connections
                .iter()
                .any(|c| c.id.source == Source::Workspace),
            user_dir: self.user_dir.display().to_string(),
            problems: config.problems.iter().map(ToString::to_string).collect(),
        })
    }

    async fn session(&self, key: &str) -> CmdResult<Arc<OpenConnection>> {
        self.sessions
            .lock()
            .await
            .get(key)
            .map(|session| session.open.clone())
            .ok_or_else(|| format!("there is no open connection for {key}"))
    }
}

/// Personal connections share history across folders; repository connections are namespaced
/// by their canonical root so identical connection IDs in different projects never collide.
fn history_workspace(config: &Config, source: Source) -> String {
    match source {
        Source::User => String::new(),
        Source::Workspace => config
            .workspace_root
            .as_ref()
            .map_or_else(String::new, |root| {
                std::fs::canonicalize(root)
                    .unwrap_or_else(|_| root.clone())
                    .display()
                    .to_string()
            }),
    }
}

fn parse_id(id: &str) -> ConnectionId {
    match id.strip_prefix("~/") {
        Some(path) => ConnectionId {
            source: Source::User,
            path: path.into(),
        },
        None => ConnectionId {
            source: Source::Workspace,
            path: id.into(),
        },
    }
}

/// File where the last opened folder is remembered.
fn last_workspace_file(user_dir: &std::path::Path) -> PathBuf {
    user_dir.join("desktop-last-workspace.txt")
}

#[tauri::command]
#[specta::specta]
async fn workspace_info(state: State<'_, AppState>) -> CmdResult<WorkspaceInfo> {
    state.info().await
}

#[tauri::command]
#[specta::specta]
async fn get_language(state: State<'_, AppState>) -> CmdResult<String> {
    state.history.language().await.map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_language(language: String, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .history
        .set_language(language)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn get_theme(state: State<'_, AppState>) -> CmdResult<String> {
    state.history.theme().await.map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_theme(theme: String, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .history
        .set_theme(theme)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn open_workspace(path: String, state: State<'_, AppState>) -> CmdResult<WorkspaceInfo> {
    let version = state.workspace_version.load(Ordering::Acquire);
    if !version.is_multiple_of(2)
        || state
            .workspace_version
            .compare_exchange(version, version + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        return Err("the folder is already changing".into());
    }
    let old_sessions = std::mem::take(&mut *state.sessions.lock().await);
    for session in old_sessions.into_values() {
        if let Ok(open) = Arc::try_unwrap(session.open) {
            open.close().await;
        }
    }
    {
        let mut workspace = state.workspace.write().await;
        *workspace = Some(PathBuf::from(&path));
        // Stay odd while switching, but invalidate reloads that saw the old path.
        state.workspace_version.fetch_add(2, Ordering::AcqRel);
    }
    let _ = std::fs::create_dir_all(&state.user_dir);
    let _ = std::fs::write(last_workspace_file(&state.user_dir), &path);
    state.reload().await;
    state.workspace_version.fetch_add(1, Ordering::Release);
    state.info().await
}

/// Trusts the open repository (ADR-009). The UI asks for it after showing what it implies.
#[tauri::command]
#[specta::specta]
async fn trust_workspace(state: State<'_, AppState>) -> CmdResult<WorkspaceInfo> {
    let root = state
        .config
        .read()
        .await
        .workspace_root
        .clone()
        .ok_or("no repository is open")?;
    let mut trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    trust.trust(&root).map_err(|e| e.to_string())?;
    state.info().await
}

#[tauri::command]
#[specta::specta]
async fn list_connections(state: State<'_, AppState>) -> CmdResult<Vec<ConnectionItem>> {
    state.reload().await;
    let config = state.config.read().await;
    let connections = config.connections.clone();
    let workspace = history_workspace(&config, Source::Workspace);
    drop(config);
    let mut items = Vec::with_capacity(connections.len());
    for c in connections {
        let scope = match c.id.source {
            Source::User => String::new(),
            Source::Workspace => workspace.clone(),
        };
        let history_enabled = state
            .history
            .enabled(scope, c.id.to_string())
            .await
            .map_err(|e| e.to_string())?;
        items.push(ConnectionItem {
            id: c.id.to_string(),
            name: c.name(),
            engine: c.file.engine.as_str().into(),
            env: c.file.env.clone(),
            folders: c.folders().into_iter().map(String::from).collect(),
            personal: c.id.source == Source::User,
            tunnel: c.file.tunnel.clone(),
            mcp_exposed: c.file.mcp.exposed,
            history_enabled,
        });
    }
    Ok(items)
}

#[tauri::command]
#[specta::specta]
async fn list_history(
    search: String,
    offset: u32,
    limit: u32,
    state: State<'_, AppState>,
) -> CmdResult<Vec<HistoryItem>> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .list(workspace, search, limit as usize, offset as usize)
        .await
        .map(|entries| entries.into_iter().map(HistoryItem::from).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_history_enabled(
    connection: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let id = parse_id(&connection);
    let config = state.config.read().await;
    if config.connection(&id).is_none() {
        return Err(format!("unknown connection: {connection}"));
    }
    let workspace = history_workspace(&config, id.source);
    drop(config);
    state
        .history
        .set_enabled(workspace, connection, enabled)
        .await
        .map_err(|e| e.to_string())
}

/// Engines shown when adding a connection, including the ones not supported yet.
#[tauri::command]
#[specta::specta]
fn engine_catalog() -> Vec<EngineItem> {
    spelunk_core::engine_catalog()
        .into_iter()
        .map(EngineItem::from)
        .collect()
}

/// Tunnels a new connection can use (from the repository and the personal directory).
#[tauri::command]
#[specta::specta]
async fn list_tunnels(state: State<'_, AppState>) -> CmdResult<Vec<TunnelItem>> {
    let config = state.config.read().await;
    Ok(config
        .tunnels
        .iter()
        .map(|t| TunnelItem {
            name: t.name.clone(),
            personal: t.source == Source::User,
        })
        .collect())
}

/// Opens and closes a connection that has not been saved yet. Nothing is written: neither
/// the file nor the keychain.
#[tauri::command]
#[specta::specta]
async fn test_connection(
    draft: ConnectionDraftItem,
    password: Option<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ConnectOutcome> {
    let draft = ConnectionDraft::try_from(draft)?;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let resolved = {
        let config = state.config.read().await;
        match Resolver::new(&config, &trust, &SystemBackend).resolve_draft(&draft) {
            Ok(resolved) => resolved,
            Err(spelunk_config::ResolveError::Untrusted { .. }) => {
                return Ok(ConnectOutcome::Untrusted);
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app, state.pending.clone())),
        password: password.filter(|p| !p.is_empty()).map(Secret::new),
    };
    let open = match open_connection(resolved, &state.engines, options, state.results.clone()).await
    {
        Ok(open) => open,
        Err(OpenError::PasswordRequired { .. }) => {
            return Ok(ConnectOutcome::PasswordRequired {
                can_remember: false,
            });
        }
        Err(OpenError::Driver(DriverError::Database {
            code: Some(code),
            message,
            ..
        })) if code.starts_with("28") => {
            return Ok(ConnectOutcome::AuthFailed {
                message,
                can_remember: false,
            });
        }
        Err(e) => return Err(e.to_string()),
    };
    let version = open.session.connection().server_version().await;
    open.close().await;
    Ok(ConnectOutcome::Connected {
        server_version: version.map_err(|e| e.to_string())?,
    })
}

/// Saves a new connection file (and, for `keychain`, its password) and reloads the
/// configuration. Returns the new connection's identifier.
#[tauri::command]
#[specta::specta]
async fn add_connection(
    draft: ConnectionDraftItem,
    password: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let draft = ConnectionDraft::try_from(draft)?;
    let password = password.map(Secret::new);
    let id = {
        let config = state.config.read().await;
        spelunk_config::add_connection(&config, &draft, password.as_ref(), &SystemBackend)
            .map_err(|e| e.to_string())?
    };
    state.reload().await;
    Ok(id.to_string())
}

/// Opens a session (`session` is chosen by the UI). If a password is needed, it says so
/// instead of failing, so the UI can ask for it.
#[tauri::command]
#[specta::specta]
async fn connect(
    session: String,
    connection: String,
    password: Option<String>,
    remember: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ConnectOutcome> {
    let workspace_version = state.workspace_version.load(Ordering::Acquire);
    if !workspace_version.is_multiple_of(2) {
        return Err("the folder is changing; select a connection again".into());
    }
    let id = parse_id(&connection);
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let (resolved, history_workspace) = {
        let config = state.config.read().await;
        let resolved = match Resolver::new(&config, &trust, &SystemBackend).resolve(&id) {
            Ok(resolved) => resolved,
            Err(spelunk_config::ResolveError::Untrusted { .. }) => {
                return Ok(ConnectOutcome::Untrusted);
            }
            Err(e) => return Err(e.to_string()),
        };
        (resolved, history_workspace(&config, id.source))
    };
    let save_to = match &resolved.password {
        spelunk_config::Password::Prompt { save_to } => save_to.clone(),
        spelunk_config::Password::None | spelunk_config::Password::Value(_) => None,
    };
    let password = password.map(Secret::new);
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app.clone(), state.pending.clone())),
        password: password.clone(),
    };
    let open = match open_connection(resolved, &state.engines, options, state.results.clone()).await
    {
        Ok(open) => open,
        Err(OpenError::PasswordRequired { save_to }) => {
            return Ok(ConnectOutcome::PasswordRequired {
                can_remember: save_to.is_some(),
            });
        }
        // SQLSTATE 28xxx: authorization rejected (password or user). Only then does it make
        // sense to ask for the password again; any other failure is shown as is.
        Err(OpenError::Driver(DriverError::Database {
            code: Some(code),
            message,
            ..
        })) if code.starts_with("28") => {
            return Ok(ConnectOutcome::AuthFailed {
                message,
                can_remember: save_to.is_some(),
            });
        }
        Err(e) => return Err(e.to_string()),
    };
    // Remember in the keychain only if it was asked for and the connection worked.
    if remember && let (Some(account), Some(secret)) = (save_to, &password) {
        let config = state.config.read().await;
        let resolver = Resolver::new(&config, &trust, &SystemBackend);
        resolver
            .remember_password(&account, secret)
            .map_err(|e| e.to_string())?;
    }
    let server_version = open
        .session
        .connection()
        .server_version()
        .await
        .map_err(|e| e.to_string())?;
    let mut sessions = state.sessions.lock().await;
    if workspace_version != state.workspace_version.load(Ordering::Acquire) {
        drop(sessions);
        open.close().await;
        return Err("the folder changed while connecting; select a connection again".into());
    }
    let old = sessions.insert(
        session,
        DesktopSession {
            open: Arc::new(open),
            history_workspace,
        },
    );
    drop(sessions);
    if let Some(old) = old.and_then(|o| Arc::try_unwrap(o.open).ok()) {
        old.close().await;
    }
    Ok(ConnectOutcome::Connected { server_version })
}

#[tauri::command]
#[specta::specta]
async fn disconnect(session: String, state: State<'_, AppState>) -> CmdResult<()> {
    let removed = state.sessions.lock().await.remove(&session);
    if let Some(open) = removed.and_then(|o| Arc::try_unwrap(o.open).ok()) {
        open.close().await;
    }
    Ok(())
}

/// Runs a script and keeps sending events through the channel. Returns as soon as it starts.
#[tauri::command]
#[specta::specta]
async fn run_script(
    session: String,
    sql: String,
    on_event: Channel<ScriptEvent>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let (open, workspace) = {
        let sessions = state.sessions.lock().await;
        let session = sessions
            .get(&session)
            .ok_or_else(|| format!("there is no open connection for {session}"))?;
        (session.open.clone(), session.history_workspace.clone())
    };
    let mut events =
        state
            .history
            .run_script(&open.session, &sql, workspace, open.resolved.id.to_string());
    tauri::async_runtime::spawn(async move {
        // `open` lives while the script runs, even if the UI closes the tab halfway.
        let _keep = open;
        while let Some(event) = events.next().await {
            // Keep consuming after the tab closes: executed statements still need their
            // completion/error recorded, and later statements must not be silently aborted.
            let _ = on_event.send(ScriptEvent::from(event));
        }
    });
    Ok(())
}

#[tauri::command]
#[specta::specta]
async fn cancel(session: String, state: State<'_, AppState>) -> CmdResult<()> {
    let open = state.session(&session).await?;
    open.session.cancel().await.map_err(|e| e.to_string())
}

/// Frees a result the UI no longer shows.
#[tauri::command]
#[specta::specta]
async fn release_result(result: u32, state: State<'_, AppState>) -> CmdResult<()> {
    state.results.remove(ResultId(result.into()));
    Ok(())
}

#[tauri::command]
#[specta::specta]
async fn list_schemas(session: String, state: State<'_, AppState>) -> CmdResult<Vec<SchemaItem>> {
    let open = state.session(&session).await?;
    let schemas = open
        .session
        .connection()
        .list_schemas()
        .await
        .map_err(|e| e.to_string())?;
    Ok(schemas
        .into_iter()
        .map(|s| SchemaItem {
            name: s.name,
            system: s.system,
        })
        .collect())
}

#[tauri::command]
#[specta::specta]
async fn list_objects(
    session: String,
    schema: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<ObjectItem>> {
    let open = state.session(&session).await?;
    let objects = open
        .session
        .connection()
        .list_objects(&schema)
        .await
        .map_err(|e| e.to_string())?;
    Ok(objects.into_iter().map(ObjectItem::from).collect())
}

#[tauri::command]
#[specta::specta]
async fn describe_table(
    session: String,
    schema: String,
    name: String,
    state: State<'_, AppState>,
) -> CmdResult<TableDetails> {
    let open = state.session(&session).await?;
    let table = open
        .session
        .connection()
        .describe_table(&schema, &name)
        .await
        .map_err(|e| e.to_string())?;
    Ok(TableDetails::from(table))
}

/// The UI's answer to a tunnel dialog (unknown host, passphrase…).
#[tauri::command]
#[specta::specta]
async fn answer_interaction(
    id: u32,
    accept: bool,
    secret: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state.pending.answer(id, accept, secret.map(Secret::new));
    Ok(())
}

/// Window of rows as binary Arrow IPC (no JSON), with every cell already converted to the
/// text the engine's console shows (`Value`, verified by the type zoo): the UI does not
/// reimplement formats. Nulls stay null.
///
/// It is the only command outside `tauri-specta`, which does not type binary responses; its
/// TS wrapper is in `src/api.ts`.
#[tauri::command]
fn fetch_window(
    result: u32,
    start: u32,
    len: u32,
    state: State<'_, AppState>,
) -> Result<Response, String> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let columns = &stored.schema().columns;
    let schema = Arc::new(Schema::new(
        columns
            .iter()
            .map(|c| Field::new(&c.name, DataType::Utf8, true))
            .collect::<Vec<_>>(),
    ));
    let mut out = Vec::new();
    let mut writer = StreamWriter::try_new(&mut out, &schema).map_err(|e| e.to_string())?;
    for batch in stored.window(start as usize, len as usize) {
        let arrays: Vec<ArrayRef> = columns
            .iter()
            .enumerate()
            .map(|(i, meta)| {
                let column = batch.column(i);
                let text: StringArray = (0..batch.num_rows())
                    .map(
                        |row| match Value::from_array(column.as_ref(), row, meta.logical) {
                            Some(Value::Null) | None => None,
                            Some(value) => Some(value.to_string()),
                        },
                    )
                    .collect();
                Arc::new(text) as ArrayRef
            })
            .collect();
        let text_batch = RecordBatch::try_new(schema.clone(), arrays).map_err(|e| e.to_string())?;
        writer.write(&text_batch).map_err(|e| e.to_string())?;
    }
    writer.finish().map_err(|e| e.to_string())?;
    drop(writer);
    Ok(Response::new(out))
}

/// Handler for the commands that do not go through `tauri-specta` (only `fetch_window`).
fn raw_handler(invoke: tauri::ipc::Invoke<tauri::Wry>) -> bool {
    let handler: fn(tauri::ipc::Invoke<tauri::Wry>) -> bool =
        tauri::generate_handler![fetch_window];
    handler(invoke)
}

fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new()
        .commands(tauri_specta::collect_commands![
            workspace_info,
            get_language,
            set_language,
            get_theme,
            set_theme,
            engine_catalog,
            list_tunnels,
            test_connection,
            add_connection,
            open_workspace,
            trust_workspace,
            list_connections,
            list_history,
            set_history_enabled,
            connect,
            disconnect,
            run_script,
            cancel,
            release_result,
            list_schemas,
            list_objects,
            describe_table,
            answer_interaction,
        ])
        .events(tauri_specta::collect_events![InteractionRequest])
}

/// Generates `src/bindings.ts`. Run by `cargo run -p spelunk-desktop --bin export-bindings`
/// and, in development, on every app start.
pub fn export_bindings(path: &std::path::Path) -> Result<(), String> {
    specta_builder()
        .export(
            specta_typescript::Typescript::default()
                .header("// @ts-nocheck\n// Generated by tauri-specta: do not edit by hand."),
            path,
        )
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = specta_builder();
    #[cfg(debug_assertions)]
    if let Err(e) =
        export_bindings(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../src/bindings.ts"))
    {
        eprintln!("could not generate the TS types: {e}");
    }

    // SPELUNK_USER_DIR allows another personal directory (tests, profiles), as in the CLI.
    let user_dir = std::env::var_os("SPELUNK_USER_DIR")
        .map(PathBuf::from)
        .or_else(default_user_dir)
        .expect("user configuration directory");
    // Initial folder: command-line argument or the last one opened.
    let workspace = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .or_else(|| {
            std::fs::read_to_string(last_workspace_file(&user_dir))
                .ok()
                .map(|s| PathBuf::from(s.trim()))
        })
        .filter(|p| p.is_dir());
    let config = load(workspace.as_deref(), &user_dir);
    let history = QueryHistory::open(user_dir.join("state.db"))
        .expect("error while opening the local query history");
    let state = AppState {
        user_dir,
        workspace: RwLock::new(workspace),
        workspace_version: AtomicU64::new(0),
        config: RwLock::new(config),
        engines: Engines::builtin(),
        tunnels: Arc::new(TunnelPool::new()),
        results: Arc::new(ResultStore::new()),
        sessions: Mutex::new(HashMap::new()),
        history,
        pending: Pending::default(),
    };

    let typed = builder.invoke_handler();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(move |invoke| match invoke.message.command() {
            "fetch_window" => raw_handler(invoke),
            _ => typed(invoke),
        })
        .setup(move |app| {
            builder.mount_events(app);
            let _ = app.handle().get_webview_window("main");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while starting SpelunkDB");
}
