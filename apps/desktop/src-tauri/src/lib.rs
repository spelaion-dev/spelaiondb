//! SpelunkDB desktop app: a thin Tauri adapter over `spelunk-core` (ADR-002). There is no
//! database or security logic here: only translation between UI commands and the core.

mod dto;
mod interaction;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    Engines, ExportFormat, ExportOptions, GenericDialect, OpenConnection, OpenError, OpenOptions,
    QueryHistory, ResultId, ResultStore, SortKey, TunnelPool, open_connection, write_result,
};
use spelunk_driver_api::{Dialect, DriverError, Secret, Value};
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

/// Saves the query under `name` for the open workspace (or personally outside one),
/// replacing a saved query with that name. Returns its identifier.
#[tauri::command]
#[specta::specta]
async fn save_snippet(
    name: String,
    sql: String,
    connection: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    let id = state
        .history
        .save_snippet(workspace, name, sql, connection.unwrap_or_default())
        .await
        .map_err(|e| e.to_string())?;
    u32::try_from(id).map_err(|_| format!("saved query id out of range: {id}"))
}

/// Saved queries of the open workspace and the personal ones, by name.
#[tauri::command]
#[specta::specta]
async fn list_snippets(search: String, state: State<'_, AppState>) -> CmdResult<Vec<SnippetItem>> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .snippets(workspace, search)
        .await
        .map(|items| items.into_iter().map(SnippetItem::from).collect())
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn delete_snippet(id: u32, state: State<'_, AppState>) -> CmdResult<bool> {
    state
        .history
        .delete_snippet(id.into())
        .await
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
/// the file nor the keychain. `editing` is the connection being edited, if any: its stored
/// password is used when none is typed.
#[tauri::command]
#[specta::specta]
async fn test_connection(
    draft: ConnectionDraftItem,
    password: Option<String>,
    editing: Option<String>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ConnectOutcome> {
    let draft = ConnectionDraft::try_from(draft)?;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let resolved = {
        let config = state.config.read().await;
        let resolver = Resolver::new(&config, &trust, &SystemBackend);
        let resolved = match &editing {
            Some(id) => resolver.resolve_edit(&parse_id(id), &draft),
            None => resolver.resolve_draft(&draft),
        };
        match resolved {
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

/// The form of an existing connection, to edit it.
#[tauri::command]
#[specta::specta]
async fn connection_draft(
    connection: String,
    state: State<'_, AppState>,
) -> CmdResult<ConnectionDraftItem> {
    let config = state.config.read().await;
    let entry = config
        .connection(&parse_id(&connection))
        .ok_or_else(|| format!("connection {connection} does not exist"))?;
    Ok(ConnectionDraft::from_entry(entry).into())
}

/// Saves an edited connection (same file; for `keychain`, a typed password replaces the
/// stored one) and reloads the configuration.
#[tauri::command]
#[specta::specta]
async fn update_connection(
    connection: String,
    draft: ConnectionDraftItem,
    password: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let draft = ConnectionDraft::try_from(draft)?;
    let password = password.map(Secret::new);
    {
        let config = state.config.read().await;
        spelunk_config::update_connection(
            &config,
            &parse_id(&connection),
            &draft,
            password.as_ref(),
            &SystemBackend,
        )
        .map_err(|e| e.to_string())?;
    }
    state.reload().await;
    Ok(())
}

/// Deletes a connection file and its stored password. The UI closes its sessions first.
#[tauri::command]
#[specta::specta]
async fn delete_connection(connection: String, state: State<'_, AppState>) -> CmdResult<()> {
    {
        let config = state.config.read().await;
        spelunk_config::delete_connection(&config, &parse_id(&connection), &SystemBackend)
            .map_err(|e| e.to_string())?;
    }
    state.reload().await;
    Ok(())
}

fn source_of(personal: bool) -> Source {
    if personal {
        Source::User
    } else {
        Source::Workspace
    }
}

fn secrets(passwords: Vec<Option<String>>) -> Vec<Option<Secret>> {
    passwords
        .into_iter()
        .map(|p| p.filter(|p| !p.is_empty()).map(Secret::new))
        .collect()
}

/// The form of an existing tunnel, to edit it.
#[tauri::command]
#[specta::specta]
async fn tunnel_draft(
    name: String,
    personal: bool,
    state: State<'_, AppState>,
) -> CmdResult<TunnelDraftItem> {
    let config = state.config.read().await;
    let entry = config
        .tunnels
        .iter()
        .find(|t| t.source == source_of(personal) && t.name == name)
        .ok_or_else(|| format!("tunnel {name} does not exist"))?;
    Ok(spelunk_config::TunnelDraft::from_entry(entry).into())
}

/// Opens the tunnel's hops and closes them again. Nothing is written. `passwords[i]` is the
/// password typed for hop `i`; stored ones are used when none is typed.
#[tauri::command]
#[specta::specta]
async fn test_tunnel(
    draft: TunnelDraftItem,
    passwords: Vec<Option<String>>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<TunnelTestOutcome> {
    let draft = spelunk_config::TunnelDraft::from(draft);
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let resolved = {
        let config = state.config.read().await;
        match Resolver::new(&config, &trust, &SystemBackend).resolve_tunnel_draft(&draft) {
            Ok(resolved) => resolved,
            Err(spelunk_config::ResolveError::Untrusted { .. }) => {
                return Ok(TunnelTestOutcome::Untrusted);
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    spelunk_core::test_tunnel(
        &resolved,
        secrets(passwords),
        &KnownHosts::for_user(&state.user_dir),
        Arc::new(AppInteraction::new(app, state.pending.clone())),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(TunnelTestOutcome::Connected)
}

/// Saves a new tunnel (and its `keychain` passwords) and reloads the configuration.
#[tauri::command]
#[specta::specta]
async fn add_tunnel(
    draft: TunnelDraftItem,
    passwords: Vec<Option<String>>,
    state: State<'_, AppState>,
) -> CmdResult<TunnelItem> {
    let personal = draft.personal;
    let draft = spelunk_config::TunnelDraft::from(draft);
    let name = {
        let config = state.config.read().await;
        spelunk_config::add_tunnel(&config, &draft, &secrets(passwords), &SystemBackend)
            .map_err(|e| e.to_string())?
    };
    state.reload().await;
    Ok(TunnelItem { name, personal })
}

/// Saves an edited tunnel (same file and name) and reloads the configuration.
#[tauri::command]
#[specta::specta]
async fn update_tunnel(
    name: String,
    personal: bool,
    draft: TunnelDraftItem,
    passwords: Vec<Option<String>>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let draft = spelunk_config::TunnelDraft::from(draft);
    {
        let config = state.config.read().await;
        spelunk_config::update_tunnel(
            &config,
            source_of(personal),
            &name,
            &draft,
            &secrets(passwords),
            &SystemBackend,
        )
        .map_err(|e| e.to_string())?;
    }
    state.reload().await;
    Ok(())
}

/// Deletes a tunnel that no connection uses, with its stored passwords.
#[tauri::command]
#[specta::specta]
async fn delete_tunnel(name: String, personal: bool, state: State<'_, AppState>) -> CmdResult<()> {
    {
        let config = state.config.read().await;
        spelunk_config::delete_tunnel(&config, source_of(personal), &name, &SystemBackend)
            .map_err(|e| e.to_string())?;
    }
    state.reload().await;
    Ok(())
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
    confirmed: bool,
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
    // The core decides what needs confirmation; the UI asks first (`script_risks`), and a
    // run that skipped the question is refused here.
    if !confirmed && !risks_of(&open, &sql).is_empty() {
        return Err("this script has statements that need confirmation before running".into());
    }
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

/// The `:name` placeholders of a script, in order, so the UI can ask for their values.
#[tauri::command]
#[specta::specta]
async fn script_params(
    session: String,
    sql: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<String>> {
    let open = state.session(&session).await?;
    Ok(spelunk_core::script_params(
        open.session.dialect().as_ref(),
        &sql,
    ))
}

/// The script with its placeholders replaced by the values as literals, written by the core
/// with the engine's quoting.
#[tauri::command]
#[specta::specta]
async fn bind_params(
    session: String,
    sql: String,
    values: HashMap<String, String>,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    spelunk_core::bind_params(open.session.dialect().as_ref(), &sql, &values)
        .map_err(|e| e.to_string())
}

fn risks_of(open: &OpenConnection, sql: &str) -> Vec<spelunk_core::RiskyStatement> {
    spelunk_core::script_risks(
        open.session.dialect().as_ref(),
        sql,
        open.resolved.is_production(),
    )
}

/// Statements of a script that need confirmation before `run_script`: a DELETE or UPDATE
/// without WHERE, DROP, TRUNCATE, or any write on a production connection.
#[tauri::command]
#[specta::specta]
async fn script_risks(
    session: String,
    sql: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<RiskyItem>> {
    let open = state.session(&session).await?;
    Ok(risks_of(&open, &sql)
        .into_iter()
        .map(RiskyItem::from)
        .collect())
}

#[tauri::command]
#[specta::specta]
async fn cancel(session: String, state: State<'_, AppState>) -> CmdResult<()> {
    let open = state.session(&session).await?;
    open.session.cancel().await.map_err(|e| e.to_string())
}

/// Most rows copied to the clipboard at once; bigger selections are exported to a file.
const MAX_COPY_ROWS: u32 = 100_000;

/// Name used in `INSERT` statements when the source table is unknown.
const INSERT_TABLE: &str = "table_name";

fn export_format(format: TextFormat) -> ExportFormat {
    match format {
        TextFormat::Tsv => ExportFormat::Tsv,
        TextFormat::Csv => ExportFormat::Csv,
        TextFormat::Json => ExportFormat::Json,
        TextFormat::Markdown => ExportFormat::Markdown,
        TextFormat::InList => ExportFormat::InList,
        TextFormat::SqlInsert => ExportFormat::SqlInsert {
            table: INSERT_TABLE.into(),
        },
    }
}

/// The dialect of the session that produced a result, for SQL literals and quoting.
async fn dialect_for(state: &AppState, session: &str) -> Arc<dyn Dialect> {
    match state.sessions.lock().await.get(session) {
        Some(open) => open.open.session.dialect().clone(),
        None => Arc::new(GenericDialect),
    }
}

/// A selection of a result as text (TSV, CSV, JSON or INSERT), formatted by the core from
/// the typed data, for the clipboard.
#[tauri::command]
#[specta::specta]
async fn copy_rows(
    result: u32,
    session: String,
    range: CellRange,
    format: TextFormat,
    header: bool,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    if range.rows > MAX_COPY_ROWS {
        return Err(format!(
            "{} rows are too many to copy (at most {MAX_COPY_ROWS}): export them to a file",
            range.rows
        ));
    }
    let dialect = dialect_for(&state, &session).await;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let options = ExportOptions {
        format: export_format(format),
        rows: range.first_row as usize..(range.first_row + range.rows) as usize,
        columns: range.first_column as usize..(range.first_column + range.columns) as usize,
        header,
    };
    let mut out = Vec::new();
    write_result(&stored, &options, dialect.as_ref(), &mut out).map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Writes a whole result to a file the user chose (in a blocking task: it may be millions of
/// rows). Returns how many rows were written.
#[tauri::command]
#[specta::specta]
async fn export_result(
    result: u32,
    session: String,
    path: String,
    format: TextFormat,
    state: State<'_, AppState>,
) -> CmdResult<f64> {
    let dialect = dialect_for(&state, &session).await;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let rows = tokio::task::spawn_blocking(move || {
        let stored = stored
            .read()
            .map_err(|_| "result lock poisoned".to_string())?;
        if !stored.is_complete() {
            return Err("the result is still loading: export it when every row has arrived".into());
        }
        let options = ExportOptions {
            format: export_format(format),
            rows: 0..stored.rows(),
            columns: 0..stored.schema().columns.len(),
            header: true,
        };
        spelunk_core::export_to_file(&stored, &options, dialect.as_ref(), Path::new(&path))
            .map_err(|e| format!("could not write {path}: {e}"))
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(rows as f64)
}

/// Reads a connection URL (`postgres://…`, `mysql://…`, `jdbc:…`, `sqlite:…`) for the form.
#[tauri::command]
#[specta::specta]
fn parse_connection_url(url: String) -> CmdResult<UrlFields> {
    spelunk_config::parse_connection_url(&url)
        .map(UrlFields::from)
        .map_err(|e| e.to_string())
}

/// Creates an empty SQLite database file for a new connection. The core never overwrites an
/// existing file.
#[tauri::command]
#[specta::specta]
async fn create_sqlite_database(path: String) -> CmdResult<()> {
    tokio::task::spawn_blocking(move || {
        spelunk_core::create_sqlite_database(Path::new(&path)).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The statement under the editor's cursor (`offset` in UTF-8 bytes), split by the
/// engine's dialect in the core like a script run, for Ctrl+Enter.
#[tauri::command]
#[specta::specta]
async fn statement_at(
    engine: String,
    sql: String,
    offset: u32,
    state: State<'_, AppState>,
) -> CmdResult<Option<TextSpan>> {
    let driver = state
        .engines
        .get(engine_named(&engine)?)
        .ok_or("no driver for this engine")?;
    Ok(
        spelunk_core::statement_at(driver.dialect().as_ref(), &sql, offset as usize).map(|span| {
            TextSpan {
                start: u32::try_from(span.start).unwrap_or(u32::MAX),
                end: u32::try_from(span.end).unwrap_or(u32::MAX),
            }
        }),
    )
}

/// Sorts a result by one column in the core (`None` restores the server's order). The
/// grid then asks for its windows again.
#[tauri::command]
#[specta::specta]
async fn sort_result(
    result: u32,
    column: Option<u32>,
    descending: bool,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let key = column.map(|column| SortKey {
        column: column as usize,
        descending,
    });
    tokio::task::spawn_blocking(move || {
        stored
            .write()
            .map_err(|_| "result lock poisoned".to_string())?
            .sort(key)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Filters a result by column conditions in the core (all must hold; an empty list removes
/// them), on top of the text filter. The grid then asks for its windows again.
#[tauri::command]
#[specta::specta]
async fn filter_columns(
    result: u32,
    filters: Vec<ColumnFilterItem>,
    state: State<'_, AppState>,
) -> CmdResult<FilterOutcome> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let filters: Vec<spelunk_core::ColumnFilter> = filters.into_iter().map(Into::into).collect();
    tokio::task::spawn_blocking(move || {
        let mut stored = stored
            .write()
            .map_err(|_| "result lock poisoned".to_string())?;
        let rows = stored.filter_columns(filters).map_err(|e| e.to_string())?;
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Ok(FilterOutcome {
            rows: n(rows),
            total: n(stored.total_rows()),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Every cell of one row as text (`None` for NULL), for the viewer's row view.
#[tauri::command]
#[specta::specta]
async fn row_values(
    result: u32,
    row: u32,
    state: State<'_, AppState>,
) -> CmdResult<Vec<Option<String>>> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let batch = stored
        .window(row as usize, 1)
        .into_iter()
        .next()
        .ok_or("no such row")?;
    Ok(stored
        .schema()
        .columns
        .iter()
        .enumerate()
        .map(
            |(i, meta)| match Value::from_array(batch.column(i).as_ref(), 0, meta.logical) {
                Some(Value::Null) | None => None,
                Some(value) => Some(value.to_string()),
            },
        )
        .collect())
}

/// Filters a result in the core: rows where some cell contains `text` (ignoring case);
/// `None` shows every row again. The grid then asks for its windows again.
#[tauri::command]
#[specta::specta]
async fn filter_result(
    result: u32,
    text: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<FilterOutcome> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    tokio::task::spawn_blocking(move || {
        let mut stored = stored
            .write()
            .map_err(|_| "result lock poisoned".to_string())?;
        let rows = stored.filter(text.as_deref()).map_err(|e| e.to_string())?;
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Ok(FilterOutcome {
            rows: n(rows),
            total: n(stored.total_rows()),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The full text of one cell (`None` for NULL), for the cell viewer: the grid only paints
/// what fits, and the row may not be in its cache.
#[tauri::command]
#[specta::specta]
async fn cell_value(
    result: u32,
    row: u32,
    column: u32,
    state: State<'_, AppState>,
) -> CmdResult<Option<String>> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let column = column as usize;
    let meta = stored
        .schema()
        .columns
        .get(column)
        .ok_or("no such column")?
        .clone();
    let batch = stored
        .window(row as usize, 1)
        .into_iter()
        .next()
        .ok_or("no such row")?;
    Ok(
        match Value::from_array(batch.column(column).as_ref(), 0, meta.logical) {
            Some(Value::Null) | None => None,
            Some(value) => Some(value.to_string()),
        },
    )
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

/// SQL for a table written by the core with the engine's quoting (preview, SELECT, INSERT,
/// UPDATE or DELETE with `:named` placeholders), to open in a new tab.
#[tauri::command]
#[specta::specta]
async fn table_sql(
    session: String,
    schema: String,
    name: String,
    template: SqlTemplateItem,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    let table = open
        .session
        .connection()
        .describe_table(&schema, &name)
        .await
        .map_err(|e| e.to_string())?;
    Ok(spelunk_core::table_sql(
        open.session.dialect().as_ref(),
        &table,
        template.into(),
    ))
}

/// The exact row count of a table or view, counted by the core on the explorer's session.
#[tauri::command]
#[specta::specta]
async fn count_rows(
    session: String,
    schema: String,
    name: String,
    state: State<'_, AppState>,
) -> CmdResult<f64> {
    let open = state.session(&session).await?;
    // f64 holds every count up to 2^53 exactly, and TypeScript numbers are f64.
    Ok(spelunk_core::count_rows(&open.session, &schema, &name).await? as f64)
}

/// The SELECT of one page of a table, written and checked by the core: the filter and the
/// order cannot turn it into another query.
#[tauri::command]
#[specta::specta]
async fn table_page_sql(
    session: String,
    page: TablePageItem,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    spelunk_core::table_page_sql(open.session.dialect().as_ref(), &page.into())
        .map_err(|e| e.to_string())
}

/// How many rows of a table the filter keeps (the table data tab's "Count").
#[tauri::command]
#[specta::specta]
async fn count_table_rows(
    session: String,
    schema: String,
    name: String,
    filter: String,
    state: State<'_, AppState>,
) -> CmdResult<f64> {
    let open = state.session(&session).await?;
    // f64 holds every count up to 2^53 exactly, and TypeScript numbers are f64.
    Ok(spelunk_core::count_table_rows(&open.session, &schema, &name, &filter).await? as f64)
}

/// An identifier quoted for the session's engine (for an ORDER BY the UI writes).
#[tauri::command]
#[specta::specta]
async fn quote_identifier(
    session: String,
    identifier: String,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    Ok(open.session.dialect().quote_identifier(&identifier))
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
            connection_draft,
            update_connection,
            delete_connection,
            tunnel_draft,
            test_tunnel,
            add_tunnel,
            update_tunnel,
            delete_tunnel,
            copy_rows,
            export_result,
            cell_value,
            sort_result,
            filter_result,
            filter_columns,
            row_values,
            table_sql,
            count_rows,
            table_page_sql,
            count_table_rows,
            quote_identifier,
            save_snippet,
            list_snippets,
            delete_snippet,
            statement_at,
            create_sqlite_database,
            script_risks,
            script_params,
            bind_params,
            parse_connection_url,
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
