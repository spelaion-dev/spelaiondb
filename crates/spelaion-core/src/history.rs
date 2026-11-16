//! Desktop query history. A single actor owns local SQLite; synchronous disk access
//! never runs on the async executor. The recorder wraps the normal session event stream.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use spelaion_store::{
    AuditEntry, AuditFilter, AuditRecord, AuditSession, Bookmark, EditorPreferences,
    ExplorerFilter, ExplorerPreferences, GridPreferences, HistoryEntry, HistoryFilter, HistoryPage,
    MapPreferences, NewHistoryEntry, RecentFile, SavedTabs, ScriptPreferences, Snippet,
    SnippetFilter, SnippetFolder, Store, StoreError, ToolOutput,
};
use tokio::sync::{mpsc, oneshot};

use crate::{CoreEvent, ResultId, ScriptOptions, Session};

/// Identity captured from the source session, never from a subsequently edited profile.
#[derive(Debug, Clone, Default)]
pub struct HistoryScope {
    pub workspace: String,
    pub connection: String,
    pub database: Option<String>,
    /// MySQL/MariaDB and SQL Server can change databases within a script.
    pub switchable_database: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("query history is unavailable")]
    Closed,
    #[error("could not start query history: {0}")]
    Start(#[source] std::io::Error),
    #[error("unsupported language; expected system, en or es")]
    UnsupportedLanguage,
    #[error("unsupported theme; expected system, light or dark")]
    UnsupportedTheme,
    #[error("a saved query needs a name of 1 to {MAX_SNIPPET_NAME} characters")]
    InvalidSnippetName,
    #[error("{0}")]
    NamePattern(#[from] spelaion_config::NamePatternError),
}

/// Longest name of a saved query, in characters.
const MAX_SNIPPET_NAME: usize = 200;

type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

enum Request {
    HistoryPage {
        workspace: String,
        filter: HistoryFilter,
        limit: usize,
        offset: usize,
        reply: Reply<HistoryPage>,
    },
    HistoryDatabases {
        workspace: String,
        reply: Reply<Vec<String>>,
    },
    PinHistory {
        workspace: String,
        id: i64,
        pinned: bool,
        reply: Reply<bool>,
    },
    ExportHistory {
        workspace: String,
        filter: HistoryFilter,
        path: PathBuf,
        cancel: std::sync::Arc<crate::ExportCancel>,
        reply: Reply<usize>,
    },
    SnippetPage {
        workspace: String,
        filter: SnippetFilter,
        limit: usize,
        offset: usize,
        reply: Reply<Vec<Snippet>>,
    },
    SnippetFolders {
        workspace: String,
        reply: Reply<Vec<SnippetFolder>>,
    },
    CreateSnippetFolder {
        workspace: String,
        name: String,
        reply: Reply<i64>,
    },
    MoveSnippet {
        workspace: String,
        id: i64,
        folder: Option<i64>,
        reply: Reply<()>,
    },
    DeleteVisibleSnippet {
        workspace: String,
        id: i64,
        reply: Reply<bool>,
    },
    GetLanguage {
        reply: Reply<String>,
    },
    SetLanguage {
        language: String,
        reply: Reply<()>,
    },
    GetTheme {
        reply: Reply<String>,
    },
    SetTheme {
        theme: String,
        reply: Reply<()>,
    },
    GetUpdateCheck {
        reply: Reply<bool>,
    },
    GetNamePattern {
        reply: Reply<Option<String>>,
    },
    SetNamePattern {
        pattern: Option<String>,
        reply: Reply<()>,
    },
    GetExplorer {
        reply: Reply<ExplorerPreferences>,
    },
    SetExplorer {
        preferences: ExplorerPreferences,
        reply: Reply<()>,
    },
    GetEditor {
        reply: Reply<EditorPreferences>,
    },
    SetEditor {
        preferences: EditorPreferences,
        reply: Reply<()>,
    },
    GetGrid {
        reply: Reply<GridPreferences>,
    },
    GetMap {
        reply: Reply<MapPreferences>,
    },
    GetBackup {
        reply: Reply<crate::BackupPreferences>,
    },
    SetBackup {
        preferences: crate::BackupPreferences,
        reply: Reply<()>,
    },
    SetMap {
        preferences: MapPreferences,
        reply: Reply<()>,
    },
    SetGrid {
        preferences: GridPreferences,
        reply: Reply<()>,
    },
    GetScript {
        reply: Reply<ScriptPreferences>,
    },
    SetScript {
        preferences: ScriptPreferences,
        reply: Reply<()>,
    },
    MoveConnection {
        workspace: Option<String>,
        from: String,
        to: String,
        reply: Reply<()>,
    },
    SetUpdateCheck {
        enabled: bool,
        reply: Reply<()>,
    },
    GetMcp {
        reply: Reply<(bool, ToolOutput)>,
    },
    AuditEvents {
        filter: AuditFilter,
        limit: usize,
        offset: usize,
        reply: Reply<(Vec<AuditRecord>, i64)>,
    },
    AuditAll {
        filter: AuditFilter,
        reply: Reply<Vec<AuditEntry>>,
    },
    AuditTools {
        reply: Reply<Vec<String>>,
    },
    AuditSessions {
        limit: usize,
        reply: Reply<Vec<AuditSession>>,
    },
    ClearAudit {
        reply: Reply<i64>,
    },
    SetMcpReadOnly {
        read_only: bool,
        reply: Reply<()>,
    },
    SetMcpOutput {
        format: ToolOutput,
        reply: Reply<()>,
    },
    List {
        workspace: String,
        search: String,
        limit: usize,
        offset: usize,
        reply: Reply<Vec<HistoryEntry>>,
    },
    Enabled {
        workspace: String,
        connection: String,
        reply: Reply<bool>,
    },
    SetEnabled {
        workspace: String,
        connection: String,
        enabled: bool,
        reply: Reply<()>,
    },
    GetFilter {
        workspace: String,
        connection: String,
        reply: Reply<ExplorerFilter>,
    },
    SetFilter {
        workspace: String,
        connection: String,
        filter: ExplorerFilter,
        reply: Reply<()>,
    },
    Record {
        entry: NewHistoryEntry,
        database: Option<String>,
        reply: Reply<()>,
    },
    AddBookmark {
        workspace: String,
        bookmark: Bookmark,
        reply: Reply<()>,
    },
    RemoveBookmark {
        workspace: String,
        bookmark: Bookmark,
        reply: Reply<bool>,
    },
    Bookmarks {
        workspace: String,
        reply: Reply<Vec<Bookmark>>,
    },
    SaveSnippet {
        workspace: String,
        name: String,
        statement: String,
        connection: String,
        reply: Reply<i64>,
    },
    Snippets {
        workspace: String,
        search: String,
        reply: Reply<Vec<Snippet>>,
    },
    DeleteSnippet {
        id: i64,
        reply: Reply<bool>,
    },
    SaveOpenTabs {
        workspace: String,
        tabs: SavedTabs,
        reply: Reply<()>,
    },
    OpenTabs {
        workspace: String,
        reply: Reply<SavedTabs>,
    },
    AddRecentFile {
        path: String,
        reply: Reply<()>,
    },
    RecentFiles {
        reply: Reply<Vec<RecentFile>>,
    },
    RemoveRecentFile {
        path: String,
        reply: Reply<bool>,
    },
    GetReconnectOnStart {
        reply: Reply<bool>,
    },
    SetReconnectOnStart {
        enabled: bool,
        reply: Reply<()>,
    },
}

/// Personal history shared by editor sessions. Dropping all handles shuts down its actor.
#[derive(Clone)]
pub struct QueryHistory {
    requests: mpsc::Sender<Request>,
}

impl QueryHistory {
    /// Opens and migrates the local store synchronously, for application startup.
    pub fn open(path: PathBuf) -> Result<Self, HistoryError> {
        let mut store = Store::open(&path)?;
        let (requests, mut receiver) = mpsc::channel(32);
        std::thread::Builder::new()
            .name("spelaion-history".into())
            .spawn(move || {
                while let Some(request) = receiver.blocking_recv() {
                    match request {
                        Request::GetLanguage { reply } => {
                            let _ = reply.send(store.get_language());
                        }
                        Request::SetLanguage { language, reply } => {
                            let _ = reply.send(store.set_language(&language));
                        }
                        Request::GetTheme { reply } => {
                            let _ = reply.send(store.get_theme());
                        }
                        Request::SetTheme { theme, reply } => {
                            let _ = reply.send(store.set_theme(&theme));
                        }
                        Request::GetUpdateCheck { reply } => {
                            let _ = reply.send(store.get_update_check());
                        }
                        Request::GetNamePattern { reply } => {
                            let _ = reply.send(store.get_connection_name_pattern());
                        }
                        Request::SetNamePattern { pattern, reply } => {
                            let _ =
                                reply.send(store.set_connection_name_pattern(pattern.as_deref()));
                        }
                        Request::GetExplorer { reply } => {
                            let _ = reply.send(store.get_explorer_preferences());
                        }
                        Request::SetExplorer { preferences, reply } => {
                            let _ = reply.send(store.set_explorer_preferences(&preferences));
                        }
                        Request::GetEditor { reply } => {
                            let _ = reply.send(store.get_editor_preferences());
                        }
                        Request::SetEditor { preferences, reply } => {
                            let _ = reply.send(store.set_editor_preferences(&preferences));
                        }
                        Request::GetGrid { reply } => {
                            let _ = reply.send(store.get_grid_preferences());
                        }
                        Request::GetMap { reply } => {
                            let _ = reply.send(store.get_map_preferences());
                        }
                        Request::GetBackup { reply } => {
                            let _ = reply.send(store.get_backup_preferences());
                        }
                        Request::SetBackup { preferences, reply } => {
                            let _ = reply.send(store.set_backup_preferences(&preferences));
                        }
                        Request::SetMap { preferences, reply } => {
                            let _ = reply.send(store.set_map_preferences(&preferences));
                        }
                        Request::SetGrid { preferences, reply } => {
                            let _ = reply.send(store.set_grid_preferences(&preferences));
                        }
                        Request::GetScript { reply } => {
                            let _ = reply.send(store.get_script_preferences());
                        }
                        Request::SetScript { preferences, reply } => {
                            let _ = reply.send(store.set_script_preferences(&preferences));
                        }
                        Request::SetUpdateCheck { enabled, reply } => {
                            let _ = reply.send(store.set_update_check(enabled));
                        }
                        Request::GetMcp { reply } => {
                            let _ = reply.send(
                                store
                                    .mcp_read_only()
                                    .and_then(|r| Ok((r, store.mcp_output_format()?))),
                            );
                        }
                        Request::AuditEvents {
                            filter,
                            limit,
                            offset,
                            reply,
                        } => {
                            let _ = reply.send(
                                store
                                    .audit_events(&filter, limit, offset)
                                    .and_then(|events| Ok((events, store.audit_count(&filter)?))),
                            );
                        }
                        Request::AuditAll { filter, reply } => {
                            let _ = reply.send(
                                store
                                    .audit_events(&filter, usize::MAX >> 1, 0)
                                    .map(|records| records.into_iter().map(|r| r.entry).collect()),
                            );
                        }
                        Request::AuditTools { reply } => {
                            let _ = reply.send(store.audit_tools());
                        }
                        Request::AuditSessions { limit, reply } => {
                            let _ = reply.send(store.audit_sessions(limit));
                        }
                        Request::ClearAudit { reply } => {
                            let _ = reply.send(store.clear_audit("SpelaionDB"));
                        }
                        Request::SetMcpReadOnly { read_only, reply } => {
                            let _ = reply.send(store.set_mcp_read_only(read_only));
                        }
                        Request::SetMcpOutput { format, reply } => {
                            let _ = reply.send(store.set_mcp_output_format(format));
                        }
                        Request::List {
                            workspace,
                            search,
                            limit,
                            offset,
                            reply,
                        } => {
                            let _ =
                                reply.send(store.history(&workspace, &search, limit, offset).map(
                                    |mut entries| {
                                        for entry in &mut entries {
                                            redact_history(entry);
                                        }
                                        entries
                                    },
                                ));
                        }
                        Request::Enabled {
                            workspace,
                            connection,
                            reply,
                        } => {
                            let _ = reply.send(store.history_enabled(&workspace, &connection));
                        }
                        Request::SetEnabled {
                            workspace,
                            connection,
                            enabled,
                            reply,
                        } => {
                            let _ = reply.send(store.set_history_enabled(
                                &workspace,
                                &connection,
                                enabled,
                            ));
                        }
                        Request::MoveConnection {
                            workspace,
                            from,
                            to,
                            reply,
                        } => {
                            let _ = reply.send(store.move_connection_settings(
                                workspace.as_deref(),
                                &from,
                                &to,
                            ));
                        }
                        Request::GetFilter {
                            workspace,
                            connection,
                            reply,
                        } => {
                            let _ = reply.send(store.explorer_filter(&workspace, &connection));
                        }
                        Request::SetFilter {
                            workspace,
                            connection,
                            filter,
                            reply,
                        } => {
                            let _ = reply.send(store.set_explorer_filter(
                                &workspace,
                                &connection,
                                &filter,
                            ));
                        }
                        Request::Record {
                            mut entry,
                            database,
                            reply,
                        } => {
                            entry.statement = crate::audit::redact_secrets(&entry.statement);
                            entry.error = entry.error.as_deref().map(crate::audit::redact_secrets);
                            let _ = reply
                                .send(store.record_history_database(&entry, database.as_deref()));
                        }
                        Request::HistoryPage {
                            workspace,
                            filter,
                            limit,
                            offset,
                            reply,
                        } => {
                            let _ = reply.send(
                                store.history_page(&workspace, &filter, limit, offset).map(
                                    |mut page| {
                                        for entry in &mut page.entries {
                                            redact_history(entry);
                                        }
                                        page
                                    },
                                ),
                            );
                        }
                        Request::HistoryDatabases { workspace, reply } => {
                            let _ = reply.send(store.history_databases(&workspace));
                        }
                        Request::PinHistory {
                            workspace,
                            id,
                            pinned,
                            reply,
                        } => {
                            let _ = reply.send(store.pin_history(&workspace, id, pinned));
                        }
                        Request::ExportHistory {
                            workspace,
                            filter,
                            path,
                            cancel,
                            reply,
                        } => {
                            let _ = reply
                                .send(export_history(&store, &workspace, &filter, &path, &cancel));
                        }
                        Request::SnippetPage {
                            workspace,
                            filter,
                            limit,
                            offset,
                            reply,
                        } => {
                            let _ =
                                reply.send(store.snippet_page(&workspace, &filter, limit, offset));
                        }
                        Request::SnippetFolders { workspace, reply } => {
                            let _ = reply.send(store.snippet_folders(&workspace));
                        }
                        Request::CreateSnippetFolder {
                            workspace,
                            name,
                            reply,
                        } => {
                            let _ = reply.send(store.create_snippet_folder(&workspace, &name));
                        }
                        Request::MoveSnippet {
                            workspace,
                            id,
                            folder,
                            reply,
                        } => {
                            let _ = reply.send(store.move_snippet(&workspace, id, folder));
                        }
                        Request::DeleteVisibleSnippet {
                            workspace,
                            id,
                            reply,
                        } => {
                            let _ = reply.send(store.delete_visible_snippet(&workspace, id));
                        }
                        Request::AddBookmark {
                            workspace,
                            bookmark,
                            reply,
                        } => {
                            let _ = reply.send(store.add_bookmark(&workspace, &bookmark));
                        }
                        Request::RemoveBookmark {
                            workspace,
                            bookmark,
                            reply,
                        } => {
                            let _ = reply.send(store.remove_bookmark(&workspace, &bookmark));
                        }
                        Request::Bookmarks { workspace, reply } => {
                            let _ = reply.send(store.bookmarks(&workspace));
                        }
                        Request::SaveSnippet {
                            workspace,
                            name,
                            statement,
                            connection,
                            reply,
                        } => {
                            let _ = reply.send(store.save_snippet(
                                &workspace,
                                &name,
                                &statement,
                                &connection,
                            ));
                        }
                        Request::Snippets {
                            workspace,
                            search,
                            reply,
                        } => {
                            let _ = reply.send(store.snippets(&workspace, &search));
                        }
                        Request::DeleteSnippet { id, reply } => {
                            let _ = reply.send(store.delete_snippet(id));
                        }
                        Request::SaveOpenTabs {
                            workspace,
                            tabs,
                            reply,
                        } => {
                            let _ = reply.send(store.save_open_tabs(&workspace, &tabs));
                        }
                        Request::OpenTabs { workspace, reply } => {
                            let _ = reply.send(store.open_tabs(&workspace));
                        }
                        Request::AddRecentFile { path, reply } => {
                            let _ = reply.send(store.add_recent_file(&path));
                        }
                        Request::RecentFiles { reply } => {
                            let _ = reply.send(store.recent_files());
                        }
                        Request::RemoveRecentFile { path, reply } => {
                            let _ = reply.send(store.remove_recent_file(&path));
                        }
                        Request::GetReconnectOnStart { reply } => {
                            let _ = reply.send(store.reconnect_on_start());
                        }
                        Request::SetReconnectOnStart { enabled, reply } => {
                            let _ = reply.send(store.set_reconnect_on_start(enabled));
                        }
                    }
                }
            })
            .map_err(HistoryError::Start)?;
        Ok(Self { requests })
    }

    async fn request<T>(&self, build: impl FnOnce(Reply<T>) -> Request) -> Result<T, HistoryError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(build(reply))
            .await
            .map_err(|_| HistoryError::Closed)?;
        response
            .await
            .map_err(|_| HistoryError::Closed)?
            .map_err(HistoryError::Store)
    }

    /// Personal interface language for this user directory, defaulting to `system`.
    pub async fn language(&self) -> Result<String, HistoryError> {
        self.request(|reply| Request::GetLanguage { reply }).await
    }

    /// Validates external input before it reaches the store actor.
    pub async fn set_language(&self, language: String) -> Result<(), HistoryError> {
        if !matches!(language.as_str(), "system" | "en" | "es") {
            return Err(HistoryError::UnsupportedLanguage);
        }
        self.request(|reply| Request::SetLanguage { language, reply })
            .await
    }

    /// Personal interface theme for this user directory, defaulting to `system`.
    pub async fn theme(&self) -> Result<String, HistoryError> {
        self.request(|reply| Request::GetTheme { reply }).await
    }

    /// Validates external input before it reaches the store actor.
    pub async fn set_theme(&self, theme: String) -> Result<(), HistoryError> {
        if !matches!(theme.as_str(), "system" | "light" | "dark") {
            return Err(HistoryError::UnsupportedTheme);
        }
        self.request(|reply| Request::SetTheme { theme, reply })
            .await
    }

    /// Whether the app checks for updates when it starts (ADR-021), defaulting to on.
    pub async fn update_check(&self) -> Result<bool, HistoryError> {
        self.request(|reply| Request::GetUpdateCheck { reply })
            .await
    }

    /// The pattern the connection form names new connections with (ADR-030): the user's,
    /// or [`spelaion_config::DEFAULT_NAME_PATTERN`] until they choose one.
    pub async fn connection_name_pattern(&self) -> Result<String, HistoryError> {
        Ok(self
            .request(|reply| Request::GetNamePattern { reply })
            .await?
            .unwrap_or_else(|| spelaion_config::DEFAULT_NAME_PATTERN.to_string()))
    }

    /// Checks the pattern before it reaches the store: an unknown variable is refused, never
    /// dropped. `None` goes back to the default; an empty pattern suggests no name.
    pub async fn set_connection_name_pattern(
        &self,
        pattern: Option<String>,
    ) -> Result<(), HistoryError> {
        if let Some(pattern) = &pattern {
            spelaion_config::NamePattern::parse(pattern)?;
        }
        self.request(|reply| Request::SetNamePattern { pattern, reply })
            .await
    }

    /// How the explorer shows its tree (system objects, sorting).
    pub async fn explorer_preferences(&self) -> Result<ExplorerPreferences, HistoryError> {
        self.request(|reply| Request::GetExplorer { reply }).await
    }

    pub async fn set_explorer_preferences(
        &self,
        preferences: ExplorerPreferences,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetExplorer { preferences, reply })
            .await
    }

    pub async fn editor_preferences(&self) -> Result<EditorPreferences, HistoryError> {
        self.request(|reply| Request::GetEditor { reply }).await
    }
    pub async fn set_editor_preferences(
        &self,
        preferences: EditorPreferences,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetEditor { preferences, reply })
            .await
    }

    /// The map view's background (ADR-045): tiles off until the user turns them on.
    pub async fn map_preferences(&self) -> Result<MapPreferences, HistoryError> {
        self.request(|reply| Request::GetMap { reply }).await
    }

    /// Saves the map's preferences; a tile server that fails `check_tile_server` is refused.
    pub async fn set_map_preferences(
        &self,
        preferences: MapPreferences,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetMap { preferences, reply })
            .await
    }

    /// The backup dialog's remembered tool folders, output folder and name pattern (ADR-049).
    pub async fn backup_preferences(&self) -> Result<crate::BackupPreferences, HistoryError> {
        self.request(|reply| Request::GetBackup { reply }).await
    }

    pub async fn set_backup_preferences(
        &self,
        preferences: crate::BackupPreferences,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetBackup { preferences, reply })
            .await
    }

    /// How the result grid draws its cells (zebra rows, NULL, booleans).
    pub async fn grid_preferences(&self) -> Result<GridPreferences, HistoryError> {
        self.request(|reply| Request::GetGrid { reply }).await
    }

    pub async fn set_grid_preferences(
        &self,
        preferences: GridPreferences,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetGrid { preferences, reply })
            .await
    }

    /// The MCP server's settings (ADR-033): read-only mode for every agent, and the default
    /// output of query tools.
    pub async fn mcp_settings(&self) -> Result<(bool, ToolOutput), HistoryError> {
        self.request(|reply| Request::GetMcp { reply }).await
    }

    /// The MCP activity `filter` keeps (ADR-033), newest first, with how many there are in all.
    /// Secrets are masked by the caller (`audit::redacted`) before anything is shown.
    pub async fn audit_events(
        &self,
        filter: AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<AuditRecord>, i64), HistoryError> {
        self.request(|reply| Request::AuditEvents {
            filter,
            limit,
            offset,
            reply,
        })
        .await
    }

    /// Every entry `filter` keeps, for an export.
    pub async fn audit_all(&self, filter: AuditFilter) -> Result<Vec<AuditEntry>, HistoryError> {
        self.request(|reply| Request::AuditAll { filter, reply })
            .await
    }

    pub async fn audit_tools(&self) -> Result<Vec<String>, HistoryError> {
        self.request(|reply| Request::AuditTools { reply }).await
    }

    pub async fn audit_sessions(&self, limit: usize) -> Result<Vec<AuditSession>, HistoryError> {
        self.request(|reply| Request::AuditSessions { limit, reply })
            .await
    }

    /// Empties the MCP activity log, leaving a record that it was cleared. Returns how many
    /// entries went.
    pub async fn clear_audit(&self) -> Result<i64, HistoryError> {
        self.request(|reply| Request::ClearAudit { reply }).await
    }

    pub async fn set_mcp_read_only(&self, read_only: bool) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetMcpReadOnly { read_only, reply })
            .await
    }

    pub async fn set_mcp_output_format(&self, format: ToolOutput) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetMcpOutput { format, reply })
            .await
    }

    /// How scripts run from the editor (ADR-035): the default error mode and the opt-in
    /// notification.
    pub async fn script_preferences(&self) -> Result<ScriptPreferences, HistoryError> {
        self.request(|reply| Request::GetScript { reply }).await
    }

    pub async fn set_script_preferences(
        &self,
        preferences: ScriptPreferences,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetScript { preferences, reply })
            .await
    }

    pub async fn set_update_check(&self, enabled: bool) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetUpdateCheck { enabled, reply })
            .await
    }

    pub async fn list(
        &self,
        workspace: String,
        search: String,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<HistoryEntry>, HistoryError> {
        self.request(|reply| Request::List {
            workspace,
            search,
            limit,
            offset,
            reply,
        })
        .await
    }

    pub async fn enabled(
        &self,
        workspace: String,
        connection: String,
    ) -> Result<bool, HistoryError> {
        self.request(|reply| Request::Enabled {
            workspace,
            connection,
            reply,
        })
        .await
    }

    pub async fn set_enabled(
        &self,
        workspace: String,
        connection: String,
        enabled: bool,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetEnabled {
            workspace,
            connection,
            enabled,
            reply,
        })
        .await
    }

    /// A connection moved to another folder: its explorer filter, bookmarks, history setting
    /// and saved tabs follow it (see `Store::move_connection_settings`). `workspace` is `None`
    /// for a personal connection.
    pub async fn move_connection(
        &self,
        workspace: Option<String>,
        from: String,
        to: String,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::MoveConnection {
            workspace,
            from,
            to,
            reply,
        })
        .await
    }

    /// A connection's explorer filter (ADR-025), empty if it has none.
    pub async fn explorer_filter(
        &self,
        workspace: String,
        connection: String,
    ) -> Result<ExplorerFilter, HistoryError> {
        self.request(|reply| Request::GetFilter {
            workspace,
            connection,
            reply,
        })
        .await
    }

    pub async fn set_explorer_filter(
        &self,
        workspace: String,
        connection: String,
        filter: ExplorerFilter,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetFilter {
            workspace,
            connection,
            filter,
            reply,
        })
        .await
    }

    /// Bookmarks an object of the explorer (ADR-026); again changes nothing.
    pub async fn add_bookmark(
        &self,
        workspace: String,
        bookmark: Bookmark,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::AddBookmark {
            workspace,
            bookmark,
            reply,
        })
        .await
    }

    /// Whether the bookmark was there.
    pub async fn remove_bookmark(
        &self,
        workspace: String,
        bookmark: Bookmark,
    ) -> Result<bool, HistoryError> {
        self.request(|reply| Request::RemoveBookmark {
            workspace,
            bookmark,
            reply,
        })
        .await
    }

    /// A workspace's bookmarks, by connection, schema and name.
    pub async fn bookmarks(&self, workspace: String) -> Result<Vec<Bookmark>, HistoryError> {
        self.request(|reply| Request::Bookmarks { workspace, reply })
            .await
    }

    /// Saves a query (replacing one with the same name in the workspace); the name is
    /// trimmed. Returns its identifier.
    pub async fn save_snippet(
        &self,
        workspace: String,
        name: String,
        statement: String,
        connection: String,
    ) -> Result<i64, HistoryError> {
        let name = name.trim().to_string();
        if name.is_empty() || name.chars().count() > MAX_SNIPPET_NAME {
            return Err(HistoryError::InvalidSnippetName);
        }
        self.request(|reply| Request::SaveSnippet {
            workspace,
            name,
            statement,
            connection,
            reply,
        })
        .await
    }

    /// Saved queries of the workspace and the personal ones, by name.
    pub async fn snippets(
        &self,
        workspace: String,
        search: String,
    ) -> Result<Vec<Snippet>, HistoryError> {
        self.request(|reply| Request::Snippets {
            workspace,
            search,
            reply,
        })
        .await
    }

    pub async fn delete_snippet(&self, id: i64) -> Result<bool, HistoryError> {
        self.request(|reply| Request::DeleteSnippet { id, reply })
            .await
    }

    /// Replaces the open tabs kept for `workspace` (ADR-036). The desktop writes them through
    /// [`crate::TabKeeper`], which decides how often.
    pub async fn save_open_tabs(
        &self,
        workspace: String,
        tabs: SavedTabs,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SaveOpenTabs {
            workspace,
            tabs,
            reply,
        })
        .await
    }

    /// The open tabs kept for `workspace`, as saved; [`crate::restore_tabs`] checks them.
    pub async fn open_tabs(&self, workspace: String) -> Result<SavedTabs, HistoryError> {
        self.request(|reply| Request::OpenTabs { workspace, reply })
            .await
    }

    /// Puts a file first in the recent files (ADR-036).
    pub async fn add_recent_file(&self, path: String) -> Result<(), HistoryError> {
        self.request(|reply| Request::AddRecentFile { path, reply })
            .await
    }

    /// The recent files, newest first.
    pub async fn recent_files(&self) -> Result<Vec<RecentFile>, HistoryError> {
        self.request(|reply| Request::RecentFiles { reply }).await
    }

    pub async fn remove_recent_file(&self, path: String) -> Result<bool, HistoryError> {
        self.request(|reply| Request::RemoveRecentFile { path, reply })
            .await
    }

    /// Whether the app connects restored tabs at startup; off by default (ADR-036).
    pub async fn reconnect_on_start(&self) -> Result<bool, HistoryError> {
        self.request(|reply| Request::GetReconnectOnStart { reply })
            .await
    }

    pub async fn set_reconnect_on_start(&self, enabled: bool) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetReconnectOnStart { enabled, reply })
            .await
    }

    #[cfg(test)]
    async fn record(&self, entry: NewHistoryEntry) -> Result<(), HistoryError> {
        self.record_database(entry, None).await
    }

    async fn record_database(
        &self,
        entry: NewHistoryEntry,
        database: Option<String>,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::Record {
            entry,
            database,
            reply,
        })
        .await
    }

    pub async fn history_page(
        &self,
        workspace: String,
        filter: HistoryFilter,
        limit: usize,
        offset: usize,
    ) -> Result<HistoryPage, HistoryError> {
        self.request(|reply| Request::HistoryPage {
            workspace,
            filter,
            limit,
            offset,
            reply,
        })
        .await
    }
    pub async fn history_databases(&self, workspace: String) -> Result<Vec<String>, HistoryError> {
        self.request(|reply| Request::HistoryDatabases { workspace, reply })
            .await
    }
    pub async fn pin_history(
        &self,
        workspace: String,
        id: i64,
        pinned: bool,
    ) -> Result<bool, HistoryError> {
        self.request(|reply| Request::PinHistory {
            workspace,
            id,
            pinned,
            reply,
        })
        .await
    }
    pub async fn export_history(
        &self,
        workspace: String,
        filter: HistoryFilter,
        path: PathBuf,
        cancel: std::sync::Arc<crate::ExportCancel>,
    ) -> Result<usize, HistoryError> {
        self.request(|reply| Request::ExportHistory {
            workspace,
            filter,
            path,
            cancel,
            reply,
        })
        .await
    }
    pub async fn snippet_page(
        &self,
        workspace: String,
        filter: SnippetFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Snippet>, HistoryError> {
        self.request(|reply| Request::SnippetPage {
            workspace,
            filter,
            limit,
            offset,
            reply,
        })
        .await
    }
    pub async fn snippet_folders(
        &self,
        workspace: String,
    ) -> Result<Vec<SnippetFolder>, HistoryError> {
        self.request(|reply| Request::SnippetFolders { workspace, reply })
            .await
    }
    pub async fn create_snippet_folder(
        &self,
        workspace: String,
        name: String,
    ) -> Result<i64, HistoryError> {
        self.request(|reply| Request::CreateSnippetFolder {
            workspace,
            name,
            reply,
        })
        .await
    }
    pub async fn move_snippet(
        &self,
        workspace: String,
        id: i64,
        folder: Option<i64>,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::MoveSnippet {
            workspace,
            id,
            folder,
            reply,
        })
        .await
    }
    pub async fn delete_visible_snippet(
        &self,
        workspace: String,
        id: i64,
    ) -> Result<bool, HistoryError> {
        self.request(|reply| Request::DeleteVisibleSnippet {
            workspace,
            id,
            reply,
        })
        .await
    }

    /// Records only statements that actually execute. The caller must drain this stream
    /// even after its UI disappears, so completion and history do not depend on that UI.
    /// A script started with history disabled never saves SQL, even if enabled mid-run.
    pub fn run_script(
        &self,
        session: &Session,
        sql: &str,
        workspace: String,
        connection: String,
    ) -> BoxStream<'static, CoreEvent> {
        self.run_script_with_original(
            session,
            sql,
            workspace,
            connection,
            None,
            ScriptOptions::default(),
        )
    }

    /// Runs a script with its error mode and timeout (ADR-035); `original` is the SQL to record
    /// instead (generated pagination SQL records the user's SELECT).
    pub fn run_script_with_original(
        &self,
        session: &Session,
        sql: &str,
        workspace: String,
        connection: String,
        original: Option<String>,
        options: ScriptOptions,
    ) -> BoxStream<'static, CoreEvent> {
        self.run_script_in_context(
            session,
            sql,
            HistoryScope {
                workspace,
                connection,
                ..Default::default()
            },
            original,
            options,
        )
    }

    pub fn run_script_in_context(
        &self,
        session: &Session,
        sql: &str,
        scope: HistoryScope,
        original: Option<String>,
        options: ScriptOptions,
    ) -> BoxStream<'static, CoreEvent> {
        let events = session.run_script_with(sql, options);
        self.observe_run(session, sql, scope, original, events)
    }

    pub(crate) fn observe_run(
        &self,
        session: &Session,
        sql: &str,
        scope: HistoryScope,
        original: Option<String>,
        mut events: BoxStream<'static, CoreEvent>,
    ) -> BoxStream<'static, CoreEvent> {
        let source = session.connection().clone();
        let HistoryScope {
            workspace,
            connection,
            database,
            switchable_database,
        } = scope;
        let sql = sql.to_string();
        let history = self.clone();
        Box::pin(async_stream::stream! {
            let enabled = match history.enabled(workspace.clone(), connection.clone()).await {
                Ok(enabled) => enabled,
                Err(error) => {
                    yield warning(error);
                    false
                }
            };
            let mut active: Option<PendingEntry> = None;
            while let Some(event) = events.next().await {
                match &event {
                    CoreEvent::StatementStarted { span, .. } => {
                        if enabled {
                            active = Some(PendingEntry {
                                database: if switchable_database {source.database_context().await.ok().flatten()} else {database.clone()},
                                entry: NewHistoryEntry {
                                    at_ms: millis(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()),
                                    workspace: workspace.clone(),
                                    connection: connection.clone(),
                                    statement: original.clone().unwrap_or_else(|| span.text(&sql).to_string()),
                                    duration_ms: 0,
                                    rows: 0,
                                    error: None,
                                    cancelled: false,
                                },
                                started: Instant::now(),
                                results: HashMap::new(),
                            });
                        }
                    }
                    CoreEvent::Rows { result, total_rows, .. } => {
                        if let Some(active) = &mut active {
                            active.results.insert(*result, *total_rows);
                        }
                    }
                    CoreEvent::StatementFinished { elapsed, .. } => {
                        if let Some(active) = active.take() {
                            let database=active.database.clone();
                            if let Err(error)=history.record_database(active.finish(*elapsed,None,false),database).await {yield warning(error);}
                        }
                    }
                    CoreEvent::StatementFailed { error, .. } => {
                        if let Some(active) = active.take() {
                            let elapsed = active.started.elapsed();
                            let message = if error.cancelled {
                                "Query cancelled".to_string()
                            } else {
                                error.message.clone()
                            };
                            let database=active.database.clone();
                            if let Err(error) = history.record_database(active.finish(elapsed, Some(message), error.cancelled),database).await {
                                yield warning(error);
                            }
                        }
                    }
                    CoreEvent::ResultStarted { .. }
                    | CoreEvent::Notice { .. }
                    | CoreEvent::ScriptFinished { .. }
                    | CoreEvent::ErrorPaused { .. }
            | CoreEvent::HistoryWarning { .. } => {}
                }
                yield event;
            }
        })
    }
}

