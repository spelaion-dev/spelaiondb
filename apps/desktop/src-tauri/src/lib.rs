//! SpelaionDB desktop app: a thin Tauri adapter over `spelaion-core` (ADR-002). There is no
//! database or security logic here: only translation between UI commands and the core.

mod connection_import;
mod dto;
mod files;
mod import;
mod interaction;
mod library;
mod mcp;
mod plans;
mod update;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use futures_util::StreamExt;
use spelaion_config::{
    Config, ConnectionDraft, ConnectionId, Resolver, Source, SystemBackend, TrustStore,
    default_user_dir, load,
};
use spelaion_core::{
    Engines, ExportCancel, ExportFormat, ExportOptions, FindQuery, GenericDialect, OpenConnection,
    OpenError, OpenOptions, QueryHistory, ResultId, ResultStore, SessionHealth, SortKey,
    StreamExport, TabKeeper, TunnelPool, open_connection, write_result,
};
use spelaion_driver_api::{Dialect, DriverError, Secret, Value};
use spelaion_ssh::KnownHosts;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, Manager, State};
use tauri_specta::Event as _;
use tokio::sync::{Mutex, RwLock};

use crate::dto::*;
use crate::interaction::{AppInteraction, Pending};
use spelaion_driver_api::NameFilter;

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
    plans: Arc<spelaion_core::plan::PlanStore>,
    startup_plan: std::sync::Mutex<Option<String>>,
    sessions: Mutex<HashMap<String, DesktopSession>>,
    history: QueryHistory,
    /// Writes the open tabs to the local store on a deadline (ADR-036).
    tabs: TabKeeper,
    pending: Pending,
    /// The user confirmed quitting with transactions open (ADR-031).
    quitting: AtomicBool,
    tests: ConnectionTests,
    /// Streaming exports running, by the id the UI gave them (ADR-034).
    exports: std::sync::Mutex<HashMap<u32, Arc<ExportCancel>>>,
    /// Imports running, by the id the UI gave them (ADR-038).
    imports: std::sync::Mutex<HashMap<u32, Arc<spelaion_core::ImportCancel>>>,
}

struct DesktopSession {
    open: Arc<OpenConnection>,
    /// A prompted password stays in memory only for the lifetime of this session.
    password: Option<Secret>,
    /// Captured when connecting: changing folders must not reattribute an existing tab.
    history_workspace: String,
}

/// Connection tests running now, by the id the UI gave them, to stop them (ADR-032).
#[derive(Default)]
struct ConnectionTests(std::sync::Mutex<HashMap<u32, tokio::sync::oneshot::Sender<()>>>);