fn warning(error: HistoryError) -> CoreEvent {
    CoreEvent::HistoryWarning {
        message: error.to_string(),
    }
}

fn redact_history(entry: &mut HistoryEntry) {
    entry.statement = crate::audit::redact_secrets(&entry.statement);
    entry.error = entry.error.as_deref().map(crate::audit::redact_secrets);
}

/// Local SQL export: one store snapshot, bounded pages, no query execution, atomic replacement.
fn export_history(
    store: &Store,
    workspace: &str,
    filter: &HistoryFilter,
    path: &std::path::Path,
    cancel: &crate::ExportCancel,
) -> Result<usize, StoreError> {
    use spelaion_config::atomic_file::{AtomicWriter, NewFileMode};
    use std::io::Write;
    // Dropped on any error or cancel, which deletes the temporary file.
    let mut writer = std::io::BufWriter::new(AtomicWriter::create(path, NewFileMode::Default)?);
    let count = store.visit_history(workspace, filter, |entries| {
        for entry in entries {
            if cancel.is_cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "History export cancelled: no file was written",
                )
                .into());
            }
            // Do not turn a connection/database/error containing a newline into SQL.
            // Only the fixed timestamp and numbers appear in generated comments.
            writeln!(
                writer,
                "-- {} · {} ms · {} rows",
                crate::audit::iso_time(entry.at_ms),
                entry.duration_ms,
                entry.rows
            )?;
            writeln!(
                writer,
                "{}\n;\n",
                crate::audit::redact_secrets(&entry.statement)
            )?;
        }
        Ok(())
    })?;
    if cancel.is_cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "History export cancelled: no file was written",
        )
        .into());
    }
    writer.into_inner().map_err(|e| e.into_error())?.commit()?;
    Ok(count)
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

struct PendingEntry {
    entry: NewHistoryEntry,
    database: Option<String>,
    started: Instant,
    results: HashMap<ResultId, usize>,
}

impl PendingEntry {
    fn finish(
        mut self,
        elapsed: Duration,
        error: Option<String>,
        cancelled: bool,
    ) -> NewHistoryEntry {
        self.entry.duration_ms = millis(elapsed);
        self.entry.rows = self.results.values().fold(0i64, |total, &rows| {
            total.saturating_add(i64::try_from(rows).unwrap_or(i64::MAX))
        });
        self.entry.error = error;
        self.entry.cancelled = cancelled;
        self.entry
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spelaion_driver_api::{ConnectParams, Driver};

    use super::*;
    use crate::ResultStore;

    #[derive(Default)]
    struct ContextState {
        database: std::sync::Mutex<String>,
        executed: std::sync::Mutex<Vec<String>>,
        pause: std::sync::atomic::AtomicBool,
        fail: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }
    struct ContextConnection {
        state: Arc<ContextState>,
        capabilities: spelaion_driver_api::Capabilities,
    }
    struct ContextCancel;
    #[async_trait::async_trait]
    impl spelaion_driver_api::CancelHandle for ContextCancel {
        async fn cancel(&self) -> spelaion_driver_api::Result<()> {
            Ok(())
        }
    }
    #[async_trait::async_trait]
    impl spelaion_driver_api::Connection for ContextConnection {
        fn capabilities(&self) -> &spelaion_driver_api::Capabilities {
            &self.capabilities
        }
        async fn execute(
            &self,
            sql: &str,
            _: &spelaion_driver_api::ExecOptions,
        ) -> spelaion_driver_api::Result<spelaion_driver_api::EventStream> {
            self.state.executed.lock().unwrap().push(sql.into());
            if sql.starts_with("USE ") {
                *self.state.database.lock().unwrap() = "second".into();
            }
            Ok(Box::pin(futures_util::stream::iter([Ok(
                spelaion_driver_api::ExecEvent::Done {
                    rows_affected: None,
                },
            )])))
        }
        fn cancel_handle(&self) -> Arc<dyn spelaion_driver_api::CancelHandle> {
            Arc::new(ContextCancel)
        }
        async fn server_version(&self) -> spelaion_driver_api::Result<String> {
            Ok("test".into())
        }
        async fn current_database(&self) -> spelaion_driver_api::Result<Option<String>> {
            panic!("history must not execute a current_database metadata query");
        }
        async fn database_context(&self) -> spelaion_driver_api::Result<Option<String>> {
            if self.state.pause.load(std::sync::atomic::Ordering::SeqCst) {
                self.state.entered.notify_one();
                self.state.resume.notified().await;
            }
            if self.state.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(spelaion_driver_api::DriverError::Other(
                    "metadata unavailable".into(),
                ));
            }
            Ok(Some(self.state.database.lock().unwrap().clone()))
        }
        async fn list_schemas(
            &self,
        ) -> spelaion_driver_api::Result<Vec<spelaion_driver_api::SchemaInfo>> {
            Ok(vec![])
        }
        async fn list_objects(
            &self,
            _: &str,
        ) -> spelaion_driver_api::Result<Vec<spelaion_driver_api::ObjectInfo>> {
            Ok(vec![])
        }
        async fn describe_table(
            &self,
            _: &str,
            _: &str,
        ) -> spelaion_driver_api::Result<spelaion_driver_api::TableInfo> {
            Err(spelaion_driver_api::DriverError::Other("not used".into()))
        }
    }
    fn context_history(name: &str) -> (QueryHistory, Arc<Session>, Arc<ContextState>, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "spelaion-history-{name}-{}-{}/state.db",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let history = QueryHistory::open(path.clone()).unwrap();
        let state = Arc::new(ContextState::default());
        *state.database.lock().unwrap() = "first".into();
        let session = Arc::new(Session::new(
            Box::new(ContextConnection {
                state: state.clone(),
                capabilities: Default::default(),
            }),
            spelaion_dialect::for_engine(
                crate::open::test_driver(spelaion_config::Engine::Sqlite).engine(),
            )
            .expect("a dialect"),
            Arc::new(ResultStore::new()),
        ));
        (history, session, state, path)
    }
    fn scope() -> HistoryScope {
        HistoryScope {
            workspace: "/repo".into(),
            connection: "shop".into(),
            database: Some("stale-profile".into()),
            switchable_database: true,
        }
    }