impl ConnectionTests {
    /// Registers a test; the future completes when it is stopped.
    fn start(&self, test: u32) -> impl std::future::Future<Output = ()> + Send + 'static {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0.lock().expect("lock poisoned").insert(test, tx);
        async move {
            // Forgotten without a stop (the test ended): never completes.
            if rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    fn stop(&self, test: u32) {
        if let Some(tx) = self.0.lock().expect("lock poisoned").remove(&test) {
            let _ = tx.send(());
        }
    }

    fn end(&self, test: u32) {
        self.0.lock().expect("lock poisoned").remove(&test);
    }
}

type CmdResult<T> = Result<T, String>;

impl AppState {
    async fn reload(&self) {
        let version = self.workspace_version.load(Ordering::Acquire);
        let workspace = self.workspace.read().await.clone();
        let loaded = load(workspace.as_deref(), &self.user_dir);
        if let Ok(mut trust) = TrustStore::load(&self.user_dir) {
            let _ = trust.adopt(&loaded);
        }
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
async fn get_explorer_preferences(
    state: State<'_, AppState>,
) -> CmdResult<ExplorerPreferencesItem> {
    state
        .history
        .explorer_preferences()
        .await
        .map(ExplorerPreferencesItem::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_explorer_preferences(
    preferences: ExplorerPreferencesItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_explorer_preferences(preferences.into())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn get_editor_preferences(state: State<'_, AppState>) -> CmdResult<EditorPreferencesItem> {
    state
        .history
        .editor_preferences()
        .await
        .map(Into::into)
        .map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
async fn set_editor_preferences(
    preferences: EditorPreferencesItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_editor_preferences(preferences.into())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn get_grid_preferences(state: State<'_, AppState>) -> CmdResult<GridPreferencesItem> {
    state
        .history
        .grid_preferences()
        .await
        .map(GridPreferencesItem::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_grid_preferences(
    preferences: GridPreferencesItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_grid_preferences(preferences.into())
        .await
        .map_err(|e| e.to_string())
}

/// A system notification that a long run finished in the background (ADR-035). The app sends
/// it only when the user turned it on; its text names the tab and counts statements, never SQL,
/// data or error text.
#[tauri::command]
#[specta::specta]
async fn notify_run_finished(title: String, body: String, app: tauri::AppHandle) -> CmdResult<()> {
    use tauri_plugin_notification::NotificationExt;
    app.notification()
        .builder()
        .title(title)
        .body(body)
        .show()
        .map_err(|e| e.to_string())
}

/// How scripts run from the editor: the default error mode and the opt-in notification
/// (ADR-035).
#[tauri::command]
#[specta::specta]
async fn get_script_preferences(state: State<'_, AppState>) -> CmdResult<ScriptPreferencesItem> {
    state
        .history
        .script_preferences()
        .await
        .map(ScriptPreferencesItem::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_script_preferences(
    preferences: ScriptPreferencesItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_script_preferences(preferences.into())
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn get_update_check(state: State<'_, AppState>) -> CmdResult<bool> {
    state
        .history
        .update_check()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_update_check(enabled: bool, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .history
        .set_update_check(enabled)
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
async fn trust_workspace(root: String, state: State<'_, AppState>) -> CmdResult<WorkspaceInfo> {
    let mut trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let config = state.config.read().await;
    if config.workspace_root.as_deref() != Some(Path::new(&root)) {
        return Err("the folder changed; trust it again".into());
    }
    trust
        .trust_if_untrusted(&config)
        .map_err(|e| e.to_string())?;
    drop(config);
    state.info().await
}

/// Saves values for `${user.…}` variables in the user's `user.toml` (ADR-022) and reloads
/// the configuration. Only those entries change; the rest of the file stays as it was.
#[tauri::command]
#[specta::specta]
async fn set_user_vars(
    values: HashMap<String, String>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let values = values.into_iter().collect();
    spelaion_config::set_user_vars(&state.user_dir, &values).map_err(|e| e.to_string())?;
    state.reload().await;
    Ok(())
}

/// Every connection type (ADR-029): built-in, the team's (from a trusted repository) and the
/// user's, with every layer applied.
#[tauri::command]
#[specta::specta]
async fn list_connection_types(state: State<'_, AppState>) -> CmdResult<Vec<ConnectionTypeItem>> {
    state.reload().await;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let config = state.config.read().await;
    Ok(spelaion_config::ConnectionTypes::load(&config, &trust)
        .all()
        .iter()
        .map(ConnectionTypeItem::from)
        .collect())
}

/// Saves the user's definition of a connection type (a new one, or their changes to a
/// built-in or team type) in their `connection-types.toml`. Safeguards only add up: the core
/// keeps what other layers turn on.
#[tauri::command]
#[specta::specta]
async fn save_connection_type(
    id: String,
    draft: ConnectionTypeDraft,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    spelaion_config::set_personal_type(&state.user_dir, id.trim(), &draft.into())
        .map_err(|e| e.to_string())?;
    state.reload().await;
    Ok(())
}

/// Removes the user's definition of a connection type: a type of their own goes, and a
/// built-in or team one goes back to how the others define it.
#[tauri::command]
#[specta::specta]
async fn delete_connection_type(id: String, state: State<'_, AppState>) -> CmdResult<()> {
    spelaion_config::remove_personal_type(&state.user_dir, &id).map_err(|e| e.to_string())?;
    state.reload().await;
    Ok(())
}

/// The personal range of local ports for SSH tunnels (`[tunnels]` in `user.toml`), if set.
#[tauri::command]
#[specta::specta]
async fn tunnel_ports(state: State<'_, AppState>) -> CmdResult<Option<TunnelPortsItem>> {
    let config = state.config.read().await;
    Ok(config
        .user
        .tunnels
        .range()
        .ok()
        .flatten()
        .map(|(min, max)| TunnelPortsItem { min, max }))
}

/// Sets (or clears, with null) the personal range of local ports for SSH tunnels. It applies
/// to tunnels opened from then on.
#[tauri::command]
#[specta::specta]
async fn set_tunnel_ports(
    range: Option<TunnelPortsItem>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    spelaion_config::set_tunnel_ports(&state.user_dir, range.map(|r| (r.min, r.max)))
        .map_err(|e| e.to_string())?;
    state.reload().await;
    Ok(())
}

/// Accepts a reviewed change to a repository connection or tunnel (ADR-022): it is recorded
/// as it is now, and connecting can go on.
#[tauri::command]
#[specta::specta]
async fn accept_change(
    root: String,
    item: String,
    is_new: bool,
    fields: Vec<FieldChangeItem>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state.reload().await;
    let config = state.config.read().await;
    let mut trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let change = spelaion_config::ItemChange {
        item,
        new: is_new,
        fields: fields
            .into_iter()
            .map(|field| spelaion_config::FieldChange {
                field: field.field,
                before: field.before,
                after: field.after,
            })
            .collect(),
    };
    trust
        .accept_reviewed(&config, Path::new(&root), &change)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn list_connections(state: State<'_, AppState>) -> CmdResult<Vec<ConnectionItem>> {
    state.reload().await;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let config = state.config.read().await;
    let connections = config.connections.clone();
    let types = spelaion_config::ConnectionTypes::load(&config, &trust);
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
            description: c.file.description.clone(),
            tags: c.file.tags.clone(),
            engine: c.file.engine.as_str().into(),
            env: c.file.env.clone(),
            kind: c
                .file
                .env
                .as_deref()
                .and_then(|env| types.get(env))
                .map(ConnectionTypeItem::from),
            folders: c.folders().into_iter().map(String::from).collect(),
            personal: c.id.source == Source::User,
            host: c.file.host.clone(),
            tunnel: c.file.tunnel.clone(),
            mcp_exposed: c.file.mcp.exposed,
            history_enabled,
            read_only: c.file.read_only,
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
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .delete_visible_snippet(workspace, id.into())
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

/// A connection's explorer filter (ADR-025), personal and per workspace.
#[tauri::command]
#[specta::specta]
async fn get_explorer_filter(
    connection: String,
    state: State<'_, AppState>,
) -> CmdResult<ExplorerFilterItem> {
    let workspace = connection_workspace(&state, &connection).await?;
    state
        .history
        .explorer_filter(workspace, connection)
        .await
        .map(ExplorerFilterItem::from)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn set_explorer_filter(
    connection: String,
    filter: ExplorerFilterItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let workspace = connection_workspace(&state, &connection).await?;
    state
        .history
        .set_explorer_filter(workspace, connection, filter.into())
        .await
        .map_err(|e| e.to_string())
}

/// "Hide" or "Show only" on the schemas, objects or columns chosen in the explorer: their
/// names, with their wildcard characters escaped, go into the connection's filter. Returns the
/// new filter.
#[tauri::command]
#[specta::specta]
async fn filter_explorer_name(
    connection: String,
    kind: FilterKindItem,
    names: Vec<String>,
    action: FilterActionItem,
    state: State<'_, AppState>,
) -> CmdResult<ExplorerFilterItem> {
    let workspace = connection_workspace(&state, &connection).await?;
    let mut filter = state
        .history
        .explorer_filter(workspace.clone(), connection.clone())
        .await
        .map_err(|e| e.to_string())?;
    let patterns: Vec<String> = names
        .iter()
        .map(|name| spelaion_driver_api::literal_pattern(name))
        .collect();
    match action {
        FilterActionItem::Hide => {
            for pattern in &patterns {
                filter.hide(kind.into(), pattern);
            }
        }
        FilterActionItem::ShowOnly => filter.show_only(kind.into(), &patterns),
    }
    state
        .history
        .set_explorer_filter(workspace, connection, filter.clone())
        .await
        .map_err(|e| e.to_string())?;
    Ok(ExplorerFilterItem::from(filter))
}

/// The explorer's bookmarks (ADR-026) of the personal connections and of the open workspace's,
/// for the connections that exist now.
#[tauri::command]
#[specta::specta]
async fn list_bookmarks(state: State<'_, AppState>) -> CmdResult<Vec<BookmarkItem>> {
    let workspaces = {
        let config = state.config.read().await;
        let mut scopes = vec![history_workspace(&config, Source::User)];
        let repository = history_workspace(&config, Source::Workspace);
        if !repository.is_empty() {
            scopes.push(repository);
        }
        scopes
    };
    let mut all = Vec::new();
    for workspace in workspaces {
        let bookmarks = state
            .history
            .bookmarks(workspace.clone())
            .await
            .map_err(|e| e.to_string())?;
        for bookmark in bookmarks {
            // Kept under this scope only if the connection of that id is still this scope's.
            if connection_workspace(&state, &bookmark.connection)
                .await
                .ok()
                == Some(workspace.clone())
            {
                all.push(BookmarkItem::from(bookmark));
            }
        }
    }
    Ok(all)
}

#[tauri::command]
#[specta::specta]
async fn add_bookmark(bookmark: BookmarkItem, state: State<'_, AppState>) -> CmdResult<()> {
    let workspace = connection_workspace(&state, &bookmark.connection).await?;
    state
        .history
        .add_bookmark(workspace, bookmark.into())
        .await
        .map_err(|e| e.to_string())
}

/// Whether the bookmark was there.
#[tauri::command]
#[specta::specta]
async fn remove_bookmark(bookmark: BookmarkItem, state: State<'_, AppState>) -> CmdResult<bool> {
    let workspace = connection_workspace(&state, &bookmark.connection).await?;
    state
        .history
        .remove_bookmark(workspace, bookmark.into())
        .await
        .map_err(|e| e.to_string())
}

/// The scope a connection's personal preferences are kept under (see `history_workspace`).
async fn connection_workspace(state: &AppState, connection: &str) -> CmdResult<String> {
    let id = parse_id(connection);
    let config = state.config.read().await;
    if config.connection(&id).is_none() {
        return Err(format!("unknown connection: {connection}"));
    }
    Ok(history_workspace(&config, id.source))
}

/// Whether there is a system keychain. Without one (Linux with no Secret Service running) the
/// forms cannot offer to save a password: it is asked when connecting.
#[tauri::command]
#[specta::specta]
async fn keychain_available() -> bool {
    // Over D-Bus on Linux: kept off the async runtime.
    tokio::task::spawn_blocking(|| {
        spelaion_config::SecretBackend::keychain_available(&SystemBackend)
    })
    .await
    .unwrap_or(false)
}

/// Engines shown when adding a connection, including the ones not supported yet.
#[tauri::command]
#[specta::specta]
fn engine_catalog() -> Vec<EngineItem> {
    spelaion_core::engine_catalog()
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

/// Every tunnel file as a network profile (ADR-027), for Settings → Network.
#[tauri::command]
#[specta::specta]
async fn network_profiles(state: State<'_, AppState>) -> CmdResult<Vec<NetworkProfileItem>> {
    let config = state.config.read().await;
    Ok(config
        .tunnels
        .iter()
        .map(|t| NetworkProfileItem::of(&config, t))
        .collect())
}

/// Tests a connection that may not be saved yet, step by step (ADR-032): every change to its
/// steps goes through `on_step`, and `stop_connection_test(test)` stops it. Nothing is
/// written: neither the file nor the keychain. `editing` is the connection being edited, if
/// any: its stored password is used when none is typed. `network_only` stops before logging
/// in. The steps, their errors and the diagnostics come from the core, redacted.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn test_connection(
    draft: ConnectionDraftItem,
    password: Option<String>,
    editing: Option<String>,
    network_only: bool,
    test: u32,
    on_step: Channel<Vec<TestStepItem>>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ConnectionTestOutcome> {
    let draft = ConnectionDraft::try_from(draft)?;
    let name = draft.name.clone();
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let resolved = {
        let config = state.config.read().await;
        let resolver = Resolver::new(&config, &trust, &SystemBackend);
        let resolved = match &editing {
            Some(id) => resolver.resolve_edit(&parse_id(id), &draft),
            None => resolver.resolve_draft(&draft),
        };
        match resolved {
            Err(spelaion_config::ResolveError::Untrusted { .. }) => {
                return Ok(ConnectionTestOutcome::Untrusted);
            }
            Err(spelaion_config::ResolveError::Changed { root, change }) => {
                return Ok(ConnectionTestOutcome::changed(root, change));
            }
            Err(spelaion_config::ResolveError::MissingVariables { names }) => {
                return Ok(ConnectionTestOutcome::MissingVariables { names });
            }
            // Any other error is the test's first step.
            other => other,
        }
    };
    let options = spelaion_core::TestOptions {
        scope: if network_only {
            spelaion_core::TestScope::Network
        } else {
            spelaion_core::TestScope::Full
        },
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app, state.pending.clone())),
        password: password.filter(|p| !p.is_empty()).map(Secret::new),
        name,
    };
    let stop = state.tests.start(test);
    let observer = |steps: &[spelaion_core::TestStep]| {
        let _ = on_step.send(steps.iter().map(TestStepItem::from).collect());
    };
    let report =
        spelaion_core::test_connection(resolved, &state.engines, options, &observer, stop).await;
    state.tests.end(test);
    Ok(ConnectionTestOutcome::Tested {
        report: report.into(),
    })
}

/// Stops a running connection or network profile test: the running step is dropped and
/// whatever it opened is closed. Nothing is retried.
#[tauri::command]
#[specta::specta]
fn stop_connection_test(test: u32, state: State<'_, AppState>) {
    state.tests.stop(test);
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
        let file = draft.to_file(&config).map_err(|e| e.to_string())?;
        check_setup_before_saving(&state, &config, &file)?;
        let id =
            spelaion_config::add_connection(&config, &draft, password.as_ref(), &SystemBackend)
                .map_err(|e| e.to_string())?;
        if id.source == Source::Workspace {
            TrustStore::load(&state.user_dir)
                .and_then(|mut trust| trust.accept_connection_write(&config, &id, &file))
                .map_err(|e| e.to_string())?;
        }
        id
    };
    state.reload().await;
    Ok(id.to_string())
}

/// A session setup the connection's sessions would refuse (ADR-037: a write on a read-only or
/// Production connection) is refused before it is saved, with the core's message.
fn check_setup_before_saving(
    state: &AppState,
    config: &spelaion_config::Config,
    file: &spelaion_config::ConnectionFile,
) -> CmdResult<()> {
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let safeguards = spelaion_config::ConnectionTypes::load(config, &trust)
        .resolve(file.env.as_deref())
        .ok()
        .flatten()
        .map(|kind| kind.safeguards)
        .unwrap_or_default();
    spelaion_core::check_file_setup(file, &safeguards).map_err(|e| e.to_string())
}

/// Checks whether an existing connection can be opened in the editor.
#[tauri::command]
#[specta::specta]
async fn connection_editor_access(
    connection: String,
    state: State<'_, AppState>,
) -> CmdResult<EditorAccessOutcome> {
    let id = parse_id(&connection);
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let config = state.config.read().await;
    match Resolver::new(&config, &trust, &SystemBackend).check_editor_access(id.source, Some(&id)) {
        Ok(()) => Ok(EditorAccessOutcome::Ready),
        Err(spelaion_config::ResolveError::Untrusted { .. }) => Ok(EditorAccessOutcome::Untrusted),
        Err(spelaion_config::ResolveError::Changed { root, change }) => {
            Ok(EditorAccessOutcome::Changed {
                root: root.to_string_lossy().into_owned(),
                item: change.item,
                new: change.new,
                fields: change
                    .fields
                    .into_iter()
                    .map(|f| FieldChangeItem {
                        field: f.field,
                        before: f.before,
                        after: f.after,
                    })
                    .collect(),
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// The form of an existing connection, to edit it.
#[tauri::command]
#[specta::specta]
async fn connection_draft(
    connection: String,
    state: State<'_, AppState>,
) -> CmdResult<(ConnectionDraftItem, String)> {
    let config = state.config.read().await;
    let id = parse_id(&connection);
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    Resolver::new(&config, &trust, &SystemBackend)
        .check_editor_access(id.source, Some(&id))
        .map_err(|e| e.to_string())?;
    let entry = config
        .connection(&id)
        .ok_or_else(|| format!("connection {connection} does not exist"))?;
    let revision = spelaion_config::connection_revision(entry).map_err(|e| e.to_string())?;
    Ok((ConnectionDraft::from_entry(entry).into(), revision))
}

/// Saves an edited connection (same file; for `keychain`, a typed password replaces the
/// stored one) and reloads the configuration.
#[tauri::command]
#[specta::specta]
async fn update_connection(
    connection: String,
    draft: ConnectionDraftItem,
    password: Option<String>,
    revision: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let draft = ConnectionDraft::try_from(draft)?;
    let password = password.map(Secret::new);
    {
        let config = state.config.read().await;
        let id = parse_id(&connection);
        let file = draft.to_file(&config).map_err(|e| e.to_string())?;
        check_setup_before_saving(&state, &config, &file)?;
        let written = spelaion_config::update_connection_at_revision(
            &config,
            &parse_id(&connection),
            &draft,
            password.as_ref(),
            &SystemBackend,
            &revision,
        )
        .map_err(|e| e.to_string())?;
        if id.source == Source::Workspace {
            TrustStore::load(&state.user_dir)
                .and_then(|mut trust| trust.accept_connection_edit(&config, &id, &written))
                .map_err(|e| e.to_string())?;
        }
    }
    state.reload().await;
    Ok(())
}

/// The form mapped to another engine (ADR-030): what still applies is kept, the rest is
/// changed or dropped by the core; nothing is saved until the form is.
#[tauri::command]
#[specta::specta]
fn change_connection_engine(
    draft: ConnectionDraftItem,
    engine: String,
) -> CmdResult<MappedConnectionItem> {
    let draft = ConnectionDraft::try_from(draft)?;
    let (mapped, change) = draft.with_engine(engine_named(&engine)?);
    Ok(MappedConnectionItem {
        draft: mapped.into(),
        change: change.into(),
    })
}

/// Duplicates a connection next to the original (ADR-030). A stored password is copied to the
/// copy's own keychain entry by the core; a repository connection must be as trusted.
#[tauri::command]
#[specta::specta]
async fn duplicate_connection(
    connection: String,
    state: State<'_, AppState>,
) -> CmdResult<DuplicatedItem> {
    let duplicated = {
        let config = state.config.read().await;
        let mut trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
        spelaion_config::duplicate_connection(
            &config,
            &mut trust,
            &parse_id(&connection),
            &SystemBackend,
        )
        .map_err(|e| e.to_string())?
    };
    state.reload().await;
    Ok(DuplicatedItem {
        id: duplicated.id.to_string(),
        name: duplicated.name,
        password_copied: duplicated.password_copied,
    })
}

/// Moves a connection to another folder (`""` for none) in its layer. The core moves its
/// file, its keychain password and, for a repository connection, what trust pinned for it,
/// without adding any (ADR-022); its explorer filter, bookmarks, history setting and saved
/// tabs follow. A connection in use is not moved: the UI disconnects it first.
#[tauri::command]
#[specta::specta]
async fn move_connection(
    connection: String,
    folder: String,
    state: State<'_, AppState>,
) -> CmdResult<MovedItem> {
    let id = parse_id(&connection);
    let in_use = state
        .sessions
        .lock()
        .await
        .values()
        .any(|session| session.open.resolved.id == id);
    if in_use {
        return Err("the connection is in use: disconnect it before moving it".into());
    }
    // Tabs waiting to be written carry the old identifier: written first, then moved with it.
    state.tabs.flush().await.map_err(|e| e.to_string())?;
    let (moved, workspace) = {
        let config = state.config.read().await;
        let mut trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
        let moved =
            spelaion_config::move_connection(&config, &mut trust, &id, &folder, &SystemBackend)
                .map_err(|e| e.to_string())?;
        let workspace = match id.source {
            Source::User => None,
            Source::Workspace => Some(history_workspace(&config, Source::Workspace)),
        };
        (moved, workspace)
    };
    let to = moved.id.to_string();
    let settings = if moved.id == id {
        Ok(())
    } else {
        state
            .history
            .move_connection(workspace, connection, to.clone())
            .await
    };
    state.reload().await;
    // The file is where the user put it: settings that could not follow are said, not undone.
    settings
        .map_err(|e| format!("the connection moved, but its explorer settings did not: {e}"))?;
    Ok(MovedItem {
        id: to,
        password_moved: moved.password_moved,
    })
}

/// The text Ctrl+C puts on the clipboard for a connection: without secrets or programs.
#[tauri::command]
#[specta::specta]
async fn copy_connection(
    connection: String,
    state: State<'_, AppState>,
) -> CmdResult<ClipboardConnectionItem> {
    let config = state.config.read().await;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let copied = spelaion_config::connection_clipboard_text(
        &config,
        &trust,
        &parse_id(&connection),
        &SystemBackend,
    )
    .map_err(|e| e.to_string())?;
    Ok(ClipboardConnectionItem {
        text: copied.text,
        left_out: copied.left_out,
    })
}

/// A personal connection from clipboard text, checked by the core as untrusted input.
#[tauri::command]
#[specta::specta]
async fn paste_connection(text: String, state: State<'_, AppState>) -> CmdResult<PastedItem> {
    let pasted = {
        let config = state.config.read().await;
        spelaion_config::paste_connection(&config, &text, &SystemBackend)
            .map_err(|e| e.to_string())?
    };
    state.reload().await;
    Ok(PastedItem {
        id: pasted.id.to_string(),
        name: pasted.name,
        left_out: pasted.left_out,
    })
}

/// The display name of an engine, as the catalog shows it.
fn engine_display_name(engine: spelaion_config::Engine) -> String {
    spelaion_core::engine_catalog()
        .into_iter()
        .find(|info| info.engine == Some(engine))
        .map_or_else(|| engine.as_str().to_string(), |info| info.name.to_string())
}

/// The pattern new connections are named with (ADR-030).
#[tauri::command]
#[specta::specta]
async fn get_connection_name_pattern(state: State<'_, AppState>) -> CmdResult<String> {
    state
        .history
        .connection_name_pattern()
        .await
        .map_err(|e| e.to_string())
}

/// Sets the pattern (checked by the core) or, with null, goes back to the default.
#[tauri::command]
#[specta::specta]
async fn set_connection_name_pattern(
    pattern: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_connection_name_pattern(pattern)
        .await
        .map_err(|e| e.to_string())
}

/// Checks a pattern and shows what it gives for two sample connections, for Settings.
#[tauri::command]
#[specta::specta]
fn preview_connection_name_pattern(pattern: String) -> NamePatternPreview {
    use spelaion_config::{Engine, NamePattern, NamePatternError, NameValues};
    let variables = spelaion_config::NAME_VARIABLES.map(String::from).to_vec();
    match NamePattern::parse(&pattern) {
        Ok(parsed) => {
            let server = NameValues {
                host: "db.example.com".into(),
                port: "5432".into(),
                database: "orders".into(),
                user: "app".into(),
                engine: engine_display_name(Engine::Postgres),
                env: "prod".into(),
                file: String::new(),
            };
            let file = NameValues {
                database: "shop".into(),
                file: "shop.sqlite".into(),
                engine: engine_display_name(Engine::Sqlite),
                env: "dev".into(),
                ..NameValues::default()
            };
            NamePatternPreview {
                server: parsed.render(&server),
                file: parsed.render(&file),
                error: None,
                unknown: Vec::new(),
                variables,
            }
        }
        Err(error) => NamePatternPreview {
            server: String::new(),
            file: String::new(),
            unknown: match &error {
                NamePatternError::Unknown(names) => names.clone(),
                NamePatternError::Unclosed
                | NamePatternError::Unopened
                | NamePatternError::Empty
                | NamePatternError::TooLong => Vec::new(),
            },
            error: Some(error.to_string()),
            variables,
        },
    }
}

/// The name the user's pattern gives the connection being added, as the form is now; null
/// when the pattern is empty (no suggestions).
#[tauri::command]
#[specta::specta]
async fn suggest_connection_name(
    draft: ConnectionDraftItem,
    state: State<'_, AppState>,
) -> CmdResult<Option<String>> {
    let draft = ConnectionDraft::try_from(draft)?;
    let pattern = state
        .history
        .connection_name_pattern()
        .await
        .map_err(|e| e.to_string())?;
    if pattern.trim().is_empty() {
        return Ok(None);
    }
    // A pattern stored by an older version that no longer parses suggests nothing.
    Ok(spelaion_config::NamePattern::parse(&pattern).ok().map(|p| {
        p.render(&spelaion_config::NameValues::of_draft(
            &draft,
            &engine_display_name(draft.engine),
        ))
    }))
}

/// Deletes a connection file and its stored password. The UI closes its sessions first.
#[tauri::command]
#[specta::specta]
async fn delete_connection(connection: String, state: State<'_, AppState>) -> CmdResult<()> {
    {
        let config = state.config.read().await;
        spelaion_config::delete_connection(&config, &parse_id(&connection), &SystemBackend)
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

/// Passwords typed in the tunnel editor: one per hop, and the proxy's (ADR-027).
fn tunnel_passwords(
    hops: Vec<Option<String>>,
    proxy: Option<String>,
) -> spelaion_config::TunnelPasswords {
    spelaion_config::TunnelPasswords {
        hops: secrets(hops),
        proxy: proxy.filter(|p| !p.is_empty()).map(Secret::new),
    }
}

/// The form of an existing tunnel, to edit it.
#[tauri::command]
#[specta::specta]
async fn tunnel_draft(
    name: String,
    personal: bool,
    state: State<'_, AppState>,
) -> CmdResult<(TunnelDraftItem, String)> {
    let config = state.config.read().await;
    let entry = config
        .tunnels
        .iter()
        .find(|t| t.source == source_of(personal) && t.name == name)
        .ok_or_else(|| format!("tunnel {name} does not exist"))?;
    let revision = spelaion_config::tunnel_revision(entry).map_err(|e| e.to_string())?;
    Ok((
        spelaion_config::TunnelDraft::from_entry(entry).into(),
        revision,
    ))
}

/// Tests a network profile step by step (ADR-032): its proxy and each hop, opened and closed
/// again. Nothing is written. `passwords[i]` is the password typed for hop `i`; stored ones
/// are used when none is typed. Steps go through `on_step`; `stop_connection_test(test)`
/// stops it.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn test_tunnel(
    draft: TunnelDraftItem,
    passwords: Vec<Option<String>>,
    proxy_password: Option<String>,
    test: u32,
    on_step: Channel<Vec<TestStepItem>>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<TunnelTestOutcome> {
    let draft = spelaion_config::TunnelDraft::from(draft);
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let resolved = {
        let config = state.config.read().await;
        match Resolver::new(&config, &trust, &SystemBackend).resolve_tunnel_draft(&draft) {
            Err(spelaion_config::ResolveError::Untrusted { .. }) => {
                return Ok(TunnelTestOutcome::Untrusted);
            }
            other => other,
        }
    };
    let stop = state.tests.start(test);
    let observer = |steps: &[spelaion_core::TestStep]| {
        let _ = on_step.send(steps.iter().map(TestStepItem::from).collect());
    };
    let report = spelaion_core::test_network_profile(
        resolved,
        tunnel_passwords(passwords, proxy_password),
        &KnownHosts::for_user(&state.user_dir),
        Arc::new(AppInteraction::new(app, state.pending.clone())),
        &observer,
        stop,
    )
    .await;
    state.tests.end(test);
    Ok(TunnelTestOutcome::Tested {
        report: report.into(),
    })
}

/// What `kubectl` is asked with, from the Kubernetes tab: empty fields mean the defaults.
fn kube_spec(k: KubernetesDraftItem) -> spelaion_config::ResolvedKubernetes {
    let text = |v: String| Some(v.trim().to_string()).filter(|v| !v.is_empty());
    spelaion_config::ResolvedKubernetes {
        context: text(k.context),
        namespace: text(k.namespace),
        resource: k.resource,
        kubeconfig: text(k.kubeconfig).map(PathBuf::from),
        kubectl: text(k.kubectl).unwrap_or_else(|| "kubectl".into()),
    }
}

async fn check_kube_editor(
    form: KubernetesDraftItem,
    personal: bool,
    editing: Option<String>,
    state: &AppState,
) -> CmdResult<spelaion_config::ResolvedKubernetes> {
    let id = editing.as_deref().map(parse_id);
    let config = state.config.read().await;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    spelaion_core::kube::editor_spec(
        &config,
        &trust,
        source_of(personal),
        id.as_ref(),
        kube_spec(form),
    )
    .map_err(|e| e.to_string())
}

/// The contexts of the kubeconfig, for the Kubernetes tab of the connection form (ADR-015).
#[tauri::command]
#[specta::specta]
async fn kube_contexts(
    form: KubernetesDraftItem,
    personal: bool,
    editing: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<String>> {
    let spec = check_kube_editor(form, personal, editing, &state).await?;
    spelaion_core::kube::contexts(&spec)
        .await
        .map_err(|e| e.to_string())
}

/// The namespaces of the chosen context.
#[tauri::command]
#[specta::specta]
async fn kube_namespaces(
    form: KubernetesDraftItem,
    personal: bool,
    editing: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<String>> {
    let spec = check_kube_editor(form, personal, editing, &state).await?;
    spelaion_core::kube::namespaces(&spec)
        .await
        .map_err(|e| e.to_string())
}

/// Names of one kind of resource (`svc`, `pod`, `deployment`) in the chosen namespace.
#[tauri::command]
#[specta::specta]
async fn kube_names(
    form: KubernetesDraftItem,
    kind: String,
    personal: bool,
    editing: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<String>> {
    let spec = check_kube_editor(form, personal, editing, &state).await?;
    spelaion_core::kube::names(&spec, &kind)
        .await
        .map_err(|e| e.to_string())
}

/// The ports of the chosen resource: a service's ports or its containers' ports.
#[tauri::command]
#[specta::specta]
async fn kube_ports(
    form: KubernetesDraftItem,
    personal: bool,
    editing: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<KubePortItem>> {
    let spec = check_kube_editor(form, personal, editing, &state).await?;
    spelaion_core::kube::ports(&spec)
        .await
        .map(|ports| ports.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

/// Saves a new tunnel (and its `keychain` passwords) and reloads the configuration.
#[tauri::command]
#[specta::specta]
async fn add_tunnel(
    draft: TunnelDraftItem,
    passwords: Vec<Option<String>>,
    proxy_password: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<TunnelItem> {
    let personal = draft.personal;
    let draft = spelaion_config::TunnelDraft::from(draft);
    let name = {
        let config = state.config.read().await;
        let file = draft.to_file(&config).map_err(|e| e.to_string())?;
        let name = spelaion_config::add_tunnel(
            &config,
            &draft,
            &tunnel_passwords(passwords, proxy_password),
            &SystemBackend,
        )
        .map_err(|e| e.to_string())?;
        if !personal {
            TrustStore::load(&state.user_dir)
                .and_then(|mut trust| trust.accept_tunnel_write(&config, &name, &file))
                .map_err(|e| e.to_string())?;
        }
        name
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
    proxy_password: Option<String>,
    revision: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let draft = spelaion_config::TunnelDraft::from(draft);
    {
        let config = state.config.read().await;
        let written = spelaion_config::update_tunnel_at_revision(
            &config,
            source_of(personal),
            &name,
            &draft,
            &tunnel_passwords(passwords, proxy_password),
            &SystemBackend,
            &revision,
        )
        .map_err(|e| e.to_string())?;
        if !personal {
            TrustStore::load(&state.user_dir)
                .and_then(|mut trust| trust.accept_tunnel_edit(&config, &name, &written))
                .map_err(|e| e.to_string())?;
        }
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
        spelaion_config::delete_tunnel(&config, source_of(personal), &name, &SystemBackend)
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
    editor: bool,
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
            Err(spelaion_config::ResolveError::Untrusted { .. }) => {
                return Ok(ConnectOutcome::Untrusted);
            }
            Err(spelaion_config::ResolveError::Changed { root, change }) => {
                return Ok(ConnectOutcome::changed(root, change));
            }
            Err(spelaion_config::ResolveError::MissingVariables { names }) => {
                return Ok(ConnectOutcome::MissingVariables { names });
            }
            Err(e) => return Err(e.to_string()),
        };
        (resolved, history_workspace(&config, id.source))
    };
    let save_to = match &resolved.password {
        spelaion_config::Password::Prompt { save_to } => save_to.clone(),
        spelaion_config::Password::None | spelaion_config::Password::Value(_) => None,
    };
    let password = password.map(Secret::new);
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app.clone(), state.pending.clone())),
        password: password.clone(),
        read_only_session: false,
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
    let setup_skipped = open
        .setup
        .skipped
        .iter()
        .cloned()
        .map(SkippedSetupItem::from)
        .collect();
    // Editor tabs follow their transaction under their type's safeguards (ADR-031); the
    // explorer's and table data tabs' sessions stay in auto-commit.
    if editor {
        open.session
            .follow_transactions(&open.resolved.safeguards())
            .await;
    }
    let mut sessions = state.sessions.lock().await;
    if workspace_version != state.workspace_version.load(Ordering::Acquire) {
        drop(sessions);
        open.close().await;
        return Err("the folder changed while connecting; select a connection again".into());
    }
    let open = Arc::new(open);
    watch_session(&app, session.clone(), &open);
    let old = sessions.insert(
        session,
        DesktopSession {
            open,
            password,
            history_workspace,
        },
    );
    drop(sessions);
    if let Some(old) = old.and_then(|o| Arc::try_unwrap(o.open).ok()) {
        old.close().await;
    }
    Ok(ConnectOutcome::Connected {
        server_version,
        setup_skipped,
    })
}

/// Open an independent tab from an existing explorer session, reusing its in-memory
/// credentials and exact resolved connection. Passwords never cross back into the UI.
#[tauri::command]
#[specta::specta]
async fn connect_from_session(
    source: String,
    session: String,
    database: Option<String>,
    editor: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    if source == session {
        return Err("the source and target sessions must differ".into());
    }
    let workspace_version = state.workspace_version.load(Ordering::Acquire);
    if !workspace_version.is_multiple_of(2) {
        return Err("the folder is changing; select a connection again".into());
    }
    let (source_open, password, history_workspace) = {
        let sessions = state.sessions.lock().await;
        let source = sessions
            .get(&source)
            .ok_or("the source connection is closed")?;
        (
            source.open.clone(),
            source.password.clone(),
            source.history_workspace.clone(),
        )
    };
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app.clone(), state.pending.clone())),
        password: password.clone(),
        read_only_session: false,
    };
    let open = open_connection(
        source_open.resolved.clone(),
        &state.engines,
        options,
        state.results.clone(),
    )
    .await
    .map_err(|e| e.to_string())?;
    if let Some(database) = database
        && let Err(error) = open.session.connection().select_database(&database).await
    {
        open.close().await;
        return Err(error.to_string());
    }
    let server_version = match open.session.connection().server_version().await {
        Ok(version) => version,
        Err(error) => {
            open.close().await;
            return Err(error.to_string());
        }
    };
    if editor {
        open.session
            .follow_transactions(&open.resolved.safeguards())
            .await;
    }
    let mut sessions = state.sessions.lock().await;
    if workspace_version != state.workspace_version.load(Ordering::Acquire)
        || !sessions
            .get(&source)
            .is_some_and(|s| Arc::ptr_eq(&s.open, &source_open))
        || sessions.contains_key(&session)
    {
        drop(sessions);
        open.close().await;
        return Err("the source connection or folder changed while connecting".into());
    }
    let open = Arc::new(open);
    watch_session(&app, session.clone(), &open);
    sessions.insert(
        session,
        DesktopSession {
            open,
            password,
            history_workspace,
        },
    );
    Ok(server_version)
}

/// Keeps a session alive if its connection asks for it, closes it when its type says it has
/// been idle too long (ADR-029), and tells the UI when it is lost or closed (ADR-028). Nothing
/// is reconnected or re-run here: the tab shows it and the user reconnects. A session
/// replaced in the meantime says nothing.
fn watch_session(app: &AppHandle, session: String, open: &Arc<OpenConnection>) {
    if let Some(interval) = open.resolved.keepalive {
        open.session.start_keep_alive(interval);
    }
    if let Some(seconds) = open.resolved.safeguards().close_idle_after {
        open.session
            .close_when_idle(std::time::Duration::from_secs(seconds.into()));
    }
    forward_transaction(app, session.clone(), open);
    let mut health = open.session.health();
    let watched = Arc::downgrade(open);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let (reason, idle) = match health
            .wait_for(|h| !matches!(h, SessionHealth::Alive))
            .await
        {
            Ok(health) => match &*health {
                SessionHealth::Lost { reason } => (reason.clone(), None),
                SessionHealth::IdleClosed { after } => (
                    format!(
                        "closed after {} s without use, as its connection type asks",
                        after.as_secs()
                    ),
                    Some(u32::try_from(after.as_secs()).unwrap_or(u32::MAX)),
                ),
                SessionHealth::Alive => return,
            },
            // The session was closed.
            Err(_) => return,
        };
        let Some(watched) = watched.upgrade() else {
            return;
        };
        // The last state read before the loss (ADR-031).
        let open_transaction = idle.is_none() && watched.session.transaction_status().is_open();
        let state = app.state::<AppState>();
        let mut sessions = state.sessions.lock().await;
        let current = sessions
            .get(&session)
            .is_some_and(|s| Arc::ptr_eq(&s.open, &watched));
        if !current {
            return;
        }
        // An idle session is closed for real, freeing it on the server; the next use
        // reconnects only when the user asks.
        let closed = if idle.is_some() {
            sessions.remove(&session)
        } else {
            None
        };
        drop(sessions);
        drop(watched);
        let _ = SessionLost {
            session,
            reason,
            idle_seconds: idle,
            open_transaction,
        }
        .emit(&app);
        if let Some(open) = closed.and_then(|s| Arc::try_unwrap(s.open).ok()) {
            open.close().await;
        }
    });
}

/// Tells the UI every change of an editor tab's transaction (ADR-031), while that session is
/// still the tab's.
fn forward_transaction(app: &AppHandle, session: String, open: &Arc<OpenConnection>) {
    if !open.session.transaction_status().supported {
        return;
    }
    let mut changes = open.session.transaction();
    let watched = Arc::downgrade(open);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        while changes.changed().await.is_ok() {
            let status = *changes.borrow_and_update();
            // Compared by address, without holding the session: `disconnect` must still be
            // able to close it.
            let still_the_tabs = app
                .state::<AppState>()
                .sessions
                .lock()
                .await
                .get(&session)
                .is_some_and(|s| std::ptr::eq(Arc::as_ptr(&s.open), watched.as_ptr()));
            if !still_the_tabs {
                return;
            }
            let _ = TransactionChanged {
                session: session.clone(),
                status: status.into(),
            }
            .emit(&app);
        }
    });
}

/// An editor tab's transaction, as the core last read it (ADR-031).
#[tauri::command]
#[specta::specta]
async fn transaction_status(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<TransactionStatusItem> {
    let open = state.session(&session).await?;
    Ok(open.session.transaction_status().into())
}

/// Turns an editor tab's auto-commit on or off; the core refuses to turn it on while a
/// transaction is open (ADR-031).
#[tauri::command]
#[specta::specta]
async fn set_auto_commit(
    session: String,
    on: bool,
    state: State<'_, AppState>,
) -> CmdResult<TransactionStatusItem> {
    let open = state.session(&session).await?;
    open.session
        .set_auto_commit(on)
        .await
        .map(Into::into)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn commit_transaction(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<TransactionOutcomeItem> {
    let open = state.session(&session).await?;
    open.session
        .commit()
        .await
        .map(Into::into)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn rollback_transaction(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<TransactionOutcomeItem> {
    let open = state.session(&session).await?;
    open.session
        .rollback()
        .await
        .map(Into::into)
        .map_err(|e| e.to_string())
}

/// Of these sessions, the ones whose closing must be confirmed: a transaction with work in it
/// is open (ADR-031). The UI asks before closing a tab, switching its connection or
/// disconnecting; the core decides who needs the question.
#[tauri::command]
#[specta::specta]
async fn sessions_to_confirm(
    sessions: Vec<String>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<String>> {
    let opened: Vec<(String, Arc<OpenConnection>)> = {
        let map = state.sessions.lock().await;
        sessions
            .into_iter()
            .filter_map(|key| map.get(&key).map(|s| (key, s.open.clone())))
            .collect()
    };
    let mut out = Vec::new();
    for (key, open) in opened {
        if open.session.close_needs_confirmation().await {
            out.push(key);
        }
    }
    Ok(out)
}

/// Quits after the user confirmed closing tabs with open transactions (`QuitBlocked`).
#[tauri::command]
#[specta::specta]
async fn quit_app(app: AppHandle, state: State<'_, AppState>) -> CmdResult<()> {
    state.quitting.store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(())
}

/// The editor tabs with an open transaction, from what each session last read (no round trip:
/// the window's close request cannot wait for servers).
fn open_transactions(sessions: &HashMap<String, DesktopSession>) -> Vec<String> {
    let mut open: Vec<String> = sessions
        .iter()
        .filter(|(_, s)| s.open.session.transaction_status().is_open())
        .map(|(key, _)| key.clone())
        .collect();
    open.sort();
    open
}

/// The window was asked to close (ADR-031): with transactions open, the UI asks first and calls
/// `quit_app`. Nothing is committed or rolled back here; the server rolls back.
fn on_close_requested(window: &tauri::Window, api: &tauri::CloseRequestApi) {
    let state = window.state::<AppState>();
    if state.quitting.load(Ordering::SeqCst) {
        return;
    }
    // The event loop must not block: when a command holds the sessions, decide in a task.
    let Ok(sessions) = state.sessions.try_lock() else {
        api.prevent_close();
        let app = window.app_handle().clone();
        tauri::async_runtime::spawn(async move {
            let state = app.state::<AppState>();
            let open = open_transactions(&*state.sessions.lock().await);
            if open.is_empty() {
                state.quitting.store(true, Ordering::SeqCst);
                app.exit(0);
            } else {
                let _ = QuitBlocked { sessions: open }.emit(&app);
            }
        });
        return;
    };
    let open = open_transactions(&sessions);
    drop(sessions);
    if !open.is_empty() {
        api.prevent_close();
        let _ = QuitBlocked { sessions: open }.emit(window.app_handle());
    }
}

#[tauri::command]
#[specta::specta]
async fn disconnect(session: String, state: State<'_, AppState>) -> CmdResult<()> {
    let removed = state.sessions.lock().await.remove(&session);
    // A run waiting for an answer after an error stops: its tab is gone (ADR-035). A run
    // that is executing goes on to its end, as before.
    if let Some(open) = &removed {
        open.open.session.stop_paused_run();
    }
    if let Some(open) = removed.and_then(|o| Arc::try_unwrap(o.open).ok()) {
        open.close().await;
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
async fn list_processes(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<ProcessSnapshotItem> {
    let open = state.session(&session).await?;
    let snapshot = spelaion_core::list_processes(&open.session).await?;
    Ok(ProcessSnapshotItem {
        own_id: snapshot.own_id,
        limited: snapshot.limited,
        processes: snapshot
            .processes
            .into_iter()
            .map(|p| ProcessItem {
                id: p.id,
                user: p.user,
                host: p.host,
                database: p.database,
                command: p.command,
                seconds: p.seconds,
                state: p.state,
                sql: p.sql,
                identity: p.identity,
                activity: p.activity,
            })
            .collect(),
    })
}

#[tauri::command]
#[specta::specta]
async fn control_process(
    session: String,
    target: ProcessTargetItem,
    terminate: bool,
    confirmed: bool,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let open = state.session(&session).await?;
    let target = spelaion_core::ProcessTarget {
        id: target.id,
        user: target.user,
        host: target.host,
        command: target.command,
        sql: target.sql,
        identity: target.identity,
    };
    spelaion_core::control_process(
        &open.session,
        &target,
        terminate,
        open.resolved.params.read_only,
        confirmed,
    )
    .await
}

/// Runs a script and keeps sending events through the channel. Returns as soon as it starts.
#[tauri::command]
#[specta::specta]
async fn run_script(
    session: String,
    sql: String,
    confirmed: bool,
    options: RunOptionsItem,
    on_event: Channel<ScriptEvent>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    start_script(&state, &session, sql, confirmed, options, on_event, None).await
}

/// Answers a run paused after an error (ADR-035). The core refuses a ticket that is not the
/// pause it is waiting on.
#[tauri::command]
#[specta::specta]
async fn answer_script_error(
    session: String,
    ticket: u32,
    answer: ErrorAnswerItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let open = state.session(&session).await?;
    open.session
        .answer_error(spelaion_core::ErrorTicket(ticket.into()), answer.into())
        .map_err(|e| e.to_string())
}

async fn start_script(
    state: &AppState,
    session: &str,
    sql: String,
    confirmed: bool,
    options: RunOptionsItem,
    on_event: Channel<ScriptEvent>,
    original: Option<String>,
) -> CmdResult<()> {
    let (open, workspace) = {
        let sessions = state.sessions.lock().await;
        let session = sessions
            .get(session)
            .ok_or_else(|| format!("there is no open connection for {session}"))?;
        (session.open.clone(), session.history_workspace.clone())
    };
    // The core decides what needs confirmation; the UI asks first (`script_risks`), and a
    // run that skipped the question is refused by the core, whatever its error mode.
    spelaion_core::check_script(
        open.session.dialect().as_ref(),
        &sql,
        &open.resolved.safeguards(),
        confirmed,
    )
    .map_err(|e| e.to_string())?;
    let mut events = state.history.run_script_in_context(
        &open.session,
        &sql,
        spelaion_core::HistoryScope {
            workspace,
            connection: open.resolved.id.to_string(),
            database: (!open.resolved.params.database.is_empty())
                .then(|| open.resolved.params.database.clone()),
            switchable_database: matches!(
                open.resolved.engine,
                spelaion_config::Engine::Mysql
                    | spelaion_config::Engine::Mariadb
                    | spelaion_config::Engine::Sqlserver
                    | spelaion_config::Engine::Duckdb
            ),
        },
        original,
        spelaion_core::ScriptOptions {
            on_error: options.on_error.into(),
            // The run's own timeout wins over the connection's; 0 is none.
            statement_timeout: match options.timeout_seconds {
                Some(0) => None,
                Some(seconds) => Some(std::time::Duration::from_secs(seconds.into())),
                None => open.resolved.statement_timeout,
            },
        },
    );
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

/// Whether a SELECT matches the conservative rules for fetching pages.
#[tauri::command]
#[specta::specta]
async fn query_page_supported(
    session: String,
    sql: String,
    state: State<'_, AppState>,
) -> CmdResult<bool> {
    let open = state.session(&session).await?;
    Ok(spelaion_core::query_page_sql(open.session.dialect().as_ref(), &sql, 1, 0).is_some())
}

/// Run the first page, keeping the original SQL in the user's query history.
#[tauri::command]
#[specta::specta]
async fn run_query_page(
    session: String,
    sql: String,
    limit: u32,
    options: RunOptionsItem,
    on_event: Channel<ScriptEvent>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let open = state.session(&session).await?;
    let paged =
        spelaion_core::query_page_sql(open.session.dialect().as_ref(), &sql, limit.into(), 0)
            .ok_or("this query cannot be paged")?;
    start_script(&state, &session, paged, false, options, on_event, Some(sql)).await
}

#[tauri::command]
#[specta::specta]
async fn append_query_page(
    session: String,
    result: u32,
    sql: String,
    limit: u32,
    offset: u32,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let open = state.session(&session).await?;
    let added = spelaion_core::append_query_page(
        &open.session,
        ResultId(result.into()),
        &sql,
        limit.into(),
        offset.into(),
    )
    .await?;
    Ok(u32::try_from(added).unwrap_or(u32::MAX))
}

#[tauri::command]
#[specta::specta]
async fn count_query_rows(
    session: String,
    sql: String,
    state: State<'_, AppState>,
) -> CmdResult<f64> {
    let open = state.session(&session).await?;
    Ok(spelaion_core::count_query_rows(&open.session, &sql).await? as f64)
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
    Ok(spelaion_core::script_params(
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
    spelaion_core::bind_params(open.session.dialect().as_ref(), &sql, &values)
        .map_err(|e| e.to_string())
}

fn risks_of(open: &OpenConnection, sql: &str) -> Vec<spelaion_core::RiskyStatement> {
    spelaion_core::script_risks(
        open.session.dialect().as_ref(),
        sql,
        &open.resolved.safeguards(),
    )
}

/// Statements of a script that need confirmation before `run_script`: a DELETE or UPDATE
/// without WHERE, DROP, TRUNCATE, and whatever the connection's type asks for (ADR-029).
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
        TextFormat::Html => ExportFormat::Html,
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
        format: match format {
            TextFormat::SqlInsert => ExportFormat::SqlInsertBatch {
                table: INSERT_TABLE.into(),
            },
            other => export_format(other),
        },
        rows: range.first_row as usize..range.first_row as usize + range.rows as usize,
        columns: range.columns.iter().map(|&c| c as usize).collect(),
        header,
        csv: spelaion_core::CsvOptions::default(),
    };
    let mut out = Vec::new();
    write_result(&stored, &options, dialect.as_ref(), &mut out).map_err(|e| e.to_string())?;
    String::from_utf8(out).map_err(|e| e.to_string())
}

/// Writes a loaded result to a file the user chose (in a blocking task: it may be millions of
/// rows): all of it, or the rows of `range` (a selection). `options.columns` (or the range's)
/// are the shown columns, in order.
#[tauri::command]
#[specta::specta]
async fn export_result(
    result: u32,
    session: String,
    path: String,
    options: ExportFileOptions,
    range: Option<CellRange>,
    state: State<'_, AppState>,
) -> CmdResult<ExportSummaryItem> {
    let dialect = dialect_for(&state, &session).await;
    let csv = options.csv()?;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let summary = tokio::task::spawn_blocking(move || {
        let stored = stored
            .read()
            .map_err(|_| "result lock poisoned".to_string())?;
        if !stored.is_complete() {
            return Err("the result is still loading: export it when every row has arrived".into());
        }
        let all = (0..stored.schema().columns.len()).collect::<Vec<_>>();
        let (rows, columns) = match range {
            Some(range) => (
                range.first_row as usize..range.first_row as usize + range.rows as usize,
                range.columns.iter().map(|&c| c as usize).collect(),
            ),
            None => (
                0..stored.rows(),
                options
                    .columns
                    .as_ref()
                    .map(|c| c.iter().map(|&c| c as usize).collect())
                    .unwrap_or(all),
            ),
        };
        let export = ExportOptions {
            format: export_format(options.format),
            rows,
            columns,
            header: options.header,
            csv,
        };
        spelaion_core::export_to_file(
            &stored,
            &export,
            options.encoding.into(),
            dialect.as_ref(),
            Path::new(&path),
        )
        .map_err(|e| format!("could not write {path}: {e}"))
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(summary.into())
}

/// Before offering to run a query (or a table data tab) again for an export: whether it may
/// be (only a single read), and whether the tab has an open transaction whose uncommitted
/// changes the export will not see (ADR-034).
#[tauri::command]
#[specta::specta]
async fn export_rerun_check(
    session: String,
    sql: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<RerunCheckItem> {
    let open = state.session(&session).await?;
    let check = spelaion_core::rerun_check(&open.session, sql.as_deref());
    Ok(RerunCheckItem {
        refused: check.refused,
        open_transaction: check.open_transaction,
    })
}

/// A streaming export's file options, for the core.
fn stream_export(options: &ExportFileOptions, table: &str) -> CmdResult<StreamExport> {
    Ok(StreamExport {
        format: match options.format {
            TextFormat::SqlInsert => ExportFormat::SqlInsert {
                table: table.into(),
            },
            other => export_format(other),
        },
        header: options.header,
        csv: options.csv()?,
        encoding: options.encoding.into(),
        columns: options
            .columns
            .as_ref()
            .map(|c| c.iter().map(|&c| c as usize).collect()),
        batch_size: spelaion_core::DEFAULT_BATCH_ROWS,
    })
}

/// A new session for an export (ADR-034), opened like `connect_from_session` from the
/// `source` session's resolved connection and in-memory password, and registered under
/// `export_id` so `cancel_export` can stop it. Never the tab's own session.
async fn open_export_session(
    state: &AppState,
    app: &AppHandle,
    source: &str,
    export_id: u32,
) -> CmdResult<(Arc<OpenConnection>, Arc<OpenConnection>, Arc<ExportCancel>)> {
    let (source_open, password) = {
        let sessions = state.sessions.lock().await;
        let source = sessions
            .get(source)
            .ok_or("the connection of this export is closed")?;
        (source.open.clone(), source.password.clone())
    };
    let cancel = Arc::new(ExportCancel::new());
    {
        let mut exports = state.exports.lock().expect("lock poisoned");
        if exports.contains_key(&export_id) {
            return Err("an export with this id is already running".into());
        }
        exports.insert(export_id, cancel.clone());
    }
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app.clone(), state.pending.clone())),
        password,
        // Only a single read is ever run again there (ADR-034).
        read_only_session: true,
    };
    match open_connection(
        source_open.resolved.clone(),
        &state.engines,
        options,
        state.results.clone(),
    )
    .await
    {
        Ok(open) => {
            cancel.attach(&open.session);
            Ok((source_open, Arc::new(open), cancel))
        }
        Err(e) => {
            state
                .exports
                .lock()
                .expect("lock poisoned")
                .remove(&export_id);
            Err(e.to_string())
        }
    }
}

/// Closes an export's session and forgets its cancel.
async fn close_export_session(state: &AppState, export_id: u32, open: Arc<OpenConnection>) {
    state
        .exports
        .lock()
        .expect("lock poisoned")
        .remove(&export_id);
    if let Ok(open) = Arc::try_unwrap(open) {
        open.close().await;
    }
}

fn progress_sender(
    channel: Channel<ExportProgressItem>,
    table: Option<u32>,
) -> impl Fn(spelaion_core::ExportProgress) + Send + Sync {
    move |p| {
        let _ = channel.send(ExportProgressItem {
            table,
            rows: p.rows as f64,
            bytes: p.bytes as f64,
        });
    }
}

/// Runs a query again in a new session and streams every row to `path` (ADR-034). Only a single
/// read is run; a connection type that confirms execution needs `confirmed`.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn export_query(
    source: String,
    export_id: u32,
    sql: String,
    path: String,
    options: ExportFileOptions,
    confirmed: bool,
    on_progress: Channel<ExportProgressItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ExportOutcome> {
    let settings = stream_export(&options, INSERT_TABLE)?;
    // Refused before connecting: nothing that is not a read reaches a new session either.
    {
        let tab = state.session(&source).await?;
        let check = spelaion_core::rerun_check(&tab.session, Some(&sql));
        if let Some(message) = check.refused {
            return Ok(ExportOutcome::NotARead { message });
        }
    }
    let (tab, open, cancel) = open_export_session(&state, &app, &source, export_id).await?;
    // The tab is only asked where it finds unqualified names; it is not held during the export.
    let context = spelaion_core::follow_session_context(&tab.session, &open.session).await;
    let safeguards = tab.resolved.safeguards();
    drop(tab);
    let outcome = match context {
        Err(message) => Err(spelaion_core::ExportError::Invalid(message)),
        Ok(()) => {
            spelaion_core::export_query(
                &open.session,
                &sql,
                &safeguards,
                confirmed,
                Path::new(&path),
                &settings,
                &cancel,
                &progress_sender(on_progress, None),
            )
            .await
        }
    };
    close_export_session(&state, export_id, open).await;
    Ok(outcome.into())
}

/// Streams every row of a table data tab's view (its filter and order, not just the loaded
/// page) to `path`, through the core's own SELECT.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn export_table_data(
    source: String,
    export_id: u32,
    view: TableExportItem,
    path: String,
    options: ExportFileOptions,
    on_progress: Channel<ExportProgressItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ExportOutcome> {
    let settings = stream_export(&options, &view.name)?;
    let (tab, open, cancel) = open_export_session(&state, &app, &source, export_id).await?;
    drop(tab);
    let table = spelaion_core::TableSource {
        schema: view.schema,
        name: view.name,
        filter: view.filter,
        order_by: view.order_by,
    };
    let outcome = spelaion_core::export_table(
        &open.session,
        &table,
        Path::new(&path),
        &settings,
        &cancel,
        &progress_sender(on_progress, None),
    )
    .await;
    close_export_session(&state, export_id, open).await;
    Ok(outcome.into())
}

/// Streams explorer tables and views, one file each, into `folder`, never over an existing
/// file. Stops at a cancel or an error; the files written before stay.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn export_tables(
    source: String,
    export_id: u32,
    tables: Vec<RelationRefItem>,
    folder: String,
    extension: String,
    options: ExportFileOptions,
    on_progress: Channel<ExportProgressItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ExportTablesOutcome> {
    if extension.is_empty() || !extension.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("the file extension must be letters and digits".into());
    }
    let settings = stream_export(&options, "")?;
    let (tab, open, cancel) = open_export_session(&state, &app, &source, export_id).await?;
    drop(tab);
    let list: Vec<(String, String)> = tables.into_iter().map(|t| (t.schema, t.name)).collect();
    let progress = move |table: usize, p: spelaion_core::ExportProgress| {
        let _ = on_progress.send(ExportProgressItem {
            table: Some(u32::try_from(table).unwrap_or(u32::MAX)),
            rows: p.rows as f64,
            bytes: p.bytes as f64,
        });
    };
    let result = spelaion_core::export_tables(
        &open.session,
        &list,
        Path::new(&folder),
        &extension,
        &settings,
        &cancel,
        &progress,
    )
    .await;
    close_export_session(&state, export_id, open).await;
    let file = |f: spelaion_core::ExportedFile| ExportedFileItem {
        schema: f.schema,
        name: f.name,
        path: f.path.display().to_string(),
        summary: f.summary.into(),
    };
    Ok(match result {
        Ok(files) => ExportTablesOutcome {
            files: files.into_iter().map(file).collect(),
            cancelled: false,
            failed_table: None,
            error: None,
        },
        Err(stopped) => {
            let cancelled = matches!(*stopped.error, spelaion_core::ExportError::Cancelled);
            ExportTablesOutcome {
                files: stopped.done.into_iter().map(file).collect(),
                cancelled,
                failed_table: (!cancelled)
                    .then(|| u32::try_from(stopped.table).unwrap_or(u32::MAX)),
                error: (!cancelled).then(|| stopped.error.to_string()),
            }
        }
    })
}

/// Stops a running streaming export: its statement and its writer; no file is left.
#[tauri::command]
#[specta::specta]
async fn cancel_export(export_id: u32, state: State<'_, AppState>) -> CmdResult<()> {
    let cancel = state
        .exports
        .lock()
        .expect("lock poisoned")
        .get(&export_id)
        .cloned();
    if let Some(cancel) = cancel {
        cancel.cancel().await;
    }
    Ok(())
}

/// Reads a connection URL (`postgres://…`, `mysql://…`, `jdbc:…`, `sqlite:…`) for the form.
#[tauri::command]
#[specta::specta]
fn parse_connection_url(url: String) -> CmdResult<UrlFields> {
    spelaion_config::parse_connection_url(&url)
        .map(UrlFields::from)
        .map_err(|e| e.to_string())
}

/// Creates an empty database file (SQLite or DuckDB) for a new connection. The core never
/// overwrites an existing file.
#[tauri::command]
#[specta::specta]
async fn create_database(engine: String, path: String) -> CmdResult<()> {
    let engine = engine_named(&engine)?;
    tokio::task::spawn_blocking(move || {
        spelaion_core::create_file_database(engine, Path::new(&path)).map_err(|e| e.to_string())
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
        spelaion_core::statement_at(driver.dialect().as_ref(), &sql, offset as usize).map(|span| {
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
    let filters: Vec<spelaion_core::ColumnFilter> = filters.into_iter().map(Into::into).collect();
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

/// Searches a result's loaded rows in the core (Ctrl+F) and keeps the matches for
/// `find_matches` and `find_step`. A newer search on the same result stops this one (`None`).
#[tauri::command]
#[specta::specta]
async fn find_in_result(
    result: u32,
    text: String,
    match_case: bool,
    columns: Vec<u32>,
    state: State<'_, AppState>,
) -> CmdResult<Option<FindOutcome>> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let query = FindQuery {
        text,
        match_case,
        columns: columns.into_iter().map(|c| c as usize).collect(),
    };
    tokio::task::spawn_blocking(move || {
        let (ticket, found) = {
            let read = stored
                .read()
                .map_err(|_| "result lock poisoned".to_string())?;
            let ticket = read.find_ticket();
            let found = read
                .find_matches(&query, &ticket)
                .map_err(|e| e.to_string())?;
            (ticket, found)
        };
        let Some(found) = found else {
            return Ok(None);
        };
        let outcome = FindOutcome {
            count: u32::try_from(found.len()).unwrap_or(u32::MAX),
            truncated: found.is_truncated(),
        };
        let kept = stored
            .write()
            .map_err(|_| "result lock poisoned".to_string())?
            .keep_find(found, &ticket);
        Ok(kept.then_some(outcome))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The matches of the last search in rows `[first_row, first_row + rows)`, to highlight
/// what the grid shows.
#[tauri::command]
#[specta::specta]
async fn find_matches(
    result: u32,
    first_row: u32,
    rows: u32,
    state: State<'_, AppState>,
) -> CmdResult<Vec<FindMatchItem>> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    Ok(stored.find().map_or_else(Vec::new, |found| {
        found
            .window(first_row as usize, rows as usize)
            .into_iter()
            .map(|m| FindMatchItem {
                row: n(m.row),
                column: n(m.column),
            })
            .collect()
    }))
}

/// The next match after the cell (`row`, `column`), or the previous one with `backwards`,
/// going round; `None` without matches (or once the rows moved).
#[tauri::command]
#[specta::specta]
async fn find_step(
    result: u32,
    row: u32,
    column: u32,
    backwards: bool,
    state: State<'_, AppState>,
) -> CmdResult<Option<FindStepItem>> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    Ok(stored.find().and_then(|found| {
        let index = found.step(row as usize, column as usize, backwards)?;
        let m = found.get(index)?;
        Some(FindStepItem {
            index: n(index),
            row: n(m.row),
            column: n(m.column),
        })
    }))
}

/// Forgets a result's search (the find bar closed).
#[tauri::command]
#[specta::specta]
async fn clear_find(result: u32, state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(stored) = state.results.get(ResultId(result.into())) {
        stored
            .write()
            .map_err(|_| "result lock poisoned")?
            .clear_find();
    }
    Ok(())
}

/// The numbers about a selection of a result (the calc panel): the UI sends the selection's
/// bounds (rows of the current view and the columns it shows), never cell values, and the
/// core computes over the stored result. A newer calculation on the same result, or
/// `cancel_selection_stats`, stops this one (`None`).
#[tauri::command]
#[specta::specta]
async fn selection_stats(
    result: u32,
    range: CellRange,
    state: State<'_, AppState>,
) -> CmdResult<Option<SelectionStatsItem>> {
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let block = spelaion_core::CellBlock {
        first_row: range.first_row as usize,
        rows: range.rows as usize,
        columns: range.columns.into_iter().map(|c| c as usize).collect(),
    };
    tokio::task::spawn_blocking(move || {
        let read = stored
            .read()
            .map_err(|_| "result lock poisoned".to_string())?;
        let ticket = read.stats_ticket();
        read.selection_stats(&block, &ticket)
            .map(|stats| stats.map(SelectionStatsItem::from))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stops the selection calculation running on a result (the calc panel's Cancel).
#[tauri::command]
#[specta::specta]
async fn cancel_selection_stats(result: u32, state: State<'_, AppState>) -> CmdResult<()> {
    if let Some(stored) = state.results.get(ResultId(result.into())) {
        stored
            .read()
            .map_err(|_| "result lock poisoned")?
            .cancel_stats();
    }
    Ok(())
}

/// A table data tab column's statistics (the column header's "Column statistics"): counted by
/// the server in a new session of the tab's connection, every statement a read inside the
/// engine's read-only transaction, sampled past `STATS_SAMPLE_ROWS` rows. The column's name and
/// type come from the tab's page (`result`, `column`). `cancel_column_stats` with the same
/// `stats_id` stops it.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn column_stats(
    source: String,
    stats_id: u32,
    table: ColumnStatsTarget,
    result: u32,
    column: u32,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ColumnStatsOutcome> {
    let (name, logical) = {
        let stored = state
            .results
            .get(ResultId(result.into()))
            .ok_or("result freed")?;
        let stored = stored.read().map_err(|_| "result lock poisoned")?;
        let meta = stored
            .schema()
            .columns
            .get(column as usize)
            .ok_or("no such column")?;
        (meta.name.clone(), meta.logical)
    };
    let request = spelaion_core::ColumnStatsRequest {
        schema: table.schema,
        table: table.name,
        column: name,
        logical,
        filter: table.filter,
        sample_rows: spelaion_core::STATS_SAMPLE_ROWS,
        top: spelaion_core::TOP_VALUES,
    };
    // Checked before connecting: a filter that would change the statements never reaches a
    // new session.
    {
        let tab = state.session(&source).await?;
        spelaion_core::column_stats_sql(tab.session.dialect().as_ref(), &request, false)
            .map_err(|e| e.to_string())?;
    }
    let (tab, open, cancel) = open_export_session(&state, &app, &source, stats_id).await?;
    // The tab's database (MySQL, SQL Server) or schema, as for exports.
    let context = spelaion_core::follow_session_context(&tab.session, &open.session).await;
    drop(tab);
    let outcome = match context {
        Err(message) => Err(message),
        Ok(()) => match spelaion_core::column_stats(&open.session, &request, &cancel).await {
            Ok(stats) => Ok(ColumnStatsOutcome::Done {
                stats: Box::new(ColumnStatsItem::new(stats, request.sample_rows)),
            }),
            Err(spelaion_core::ColumnStatsError::Cancelled) => Ok(ColumnStatsOutcome::Cancelled),
            Err(e) => Err(e.to_string()),
        },
    };
    close_export_session(&state, stats_id, open).await;
    outcome
}

/// Stops a column's statistics while they are counted: the running statement and the rest.
#[tauri::command]
#[specta::specta]
async fn cancel_column_stats(stats_id: u32, state: State<'_, AppState>) -> CmdResult<()> {
    let cancel = state
        .exports
        .lock()
        .expect("lock poisoned")
        .get(&stats_id)
        .cloned();
    if let Some(cancel) = cancel {
        cancel.cancel().await;
    }
    Ok(())
}

/// A table data tab's filter with a condition the core wrote (one of the column statistics'
/// most frequent values) added with AND. The page's SELECT is checked when it loads.
#[tauri::command]
#[specta::specta]
async fn add_table_condition(
    session: String,
    filter: String,
    condition: String,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let dialect = dialect_for(&state, &session).await;
    spelaion_core::add_condition(dialect.as_ref(), &filter, &condition).map_err(|e| e.to_string())
}

/// "Filter by this value" in a table data tab: the core writes the condition from the typed
/// value (the cell at `row` of the page, or `value` typed for the column) and returns the
/// tab's filter with it added. The page's SELECT is checked when it loads.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn table_filter_condition(
    session: String,
    result: u32,
    column: u32,
    op: FilterOpItem,
    row: Option<u32>,
    value: String,
    filter: String,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let dialect = dialect_for(&state, &session).await;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let (name, logical, typed) = {
        let stored = stored.read().map_err(|_| "result lock poisoned")?;
        let meta = stored
            .schema()
            .columns
            .get(column as usize)
            .ok_or("no such column")?;
        let typed = match row {
            Some(row) => stored
                .value_at(row as usize, column as usize)
                .ok_or("no such row")?,
            None => spelaion_core::typed_value(&value, meta.logical),
        };
        (meta.name.clone(), meta.logical, typed)
    };
    let condition =
        spelaion_core::value_condition(dialect.as_ref(), &name, logical, op.into(), &typed)
            .map_err(|e| e.to_string())?;
    spelaion_core::add_condition(dialect.as_ref(), &filter, &condition).map_err(|e| e.to_string())
}

/// Column names for the clipboard: plain, quoted for the engine, qualified with the table
/// (a table data tab's) or one per line; written by the core.
#[tauri::command]
#[specta::specta]
async fn copy_column_names(
    result: u32,
    session: String,
    columns: Vec<u32>,
    style: NameStyleItem,
    table: Option<TableRefItem>,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let dialect = dialect_for(&state, &session).await;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let columns: Vec<usize> = columns.into_iter().map(|c| c as usize).collect();
    spelaion_core::column_names(
        &stored,
        &columns,
        style.into(),
        dialect.as_ref(),
        table.as_ref().map(|t| (t.schema.as_str(), t.name.as_str())),
    )
    .map_err(|e| e.to_string())
}

/// Every different value of a column (all its rows, in the current view) as an SQL `IN`
/// list with the engine's literals; refused beyond the core's limit rather than cut.
#[tauri::command]
#[specta::specta]
async fn copy_column_in_list(
    result: u32,
    session: String,
    column: u32,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let dialect = dialect_for(&state, &session).await;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    tokio::task::spawn_blocking(move || {
        let stored = stored
            .read()
            .map_err(|_| "result lock poisoned".to_string())?;
        spelaion_core::column_in_list(&stored, column as usize, dialect.as_ref())
            .map_err(|e| e.to_string())
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
async fn current_database(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<Option<String>> {
    let open = state.session(&session).await?;
    open.session
        .connection()
        .current_database()
        .await
        .map_err(|e| e.to_string())
}

/// The databases this tab's session can switch to (MySQL, MariaDB, SQL Server).
#[tauri::command]
#[specta::specta]
async fn list_databases(session: String, state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    let open = state.session(&session).await?;
    open.session
        .connection()
        .list_databases()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn select_database(
    session: String,
    database: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let open = state.session(&session).await?;
    open.session
        .connection()
        .select_database(&database)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
async fn list_schemas(
    session: String,
    filter: Option<NameFilterItem>,
    server_side: bool,
    state: State<'_, AppState>,
) -> CmdResult<Vec<SchemaItem>> {
    let open = state.session(&session).await?;
    let connection = open.session.connection();
    // The explorer's filter, narrowed in the catalog query or only here (ADR-025).
    let schemas = match filter.map(NameFilter::from) {
        Some(filter) if server_side => connection.list_schemas_matching(&filter).await,
        Some(filter) => connection.list_schemas().await.map(|mut all| {
            all.retain(|s| filter.matches(&s.name));
            all
        }),
        None => connection.list_schemas().await,
    }
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
    filter: Option<NameFilterItem>,
    server_side: bool,
    state: State<'_, AppState>,
) -> CmdResult<Vec<ObjectItem>> {
    let open = state.session(&session).await?;
    let connection = open.session.connection();
    let objects = match filter.map(NameFilter::from) {
        Some(filter) if server_side => connection.list_objects_matching(&schema, &filter).await,
        Some(filter) => connection.list_objects(&schema).await.map(|mut all| {
            all.retain(|o| filter.matches(&o.name));
            all
        }),
        None => connection.list_objects(&schema).await,
    }
    .map_err(|e| e.to_string())?;
    Ok(objects.into_iter().map(ObjectItem::from).collect())
}

#[tauri::command]
#[specta::specta]
async fn describe_table(
    session: String,
    schema: String,
    name: String,
    columns: Option<NameFilterItem>,
    state: State<'_, AppState>,
) -> CmdResult<TableDetails> {
    let open = state.session(&session).await?;
    let mut table = open
        .session
        .connection()
        .describe_table(&schema, &name)
        .await
        .map_err(|e| e.to_string())?;
    // The explorer's column filter (ADR-025); editing asks without one, for every column.
    if let Some(columns) = columns {
        let columns = spelaion_driver_api::NameFilter::from(columns);
        table.columns.retain(|c| columns.matches(&c.name));
    }
    Ok(TableDetails::from(table))
}

/// Functions, sequences, triggers, types… of a schema, for its folders (ADR-024).
#[tauri::command]
#[specta::specta]
async fn list_schema_objects(
    session: String,
    schema: String,
    filter: Option<NameFilterItem>,
    server_side: bool,
    state: State<'_, AppState>,
) -> CmdResult<Vec<SchemaObjectItem>> {
    let open = state.session(&session).await?;
    let connection = open.session.connection();
    let objects = match filter.map(NameFilter::from) {
        Some(filter) if server_side => {
            connection
                .list_schema_objects_matching(&schema, &filter)
                .await
        }
        Some(filter) => connection
            .list_schema_objects(&schema)
            .await
            .map(|mut all| {
                all.retain(|o| filter.matches(&o.name));
                all
            }),
        None => connection.list_schema_objects(&schema).await,
    }
    .map_err(|e| e.to_string())?;
    Ok(objects.into_iter().map(SchemaObjectItem::from).collect())
}

/// Roles, logins, tablespaces and extensions, for the connection's folders (ADR-024).
#[tauri::command]
#[specta::specta]
async fn list_server_objects(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<ServerObjectItem>> {
    let open = state.session(&session).await?;
    let objects = open
        .session
        .connection()
        .list_server_objects()
        .await
        .map_err(|e| e.to_string())?;
    Ok(objects.into_iter().map(ServerObjectItem::from).collect())
}

/// The DDL of a table or view (`CREATE TABLE` with its constraints, indexes and comments),
/// to open in a new tab (nothing runs).
#[tauri::command]
#[specta::specta]
async fn relation_definition(
    session: String,
    schema: String,
    name: String,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    open.session
        .connection()
        .relation_definition(&schema, &name)
        .await
        .map_err(|e| e.to_string())
}

/// The SQL that calls a procedure (inputs as `:name`, outputs collected), to open in a new tab.
#[tauri::command]
#[specta::specta]
async fn procedure_call(
    session: String,
    object: SchemaObjectItem,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    open.session
        .connection()
        .procedure_call(&object.into())
        .await
        .map_err(|e| e.to_string())
}

/// The DDL of an object, to open in a new tab (nothing runs).
#[tauri::command]
#[specta::specta]
async fn object_definition(
    session: String,
    object: SchemaObjectItem,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    open.session
        .connection()
        .object_definition(&object.into())
        .await
        .map_err(|e| e.to_string())
}

/// The table a query's result can be edited as, or none: the core decides (ADR-014).
#[tauri::command]
#[specta::specta]
async fn editable_query_table(
    session: String,
    result: u32,
    sql: String,
    state: State<'_, AppState>,
) -> CmdResult<Option<EditableTableItem>> {
    let open = state.session(&session).await?;
    let table = spelaion_core::editable_query_table(
        &open.session,
        ResultId(result.into()),
        &sql,
        open.resolved.params.read_only,
    )
    .await?;
    Ok(table.map(|table| EditableTableItem {
        schema: table.object.schema,
        name: table.object.name,
        primary_key: table.primary_key,
    }))
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
    Ok(spelaion_core::table_sql(
        open.session.dialect().as_ref(),
        &table,
        template.into(),
    ))
}

/// A SELECT joining the tables chosen in the explorer on the foreign keys between them
/// (`INNER JOIN … ON`, or the simple `FROM a, b WHERE`), to open in a new tab.
#[tauri::command]
#[specta::specta]
async fn join_sql(
    session: String,
    tables: Vec<TableRefItem>,
    inner: bool,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    let mut described = Vec::new();
    for table in &tables {
        described.push(
            open.session
                .connection()
                .describe_table(&table.schema, &table.name)
                .await
                .map_err(|e| e.to_string())?,
        );
    }
    if described.len() < 2 {
        return Err("choose two tables or more to join".into());
    }
    Ok(spelaion_core::join_sql(
        open.session.dialect().as_ref(),
        &described,
        inner,
    ))
}

/// The SQL a table's menu can generate on this connection's engine (the core decides: an
/// upsert or MERGE only where the engine has them).
#[tauri::command]
#[specta::specta]
async fn table_templates(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<SqlTemplateItem>> {
    let open = state.session(&session).await?;
    Ok(
        spelaion_core::table_templates(open.session.dialect().as_ref())
            .into_iter()
            .map(SqlTemplateItem::from)
            .collect(),
    )
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
    Ok(spelaion_core::count_rows(&open.session, &schema, &name).await? as f64)
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
    spelaion_core::table_page_sql(open.session.dialect().as_ref(), &page.into())
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
    Ok(spelaion_core::count_table_rows(&open.session, &schema, &name, &filter).await? as f64)
}

/// The foreign keys a table data tab can follow: the table's own, then those of other tables
/// that point to it, from the catalog.
#[tauri::command]
#[specta::specta]
async fn table_keys(
    session: String,
    schema: String,
    table: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<TableKeyItem>> {
    let open = state.session(&session).await?;
    let keys = spelaion_core::table_keys(open.session.connection().as_ref(), &schema, &table)
        .await
        .map_err(|e| e.to_string())?;
    Ok(keys.into_iter().map(TableKeyItem::from).collect())
}

/// The foreign keys of other tables that point to `schema.table` (the explorer's and the table
/// details' "References"), from the catalog.
#[tauri::command]
#[specta::specta]
async fn referencing_keys(
    session: String,
    schema: String,
    table: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<TableKeyItem>> {
    let open = state.session(&session).await?;
    let keys = open
        .session
        .connection()
        .list_referencing_keys(&schema, &table)
        .await
        .map_err(|e| e.to_string())?;
    Ok(keys
        .into_iter()
        .map(|key| TableKeyItem {
            direction: KeyDirectionItem::Incoming,
            name: key.name,
            schema: key.schema,
            table: key.table,
            columns: key.referenced_columns,
            other_columns: key.columns,
        })
        .collect())
}

/// Follows a foreign key from the row at `row` of a table data tab's page (`result`, a page
/// of `schema.table`): the core finds the key in the catalog again, reads the row's typed
/// values and writes the filter of the table data tab it leads to.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn follow_key(
    session: String,
    result: u32,
    row: u32,
    schema: String,
    table: String,
    key: KeyRefItem,
    state: State<'_, AppState>,
) -> CmdResult<KeyTargetItem> {
    let open = state.session(&session).await?;
    let key = spelaion_core::find_key(
        open.session.connection().as_ref(),
        &schema,
        &table,
        &key.into(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    spelaion_core::follow(open.session.dialect().as_ref(), &stored, row as usize, &key)
        .map(KeyTargetItem::from)
        .map_err(|e| e.to_string())
}

/// The references panel of a table data tab: every foreign key of the row at `row` (the
/// table's own and those that point to it), each with up to `limit` of the rows it leads to,
/// read in turn on the tab's session.
#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
async fn row_references(
    session: String,
    result: u32,
    row: u32,
    schema: String,
    table: String,
    limit: u32,
    state: State<'_, AppState>,
) -> CmdResult<Vec<RowReferenceItem>> {
    let open = state.session(&session).await?;
    let keys = spelaion_core::table_keys(open.session.connection().as_ref(), &schema, &table)
        .await
        .map_err(|e| e.to_string())?;
    let targets = {
        let stored = state
            .results
            .get(ResultId(result.into()))
            .ok_or("result freed")?;
        let stored = stored.read().map_err(|_| "result lock poisoned")?;
        spelaion_core::reference_targets(
            open.session.dialect().as_ref(),
            &stored,
            row as usize,
            keys,
        )
    };
    let references = spelaion_core::read_references(&open.session, targets, u64::from(limit)).await;
    Ok(references.into_iter().map(RowReferenceItem::from).collect())
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

/// Objects' names as the engine writes them (`schema.table`, quoted where it needs to be),
/// for the explorer's "Copy qualified name".
#[tauri::command]
#[specta::specta]
async fn qualified_names(
    session: String,
    paths: Vec<ObjectPathItem>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<String>> {
    let open = state.session(&session).await?;
    let dialect = open.session.dialect();
    Ok(paths
        .iter()
        .map(|path| spelaion_core::qualified_name(dialect.as_ref(), &path.schema, &path.names))
        .collect())
}

/// What the explorer may offer on this session's engine: the core decides (ADR-026).
#[tauri::command]
#[specta::specta]
async fn object_actions(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<ObjectActionsItem> {
    let open = state.session(&session).await?;
    let dialect = open.session.dialect();
    let mut actions = spelaion_core::object_actions(dialect.as_ref());
    if dialect.name() == "mysql" {
        let version = open
            .session
            .connection()
            .server_version()
            .await
            .unwrap_or_default();
        actions.add_check_constraints =
            spelaion_core::supports_added_checks(dialect.as_ref(), &version);
    }
    Ok(actions.into())
}

/// The SQL of a change to an object (rename, drop…), written by the core, to show before it
/// runs or to open in the editor, and whether its object can be dropped in cascade.
#[tauri::command]
#[specta::specta]
async fn preview_object_change(
    session: String,
    change: ObjectChangeItem,
    state: State<'_, AppState>,
) -> CmdResult<ObjectChangePreview> {
    let open = state.session(&session).await?;
    let change = spelaion_core::ObjectChange::try_from(change)?;
    let dialect = open.session.dialect();
    let (spelaion_core::ObjectChange::Rename { target, .. }
    | spelaion_core::ObjectChange::Drop { target, .. }) = &change;
    Ok(ObjectChangePreview {
        sql: spelaion_core::change_script(dialect.as_ref(), &change).map_err(|e| e.to_string())?,
        cascade: matches!(change, spelaion_core::ObjectChange::Drop { .. })
            && spelaion_core::can_cascade(dialect.as_ref(), target),
    })
}

/// The statement that starts a new object (table, view, schema…), written by the core in the
/// engine's types, to finish in a new tab: nothing runs (ADR-026).
#[tauri::command]
#[specta::specta]
async fn create_sql(
    session: String,
    kind: CreateKindItem,
    schema: String,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    Ok(spelaion_core::create_sql(
        open.session.dialect().as_ref(),
        kind.into(),
        &schema,
    ))
}

/// An index or constraint on the columns chosen in the explorer, in their order, written by
/// the core, to finish in a new tab: nothing runs (ADR-026).
#[tauri::command]
#[specta::specta]
async fn new_index_sql(
    session: String,
    schema: String,
    table: String,
    columns: Vec<String>,
    kind: NewIndexKindItem,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    spelaion_core::new_index_sql(
        open.session.dialect().as_ref(),
        &schema,
        &table,
        &columns,
        kind.into(),
    )
    .map_err(|e| e.to_string())
}

/// Prepares a table element in the editor without executing it (ADR-026).
#[tauri::command]
#[specta::specta]
async fn new_table_element_sql(
    session: String,
    schema: String,
    table: String,
    element: NewTableElementItem,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let open = state.session(&session).await?;
    let version = open
        .session
        .connection()
        .server_version()
        .await
        .unwrap_or_default();
    spelaion_core::new_table_element_sql(
        open.session.dialect().as_ref(),
        &version,
        &schema,
        &table,
        &element.into(),
    )
    .map_err(|e| e.to_string())
}

/// "Open object by name": the objects whose name contains `text`, in the schemas and among the
/// objects the explorer shows (its filter, and system schemas only with `show_system`).
#[tauri::command]
#[specta::specta]
async fn search_objects(
    session: String,
    text: String,
    schema_filter: Option<NameFilterItem>,
    object_filter: Option<NameFilterItem>,
    show_system: bool,
    state: State<'_, AppState>,
) -> CmdResult<Vec<FoundObjectItem>> {
    let open = state.session(&session).await?;
    let found = spelaion_core::search_objects(
        open.session.connection().as_ref(),
        &text,
        &schema_filter.map(NameFilter::from).unwrap_or_default(),
        &object_filter.map(NameFilter::from).unwrap_or_default(),
        show_system,
        spelaion_core::SEARCH_LIMIT,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(found.into_iter().map(FoundObjectItem::from).collect())
}

/// "Set as the editor's schema": where this tab's own session finds unqualified names from now
/// on (the saved connection does not change).
#[tauri::command]
#[specta::specta]
async fn set_active_schema(
    session: String,
    schema: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let open = state.session(&session).await?;
    spelaion_core::set_active_schema(&open.session, &schema)
        .await
        .map_err(|e| e.to_string())
}

/// The schema where a session finds unqualified names ("Link with editor").
#[tauri::command]
#[specta::specta]
async fn active_schema(session: String, state: State<'_, AppState>) -> CmdResult<Option<String>> {
    let open = state.session(&session).await?;
    spelaion_core::active_schema(&open.session).await
}

/// Runs a change to an object on the explorer's session, once the user confirmed the SQL it
/// was shown. The core writes the statements again from the change, and refuses read-only
/// connections (ADR-026). Returns how many statements ran.
#[tauri::command]
#[specta::specta]
async fn apply_object_change(
    session: String,
    change: ObjectChangeItem,
    confirmed: bool,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let open = state.session(&session).await?;
    let change = spelaion_core::ObjectChange::try_from(change)?;
    let ran = spelaion_core::apply_change(
        &open.session,
        &change,
        open.resolved.params.read_only,
        confirmed,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(u32::try_from(ran).unwrap_or(u32::MAX))
}

/// The statements the core would run for the pending changes of a table data tab, to show
/// before applying them (production asks for confirmation).
#[tauri::command]
#[specta::specta]
async fn preview_table_edits(
    session: String,
    result: u32,
    schema: String,
    name: String,
    changes: Vec<RowChangeItem>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<String>> {
    let open = state.session(&session).await?;
    table_edit_statements(&state, &open, result, &schema, &name, changes).await
}

/// Applies the pending changes in one transaction (ADR-014): refused on read-only
/// connections, and on production unless the user confirmed the statements.
#[tauri::command]
#[specta::specta]
async fn apply_table_edits(
    session: String,
    result: u32,
    schema: String,
    name: String,
    changes: Vec<RowChangeItem>,
    confirmed: bool,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let open = state.session(&session).await?;
    spelaion_core::check_edit_policy(
        open.resolved.params.read_only,
        open.resolved.safeguards().confirm_data_changes,
        confirmed,
    )
    .map_err(|e| e.to_string())?;
    let statements = table_edit_statements(&state, &open, result, &schema, &name, changes).await?;
    let applied = spelaion_core::apply_edits(&open.session, &statements)
        .await
        .map_err(|e| e.to_string())?;
    Ok(u32::try_from(applied).unwrap_or(u32::MAX))
}

async fn table_edit_statements(
    state: &AppState,
    open: &OpenConnection,
    result: u32,
    schema: &str,
    name: &str,
    changes: Vec<RowChangeItem>,
) -> CmdResult<Vec<String>> {
    // The primary key comes from the database, never from the UI.
    let table = open
        .session
        .connection()
        .describe_table(schema, name)
        .await
        .map_err(|e| e.to_string())?;
    let stored = state
        .results
        .get(ResultId(result.into()))
        .ok_or("result freed")?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    let changes: Vec<spelaion_core::RowChange> = changes.into_iter().map(Into::into).collect();
    spelaion_core::edit_statements(open.session.dialect().as_ref(), &table, &stored, &changes)
        .map_err(|e| e.to_string())
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
            plans::explain_preflight,
            plans::plan_capabilities,
            plans::explain_query,
            plans::open_plan_file,
            plans::startup_plan_path,
            plans::plan_page,
            plans::plan_statistics,
            plans::plan_raw_window,
            plans::release_plan,
            plans::export_plan,
            workspace_info,
            mcp::mcp_clients,
            mcp::mcp_plan_install,
            mcp::mcp_apply_install,
            mcp::mcp_settings,
            mcp::set_mcp_read_only,
            mcp::set_mcp_output_format,
            mcp::mcp_access,
            mcp::audit_events,
            mcp::audit_tools,
            mcp::audit_sessions,
            mcp::export_audit,
            mcp::clear_audit,
            import::import_preview,
            import::import_plan,
            import::run_import,
            import::cancel_import,
            get_language,
            set_language,
            get_theme,
            set_theme,
            get_update_check,
            set_update_check,
            get_explorer_preferences,
            set_explorer_preferences,
            get_editor_preferences,
            set_editor_preferences,
            get_grid_preferences,
            set_grid_preferences,
            get_script_preferences,
            set_script_preferences,
            files::open_sql_file,
            files::save_sql_file,
            files::recent_files,
            files::forget_recent_file,
            files::restore_tabs_for_workspace,
            files::save_tabs,
            files::flush_tabs,
            files::get_reconnect_on_start,
            files::set_reconnect_on_start,
            notify_run_finished,
            update::update_check,
            update::update_install,
            engine_catalog,
            keychain_available,
            list_tunnels,
            test_connection,
            stop_connection_test,
            add_connection,
            connection_editor_access,
            connection_draft,
            update_connection,
            delete_connection,
            change_connection_engine,
            duplicate_connection,
            move_connection,
            copy_connection,
            paste_connection,
            get_connection_name_pattern,
            set_connection_name_pattern,
            preview_connection_name_pattern,
            suggest_connection_name,
            tunnel_draft,
            test_tunnel,
            add_tunnel,
            update_tunnel,
            delete_tunnel,
            copy_rows,
            copy_column_names,
            copy_column_in_list,
            find_in_result,
            find_matches,
            find_step,
            clear_find,
            selection_stats,
            cancel_selection_stats,
            column_stats,
            cancel_column_stats,
            add_table_condition,
            table_filter_condition,
            table_keys,
            referencing_keys,
            follow_key,
            row_references,
            export_result,
            export_rerun_check,
            export_query,
            export_table_data,
            export_tables,
            cancel_export,
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
            qualified_names,
            object_actions,
            preview_object_change,
            apply_object_change,
            create_sql,
            new_index_sql,
            new_table_element_sql,
            search_objects,
            set_active_schema,
            active_schema,
            preview_table_edits,
            apply_table_edits,
            save_snippet,
            list_snippets,
            delete_snippet,
            statement_at,
            create_database,
            script_risks,
            script_params,
            bind_params,
            parse_connection_url,
            connection_import::scan_connections,
            kube_contexts,
            kube_namespaces,
            kube_names,
            kube_ports,
            connection_import::import_connections,
            open_workspace,
            trust_workspace,
            accept_change,
            set_user_vars,
            list_connection_types,
            save_connection_type,
            delete_connection_type,
            tunnel_ports,
            network_profiles,
            set_tunnel_ports,
            list_connections,
            list_history,
            library::history_page,
            library::history_databases,
            library::pin_history,
            library::export_history,
            library::snippet_page,
            library::snippet_folders,
            library::create_snippet_folder,
            library::move_snippet,
            set_history_enabled,
            connect,
            connect_from_session,
            disconnect,
            list_processes,
            control_process,
            run_script,
            answer_script_error,
            query_page_supported,
            run_query_page,
            append_query_page,
            count_query_rows,
            cancel,
            release_result,
            current_database,
            list_databases,
            select_database,
            list_schemas,
            list_objects,
            describe_table,
            list_schema_objects,
            list_server_objects,
            get_explorer_filter,
            set_explorer_filter,
            list_bookmarks,
            add_bookmark,
            remove_bookmark,
            filter_explorer_name,
            object_definition,
            relation_definition,
            procedure_call,
            table_templates,
            join_sql,
            editable_query_table,
            answer_interaction,
            transaction_status,
            set_auto_commit,
            commit_transaction,
            rollback_transaction,
            sessions_to_confirm,
            quit_app,
        ])
        .events(tauri_specta::collect_events![
            InteractionRequest,
            InteractionClosed,
            SessionLost,
            TransactionChanged,
            QuitBlocked
        ])
}

/// Generates `src/bindings.ts`. Run by `cargo run -p spelaion-desktop --features bindings --bin export-bindings`
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

    // SPELAION_USER_DIR allows another personal directory (tests, profiles), as in the CLI.
    let user_dir = match std::env::var_os("SPELAION_USER_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let dir = default_user_dir().expect("user configuration directory");
            // First start after the rename (ADR-023): the old personal folder is copied,
            // never moved, so nothing is lost if this version is uninstalled.
            if let Some(old) = spelaion_config::legacy_user_dir()
                && let Err(e) = spelaion_config::migrate_user_dir(&old, &dir)
            {
                eprintln!("could not copy the old personal folder: {e}");
            }
            dir
        }
    };
    // Query history and audit live there: other local users must not read them.
    if let Err(e) = spelaion_config::ensure_private_dir(&user_dir) {
        eprintln!("could not make the personal folder private: {e}");
    }
    // Initial folder: command-line argument or the last one opened.
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let startup_plan = arguments
        .windows(2)
        .find(|args| args[0] == "--plan")
        .map(|args| args[1].clone())
        .or_else(|| {
            arguments
                .first()
                .filter(|path| {
                    Path::new(path).is_file()
                        && matches!(
                            Path::new(path).extension().and_then(|s| s.to_str()),
                            Some("json" | "plan" | "sqlplan")
                        )
                })
                .cloned()
        });
    let workspace = arguments
        .first()
        .filter(|arg| !arg.starts_with('-') && Some(*arg) != startup_plan.as_ref())
        .cloned()
        .map(PathBuf::from)
        .or_else(|| {
            std::fs::read_to_string(last_workspace_file(&user_dir))
                .ok()
                .map(|s| PathBuf::from(s.trim()))
        })
        .filter(|p| p.is_dir());
    let config = load(workspace.as_deref(), &user_dir);
    let history = QueryHistory::open(spelaion_config::state_db(&user_dir))
        .expect("error while opening the local query history");
    // Its writer runs on Tauri's runtime, which `block_on` starts here.
    let tabs = tauri::async_runtime::block_on(async { TabKeeper::spawn(history.clone()) });
    let state = AppState {
        user_dir,
        workspace: RwLock::new(workspace),
        workspace_version: AtomicU64::new(0),
        config: RwLock::new(config),
        engines: Engines::builtin(),
        tunnels: Arc::new(TunnelPool::new()),
        results: Arc::new(ResultStore::new()),
        plans: Arc::new(spelaion_core::plan::PlanStore::new()),
        startup_plan: std::sync::Mutex::new(startup_plan),
        sessions: Mutex::new(HashMap::new()),
        history,
        tabs,
        pending: Pending::default(),
        quitting: AtomicBool::new(false),
        tests: ConnectionTests::default(),
        exports: std::sync::Mutex::new(HashMap::new()),
        imports: std::sync::Mutex::new(HashMap::new()),
    };

    let typed = builder.invoke_handler();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(state)
        .manage(update::PendingUpdate::default())
        .invoke_handler(move |invoke| match invoke.message.command() {
            "fetch_window" => raw_handler(invoke),
            _ => typed(invoke),
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                on_close_requested(window, api);
            }
        })
        .setup(move |app| {
            builder.mount_events(app);
            let _ = app.handle().get_webview_window("main");
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while starting SpelaionDB")
        .run(|app, event| {
            // Tauri exits without dropping the state: stop each `kubectl port-forward`
            // first, or it would outlive the app (ADR-015).
            if let tauri::RunEvent::Exit = event {
                let state = app.state::<AppState>();
                // The open tabs the core already has are written now, not in a second
                // (ADR-036).
                let tabs = state.tabs.clone();
                let _ = tauri::async_runtime::block_on(tabs.flush());
                let tunnels = state.tunnels.clone();
                tauri::async_runtime::block_on(tunnels.close_all());
            }
        });
}