    #[tokio::test]
    async fn mysql_history_preserves_row_count() {
        real_mysql_history_preserves_row_count(
            crate::open::test_driver(spelaion_config::Engine::Mysql),
            "SPELAION_TEST_MYSQL",
        )
        .await;
    }

    #[tokio::test]
    async fn mariadb_history_preserves_row_count() {
        real_mysql_history_preserves_row_count(
            crate::open::test_driver(spelaion_config::Engine::Mariadb),
            "SPELAION_TEST_MARIADB",
        )
        .await;
    }

    async fn real_mysql_history_preserves_row_count(driver: Arc<dyn Driver>, variable: &str) {
        let Ok(address) = std::env::var(variable) else {
            eprintln!("{variable} not set: test skipped");
            return;
        };
        let (host, port) = address.rsplit_once(':').unwrap();
        let connection = driver
            .connect(&ConnectParams {
                host: host.into(),
                port: port.parse().unwrap(),
                user: "spelaion".into(),
                password: Some(spelaion_driver_api::Secret::new("spelaion")),
                database: "spelaion".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let session = Session::new(
            connection,
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        let (history, _, _, _) = context_history(variable);
        let sql = |table: &str| {
            format!(
                "CREATE TEMPORARY TABLE {table}(v int); INSERT INTO {table} VALUES(1); UPDATE {table} SET v=2 WHERE v=1; SELECT ROW_COUNT();"
            )
        };
        let unwrapped: Vec<_> = session
            .run_script(&sql("history_unwrapped"))
            .collect()
            .await;
        let wrapped: Vec<_> = history
            .run_script_in_context(
                &session,
                &sql("history_wrapped"),
                HistoryScope {
                    database: Some("spelaion".into()),
                    ..scope()
                },
                None,
                ScriptOptions::default(),
            )
            .collect()
            .await;
        let last_value = |events: &[CoreEvent]| {
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
                "{events:?}"
            );
            let id = events
                .iter()
                .filter_map(|event| {
                    if let CoreEvent::ResultStarted { result, .. } = event {
                        Some(*result)
                    } else {
                        None
                    }
                })
                .next_back()
                .unwrap();
            let stored = session.results().get(id).unwrap();
            let stored = stored.read().unwrap();
            let batch = stored.window(0, 1).remove(0);
            spelaion_driver_api::Value::from_array(
                batch.column(0).as_ref(),
                0,
                stored.schema().columns[0].logical,
            )
            .unwrap()
            .to_string()
        };
        assert_eq!(
            last_value(&unwrapped),
            "1",
            "baseline without recording must preserve ROW_COUNT"
        );
        assert_eq!(
            last_value(&wrapped),
            "1",
            "history must not alter the user's ROW_COUNT"
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("spelaion")
        );
        let changed: Vec<_> = session
            .run_script("USE `information_schema`; SELECT 1;")
            .collect()
            .await;
        assert!(
            !changed
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
            "{changed:?}"
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("information_schema")
        );
        let failed: Vec<_> = session
            .run_script("USE `spelaion_missing_database_context`")
            .collect()
            .await;
        assert!(
            failed
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. }))
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("information_schema")
        );
        let opaque:Vec<_>=session.run_script("PREPARE context_stmt FROM 'SELECT 1'; EXECUTE context_stmt; DEALLOCATE PREPARE context_stmt;").collect().await;
        assert!(
            !opaque
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
            "{opaque:?}"
        );
        assert_eq!(session.connection().database_context().await.unwrap(), None);
        let restored: Vec<_> = session.run_script("USE `spelaion`").collect().await;
        assert!(
            !restored
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. }))
        );
        assert_eq!(
            session.connection().database_context().await.unwrap(),
            None,
            "opaque context stays unknown until an authoritative read"
        );
        assert_eq!(
            session
                .connection()
                .current_database()
                .await
                .unwrap()
                .as_deref(),
            Some("spelaion")
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("spelaion")
        );
    }

    #[tokio::test]
    async fn sqlserver_history_context_snapshot_preserves_row_count_and_handles_parseonly() {
        let Ok(address) = std::env::var("SPELAION_TEST_MSSQL") else {
            eprintln!("SPELAION_TEST_MSSQL not set: test skipped");
            return;
        };
        let (host, port) = address.rsplit_once(':').unwrap();
        let driver = crate::open::test_driver(spelaion_config::Engine::Sqlserver);
        let connection = driver
            .connect(&ConnectParams {
                host: host.into(),
                port: port.parse().unwrap(),
                user: "sa".into(),
                password: Some(spelaion_driver_api::Secret::new("Spelaion-test-1")),
                database: "master".into(),
                tls: spelaion_driver_api::TlsMode::Require,
                ..Default::default()
            })
            .await
            .unwrap();
        let session = Session::new(
            connection,
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        let (history, _, _, _) = context_history("sqlserver-snapshot");
        let setup:Vec<_>=session.run_script("CREATE TABLE #history_context(v int); INSERT INTO #history_context VALUES(1),(3); UPDATE #history_context SET v=v+1;").collect().await;
        assert!(
            !setup
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
            "{setup:?}"
        );
        let baseline: Vec<_> = session
            .run_script("SELECT @@ROWCOUNT AS affected")
            .collect()
            .await;
        let value = |events: &[CoreEvent]| {
            let id = events
                .iter()
                .filter_map(|event| {
                    if let CoreEvent::ResultStarted { result, .. } = event {
                        Some(*result)
                    } else {
                        None
                    }
                })
                .next_back()
                .unwrap();
            let stored = session.results().get(id).unwrap();
            let stored = stored.read().unwrap();
            let batch = stored.window(0, 1).remove(0);
            spelaion_driver_api::Value::from_array(
                batch.column(0).as_ref(),
                0,
                stored.schema().columns[0].logical,
            )
            .unwrap()
            .to_string()
        };
        let baseline_value = value(&baseline);
        eprintln!("SQL Server unwrapped row count: {baseline_value}");
        let repeated: Vec<_> = session
            .run_script("UPDATE #history_context SET v=v+1;")
            .collect()
            .await;
        assert!(
            !repeated
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
            "{repeated:?}"
        );
        let count: Vec<_> = history
            .run_script_in_context(
                &session,
                "SELECT @@ROWCOUNT AS affected",
                HistoryScope {
                    database: Some("master".into()),
                    ..scope()
                },
                None,
                ScriptOptions::default(),
            )
            .collect()
            .await;
        assert_eq!(
            value(&count),
            baseline_value,
            "history must preserve @@ROWCOUNT from the previous request"
        );
        let switched: Vec<_> = session.run_script("USE [tempdb]").collect().await;
        assert!(
            !switched
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
            "{switched:?}"
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("tempdb")
        );
        let failed: Vec<_> = session
            .run_script("USE [spelaion_missing_database_context]")
            .collect()
            .await;
        assert!(
            failed
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. }))
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("tempdb")
        );
        for sql in [
            "CREATE PROCEDURE #context_snapshot AS SELECT 1",
            "#context_snapshot",
            "USE [master]",
        ] {
            let events: Vec<_> = session.run_script(sql).collect().await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
                "{sql}: {events:?}"
            );
        }
        assert_eq!(
            session.connection().database_context().await.unwrap(),
            None,
            "a bare procedure is opaque without EXEC too"
        );
        let actual = session.connection().current_database().await.unwrap();
        assert_eq!(actual.as_deref(), Some("master"));
        assert_eq!(
            session.connection().database_context().await.unwrap(),
            actual
        );
        // Both system databases exist on a fresh SQL Server fixture; no earlier test
        // or native smoke run needs to have created an application database.
        for sql in ["SET PARSEONLY ON", "USE [tempdb]", "SET PARSEONLY OFF"] {
            let events: Vec<_> = session.run_script(sql).collect().await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
                "{sql}: {events:?}"
            );
            assert_eq!(
                session.connection().database_context().await.unwrap(),
                None,
                "a parse-only USE must not invent an observed target"
            );
        }
        let actual = session.connection().current_database().await.unwrap();
        eprintln!("SQL Server actual database after PARSEONLY probe: {actual:?}");
        assert_eq!(actual.as_deref(), Some("master"));
        assert_eq!(
            session.connection().database_context().await.unwrap(),
            actual
        );
        for mode in [
            "PARSEONLY",
            "NOEXEC",
            "FMTONLY",
            "SHOWPLAN_TEXT",
            "SHOWPLAN_ALL",
            "SHOWPLAN_XML",
        ] {
            let on: Vec<_> = session
                .run_script(&format!("SET {mode} ON"))
                .collect()
                .await;
            assert!(
                !on.iter()
                    .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
                "{mode}: {on:?}"
            );
            assert_eq!(session.connection().database_context().await.unwrap(), None);
            assert_eq!(
                session.connection().current_database().await.unwrap(),
                None,
                "{mode} must not turn a plan into a database name"
            );
            assert_eq!(session.connection().database_context().await.unwrap(), None);
            let off: Vec<_> = session
                .run_script(&format!("SET {mode} OFF"))
                .collect()
                .await;
            assert!(
                !off.iter()
                    .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
                "{mode}: {off:?}"
            );
            let actual = session.connection().current_database().await.unwrap();
            assert!(actual.is_some(), "{mode} off restores an actual row");
            assert_eq!(
                session.connection().database_context().await.unwrap(),
                actual
            );
        }
    }

    #[tokio::test]
    async fn duckdb_history_follows_an_attached_catalog_after_use() {
        let (history, _, _, _) = context_history("duckdb-context");
        let driver = crate::open::test_driver(spelaion_config::Engine::Duckdb);
        let connection = driver
            .connect(&ConnectParams {
                database: ":memory:".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let session = Session::new(
            connection,
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        let events: Vec<_> = history
            .run_script_in_context(
                &session,
                "ATTACH ':memory:' AS another; USE another.main; SELECT 1;",
                scope(),
                None,
                ScriptOptions::default(),
            )
            .collect()
            .await;
        assert!(
            matches!(
                events.last(),
                Some(CoreEvent::ScriptFinished {
                    executed: 3,
                    failed: false,
                    ..
                })
            ),
            "{events:?}"
        );
        let page = history
            .history_page("/repo".into(), HistoryFilter::default(), 50, 0)
            .await
            .unwrap();
        assert_eq!(
            page.entries
                .iter()
                .find(|entry| entry.statement.starts_with("SELECT"))
                .unwrap()
                .database
                .as_deref(),
            Some("another")
        );
        assert_ne!(
            page.entries
                .iter()
                .find(|entry| entry.statement.starts_with("USE"))
                .unwrap()
                .database
                .as_deref(),
            Some("another")
        );
        let quoted: Vec<_> = session
            .run_script("USE \"another\".\"main\"")
            .collect()
            .await;
        assert!(
            !quoted
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. }))
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("another")
        );
        let failed: Vec<_> = session.run_script("USE missing.main").collect().await;
        assert!(
            failed
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. }))
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("another")
        );
        let ambiguous: Vec<_> = session.run_script("USE main").collect().await;
        assert!(
            !ambiguous
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. }))
        );
        assert_eq!(
            session.connection().database_context().await.unwrap(),
            None,
            "one-part USE can name a schema or catalog"
        );
        assert_eq!(
            session
                .connection()
                .current_database()
                .await
                .unwrap()
                .as_deref(),
            Some("another")
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("another")
        );
        let schema_change: Vec<_> = session
            .run_script("SET schema = 'memory.main'")
            .collect()
            .await;
        assert!(
            !schema_change
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
            "{schema_change:?}"
        );
        assert_eq!(session.connection().database_context().await.unwrap(), None);
        assert_eq!(
            session
                .connection()
                .current_database()
                .await
                .unwrap()
                .as_deref(),
            Some("memory")
        );
        assert_eq!(
            session
                .connection()
                .database_context()
                .await
                .unwrap()
                .as_deref(),
            Some("memory")
        );
        let reset: Vec<_> = session.run_script("RESET search_path").collect().await;
        assert!(
            !reset
                .iter()
                .any(|event| matches!(event, CoreEvent::StatementFailed { .. })),
            "{reset:?}"
        );
        assert_eq!(session.connection().database_context().await.unwrap(), None);
    }

    #[tokio::test]
    async fn source_context_follows_use_and_metadata_failure_never_prevents_execution() {
        let (history, session, state, _) = context_history("database");
        let events: Vec<_> = history
            .run_script_in_context(
                &session,
                "USE second; SELECT 1;",
                scope(),
                None,
                ScriptOptions::default(),
            )
            .collect()
            .await;
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished { executed: 2, .. })
        ));
        let page = history
            .history_page("/repo".into(), HistoryFilter::default(), 50, 0)
            .await
            .unwrap();
        let first = page
            .entries
            .iter()
            .find(|e| e.statement.starts_with("USE"))
            .unwrap();
        let second = page
            .entries
            .iter()
            .find(|e| e.statement.starts_with("SELECT"))
            .unwrap();
        assert_eq!(first.database.as_deref(), Some("first"));
        assert_eq!(second.database.as_deref(), Some("second"));
        state.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        let events: Vec<_> = history
            .run_script_in_context(
                &session,
                "SELECT 2",
                scope(),
                None,
                ScriptOptions::default(),
            )
            .collect()
            .await;
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished { executed: 1, .. })
        ));
        assert_eq!(
            history
                .history_page("/repo".into(), HistoryFilter::default(), 1, 0)
                .await
                .unwrap()
                .entries[0]
                .database,
            None
        );
    }

    #[tokio::test]
    async fn cancel_before_first_poll_affects_only_already_requested_streams() {
        let (_, session, state, _) = context_history("cancel-before-poll");
        let requested = session.run_script("SELECT before_cancel");
        session.cancel().await.unwrap();
        let events: Vec<_> = requested.collect().await;
        assert!(state.executed.lock().unwrap().is_empty());
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished { executed: 0, .. })
        ));
        let fresh: Vec<_> = session.run_script("SELECT after_cancel").collect().await;
        assert!(matches!(
            fresh.last(),
            Some(CoreEvent::ScriptFinished { executed: 1, .. })
        ));
        assert_eq!(*state.executed.lock().unwrap(), ["SELECT after_cancel"]);
    }

    #[tokio::test]
    async fn cancel_waiting_for_history_actor_never_executes_or_records_sql() {
        let (_, session, state, _) = context_history("cancel-history-queue");
        let (requests, mut inbox) = mpsc::channel(2);
        let history = QueryHistory { requests };
        let requested = history.run_script(
            &session,
            "SELECT waiting_for_history",
            "/repo".into(),
            "shop".into(),
        );
        let mut consume = tokio::spawn(async move { requested.collect::<Vec<_>>().await });
        // Hold the Enabled reply exactly as a long export ahead of it in the actor would.
        let Request::Enabled { reply, .. } = inbox.recv().await.unwrap() else {
            panic!("history must first ask whether recording is enabled");
        };
        session.cancel().await.unwrap();
        reply.send(Ok(true)).unwrap();
        let events = tokio::select! {
            completed = &mut consume => completed.unwrap(),
            _unexpected = inbox.recv() => panic!("a cancelled pending run must not request a history record"),
        };
        assert!(state.executed.lock().unwrap().is_empty());
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished { executed: 0, .. })
        ));
    }

    #[tokio::test]
    async fn cancel_queued_on_session_control_never_starts_the_queued_sql() {
        use futures_util::FutureExt;
        let (_, session, state, _) = context_history("cancel-control-queue");
        let mut first = session.run_script("SELECT holds_control");
        assert!(matches!(
            first.next().await,
            Some(CoreEvent::StatementStarted { .. })
        ));
        let mut queued = session.run_script("SELECT queued_on_control");
        assert!(
            queued.next().now_or_never().is_none(),
            "the first stream still owns control"
        );
        session.cancel().await.unwrap();
        drop(first);
        let events: Vec<_> = queued.collect().await;
        assert!(state.executed.lock().unwrap().is_empty());
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished { executed: 0, .. })
        ));
        let fresh: Vec<_> = session
            .run_script("SELECT fresh_after_queue_cancel")
            .collect()
            .await;
        assert!(matches!(
            fresh.last(),
            Some(CoreEvent::ScriptFinished { executed: 1, .. })
        ));
        assert_eq!(
            *state.executed.lock().unwrap(),
            ["SELECT fresh_after_queue_cancel"]
        );
    }

    #[tokio::test]
    async fn cancellation_during_context_lookup_never_executes_or_records_the_statement() {
        let (history, session, state, _) = context_history("cancel-context");
        state.pause.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut events = history.run_script_in_context(
            &session,
            "SELECT 1",
            scope(),
            None,
            ScriptOptions::default(),
        );
        let consume = tokio::spawn(async move {
            let mut result = vec![];
            while let Some(event) = events.next().await {
                result.push(event);
            }
            result
        });
        state.entered.notified().await;
        session.cancel().await.unwrap();
        state.resume.notify_one();
        let events = consume.await.unwrap();
        assert!(state.executed.lock().unwrap().is_empty());
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished { executed: 0, .. })
        ));
        assert!(
            history
                .history_page("/repo".into(), HistoryFilter::default(), 50, 0)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
    }

    // Several threads: the mode and link check blocks on exports in place.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn history_redaction_and_atomic_paged_export_preserve_scope_and_cancelled_destination() {
        let (history, _, _, path) = context_history("export");
        let entry = |workspace: &str, sql: &str| NewHistoryEntry {
            workspace: workspace.into(),
            connection: "shop".into(),
            statement: sql.into(),
            at_ms: 123,
            rows: 1,
            duration_ms: 2,
            error: Some("PASSWORD 'error-secret'".into()),
            cancelled: false,
        };
        history
            .record(entry("/repo", "ALTER USER bob PASSWORD 'not-stored'"))
            .await
            .unwrap();
        history
            .record(entry("/other", "SELECT 'other-workspace'"))
            .await
            .unwrap();
        let legacy = Store::open(&path).unwrap();
        legacy
            .record_history(&entry(
                "/repo",
                "SELECT 'postgres://bob:legacy-secret@db/shop'",
            ))
            .unwrap();
        let page = history
            .history_page("/repo".into(), HistoryFilter::default(), 50, 0)
            .await
            .unwrap();
        for row in &page.entries {
            assert!(
                !row.statement.contains("not-stored") && !row.statement.contains("legacy-secret")
            );
            assert!(!row.error.as_ref().unwrap().contains("error-secret"));
        }
        let stored = legacy.history("/repo", "", 50, 0).unwrap();
        assert!(
            !stored
                .iter()
                .find(|row| row.statement.starts_with("ALTER"))
                .unwrap()
                .statement
                .contains("not-stored")
        );
        let output = path.with_file_name("history.sql");
        let count = history
            .export_history(
                "/repo".into(),
                HistoryFilter::default(),
                output.clone(),
                Arc::new(crate::ExportCancel::new()),
            )
            .await
            .unwrap();
        assert_eq!(count, 2);
        let sql = std::fs::read_to_string(&output).unwrap();
        assert!(sql.contains("ALTER USER"));
        assert!(
            !sql.contains("not-stored")
                && !sql.contains("legacy-secret")
                && !sql.contains("other-workspace")
        );
        let cancel = Arc::new(crate::ExportCancel::new());
        cancel.cancel().await;
        assert!(
            history
                .export_history(
                    "/repo".into(),
                    HistoryFilter::default(),
                    output.clone(),
                    cancel
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(&output).unwrap(), sql);
        assert!(
            !std::fs::read_dir(output.parent().unwrap())
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".spelaion-tmp"))
        );
        #[cfg(unix)]
        {
            let handle = tokio::runtime::Handle::current();
            crate::test_files::assert_keeps_mode_and_links("history", |path| {
                let count = tokio::task::block_in_place(|| {
                    handle.block_on(history.export_history(
                        "/repo".into(),
                        HistoryFilter::default(),
                        path.to_path_buf(),
                        Arc::new(crate::ExportCancel::new()),
                    ))
                })
                .unwrap();
                assert_eq!(count, 2);
            });
        }
    }

    #[tokio::test]
    async fn language_settings_are_personal_persistent_and_coexist_with_history() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let user_dirs = std::env::temp_dir().join(format!(
            "spelaion-core-language-users-{}-{nonce}",
            std::process::id()
        ));
        let first_path = user_dirs.join("first-user/state.db");
        let second_path = user_dirs.join("second-user/state.db");
        let history = QueryHistory::open(first_path.clone()).unwrap();
        let other_user = QueryHistory::open(second_path).unwrap();
        assert_eq!(history.language().await.unwrap(), "system");
        history.set_language("es".into()).await.unwrap();
        assert_eq!(other_user.language().await.unwrap(), "system");
        assert!(matches!(
            history.set_language("fr".into()).await,
            Err(HistoryError::UnsupportedLanguage)
        ));
        assert_eq!(history.language().await.unwrap(), "es");
        history
            .record(NewHistoryEntry {
                at_ms: 1,
                workspace: "/repo".into(),
                connection: "shop".into(),
                statement: "select 1".into(),
                duration_ms: 2,
                rows: 1,
                error: None,
                cancelled: false,
            })
            .await
            .unwrap();
        history
            .set_enabled("/repo".into(), "shop".into(), false)
            .await
            .unwrap();
        let reopened = QueryHistory::open(first_path).unwrap();
        assert_eq!(reopened.language().await.unwrap(), "es");
        assert_eq!(
            reopened
                .list("/repo".into(), "select".into(), 10, 0)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            !reopened
                .enabled("/repo".into(), "shop".into())
                .await
                .unwrap()
        );
        history.set_language("en".into()).await.unwrap();
        assert_eq!(reopened.language().await.unwrap(), "en");
        history.set_language("system".into()).await.unwrap();
        assert_eq!(reopened.language().await.unwrap(), "system");
    }

    #[tokio::test]
    async fn invalid_language_is_rejected_before_reaching_the_store_actor() {
        let (requests, receiver) = mpsc::channel(1);
        drop(receiver);
        let history = QueryHistory { requests };
        assert!(matches!(
            history.set_language("es-ES".into()).await,
            Err(HistoryError::UnsupportedLanguage)
        ));
        assert!(matches!(
            history.set_language("es".into()).await,
            Err(HistoryError::Closed)
        ));
    }

    #[tokio::test]
    async fn theme_setting_is_personal_and_persistent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "spelaion-core-theme-{}-{nonce}/state.db",
            std::process::id()
        ));
        let history = QueryHistory::open(path.clone()).unwrap();
        assert_eq!(history.theme().await.unwrap(), "system");
        history.set_theme("dark".into()).await.unwrap();
        history.set_language("es".into()).await.unwrap();
        drop(history);
        let reopened = QueryHistory::open(path).unwrap();
        assert_eq!(reopened.theme().await.unwrap(), "dark");
        assert_eq!(reopened.language().await.unwrap(), "es");
    }

    #[tokio::test]
    async fn invalid_theme_is_rejected_before_reaching_the_store_actor() {
        let (requests, receiver) = mpsc::channel(1);
        drop(receiver);
        let history = QueryHistory { requests };
        for invalid in ["", "Dark", "blue", "light "] {
            assert!(matches!(
                history.set_theme(invalid.into()).await,
                Err(HistoryError::UnsupportedTheme)
            ));
        }
        assert!(matches!(
            history.set_theme("dark".into()).await,
            Err(HistoryError::Closed)
        ));
    }

    #[tokio::test]
    async fn connection_name_patterns_are_checked_and_persistent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "spelaion-core-name-pattern-{}-{nonce}/state.db",
            std::process::id()
        ));
        let history = QueryHistory::open(path.clone()).unwrap();
        assert_eq!(
            history.connection_name_pattern().await.unwrap(),
            spelaion_config::DEFAULT_NAME_PATTERN
        );
        for invalid in ["{hots}", "{database", "x}", "{}"] {
            assert!(matches!(
                history
                    .set_connection_name_pattern(Some(invalid.into()))
                    .await,
                Err(HistoryError::NamePattern(_))
            ));
        }
        history
            .set_connection_name_pattern(Some("{engine} · {host}".into()))
            .await
            .unwrap();
        drop(history);
        let reopened = QueryHistory::open(path).unwrap();
        assert_eq!(
            reopened.connection_name_pattern().await.unwrap(),
            "{engine} · {host}"
        );
        reopened.set_connection_name_pattern(None).await.unwrap();
        assert_eq!(
            reopened.connection_name_pattern().await.unwrap(),
            spelaion_config::DEFAULT_NAME_PATTERN
        );
    }

    #[tokio::test]
    async fn snippet_names_are_checked_before_reaching_the_store_actor() {
        let (requests, receiver) = mpsc::channel(1);
        drop(receiver);
        let history = QueryHistory { requests };
        for invalid in [
            String::new(),
            "   ".into(),
            "x".repeat(MAX_SNIPPET_NAME + 1),
        ] {
            assert!(matches!(
                history
                    .save_snippet(String::new(), invalid, "select 1".into(), String::new())
                    .await,
                Err(HistoryError::InvalidSnippetName)
            ));
        }
        assert!(matches!(
            history
                .save_snippet(
                    String::new(),
                    " ok ".into(),
                    "select 1".into(),
                    String::new()
                )
                .await,
            Err(HistoryError::Closed)
        ));
    }

    #[tokio::test]
    async fn unavailable_history_warns_without_changing_query_execution() {
        let (requests, receiver) = mpsc::channel(1);
        drop(receiver);
        let history = QueryHistory { requests };
        let driver = crate::open::test_driver(spelaion_config::Engine::Sqlite);
        let connection = driver
            .connect(&ConnectParams {
                database: ":memory:".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let session = Session::new(
            connection,
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        let events: Vec<_> = history
            .run_script(
                &session,
                "select 'sensitive_value'",
                "/repo".into(),
                "shop".into(),
            )
            .collect()
            .await;
        assert!(
            matches!(events.first(), Some(CoreEvent::HistoryWarning { message })
            if message.contains("history is unavailable") && !message.contains("sensitive_value"))
        );
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished {
                executed: 1,
                failed: false,
                ..
            })
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CoreEvent::Rows { total_rows: 1, .. }))
        );
    }
}
