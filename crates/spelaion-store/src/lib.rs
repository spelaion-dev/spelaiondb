//! SpelaionDB local storage (ADR-011): a SQLite database in the personal directory.
//!
//! Query history, saved queries (snippets), the MCP audit log, and the editor's open tabs and
//! recent files (ADR-036). The schema cache will come here with its own migration.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

mod aside;
mod backup;
mod editor_snippets;
mod library;
mod map;
mod table_filters;
mod tabs;
pub use aside::{aside_path, set_aside};
pub use backup::{BACKUP_TEXT_MAX, BackupPreferences};
pub use editor_snippets::{EditorSnippet, is_snippet_name};
pub use library::{HistoryFilter, HistoryPage, HistoryStatus, SnippetFilter, SnippetFolder};
pub use map::{
    MapPreferences, TILE_ATTRIBUTION_MAX, TILE_NAME_MAX, TILE_URL_MAX, TileServerError,
    check_tile_server,
};
pub use table_filters::{FilteredTable, TABLE_FILTER_HISTORY, TABLE_FILTER_MAX_CHARS};
pub use tabs::{
    FileEncoding, LineEnding, MAX_RECENT_FILES, RecentFile, SavedFile, SavedTab, SavedTabs,
};

#[derive(Debug, thiserror::Error)]
#[error("local store: {message}")]
pub struct StoreError {
    message: String,
    /// The schema version of a store a newer SpelaionDB wrote (see [`StoreError::newer_schema`]).
    newer_schema: Option<i64>,
}

impl StoreError {
    fn new(message: String) -> Self {
        Self {
            message,
            newer_schema: None,
        }
    }

    /// The store's schema version when it could not be opened because a newer SpelaionDB wrote
    /// it. That store is still in use by that version: it must not be started over, only opened
    /// by updating (SDB-283).
    pub fn newer_schema(&self) -> Option<i64> {
        self.newer_schema
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::new(e.to_string())
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// Migrations in order. A published one is never edited: another one is added.
const MIGRATIONS: &[&str] = &["create table audit (
        id            integer primary key,
        at_ms         integer not null,
        client        text not null,
        connection    text not null,
        statement     text not null,
        decision      text not null,
        rows          integer,
        duration_ms   integer,
        detail        text
    );
    create index audit_at on audit (at_ms);
    -- Append-only: the audit log is never edited or deleted from the application.
    create trigger audit_no_update before update on audit begin select raise(abort, 'the audit log cannot be modified'); end;
    create trigger audit_no_delete before delete on audit begin select raise(abort, 'the audit log cannot be deleted'); end;",
    "create table history (
        id            integer primary key,
        at_ms         integer not null,
        workspace     text not null,
        connection    text not null,
        statement     text not null,
        duration_ms   integer not null check (duration_ms >= 0),
        rows          integer not null check (rows >= 0),
        error         text
    );
    create index history_workspace_at on history (workspace, at_ms desc, id desc);
    create virtual table history_fts using fts5(
        connection, statement, content = 'history', content_rowid = 'id'
    );
    create trigger history_insert after insert on history begin
        insert into history_fts (rowid, connection, statement)
        values (new.id, new.connection, new.statement);
    end;
    create trigger history_delete after delete on history begin
        insert into history_fts (history_fts, rowid, connection, statement)
        values ('delete', old.id, old.connection, old.statement);
    end;
    create trigger history_update after update on history begin
        insert into history_fts (history_fts, rowid, connection, statement)
        values ('delete', old.id, old.connection, old.statement);
        insert into history_fts (rowid, connection, statement)
        values (new.id, new.connection, new.statement);
    end;
    create table history_preferences (
        workspace     text not null,
        connection    text not null,
        enabled       integer not null check (enabled in (0, 1)),
        primary key (workspace, connection)
    );",
    "create table personal_settings (
        key           text primary key,
        value         text not null,
        check (key <> 'language' or value in ('system', 'en', 'es'))
    );
    alter table history add column cancelled integer not null default 0 check (cancelled in (0, 1));",
    // SQLite cannot add a check constraint to an existing table: triggers enforce it.
    "create trigger personal_settings_theme_insert before insert on personal_settings
        when new.key = 'theme' and new.value not in ('system', 'light', 'dark')
        begin select raise(abort, 'unsupported theme'); end;
    create trigger personal_settings_theme_update before update on personal_settings
        when new.key = 'theme' and new.value not in ('system', 'light', 'dark')
        begin select raise(abort, 'unsupported theme'); end;",
    "create table snippets (
        id            integer primary key,
        workspace     text not null,
        name          text not null check (length(trim(name)) > 0),
        statement     text not null,
        connection    text not null,
        updated_ms    integer not null,
        unique (workspace, name)
    );",
    // Patterns one per line (ADR-025); a row with every list empty is deleted instead.
    "create table explorer_filters (
        workspace       text not null,
        connection      text not null,
        schema_include  text not null,
        schema_exclude  text not null,
        object_include  text not null,
        object_exclude  text not null,
        column_include  text not null,
        column_exclude  text not null,
        primary key (workspace, connection)
    );",
    // Explorer bookmarks (ADR-026): an object by its schema, name and kind, per workspace and
    // connection like the filters. `detail` tells apart overloads (a function's arguments).
    "create table explorer_bookmarks (
        workspace   text not null,
        connection  text not null,
        schema      text not null,
        name        text not null,
        kind        text not null,
        detail      text not null,
        created_ms  integer not null,
        primary key (workspace, connection, schema, name, kind, detail)
    );",
    // The MCP server's settings (ADR-033): read by `spelaiondb mcp` on every call, so a value
    // it does not know must never get in.
    "create trigger personal_settings_mcp_insert before insert on personal_settings
        when (new.key = 'mcp.read_only' and new.value not in ('on', 'off'))
          or (new.key = 'mcp.output_format' and new.value not in ('json', 'toon'))
        begin select raise(abort, 'unsupported MCP setting'); end;
    create trigger personal_settings_mcp_update before update on personal_settings
        when (new.key = 'mcp.read_only' and new.value not in ('on', 'off'))
          or (new.key = 'mcp.output_format' and new.value not in ('json', 'toon'))
        begin select raise(abort, 'unsupported MCP setting'); end;",
    // The MCP activity (ADR-033): which `spelaiondb mcp` process and which tool made each call.
    // Still append-only: the triggers above stay; clearing is `clear_audit`.
    "alter table audit add column session text;
    alter table audit add column tool text;
    create index audit_session on audit (session, at_ms);",
    // Open editor tabs per workspace (ADR-036), restored after a restart, disconnected: never
    // results, secrets, or a session's or transaction's state. A file tab keeps what it needs to
    // save the file the way it was read and to notice a change on disk.
    "create table open_tabs (
        workspace         text not null,
        position          integer not null check (position >= 0),
        active            integer not null check (active in (0, 1)),
        title             text,
        text              text not null,
        connection        text,
        pinned            integer not null check (pinned in (0, 1)),
        on_error          text check (on_error in ('stop', 'continue', 'ask')),
        timeout_seconds   integer check (timeout_seconds > 0),
        snippet_name      text,
        file_path         text check (length(file_path) > 0),
        file_hash         text,
        file_encoding     text check (file_encoding in
                            ('utf-8', 'utf-8-bom', 'utf-16le', 'utf-16be', 'windows-1252')),
        file_line_ending  text check (file_line_ending in ('lf', 'crlf', 'cr')),
        file_modified     integer check (file_modified in (0, 1)),
        primary key (workspace, position),
        check ((file_path is null) = (file_encoding is null)
           and (file_path is null) = (file_line_ending is null)
           and (file_path is null) = (file_modified is null))
    );
    create table recent_files (
        path       text primary key check (length(path) > 0),
        opened_ms  integer not null
    );
    create trigger personal_settings_editor_insert before insert on personal_settings
        when new.key = 'editor.reconnect_on_start' and new.value not in ('on', 'off')
        begin select raise(abort, 'unsupported editor setting'); end;
    create trigger personal_settings_editor_update before update on personal_settings
        when new.key = 'editor.reconnect_on_start' and new.value not in ('on', 'off')
        begin select raise(abort, 'unsupported editor setting'); end;",
    library::MIGRATION,
    // A connection's `${name}` variables for the editor's scripts (SDB-383), per workspace and
    // connection like its explorer filter. Local values the user typed: never sent anywhere,
    // never read by `spelaiondb mcp`.
    "create table connection_variables (
        workspace   text not null,
        connection  text not null,
        name        text not null check (length(name) between 1 and 64),
        value       text not null,
        primary key (workspace, connection, name)
    );",
    table_filters::MIGRATION,
];

/// The longest name of an editor variable (SDB-383), in bytes.
pub const MAX_VARIABLE_NAME: usize = 64;
/// The longest value of an editor variable, in bytes.
pub const MAX_VARIABLE_VALUE: usize = 64 * 1024;

/// Whether `name` can name an editor variable: a letter or `_`, then letters, digits and `_`.
pub fn is_variable_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= MAX_VARIABLE_NAME
        && bytes
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// Whether `value` can be an editor variable's value: bounded, and without the NUL character.
pub fn is_variable_value(value: &str) -> bool {
    value.len() <= MAX_VARIABLE_VALUE && !value.contains('\0')
}

/// Milliseconds since 1970 (UTC); 0 if the clock is before it.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A saved query. Like history, it belongs to a workspace (a repository's canonical root,
/// or empty outside one) and remembers the connection it was saved with (may be empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub id: i64,
    pub workspace: String,
    pub name: String,
    pub statement: String,
    pub connection: String,
    pub updated_ms: i64,
    pub folder_id: Option<i64>,
}

/// One execution from the desktop app, scoped to its workspace and connection identifier.
/// An empty workspace identifies a personal connection, visible from every workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewHistoryEntry {
    /// Time execution started, in milliseconds since 1970 (UTC).
    pub at_ms: i64,
    pub workspace: String,
    pub connection: String,
    pub statement: String,
    pub duration_ms: i64,
    /// Rows returned, including any partial results before an error or cancellation.
    pub rows: i64,
    /// The error or cancellation message; `None` means successful execution.
    pub error: Option<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    pub at_ms: i64,
    pub workspace: String,
    pub connection: String,
    pub statement: String,
    pub duration_ms: i64,
    pub rows: i64,
    pub error: Option<String>,
    pub cancelled: bool,
    pub database: Option<String>,
    pub pinned: bool,
}

/// What was done with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// It was executed.
    Executed,
    /// It was rejected before reaching the server (classifier, connection not exposed…).
    Rejected,
    /// It reached the server and failed.
    Failed,
    /// The user cleared the log: this record says when and how many entries went.
    Cleared,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::Executed => "executed",
            Decision::Rejected => "rejected",
            Decision::Failed => "failed",
            Decision::Cleared => "cleared",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "executed" => Some(Decision::Executed),
            "rejected" => Some(Decision::Rejected),
            "failed" => Some(Decision::Failed),
            "cleared" => Some(Decision::Cleared),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Milliseconds since 1970 (UTC).
    pub at_ms: i64,
    /// Client declared by the agent (informational: the client itself says so).
    pub client: String,
    pub connection: String,
    pub statement: String,
    pub decision: Decision,
    pub rows: Option<i64>,
    pub duration_ms: Option<i64>,
    /// Reason for the rejection or error message.
    pub detail: Option<String>,
    /// The `spelaiondb mcp` process that made the call (ADR-033); `None` before 0.7.
    pub session: Option<String>,
    /// The MCP tool called (`run_query`, `execute_write`…); `None` before 0.7.
    pub tool: Option<String>,
}

impl AuditEntry {
    pub fn now(client: &str, connection: &str, statement: &str, decision: Decision) -> Self {
        Self {
            at_ms: now_ms(),
            client: client.into(),
            connection: connection.into(),
            statement: statement.into(),
            decision,
            rows: None,
            duration_ms: None,
            detail: None,
            session: None,
            tool: None,
        }
    }
}

/// Which audit entries to show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// Text the statement contains (literal, case-insensitive for ASCII).
    pub search: String,
    pub tool: Option<String>,
    pub decision: Option<Decision>,
    pub session: Option<String>,
}

/// An audit entry with its position in the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditRecord {
    pub id: i64,
    pub entry: AuditEntry,
}

/// The calls of one `spelaiondb mcp` process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditSession {
    pub session: String,
    pub client: String,
    pub started_ms: i64,
    pub ended_ms: i64,
    pub calls: i64,
    pub refused: i64,
    pub failed: i64,
}

/// `where …` and its parameters for a filter.
fn audit_where(filter: &AuditFilter) -> (String, Vec<String>) {
    let mut clauses = Vec::new();
    let mut params = Vec::new();
    if !filter.search.is_empty() {
        let escaped = filter
            .search
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        params.push(format!("%{escaped}%"));
        clauses.push(format!("statement like ?{} escape '\\'", params.len()));
    }
    if let Some(tool) = &filter.tool {
        params.push(tool.clone());
        clauses.push(format!("tool = ?{}", params.len()));
    }
    if let Some(decision) = filter.decision {
        params.push(decision.as_str().into());
        clauses.push(format!("decision = ?{}", params.len()));
    }
    if let Some(session) = &filter.session {
        params.push(session.clone());
        clauses.push(format!("session = ?{}", params.len()));
    }
    let clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("where {}", clauses.join(" and "))
    };
    (clause, params)
}

fn audit_row(r: &rusqlite::Row) -> rusqlite::Result<AuditRecord> {
    let decision: String = r.get(5)?;
    Ok(AuditRecord {
        id: r.get(0)?,
        entry: AuditEntry {
            at_ms: r.get(1)?,
            client: r.get(2)?,
            connection: r.get(3)?,
            statement: r.get(4)?,
            decision: Decision::parse(&decision).unwrap_or(Decision::Failed),
            rows: r.get(6)?,
            duration_ms: r.get(7)?,
            detail: r.get(8)?,
            session: r.get(9)?,
            tool: r.get(10)?,
        },
    })
}

/// How `spelaiondb mcp` writes a query's rows for the agent when the call does not say
/// (ADR-033).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolOutput {
    /// An object with the columns and the rows as arrays.
    #[default]
    Json,
    /// TOON: one header with the column names, then one line of values per row.
    Toon,
}

impl ToolOutput {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolOutput::Json => "json",
            ToolOutput::Toon => "toon",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(ToolOutput::Json),
            "toon" => Some(ToolOutput::Toon),
            _ => None,
        }
    }
}

/// Local database. Used by the app and `spelaiondb mcp` at the same time: WAL mode and waiting on locks.
/// A connection's explorer filter (ADR-025): include and exclude patterns for its schemas,
/// objects and columns. Empty lists show everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExplorerFilter {
    pub schema_include: Vec<String>,
    pub schema_exclude: Vec<String>,
    pub object_include: Vec<String>,
    pub object_exclude: Vec<String>,
    pub column_include: Vec<String>,
    pub column_exclude: Vec<String>,
}

impl ExplorerFilter {
    fn lists(&self) -> [&Vec<String>; 6] {
        [
            &self.schema_include,
            &self.schema_exclude,
            &self.object_include,
            &self.object_exclude,
            &self.column_include,
            &self.column_exclude,
        ]
    }

    pub fn is_empty(&self) -> bool {
        self.lists().iter().all(|list| list.is_empty())
    }

    fn lists_of(&mut self, kind: FilterKind) -> (&mut Vec<String>, &mut Vec<String>) {
        match kind {
            FilterKind::Schemas => (&mut self.schema_include, &mut self.schema_exclude),
            FilterKind::Objects => (&mut self.object_include, &mut self.object_exclude),
            FilterKind::Columns => (&mut self.column_include, &mut self.column_exclude),
        }
    }

    /// "Hide": adds a pattern to the excluded ones of a kind.
    pub fn hide(&mut self, kind: FilterKind, pattern: &str) {
        let (_, exclude) = self.lists_of(kind);
        if !exclude.iter().any(|p| p == pattern) {
            exclude.push(pattern.to_string());
        }
    }

    /// "Show only": the patterns become the only included ones of a kind, and stop being
    /// excluded.
    pub fn show_only(&mut self, kind: FilterKind, patterns: &[String]) {
        let (include, exclude) = self.lists_of(kind);
        *include = patterns.to_vec();
        exclude.retain(|p| !patterns.contains(p));
    }
}

/// The three kinds of names an explorer filter has patterns for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterKind {
    Schemas,
    Objects,
    Columns,
}

/// Patterns are kept one per line: blank ones are dropped and none may contain a line break.
fn pattern_lines(patterns: &[String]) -> Result<String, StoreError> {
    let mut lines = Vec::new();
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.contains(['\n', '\r']) {
            return Err(StoreError::new(
                "a filter pattern cannot contain a line break".into(),
            ));
        }
        if !pattern.is_empty() {
            lines.push(pattern);
        }
    }
    Ok(lines.join("\n"))
}

fn patterns_of(lines: &str) -> Vec<String> {
    lines
        .lines()
        .filter(|line| !line.is_empty())
        .map(String::from)
        .collect()
}

/// An object bookmarked in the explorer (ADR-026), personal like the explorer filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub connection: String,
    /// The schema the object is in, or the schema itself (with an empty `name`).
    pub schema: String,
    pub name: String,
    /// `table`, `view`… for a relation, `function`, `sequence`… for another object, `schema`.
    pub kind: String,
    /// What tells same-name objects apart (a function's arguments); empty if nothing.
    pub detail: String,
}

/// How the explorer shows its tree, shared by every workspace in this user directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExplorerPreferences {
    /// System schemas (`pg_catalog`, `information_schema`, `sys`…) and built-in roles and
    /// logins. Off by default.
    pub show_system: bool,
    /// Names in alphabetical order rather than the database's; on by default. Columns always
    /// keep their table's order.
    pub sort_alphabetically: bool,
    /// Alphabetical order ignores case (`b` between `A` and `C`); on by default.
    pub sort_ignore_case: bool,
    /// Folders (roles, tablespaces…) before the schemas next to them; off by default.
    pub folders_first: bool,
    /// Only the connections that are connected (opened in the explorer or used by a tab);
    /// off by default.
    pub connected_only: bool,
    /// Filters go into the catalog queries, so big catalogs are not read whole (ADR-025);
    /// on by default. Off, every name is read and filtered on this computer.
    pub server_side_filters: bool,
    /// The tree follows the active editor tab, showing its connection, schema and table
    /// ("Link with editor", ADR-026); off by default.
    pub link_with_editor: bool,
}

impl Default for ExplorerPreferences {
    fn default() -> Self {
        Self {
            show_system: false,
            sort_alphabetically: true,
            sort_ignore_case: true,
            folders_first: false,
            connected_only: false,
            server_side_filters: true,
            link_with_editor: false,
        }
    }
}

/// The interface sizes the user may choose (SDB-389), in percent of the normal size: the
/// whole window is drawn at that scale, every size the other settings give included.
pub const INTERFACE_SIZES: [u16; 5] = [90, 100, 110, 125, 150];
/// The interface size until the user chooses another.
pub const DEFAULT_INTERFACE_SIZE: u16 = 100;
/// The editor's zoom steps (SDB-389), in percent of the editor's own font size and line
/// height: Ctrl+= and Ctrl+- move to the next step, Ctrl+0 goes back to 100.
pub const EDITOR_ZOOMS: [u16; 11] = [50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200];
/// The editor's zoom until the user zooms.
pub const DEFAULT_EDITOR_ZOOM: u16 = 100;

/// Personal SQL editor and formatter preferences. Values are checked before storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorPreferences {
    pub font: String,
    pub font_size: u8,
    pub line_height: u8,
    /// The editor's zoom, one of [`EDITOR_ZOOMS`]: it multiplies `font_size` and
    /// `line_height`, so going back to 100 gives the sizes chosen in Settings again.
    pub zoom: u16,
    pub tab_size: u8,
    pub line_numbers: bool,
    pub word_wrap: bool,
    pub keyword_case: String,
    pub indent: u8,
    pub comma_position: String,
}
impl Default for EditorPreferences {
    fn default() -> Self {
        Self {
            font: "system".into(),
            font_size: 13,
            line_height: 18,
            zoom: DEFAULT_EDITOR_ZOOM,
            tab_size: 2,
            line_numbers: true,
            word_wrap: false,
            keyword_case: "preserve".into(),
            indent: 2,
            comma_position: "trailing".into(),
        }
    }
}
impl EditorPreferences {
    fn validate(&self) -> Result<(), StoreError> {
        if !["system", "Consolas", "Cascadia Code", "Courier New"].contains(&self.font.as_str())
            || !(10..=32).contains(&self.font_size)
            || !(14..=48).contains(&self.line_height)
            || self.line_height < self.font_size
            || !EDITOR_ZOOMS.contains(&self.zoom)
            || !(1..=8).contains(&self.tab_size)
            || !(1..=8).contains(&self.indent)
            || !["preserve", "upper", "lower"].contains(&self.keyword_case.as_str())
            || !["leading", "trailing"].contains(&self.comma_position.as_str())
        {
            return Err(StoreError::new("invalid editor preferences".into()));
        }
        Ok(())
    }
}

/// The result tabs an editor tab keeps until the user chooses another number.
pub const DEFAULT_RESULT_TABS: u32 = 20;
/// The most result tabs an editor tab may be set to keep.
pub const MAX_RESULT_TABS: u32 = 100;

/// How the result grid draws its cells and how many result tabs an editor tab keeps, shared
/// by every workspace in this user directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPreferences {
    /// Wrap long text in the cell viewer; on by default.
    pub viewer_wrap: bool,
    /// Every other row tinted; on by default.
    pub zebra: bool,
    /// NULL cells read `NULL` (dimmed); on by default. Off, they are drawn empty.
    pub null_text: bool,
    /// Boolean columns drawn as checkboxes instead of `true` / `false`; off by default.
    pub bool_checkboxes: bool,
    /// How many result tabs an editor tab keeps (SDB-378), from 1 to [`MAX_RESULT_TABS`]:
    /// past it, the oldest result that is not pinned is closed.
    pub result_tabs: u32,
    /// Values colored by their type (numbers, text, dates, booleans, NULL); off by default.
    pub colorize: bool,
    /// The column header shows the column's type under its name; on by default.
    pub header_type: bool,
    /// The column header shows the column's comment, where it is known; off by default.
    pub header_comment: bool,
    /// The grid's own font size in pixels (SDB-385), from [`MIN_GRID_FONT`] to
    /// [`MAX_GRID_FONT`]: not the editor's, nor the interface's.
    pub font_size: u32,
}

/// The grid's font size until the user chooses another, and its bounds, in pixels.
pub const DEFAULT_GRID_FONT: u32 = 12;
pub const MIN_GRID_FONT: u32 = 10;
pub const MAX_GRID_FONT: u32 = 20;

/// How the grid shows values (SDB-385), as the user set it: `strftime` patterns for dates,
/// times and timestamps, a number's marks, a display time zone and how binary reads. Kept as
/// text; the core checks it before it is saved and reads it leniently (a value it does not
/// know is the default). An empty text means "as the database prints it".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayPreferences {
    /// Everything as the database prints it, whatever the other settings say; on by default.
    pub as_stored: bool,
    pub date: String,
    pub time: String,
    pub timestamp: String,
    pub grouping: String,
    pub decimal: String,
    pub time_zone: String,
    pub binary: String,
}

impl Default for DisplayPreferences {
    fn default() -> Self {
        Self {
            as_stored: true,
            date: String::new(),
            time: String::new(),
            timestamp: String::new(),
            grouping: String::new(),
            decimal: String::new(),
            time_zone: String::new(),
            binary: String::new(),
        }
    }
}

impl Default for GridPreferences {
    fn default() -> Self {
        Self {
            viewer_wrap: true,
            zebra: true,
            null_text: true,
            bool_checkboxes: false,
            result_tabs: DEFAULT_RESULT_TABS,
            colorize: false,
            header_type: true,
            header_comment: false,
            font_size: DEFAULT_GRID_FONT,
        }
    }
}

/// The copy defaults (SDB-388): what "Copy special…" chose last, for every workspace in this
/// user directory. Kept as text; the core checks what they mean before they are saved and
/// reads them leniently (`spelaion-core`'s `History::copy_defaults`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPreferences {
    /// One character.
    pub delimiter: String,
    /// One character, or empty: never quoted.
    pub quote: String,
    /// `lf` or `crlf`.
    pub row_delimiter: String,
    pub null_text: String,
    pub header: bool,
    pub row_numbers: bool,
}

impl Default for CopyPreferences {
    fn default() -> Self {
        Self {
            delimiter: "\t".into(),
            quote: "\"".into(),
            row_delimiter: "lf".into(),
            null_text: String::new(),
            header: true,
            row_numbers: false,
        }
    }
}

/// What a script run does by default when a statement fails (ADR-035); each run may choose
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptErrorMode {
    #[default]
    Stop,
    Continue,
    Ask,
}

impl ScriptErrorMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "stop" => Some(ScriptErrorMode::Stop),
            "continue" => Some(ScriptErrorMode::Continue),
            "ask" => Some(ScriptErrorMode::Ask),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ScriptErrorMode::Stop => "stop",
            ScriptErrorMode::Continue => "continue",
            ScriptErrorMode::Ask => "ask",
        }
    }
}

/// How scripts run from the editor, shared by every workspace in this user directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptPreferences {
    /// What a run does when a statement fails; stop by default.
    pub on_error: ScriptErrorMode,
    /// A system notification when a run that took at least `notify_after_seconds` finishes
    /// while the app is in the background; off by default.
    pub notify: bool,
    pub notify_after_seconds: u32,
    /// Seconds "Run anyway" stays disabled in the confirmation of a risky statement, so it is
    /// never pressed on reflex; 0 (the default) enables it at once. Only a pause: whether to
    /// confirm is the core's decision (`guard.rs`), never this setting's.
    pub confirm_delay_seconds: u32,
}

impl ScriptPreferences {
    /// Longest notification threshold accepted: a day.
    pub const MAX_NOTIFY_AFTER: u32 = 86_400;
    /// Longest pause before "Run anyway".
    pub const MAX_CONFIRM_DELAY: u32 = 10;
}

impl Default for ScriptPreferences {
    fn default() -> Self {
        Self {
            on_error: ScriptErrorMode::Stop,
            notify: false,
            notify_after_seconds: 30,
            confirm_delay_seconds: 0,
        }
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::new(e.to_string()))?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "wal")?;
        let mut store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        // Re-read the version only after taking the writer lock: another process may
        // have migrated while this connection waited for the lock.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let Ok(current) = usize::try_from(version) else {
            return Err(StoreError::new(format!(
                "the local database has an unknown schema version ({version})"
            )));
        };
        if current > MIGRATIONS.len() {
            return Err(StoreError {
                message: format!(
                    "the local database is from a newer version of SpelaionDB (schema {version})"
                ),
                newer_schema: Some(version),
            });
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            // Which step failed tells a store left half-way by another build (SDB-283).
            let step = |e: rusqlite::Error| {
                StoreError::new(format!(
                    "could not update the local database to schema {}: {e}",
                    i + 1
                ))
            };
            tx.execute_batch(sql).map_err(step)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)
                .map_err(step)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record(&self, entry: &AuditEntry) -> Result<(), StoreError> {
        self.conn.execute(
            "insert into audit (at_ms, client, connection, statement, decision, rows, duration_ms, detail, session, tool)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.at_ms,
                entry.client,
                entry.connection,
                entry.statement,
                entry.decision.as_str(),
                entry.rows,
                entry.duration_ms,
                entry.detail,
                entry.session,
                entry.tool
            ],
        )?;
        Ok(())
    }

    /// Latest entries, from newest to oldest.
    pub fn recent(&self, limit: usize) -> Result<Vec<AuditEntry>, StoreError> {
        Ok(self
            .audit_events(&AuditFilter::default(), limit, 0)?
            .into_iter()
            .map(|r| r.entry)
            .collect())
    }

    /// The entries `filter` keeps, newest first.
    pub fn audit_events(
        &self,
        filter: &AuditFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<AuditRecord>, StoreError> {
        let (clause, mut params) = audit_where(filter);
        params.push(limit.min(i64::MAX as usize).to_string());
        params.push(offset.min(i64::MAX as usize).to_string());
        let sql = format!(
            "select id, at_ms, client, connection, statement, decision, rows, duration_ms, detail, session, tool
             from audit {clause} order by id desc limit cast(?{} as integer) offset cast(?{} as integer)",
            params.len() - 1,
            params.len()
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), audit_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// How many entries `filter` keeps.
    pub fn audit_count(&self, filter: &AuditFilter) -> Result<i64, StoreError> {
        let (clause, params) = audit_where(filter);
        Ok(self.conn.query_row(
            &format!("select count(*) from audit {clause}"),
            rusqlite::params_from_iter(params.iter()),
            |r| r.get(0),
        )?)
    }

    /// The tools seen in the log, for the page's filter.
    pub fn audit_tools(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("select distinct tool from audit where tool is not null order by tool")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The latest `spelaiondb mcp` processes and their calls, newest first. Calls recorded
    /// before sessions were (0.7) belong to none.
    pub fn audit_sessions(&self, limit: usize) -> Result<Vec<AuditSession>, StoreError> {
        let mut stmt = self.conn.prepare(
            "select session, min(client), min(at_ms), max(at_ms), count(*),
                    sum(decision = 'rejected'), sum(decision = 'failed')
             from audit where session is not null
             group by session order by max(id) desc limit ?1",
        )?;
        let rows = stmt.query_map([limit.min(i64::MAX as usize) as i64], |r| {
            Ok(AuditSession {
                session: r.get(0)?,
                client: r.get(1)?,
                started_ms: r.get(2)?,
                ended_ms: r.get(3)?,
                calls: r.get(4)?,
                refused: r.get(5)?,
                failed: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Empties the audit log at the user's request, leaving one record that says when and how
    /// many entries went. The triggers that keep the log append-only are lifted only inside
    /// this transaction, so nothing else can delete or change an entry.
    pub fn clear_audit(&mut self, client: &str) -> Result<i64, StoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed: i64 = tx.query_row("select count(*) from audit", [], |r| r.get(0))?;
        tx.execute_batch(
            "drop trigger audit_no_delete;
             delete from audit;
             create trigger audit_no_delete before delete on audit begin select raise(abort, 'the audit log cannot be deleted'); end;",
        )?;
        let mut record = AuditEntry::now(client, "", "", Decision::Cleared);
        record.detail = Some(format!("{removed} entries cleared"));
        tx.execute(
            "insert into audit (at_ms, client, connection, statement, decision, detail)
             values (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.at_ms,
                record.client,
                record.connection,
                record.statement,
                record.decision.as_str(),
                record.detail
            ],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Saves a query under `name` in `workspace`, replacing one with the same name there.
    /// Returns its identifier.
    pub fn save_snippet(
        &self,
        workspace: &str,
        name: &str,
        statement: &str,
        connection: &str,
    ) -> Result<i64, StoreError> {
        Ok(self.conn.query_row(
            "insert into snippets (workspace, name, statement, connection, updated_ms)
             values (?1, ?2, ?3, ?4, ?5)
             on conflict (workspace, name) do update set
                 statement = excluded.statement,
                 connection = excluded.connection,
                 updated_ms = excluded.updated_ms
             returning id",
            params![workspace, name, statement, connection, now_ms()],
            |r| r.get(0),
        )?)
    }

    /// Saved queries of `workspace` and the personal ones (empty workspace), by name; `search`
    /// keeps those whose name or SQL contains it, ignoring case.
    pub fn snippets(&self, workspace: &str, search: &str) -> Result<Vec<Snippet>, StoreError> {
        let pattern = format!(
            "%{}%",
            search
                .trim()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut statement = self.conn.prepare(
            "select id, workspace, name, statement, connection, updated_ms, folder_id from snippets
             where workspace in (?1, '')
               and (name like ?2 escape '\\' or statement like ?2 escape '\\')
             order by name collate nocase, id",
        )?;
        let rows = statement.query_map(params![workspace, pattern], |r| {
            Ok(Snippet {
                id: r.get(0)?,
                workspace: r.get(1)?,
                name: r.get(2)?,
                statement: r.get(3)?,
                connection: r.get(4)?,
                updated_ms: r.get(5)?,
                folder_id: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Deletes a saved query; `false` if it did not exist.
    pub fn delete_snippet(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("delete from snippets where id = ?1", params![id])?
            > 0)
    }

    /// Inserts only when history is currently enabled for this connection.
    pub fn record_history(&self, entry: &NewHistoryEntry) -> Result<(), StoreError> {
        self.conn.execute(
            "insert into history (at_ms, workspace, connection, statement, duration_ms, rows, error, cancelled)
             select ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
             where coalesce((select enabled from history_preferences
                             where workspace = ?2 and connection = ?3), 1)",
            params![
                entry.at_ms,
                entry.workspace,
                entry.connection,
                entry.statement,
                entry.duration_ms,
                entry.rows,
                entry.error,
                entry.cancelled
            ],
        )?;
        Ok(())
    }

    /// A personal setting: no preference means history is enabled.
    pub fn history_enabled(&self, workspace: &str, connection: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .query_row(
                "select enabled from history_preferences where workspace = ?1 and connection = ?2",
                params![workspace, connection],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(true))
    }

    /// Stops future records without removing earlier history. `record_history` checks this
    /// setting atomically at insertion too, so disabling it also covers in-flight queries.
    pub fn set_history_enabled(
        &self,
        workspace: &str,
        connection: &str,
        enabled: bool,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "insert into history_preferences (workspace, connection, enabled) values (?1, ?2, ?3)
             on conflict (workspace, connection) do update set enabled = excluded.enabled",
            params![workspace, connection, enabled],
        )?;
        Ok(())
    }

    /// A connection moved to another folder: its explorer filter, bookmarks, history setting
    /// and the saved open tabs that use it follow its new identifier. Earlier history, saved
    /// queries and the audit keep the identifier they were recorded with. `workspace` is the
    /// repository's for a repository connection, `None` for a personal one (every workspace
    /// shares it). What an older connection left under the new identifier is replaced, so a
    /// moved connection never takes another's settings (a history it had switched off stays off).
    pub fn move_connection_settings(
        &self,
        workspace: Option<&str>,
        from: &str,
        to: &str,
    ) -> Result<(), StoreError> {
        let transaction = self.conn.unchecked_transaction()?;
        for table in [
            "explorer_filters",
            "explorer_bookmarks",
            "history_preferences",
            "table_filter_history",
            "table_filter_defaults",
        ] {
            transaction.execute(
                &format!(
                    "delete from {table} where connection = ?2 and (?1 is null or workspace = ?1)"
                ),
                params![workspace, to],
            )?;
            transaction.execute(
                &format!(
                    "update {table} set connection = ?3
                     where connection = ?2 and (?1 is null or workspace = ?1)"
                ),
                params![workspace, from, to],
            )?;
        }
        transaction.execute(
            "update open_tabs set connection = ?3
             where connection = ?2 and (?1 is null or workspace = ?1)",
            params![workspace, from, to],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// A connection's explorer filter; empty if it has none.
    pub fn explorer_filter(
        &self,
        workspace: &str,
        connection: &str,
    ) -> Result<ExplorerFilter, StoreError> {
        Ok(self
            .conn
            .query_row(
                "select schema_include, schema_exclude, object_include, object_exclude,
                        column_include, column_exclude
                 from explorer_filters where workspace = ?1 and connection = ?2",
                params![workspace, connection],
                |r| {
                    Ok(ExplorerFilter {
                        schema_include: patterns_of(&r.get::<_, String>(0)?),
                        schema_exclude: patterns_of(&r.get::<_, String>(1)?),
                        object_include: patterns_of(&r.get::<_, String>(2)?),
                        object_exclude: patterns_of(&r.get::<_, String>(3)?),
                        column_include: patterns_of(&r.get::<_, String>(4)?),
                        column_exclude: patterns_of(&r.get::<_, String>(5)?),
                    })
                },
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Replaces a connection's explorer filter (blank patterns dropped); an empty one is
    /// removed.
    pub fn set_explorer_filter(
        &self,
        workspace: &str,
        connection: &str,
        filter: &ExplorerFilter,
    ) -> Result<(), StoreError> {
        let lines = filter
            .lists()
            .map(|list| pattern_lines(list))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        if lines.iter().all(String::is_empty) {
            self.conn.execute(
                "delete from explorer_filters where workspace = ?1 and connection = ?2",
                params![workspace, connection],
            )?;
            return Ok(());
        }
        self.conn.execute(
            "insert into explorer_filters (workspace, connection, schema_include, schema_exclude,
                                           object_include, object_exclude, column_include,
                                           column_exclude)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             on conflict (workspace, connection) do update set
                schema_include = excluded.schema_include, schema_exclude = excluded.schema_exclude,
                object_include = excluded.object_include, object_exclude = excluded.object_exclude,
                column_include = excluded.column_include, column_exclude = excluded.column_exclude",
            params![
                workspace, connection, lines[0], lines[1], lines[2], lines[3], lines[4], lines[5]
            ],
        )?;
        Ok(())
    }

    /// A connection's editor variables (SDB-383), by name.
    pub fn connection_variables(
        &self,
        workspace: &str,
        connection: &str,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self.conn.prepare(
            "select name, value from connection_variables
             where workspace = ?1 and connection = ?2 order by name",
        )?;
        let rows = statement.query_map(params![workspace, connection], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Sets one of a connection's editor variables, or removes it (`None`). A name or a value
    /// that cannot be one is refused and nothing changes.
    pub fn set_connection_variable(
        &self,
        workspace: &str,
        connection: &str,
        name: &str,
        value: Option<&str>,
    ) -> Result<(), StoreError> {
        if !is_variable_name(name) {
            return Err(StoreError::new(
                "a variable's name is a letter or _ followed by letters, digits and _, at most 64"
                    .into(),
            ));
        }
        match value {
            None => {
                self.conn.execute(
                    "delete from connection_variables
                     where workspace = ?1 and connection = ?2 and name = ?3",
                    params![workspace, connection, name],
                )?;
            }
            Some(value) => {
                if !is_variable_value(value) {
                    return Err(StoreError::new(
                        "a variable's value is at most 64 KiB and has no NUL character".into(),
                    ));
                }
                self.conn.execute(
                    "insert into connection_variables (workspace, connection, name, value)
                     values (?1, ?2, ?3, ?4)
                     on conflict (workspace, connection, name) do update set value = excluded.value",
                    params![workspace, connection, name, value],
                )?;
            }
        }
        Ok(())
    }

    /// Personal interface language, shared by every workspace in this user directory.
    /// `system` follows the operating system; `en` and `es` are explicit overrides.
    /// Bookmarks an object; bookmarking it again changes nothing.
    pub fn add_bookmark(&self, workspace: &str, bookmark: &Bookmark) -> Result<(), StoreError> {
        self.conn.execute(
            "insert into explorer_bookmarks (workspace, connection, schema, name, kind, detail,
                                             created_ms)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             on conflict do nothing",
            params![
                workspace,
                bookmark.connection,
                bookmark.schema,
                bookmark.name,
                bookmark.kind,
                bookmark.detail,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// Whether the bookmark was there.
    pub fn remove_bookmark(
        &self,
        workspace: &str,
        bookmark: &Bookmark,
    ) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "delete from explorer_bookmarks
             where workspace = ?1 and connection = ?2 and schema = ?3 and name = ?4
               and kind = ?5 and detail = ?6",
            params![
                workspace,
                bookmark.connection,
                bookmark.schema,
                bookmark.name,
                bookmark.kind,
                bookmark.detail
            ],
        )? > 0)
    }

    /// A workspace's bookmarks, by connection, schema and name.
    pub fn bookmarks(&self, workspace: &str) -> Result<Vec<Bookmark>, StoreError> {
        let mut statement = self.conn.prepare(
            "select connection, schema, name, kind, detail from explorer_bookmarks
             where workspace = ?1 order by connection, schema, name, kind, detail",
        )?;
        let rows = statement.query_map([workspace], |r| {
            Ok(Bookmark {
                connection: r.get(0)?,
                schema: r.get(1)?,
                name: r.get(2)?,
                kind: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn get_language(&self) -> Result<String, StoreError> {
        let language = self
            .conn
            .query_row(
                "select value from personal_settings where key = 'language'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| "system".into());
        validate_language(&language)?;
        Ok(language)
    }

    pub fn set_language(&self, language: &str) -> Result<(), StoreError> {
        validate_language(language)?;
        self.set_personal_setting("language", language)
    }

    /// Personal interface theme, shared by every workspace in this user directory.
    /// `system` follows the operating system; `light` and `dark` are explicit overrides.
    pub fn get_theme(&self) -> Result<String, StoreError> {
        let theme = self
            .conn
            .query_row(
                "select value from personal_settings where key = 'theme'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| "system".into());
        validate_theme(&theme)?;
        Ok(theme)
    }

    pub fn set_theme(&self, theme: &str) -> Result<(), StoreError> {
        validate_theme(theme)?;
        self.set_personal_setting("theme", theme)
    }

    /// The interface size (SDB-389), one of [`INTERFACE_SIZES`], shared by every workspace in
    /// this user directory. A value that is not one of them reads as the default.
    pub fn get_interface_size(&self) -> Result<u16, StoreError> {
        Ok(self
            .personal_setting("interface_size")?
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|size| INTERFACE_SIZES.contains(size))
            .unwrap_or(DEFAULT_INTERFACE_SIZE))
    }

    /// Saves the interface size; anything but one of [`INTERFACE_SIZES`] is refused.
    pub fn set_interface_size(&self, size: u16) -> Result<(), StoreError> {
        if !INTERFACE_SIZES.contains(&size) {
            return Err(StoreError::new(format!(
                "the interface size is one of {INTERFACE_SIZES:?} percent"
            )));
        }
        self.set_personal_setting("interface_size", &size.to_string())
    }

    /// Whether the app checks for updates when it starts (ADR-021). On unless turned off.
    pub fn get_update_check(&self) -> Result<bool, StoreError> {
        let value = self
            .conn
            .query_row(
                "select value from personal_settings where key = 'update_check'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(value.as_deref() != Some("off"))
    }

    pub fn set_update_check(&self, enabled: bool) -> Result<(), StoreError> {
        self.set_personal_setting("update_check", if enabled { "on" } else { "off" })
    }

    /// The pattern the connection form names new connections with (ADR-030); `None` until
    /// the user sets one (the app then uses its default). The core checks it before it is set.
    pub fn get_connection_name_pattern(&self) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "select value from personal_settings where key = 'connection_name_pattern'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Sets the pattern, or with `None` goes back to the app's default.
    pub fn set_connection_name_pattern(&self, pattern: Option<&str>) -> Result<(), StoreError> {
        match pattern {
            Some(pattern) => self.set_personal_setting("connection_name_pattern", pattern),
            None => {
                self.conn.execute(
                    "delete from personal_settings where key = 'connection_name_pattern'",
                    [],
                )?;
                Ok(())
            }
        }
    }

    /// The explorer's preferences, each one at its default until it is changed.
    pub fn get_explorer_preferences(&self) -> Result<ExplorerPreferences, StoreError> {
        let defaults = ExplorerPreferences::default();
        Ok(ExplorerPreferences {
            show_system: self.get_flag("explorer.show_system", defaults.show_system)?,
            sort_alphabetically: self
                .get_flag("explorer.sort_alphabetically", defaults.sort_alphabetically)?,
            sort_ignore_case: self
                .get_flag("explorer.sort_ignore_case", defaults.sort_ignore_case)?,
            folders_first: self.get_flag("explorer.folders_first", defaults.folders_first)?,
            connected_only: self.get_flag("explorer.connected_only", defaults.connected_only)?,
            server_side_filters: self
                .get_flag("explorer.server_side_filters", defaults.server_side_filters)?,
            link_with_editor: self
                .get_flag("explorer.link_with_editor", defaults.link_with_editor)?,
        })
    }

    pub fn set_explorer_preferences(
        &self,
        preferences: &ExplorerPreferences,
    ) -> Result<(), StoreError> {
        let ExplorerPreferences {
            show_system,
            sort_alphabetically,
            sort_ignore_case,
            folders_first,
            connected_only,
            server_side_filters,
            link_with_editor,
        } = *preferences;
        self.set_flag("explorer.show_system", show_system)?;
        self.set_flag("explorer.sort_alphabetically", sort_alphabetically)?;
        self.set_flag("explorer.sort_ignore_case", sort_ignore_case)?;
        self.set_flag("explorer.folders_first", folders_first)?;
        self.set_flag("explorer.connected_only", connected_only)?;
        self.set_flag("explorer.server_side_filters", server_side_filters)?;
        self.set_flag("explorer.link_with_editor", link_with_editor)
    }

    pub fn get_editor_preferences(&self) -> Result<EditorPreferences, StoreError> {
        let defaults = EditorPreferences::default();
        let preferences = EditorPreferences {
            font: self
                .personal_setting("editor.font")?
                .unwrap_or(defaults.font),
            font_size: self
                .personal_setting("editor.font_size")?
                .and_then(|s| s.parse().ok())
                .unwrap_or(defaults.font_size),
            line_height: self
                .personal_setting("editor.line_height")?
                .and_then(|s| s.parse().ok())
                .unwrap_or(defaults.line_height),
            // Read on its own: a zoom that is not a step is 100, and never resets the rest.
            zoom: self
                .personal_setting("editor.zoom")?
                .and_then(|s| s.parse().ok())
                .filter(|zoom| EDITOR_ZOOMS.contains(zoom))
                .unwrap_or(defaults.zoom),
            tab_size: self
                .personal_setting("editor.tab_size")?
                .and_then(|s| s.parse().ok())
                .unwrap_or(defaults.tab_size),
            line_numbers: self.get_flag("editor.line_numbers", defaults.line_numbers)?,
            word_wrap: self.get_flag("editor.word_wrap", defaults.word_wrap)?,
            keyword_case: self
                .personal_setting("editor.keyword_case")?
                .unwrap_or(defaults.keyword_case),
            indent: self
                .personal_setting("editor.indent")?
                .and_then(|s| s.parse().ok())
                .unwrap_or(defaults.indent),
            comma_position: self
                .personal_setting("editor.comma_position")?
                .unwrap_or(defaults.comma_position),
        };
        Ok(if preferences.validate().is_ok() {
            preferences
        } else {
            EditorPreferences::default()
        })
    }
    pub fn set_editor_preferences(
        &self,
        preferences: &EditorPreferences,
    ) -> Result<(), StoreError> {
        preferences.validate()?;
        let transaction = self.conn.unchecked_transaction()?;
        self.set_personal_setting("editor.font", &preferences.font)?;
        self.set_personal_setting("editor.font_size", &preferences.font_size.to_string())?;
        self.set_personal_setting("editor.line_height", &preferences.line_height.to_string())?;
        self.set_personal_setting("editor.zoom", &preferences.zoom.to_string())?;
        self.set_personal_setting("editor.tab_size", &preferences.tab_size.to_string())?;
        self.set_flag("editor.line_numbers", preferences.line_numbers)?;
        self.set_flag("editor.word_wrap", preferences.word_wrap)?;
        self.set_personal_setting("editor.keyword_case", &preferences.keyword_case)?;
        self.set_personal_setting("editor.indent", &preferences.indent.to_string())?;
        self.set_personal_setting("editor.comma_position", &preferences.comma_position)?;
        transaction.commit()?;
        Ok(())
    }

    /// The result grid's display preferences, each one at its default until it is changed.
    pub fn get_grid_preferences(&self) -> Result<GridPreferences, StoreError> {
        let defaults = GridPreferences::default();
        Ok(GridPreferences {
            viewer_wrap: self.get_flag("grid.viewer_wrap", defaults.viewer_wrap)?,
            zebra: self.get_flag("grid.zebra", defaults.zebra)?,
            null_text: self.get_flag("grid.null_text", defaults.null_text)?,
            bool_checkboxes: self.get_flag("grid.bool_checkboxes", defaults.bool_checkboxes)?,
            // A value that is not a number in range reads as the default.
            result_tabs: self
                .personal_setting("results.kept_tabs")?
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|tabs| (1..=MAX_RESULT_TABS).contains(tabs))
                .unwrap_or(defaults.result_tabs),
            colorize: self.get_flag("grid.colorize", defaults.colorize)?,
            header_type: self.get_flag("grid.header_type", defaults.header_type)?,
            header_comment: self.get_flag("grid.header_comment", defaults.header_comment)?,
            font_size: self
                .personal_setting("grid.font_size")?
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|size| (MIN_GRID_FONT..=MAX_GRID_FONT).contains(size))
                .unwrap_or(defaults.font_size),
        })
    }

    /// How the grid shows values, each setting at its default until it is changed.
    pub fn get_display_preferences(&self) -> Result<DisplayPreferences, StoreError> {
        let text = |key: &str| Ok::<_, StoreError>(self.personal_setting(key)?.unwrap_or_default());
        Ok(DisplayPreferences {
            as_stored: self.get_flag("display.as_stored", true)?,
            date: text("display.date")?,
            time: text("display.time")?,
            timestamp: text("display.timestamp")?,
            grouping: text("display.grouping")?,
            decimal: text("display.decimal")?,
            time_zone: text("display.time_zone")?,
            binary: text("display.binary")?,
        })
    }

    /// Saves how the grid shows values, all of it or nothing. Each text is one line of at
    /// most 128 bytes (the core has checked what it means).
    pub fn set_display_preferences(
        &self,
        preferences: &DisplayPreferences,
    ) -> Result<(), StoreError> {
        let texts = [
            ("display.date", &preferences.date),
            ("display.time", &preferences.time),
            ("display.timestamp", &preferences.timestamp),
            ("display.grouping", &preferences.grouping),
            ("display.decimal", &preferences.decimal),
            ("display.time_zone", &preferences.time_zone),
            ("display.binary", &preferences.binary),
        ];
        if texts
            .iter()
            .any(|(_, text)| text.len() > 128 || text.chars().any(char::is_control))
        {
            return Err(StoreError::new("invalid display preferences".into()));
        }
        let transaction = self.conn.unchecked_transaction()?;
        self.set_flag("display.as_stored", preferences.as_stored)?;
        for (key, text) in texts {
            self.set_personal_setting(key, text)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn set_grid_preferences(&self, preferences: &GridPreferences) -> Result<(), StoreError> {
        let GridPreferences {
            viewer_wrap,
            zebra,
            null_text,
            bool_checkboxes,
            result_tabs,
            colorize,
            header_type,
            header_comment,
            font_size,
        } = *preferences;
        if !(1..=MAX_RESULT_TABS).contains(&result_tabs) {
            return Err(StoreError::new(format!(
                "an editor tab keeps from 1 to {MAX_RESULT_TABS} result tabs"
            )));
        }
        if !(MIN_GRID_FONT..=MAX_GRID_FONT).contains(&font_size) {
            return Err(StoreError::new(format!(
                "the grid's font size is from {MIN_GRID_FONT} to {MAX_GRID_FONT} pixels"
            )));
        }
        self.set_flag("grid.viewer_wrap", viewer_wrap)?;
        self.set_flag("grid.zebra", zebra)?;
        self.set_flag("grid.null_text", null_text)?;
        self.set_flag("grid.bool_checkboxes", bool_checkboxes)?;
        self.set_flag("grid.colorize", colorize)?;
        self.set_flag("grid.header_type", header_type)?;
        self.set_flag("grid.header_comment", header_comment)?;
        self.set_personal_setting("grid.font_size", &font_size.to_string())?;
        self.set_personal_setting("results.kept_tabs", &result_tabs.to_string())
    }

    /// The copy defaults (SDB-388), each at its default until it is changed.
    pub fn get_copy_preferences(&self) -> Result<CopyPreferences, StoreError> {
        let defaults = CopyPreferences::default();
        let text = |key: &str, default: String| {
            Ok::<_, StoreError>(self.personal_setting(key)?.unwrap_or(default))
        };
        Ok(CopyPreferences {
            delimiter: text("copy.delimiter", defaults.delimiter)?,
            quote: text("copy.quote", defaults.quote)?,
            row_delimiter: text("copy.row_delimiter", defaults.row_delimiter)?,
            null_text: text("copy.null_text", defaults.null_text)?,
            header: self.get_flag("copy.header", defaults.header)?,
            row_numbers: self.get_flag("copy.row_numbers", defaults.row_numbers)?,
        })
    }

    /// Saves the copy defaults, all of them or nothing: a delimiter of one character, a quote
    /// of at most one, a known row delimiter and a NULL text of one short line (the core has
    /// checked what they mean together).
    pub fn set_copy_preferences(&self, preferences: &CopyPreferences) -> Result<(), StoreError> {
        let one_char = |text: &str| text.chars().count() == 1;
        let valid = one_char(&preferences.delimiter)
            && (preferences.quote.is_empty() || one_char(&preferences.quote))
            && matches!(preferences.row_delimiter.as_str(), "lf" | "crlf")
            && preferences.null_text.len() <= 128
            && !preferences.null_text.chars().any(char::is_control);
        if !valid {
            return Err(StoreError::new("invalid copy preferences".into()));
        }
        let transaction = self.conn.unchecked_transaction()?;
        self.set_personal_setting("copy.delimiter", &preferences.delimiter)?;
        self.set_personal_setting("copy.quote", &preferences.quote)?;
        self.set_personal_setting("copy.row_delimiter", &preferences.row_delimiter)?;
        self.set_personal_setting("copy.null_text", &preferences.null_text)?;
        self.set_flag("copy.header", preferences.header)?;
        self.set_flag("copy.row_numbers", preferences.row_numbers)?;
        transaction.commit()?;
        Ok(())
    }

    /// The user's read-only mode for every agent (ADR-033). Off until turned on; any value
    /// other than `off` counts as on, so a damaged setting never lets agents write.
    pub fn mcp_read_only(&self) -> Result<bool, StoreError> {
        Ok(match self.personal_setting("mcp.read_only")? {
            None => false,
            Some(value) => value != "off",
        })
    }

    pub fn set_mcp_read_only(&self, read_only: bool) -> Result<(), StoreError> {
        self.set_flag("mcp.read_only", read_only)
    }

    /// The default output of the MCP server's query tools (ADR-033): JSON until changed.
    pub fn mcp_output_format(&self) -> Result<ToolOutput, StoreError> {
        Ok(self
            .personal_setting("mcp.output_format")?
            .as_deref()
            .and_then(ToolOutput::parse)
            .unwrap_or_default())
    }

    pub fn set_mcp_output_format(&self, format: ToolOutput) -> Result<(), StoreError> {
        self.set_personal_setting("mcp.output_format", format.as_str())
    }

    /// How scripts run, each preference at its default until it is changed.
    pub fn get_script_preferences(&self) -> Result<ScriptPreferences, StoreError> {
        let defaults = ScriptPreferences::default();
        let on_error = match self.personal_setting("script.on_error")?.as_deref() {
            Some("continue") => ScriptErrorMode::Continue,
            Some("ask") => ScriptErrorMode::Ask,
            Some(_) | None => defaults.on_error,
        };
        let notify_after_seconds = self
            .personal_setting("script.notify_after")?
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|s| (1..=ScriptPreferences::MAX_NOTIFY_AFTER).contains(s))
            .unwrap_or(defaults.notify_after_seconds);
        let confirm_delay_seconds = self
            .personal_setting("script.confirm_delay")?
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|s| *s <= ScriptPreferences::MAX_CONFIRM_DELAY)
            .unwrap_or(defaults.confirm_delay_seconds);
        Ok(ScriptPreferences {
            on_error,
            notify: self.get_flag("script.notify", defaults.notify)?,
            notify_after_seconds,
            confirm_delay_seconds,
        })
    }

    pub fn set_script_preferences(
        &self,
        preferences: &ScriptPreferences,
    ) -> Result<(), StoreError> {
        let ScriptPreferences {
            on_error,
            notify,
            notify_after_seconds,
            confirm_delay_seconds,
        } = *preferences;
        if !(1..=ScriptPreferences::MAX_NOTIFY_AFTER).contains(&notify_after_seconds) {
            return Err(StoreError::new(format!(
                "the notification threshold is in seconds, from 1 to {}",
                ScriptPreferences::MAX_NOTIFY_AFTER
            )));
        }
        if confirm_delay_seconds > ScriptPreferences::MAX_CONFIRM_DELAY {
            return Err(StoreError::new(format!(
                "the pause before \"Run anyway\" is in seconds, from 0 to {}",
                ScriptPreferences::MAX_CONFIRM_DELAY
            )));
        }
        self.set_personal_setting("script.on_error", on_error.as_str())?;
        self.set_personal_setting("script.confirm_delay", &confirm_delay_seconds.to_string())?;
        self.set_flag("script.notify", notify)?;
        self.set_personal_setting("script.notify_after", &notify_after_seconds.to_string())
    }

    fn personal_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "select value from personal_settings where key = ?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    fn get_flag(&self, key: &str, default: bool) -> Result<bool, StoreError> {
        let value = self
            .conn
            .query_row(
                "select value from personal_settings where key = ?1",
                [key],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(match value.as_deref() {
            Some("on") => true,
            Some("off") => false,
            _ => default,
        })
    }

    fn set_flag(&self, key: &str, value: bool) -> Result<(), StoreError> {
        self.set_personal_setting(key, if value { "on" } else { "off" })
    }

    fn set_personal_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "insert into personal_settings (key, value) values (?1, ?2)
             on conflict (key) do update set value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Newest executions for this workspace and personal connections first, with a maximum
    /// of 500 entries per request. Other workspaces' project connections stay private.
    /// Search is a literal FTS5 phrase (with a prefix on its last token), never FTS syntax.
    pub fn history(
        &self,
        workspace: &str,
        search: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<HistoryEntry>, StoreError> {
        let search = search.trim();
        let limit = limit.min(500) as i64;
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        let read = |r: &rusqlite::Row<'_>| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                at_ms: r.get(1)?,
                workspace: r.get(2)?,
                connection: r.get(3)?,
                statement: r.get(4)?,
                duration_ms: r.get(5)?,
                rows: r.get(6)?,
                error: r.get(7)?,
                cancelled: r.get(8)?,
                database: r.get(9)?,
                pinned: r.get(10)?,
            })
        };
        if search.is_empty() {
            let mut stmt = self.conn.prepare(
                "select id, at_ms, workspace, connection, statement, duration_ms, rows, error, cancelled, database, pinned
                 from history where (workspace = ?1 or workspace = '')
                 order by at_ms desc, id desc limit ?2 offset ?3",
            )?;
            let rows = stmt.query_map(params![workspace, limit, offset], read)?;
            Ok(rows.collect::<Result<_, _>>()?)
        } else {
            let phrase = format!("\"{}\"*", search.replace('"', "\"\""));
            let mut stmt = self.conn.prepare(
                "select h.id, h.at_ms, h.workspace, h.connection, h.statement,
                        h.duration_ms, h.rows, h.error, h.cancelled, h.database, h.pinned
                 from history h join history_fts on history_fts.rowid = h.id
                 where (h.workspace = ?1 or h.workspace = '') and history_fts match ?2
                 order by h.at_ms desc, h.id desc limit ?3 offset ?4",
            )?;
            let rows = stmt.query_map(params![workspace, phrase, limit, offset], read)?;
            Ok(rows.collect::<Result<_, _>>()?)
        }
    }
}

fn validate_language(language: &str) -> Result<(), StoreError> {
    if matches!(language, "system" | "en" | "es") {
        Ok(())
    } else {
        Err(StoreError::new(
            "unsupported language; expected system, en or es".into(),
        ))
    }
}

fn validate_theme(theme: &str) -> Result<(), StoreError> {
    if matches!(theme, "system" | "light" | "dark") {
        Ok(())
    } else {
        Err(StoreError::new(
            "unsupported theme; expected system, light or dark".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn history_entry(
        workspace: &str,
        connection: &str,
        statement: &str,
        at_ms: i64,
    ) -> NewHistoryEntry {
        NewHistoryEntry {
            at_ms,
            workspace: workspace.into(),
            connection: connection.into(),
            statement: statement.into(),
            duration_ms: 23,
            rows: 3,
            error: None,
            cancelled: false,
        }
    }

    pub(super) fn temp(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("spelaion-store-{name}-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        path
    }

    #[test]
    fn snippets_are_saved_by_name_searched_and_deleted() {
        let path = temp("snippets");
        let store = Store::open(&path).unwrap();
        let first = store
            .save_snippet("/repo", "Top customers", "select * from customer", "shop")
            .unwrap();
        store
            .save_snippet("", "Personal 100%_done", "select 1", "")
            .unwrap();
        store
            .save_snippet("/other", "Elsewhere", "select 2", "")
            .unwrap();
        // Same name in the same workspace: replaced, same identifier.
        let again = store
            .save_snippet(
                "/repo",
                "Top customers",
                "select * from customer limit 5",
                "shop",
            )
            .unwrap();
        assert_eq!(again, first);
        let names = |search: &str| -> Vec<String> {
            store
                .snippets("/repo", search)
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect()
        };
        assert_eq!(names(""), ["Personal 100%_done", "Top customers"]);
        assert_eq!(names("LIMIT 5"), ["Top customers"]);
        assert_eq!(names("100%_"), ["Personal 100%_done"]);
        assert!(names("%").len() == 1, "% is literal");
        assert!(store.save_snippet("/repo", "  ", "select 3", "").is_err());
        assert!(store.delete_snippet(first).unwrap());
        assert!(!store.delete_snippet(first).unwrap());
        assert_eq!(names(""), ["Personal 100%_done"]);
    }

    #[test]
    fn records_and_reads_back_audit_entries() {
        let path = temp("audit");
        let store = Store::open(&path).unwrap();
        let mut entry =
            AuditEntry::now("claude-code", "prod/orders", "select 1", Decision::Executed);
        entry.rows = Some(1);
        store.record(&entry).unwrap();
        store
            .record(&AuditEntry::now(
                "claude-code",
                "prod/orders",
                "delete from t",
                Decision::Rejected,
            ))
            .unwrap();
        let recent = store.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].decision, Decision::Rejected);
        assert_eq!(recent[1], entry);

        // Reopening neither repeats migrations nor loses data.
        drop(store);
        assert_eq!(Store::open(&path).unwrap().recent(10).unwrap().len(), 2);
    }

    #[test]
    fn audit_is_append_only() {
        let path = temp("append-only");
        let store = Store::open(&path).unwrap();
        store
            .record(&AuditEntry::now("x", "c", "select 1", Decision::Executed))
            .unwrap();
        assert!(
            store
                .conn
                .execute("update audit set statement = 'nothing'", [])
                .is_err()
        );
        assert!(store.conn.execute("delete from audit", []).is_err());
        assert_eq!(store.recent(10).unwrap()[0].statement, "select 1");
    }

    #[test]
    fn history_migration_preserves_existing_audit() {
        let path = temp("history-upgrade");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "insert into audit (at_ms, client, connection, statement, decision)
             values (1, 'agent', 'shop', 'select 1', 'executed')",
            [],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.recent(10).unwrap()[0].statement, "select 1");
        store
            .record_history(&history_entry("/repo", "shop", "select 2", 2))
            .unwrap();
        assert_eq!(store.history("/repo", "select", 10, 0).unwrap().len(), 1);
        // The original audit protections survive the upgrade.
        assert!(store.conn.execute("delete from audit", []).is_err());
    }

    #[test]
    fn history_roundtrip_and_pagination_are_scoped_to_workspace() {
        let path = temp("history-roundtrip");
        let store = Store::open(&path).unwrap();
        store
            .record_history(&history_entry("/repo", "shop", "select 1", 100))
            .unwrap();
        let mut failed = history_entry("/repo", "shop", "select missing", 200);
        failed.error = Some("no such column: missing".into());
        failed.rows = 0;
        store.record_history(&failed).unwrap();
        store
            .record_history(&history_entry("/other-repo", "shop", "select 9", 300))
            .unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        let rows = store.history("/repo", "", 10, 0).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].statement, failed.statement);
        assert_eq!(rows[0].error, failed.error);
        assert_eq!(rows[0].duration_ms, 23);
        assert_eq!(rows[0].rows, 0);
        assert_eq!(rows[1].rows, 3);
        assert!(rows[0].id > rows[1].id);
        assert_eq!(store.history("/repo", "", 1, 1).unwrap(), rows[1..]);
        assert!(store.history("/repo", "", 0, 0).unwrap().is_empty());
        assert!(
            store
                .history("/repo", "", 10, usize::MAX)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.history("/other-repo", "", 10, 0).unwrap().len(), 1);
    }

    #[test]
    fn history_search_treats_sql_and_fts_punctuation_as_literal_text() {
        let path = temp("history-search");
        let store = Store::open(&path).unwrap();
        store
            .record_history(&history_entry(
                "/repo",
                "prod/orders",
                "SELECT * FROM \"order-items\" WHERE status = 'active'; -- comment",
                100,
            ))
            .unwrap();
        store
            .record_history(&history_entry(
                "/repo",
                "shop",
                "select count(*) from products",
                200,
            ))
            .unwrap();
        store
            .record_history(&history_entry(
                "/other",
                "prod/orders",
                "select * from order_items",
                300,
            ))
            .unwrap();
        for search in [
            "order-items",
            "\"order-items\"",
            "SELECT * FROM",
            "stat",
            "prod/orders",
        ] {
            let found = store.history("/repo", search, 10, 0).unwrap();
            assert_eq!(found.len(), 1, "search: {search}");
            assert!(found[0].statement.contains("'active'"));
        }
        for search in [
            "*",
            "\"",
            "()",
            "' OR 1=1 --",
            "statement:select",
            "active OR products",
        ] {
            assert!(
                store.history("/repo", search, 10, 0).unwrap().is_empty(),
                "search: {search}"
            );
        }
        assert_eq!(store.history("/repo", "select", 1, 1).unwrap().len(), 1);
        assert_eq!(store.history("/repo", "   ", 10, 0).unwrap().len(), 2);
    }

    #[test]
    fn personal_history_is_visible_from_all_workspaces_without_exposing_other_projects() {
        let path = temp("history-personal");
        let store = Store::open(&path).unwrap();
        for (workspace, statement, at_ms) in [
            ("", "select personal", 100),
            ("/repo", "select project_a", 200),
            ("/other", "select project_b", 300),
        ] {
            store
                .record_history(&history_entry(workspace, "shop", statement, at_ms))
                .unwrap();
        }
        for search in ["", "select"] {
            let first = store.history("/repo", search, 10, 0).unwrap();
            assert_eq!(first.len(), 2);
            assert_eq!(first[0].statement, "select project_a");
            assert_eq!(first[1].statement, "select personal");
            let other = store.history("/other", search, 10, 0).unwrap();
            assert_eq!(other.len(), 2);
            assert_eq!(other[0].statement, "select project_b");
            assert_eq!(other[1].statement, "select personal");
            let personal = store.history("", search, 10, 0).unwrap();
            assert_eq!(personal.len(), 1);
            assert_eq!(personal[0].statement, "select personal");
        }
    }

    #[test]
    fn disabled_history_skips_sql_and_errors_and_persists_personally() {
        let path = temp("history-preferences");
        let store = Store::open(&path).unwrap();
        assert!(store.history_enabled("/repo", "shop").unwrap());
        store
            .record_history(&history_entry("/repo", "shop", "select 1", 100))
            .unwrap();
        store.set_history_enabled("/repo", "shop", false).unwrap();
        let mut secret = history_entry(
            "/repo",
            "shop",
            "CREATE USER alice PASSWORD 'private_value'",
            200,
        );
        secret.error = Some("Error near private_value".into());
        store.record_history(&secret).unwrap();
        store
            .record_history(&history_entry("/repo", "shop", "select private_value", 250))
            .unwrap();
        assert_eq!(store.history("/repo", "", 10, 0).unwrap().len(), 1);
        assert!(
            store
                .history("/repo", "private_value", 10, 0)
                .unwrap()
                .is_empty()
        );
        let indexed: i64 = store
            .conn
            .query_row(
                "select count(*) from history_fts where history_fts match 'private'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 0);
        drop(store);

        let store = Store::open(&path).unwrap();
        assert!(!store.history_enabled("/repo", "shop").unwrap());
        assert!(store.history_enabled("/other", "shop").unwrap());
        assert!(store.history_enabled("/repo", "another").unwrap());
        store.record_history(&secret).unwrap();
        assert_eq!(store.history("/repo", "", 10, 0).unwrap().len(), 1);
        store
            .record_history(&history_entry("/other", "shop", "select 2", 300))
            .unwrap();
        assert_eq!(store.history("/other", "", 10, 0).unwrap().len(), 1);
        store.set_history_enabled("/repo", "shop", true).unwrap();
        store
            .record_history(&history_entry("/repo", "shop", "select 3", 400))
            .unwrap();
        assert_eq!(store.history("/repo", "", 10, 0).unwrap().len(), 2);
    }

    #[test]
    fn language_migration_preserves_history_and_personal_preferences() {
        let path = temp("language-upgrade");
        let conn = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..2] {
            conn.execute_batch(migration).unwrap();
        }
        conn.pragma_update(None, "user_version", 2).unwrap();
        conn.execute(
            "insert into history (at_ms, workspace, connection, statement, duration_ms, rows)
             values (1, '/repo', 'shop', 'select 42', 3, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into history_preferences values ('/repo', 'shop', 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_language().unwrap(), "system");
        store.set_language("es").unwrap();
        let history = store.history("/repo", "42", 10, 0).unwrap();
        assert_eq!(history.len(), 1);
        assert!(!history[0].cancelled);
        assert!(!store.history_enabled("/repo", "shop").unwrap());
        drop(store);
        assert_eq!(Store::open(&path).unwrap().get_language().unwrap(), "es");
    }

    #[test]
    fn cancellation_flag_is_independent_of_verbatim_database_errors() {
        let path = temp("history-cancellation");
        let store = Store::open(&path).unwrap();
        let mut database_error = history_entry("/repo", "shop", "select broken", 100);
        database_error.error = Some("Query cancelled".into());
        store.record_history(&database_error).unwrap();
        let mut cancelled = history_entry("/repo", "shop", "select many_rows", 200);
        cancelled.error = Some("canceling statement due to user request".into());
        cancelled.cancelled = true;
        store.record_history(&cancelled).unwrap();
        drop(store);

        let store = Store::open(&path).unwrap();
        for search in ["", "select"] {
            let history = store.history("/repo", search, 10, 0).unwrap();
            assert_eq!(history.len(), 2);
            assert!(history[0].cancelled);
            assert_eq!(history[0].error, cancelled.error);
            assert_eq!(history[0].rows, 3);
            assert!(!history[1].cancelled);
            assert_eq!(history[1].error, database_error.error);
        }
    }

    #[test]
    fn simultaneous_migrations_recheck_version_after_acquiring_writer_lock() {
        use std::cell::RefCell;
        use std::sync::mpsc;
        use std::time::Duration;

        // A per-thread busy hook lets the test wait for both writers to be blocked,
        // rather than relying on scheduling or sleeps to reproduce the race.
        thread_local! {
            static WAITING: RefCell<Option<mpsc::Sender<()>>> = const { RefCell::new(None) };
        }

        let path = temp("language-concurrent-upgrade");
        let mut blocker = Connection::open(&path).unwrap();
        blocker.pragma_update(None, "journal_mode", "wal").unwrap();
        for migration in &MIGRATIONS[..2] {
            blocker.execute_batch(migration).unwrap();
        }
        blocker.pragma_update(None, "user_version", 2).unwrap();
        let connections = [
            Connection::open(&path).unwrap(),
            Connection::open(&path).unwrap(),
        ];
        for conn in &connections {
            assert_eq!(
                conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                    .unwrap(),
                2
            );
        }
        let lock = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let (waiting, ready) = mpsc::channel();
        let workers = connections.map(|mut conn| {
            let waiting = waiting.clone();
            std::thread::spawn(move || {
                WAITING.with(|slot| slot.replace(Some(waiting)));
                conn.busy_handler(Some(|_| {
                    WAITING.with(|slot| {
                        if let Some(waiting) = slot.borrow_mut().take() {
                            let _ = waiting.send(());
                        }
                    });
                    std::thread::sleep(Duration::from_millis(1));
                    true
                }))
                .unwrap();
                Store { conn }.migrate()
            })
        });
        for _ in 0..2 {
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        lock.commit().unwrap();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_language().unwrap(), "system");
        assert!(store.history("/repo", "", 10, 0).unwrap().is_empty());
        assert_eq!(
            store
                .conn
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            MIGRATIONS.len() as i64
        );
    }

    /// The store 0.8.0 met on 2026-11-13 (SDB-283): schema 10 by its version, but its history
    /// table without the `cancelled` column migration 3 adds, left by a development build.
    #[test]
    fn a_store_left_half_way_by_another_build_is_an_error_that_names_the_step() {
        let path = temp("half-way");
        {
            let conn = Connection::open(&path).unwrap();
            for (i, migration) in MIGRATIONS[..10].iter().enumerate() {
                let sql = if i == 2 {
                    migration.replace(
                        "alter table history add column cancelled integer not null default 0 check (cancelled in (0, 1));",
                        "",
                    )
                } else {
                    (*migration).to_string()
                };
                conn.execute_batch(&sql).unwrap();
            }
            conn.pragma_update(None, "user_version", 10).unwrap();
        }
        let error = Store::open(&path)
            .err()
            .expect("the broken store must not open");
        assert_eq!(error.newer_schema(), None);
        let message = error.to_string();
        assert!(
            message.contains("schema 11") && message.contains("cancelled"),
            "{message}"
        );
        // Nothing was half-applied: the version is still the one the file had.
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 10);
    }

    #[test]
    fn a_store_from_a_newer_version_is_an_error_that_says_so_and_is_left_as_it_is() {
        let path = temp("newer");
        drop(Store::open(&path).unwrap());
        let newer = MIGRATIONS.len() as i64 + 5;
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", newer)
            .unwrap();
        let error = Store::open(&path)
            .err()
            .expect("a newer store must not open");
        assert_eq!(error.newer_schema(), Some(newer));
        assert!(error.to_string().contains("newer version"), "{error}");
        let version: i64 = Connection::open(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, newer);
    }

    #[test]
    fn a_negative_schema_version_is_an_error_not_a_newer_store() {
        let path = temp("negative");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", -1)
            .unwrap();
        let error = Store::open(&path).err().expect("must not open");
        assert_eq!(error.newer_schema(), None);
        assert!(error.to_string().contains("unknown schema"), "{error}");
    }

    #[test]
    fn language_is_validated_persistent_and_isolated_by_user_directory() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let user_dirs = std::env::temp_dir().join(format!(
            "spelaion-store-language-users-{}-{nonce}",
            std::process::id()
        ));
        let first_path = user_dirs.join("first-user/state.db");
        let second_path = user_dirs.join("second-user/state.db");
        let first = Store::open(&first_path).unwrap();
        let second = Store::open(&second_path).unwrap();
        assert_eq!(first.get_language().unwrap(), "system");
        first.set_language("es").unwrap();
        assert_eq!(second.get_language().unwrap(), "system");
        second.set_language("en").unwrap();
        for invalid in ["", "fr", "ES", "es-ES", " system", "en' OR 1=1 --"] {
            assert!(first.set_language(invalid).is_err(), "{invalid}");
            assert_eq!(first.get_language().unwrap(), "es");
        }
        assert!(
            first
                .conn
                .execute(
                    "update personal_settings set value = 'invalid' where key = 'language'",
                    []
                )
                .is_err()
        );
        first.set_language("system").unwrap();
        drop(first);
        drop(second);
        assert_eq!(
            Store::open(&first_path).unwrap().get_language().unwrap(),
            "system"
        );
        assert_eq!(
            Store::open(&second_path).unwrap().get_language().unwrap(),
            "en"
        );
    }

    #[test]
    fn explorer_filters_are_per_workspace_and_connection() {
        let path = temp("explorer-filters");
        let store = Store::open(&path).unwrap();
        assert!(store.explorer_filter("/repo", "shop").unwrap().is_empty());
        let filter = ExplorerFilter {
            schema_include: vec![" tenant_* ".into(), "".into(), "public".into()],
            object_exclude: vec!["*_old".into()],
            column_exclude: vec!["password".into()],
            ..ExplorerFilter::default()
        };
        store.set_explorer_filter("/repo", "shop", &filter).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let saved = store.explorer_filter("/repo", "shop").unwrap();
        // Trimmed, blank patterns dropped.
        assert_eq!(saved.schema_include, ["tenant_*", "public"]);
        assert_eq!(saved.object_exclude, ["*_old"]);
        assert_eq!(saved.column_exclude, ["password"]);
        assert!(saved.schema_exclude.is_empty() && saved.object_include.is_empty());
        // Another workspace's connection with the same id has its own.
        assert!(store.explorer_filter("/other", "shop").unwrap().is_empty());
        assert!(store.explorer_filter("/repo", "other").unwrap().is_empty());
        // A line break would split one pattern into two: refused.
        let broken = ExplorerFilter {
            object_include: vec!["a\nb".into()],
            ..ExplorerFilter::default()
        };
        assert!(store.set_explorer_filter("/repo", "shop", &broken).is_err());
        assert_eq!(store.explorer_filter("/repo", "shop").unwrap(), saved);
        // Clearing it removes the row.
        store
            .set_explorer_filter("/repo", "shop", &ExplorerFilter::default())
            .unwrap();
        assert!(store.explorer_filter("/repo", "shop").unwrap().is_empty());
    }

    #[test]
    fn hide_and_show_only_edit_one_kind() {
        let mut filter = ExplorerFilter {
            object_include: vec!["a*".into(), "b*".into()],
            object_exclude: vec!["orders".into()],
            ..ExplorerFilter::default()
        };
        filter.hide(FilterKind::Objects, "audit");
        filter.hide(FilterKind::Objects, "audit");
        assert_eq!(filter.object_exclude, ["orders", "audit"]);
        filter.show_only(FilterKind::Objects, &["orders".into()]);
        assert_eq!(filter.object_include, ["orders"]);
        assert_eq!(filter.object_exclude, ["audit"]);
        filter.hide(FilterKind::Columns, "password");
        filter.show_only(FilterKind::Schemas, &["public".into(), "audit".into()]);
        assert_eq!(filter.column_exclude, ["password"]);
        assert_eq!(filter.schema_include, ["public", "audit"]);
        assert!(filter.schema_exclude.is_empty() && filter.column_include.is_empty());
    }

    #[test]
    fn explorer_preferences_start_at_their_defaults_and_persist() {
        let path = temp("explorer");
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_explorer_preferences().unwrap(),
            ExplorerPreferences::default()
        );
        let defaults = ExplorerPreferences::default();
        assert!(
            defaults.sort_alphabetically
                && defaults.sort_ignore_case
                && defaults.server_side_filters
        );
        assert!(
            !defaults.show_system
                && !defaults.folders_first
                && !defaults.connected_only
                && !defaults.link_with_editor
        );
        // Every field is saved on its own: each one flipped from its default.
        let changed = ExplorerPreferences {
            show_system: true,
            sort_alphabetically: false,
            sort_ignore_case: false,
            folders_first: true,
            connected_only: true,
            server_side_filters: false,
            link_with_editor: true,
        };
        store.set_explorer_preferences(&changed).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_explorer_preferences().unwrap(), changed);
        // Another setting does not touch them.
        reopened.set_theme("light").unwrap();
        assert_eq!(reopened.get_explorer_preferences().unwrap(), changed);
    }

    #[test]
    fn editor_preferences_persist_and_reject_invalid_values_without_partial_writes() {
        let path = std::env::temp_dir().join(format!("spelaion-editor-{}.db", std::process::id()));
        let store = Store::open(&path).unwrap();
        let defaults = EditorPreferences::default();
        assert_eq!(store.get_editor_preferences().unwrap(), defaults);
        let changed = EditorPreferences {
            font: "Consolas".into(),
            font_size: 16,
            line_height: 24,
            zoom: 125,
            keyword_case: "upper".into(),
            word_wrap: true,
            ..defaults.clone()
        };
        store.set_editor_preferences(&changed).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_editor_preferences().unwrap(), changed);
        for invalid in [
            EditorPreferences {
                tab_size: 0,
                ..defaults.clone()
            },
            EditorPreferences {
                font: "url(evil)".into(),
                ..defaults.clone()
            },
            EditorPreferences {
                line_height: 1,
                ..defaults.clone()
            },
            EditorPreferences {
                comma_position: "unknown".into(),
                ..defaults.clone()
            },
            // Only the zoom steps: never a size of its own, nor nothing.
            EditorPreferences {
                zoom: 0,
                ..defaults.clone()
            },
            EditorPreferences {
                zoom: 133,
                ..defaults.clone()
            },
            EditorPreferences {
                zoom: 1000,
                ..defaults
            },
        ] {
            assert!(store.set_editor_preferences(&invalid).is_err());
            assert_eq!(store.get_editor_preferences().unwrap(), changed);
        }
        // A damaged zoom reads as 100 and leaves the other preferences as they were.
        store.set_personal_setting("editor.zoom", "9999").unwrap();
        assert_eq!(
            store.get_editor_preferences().unwrap(),
            EditorPreferences {
                zoom: DEFAULT_EDITOR_ZOOM,
                ..changed
            }
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn interface_size_is_one_of_its_steps_and_persists() {
        let path = temp("interface-size");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_interface_size().unwrap(), DEFAULT_INTERFACE_SIZE);
        store.set_interface_size(150).unwrap();
        for refused in [0, 1, 99, 101, 151, 200, u16::MAX] {
            assert!(store.set_interface_size(refused).is_err(), "{refused}");
        }
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_interface_size().unwrap(), 150);
        // What is not a step (a damaged file) reads as the default.
        for damaged in ["abc", "", "-1", "1000", "99"] {
            store
                .set_personal_setting("interface_size", damaged)
                .unwrap();
            assert_eq!(
                store.get_interface_size().unwrap(),
                DEFAULT_INTERFACE_SIZE,
                "{damaged}"
            );
        }
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn connection_variables_are_kept_per_workspace_and_connection() {
        let path = temp("variables");
        let store = Store::open(&path).unwrap();
        assert!(store.connection_variables("w", "shop").unwrap().is_empty());
        store
            .set_connection_variable("w", "shop", "customer", Some("42"))
            .unwrap();
        store
            .set_connection_variable("w", "shop", "_cols", Some("id, name\n-- with a comment"))
            .unwrap();
        store
            .set_connection_variable("w", "shop", "customer", Some("43"))
            .unwrap();
        store
            .set_connection_variable("w", "other", "customer", Some("1"))
            .unwrap();
        store
            .set_connection_variable("elsewhere", "shop", "customer", Some("2"))
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.connection_variables("w", "shop").unwrap(),
            [
                (
                    "_cols".to_string(),
                    "id, name\n-- with a comment".to_string()
                ),
                ("customer".to_string(), "43".to_string()),
            ]
        );
        assert_eq!(store.connection_variables("w", "other").unwrap().len(), 1);
        // Removing one leaves the others, and removing it again changes nothing.
        store
            .set_connection_variable("w", "shop", "customer", None)
            .unwrap();
        store
            .set_connection_variable("w", "shop", "customer", None)
            .unwrap();
        assert_eq!(store.connection_variables("w", "shop").unwrap().len(), 1);
        assert_eq!(
            store.connection_variables("elsewhere", "shop").unwrap(),
            [("customer".to_string(), "2".to_string())]
        );
    }

    #[test]
    fn a_variable_that_cannot_be_one_is_refused_and_nothing_is_saved() {
        let store = Store::open(&temp("variables-refused")).unwrap();
        for name in [
            "",
            "1x",
            "a b",
            "a-b",
            "env:HOME",
            "user.name",
            &"a".repeat(65),
        ] {
            assert!(
                store
                    .set_connection_variable("w", "shop", name, Some("1"))
                    .is_err(),
                "{name}"
            );
            assert!(!is_variable_name(name), "{name}");
        }
        for value in ["a\0b".to_string(), "x".repeat(MAX_VARIABLE_VALUE + 1)] {
            assert!(
                store
                    .set_connection_variable("w", "shop", "v", Some(&value))
                    .is_err()
            );
        }
        assert!(store.connection_variables("w", "shop").unwrap().is_empty());
        // The longest name and value are accepted.
        store
            .set_connection_variable(
                "w",
                "shop",
                &"a".repeat(MAX_VARIABLE_NAME),
                Some(&"x".repeat(MAX_VARIABLE_VALUE)),
            )
            .unwrap();
        assert_eq!(store.connection_variables("w", "shop").unwrap().len(), 1);
    }

    #[test]
    fn display_preferences_start_as_the_database_prints_and_persist() {
        let path = temp("display");
        let store = Store::open(&path).unwrap();
        let defaults = DisplayPreferences::default();
        assert!(defaults.as_stored);
        assert_eq!(store.get_display_preferences().unwrap(), defaults);
        let changed = DisplayPreferences {
            as_stored: false,
            date: "%d/%m/%Y".into(),
            time: "%H:%M".into(),
            timestamp: "%d/%m/%Y %H:%M:%S".into(),
            grouping: "dot".into(),
            decimal: "comma".into(),
            time_zone: "Europe/Madrid".into(),
            binary: "base64".into(),
        };
        store.set_display_preferences(&changed).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_display_preferences().unwrap(), changed);
        // A text that is not one short line is refused, and nothing changes.
        for bad in ["%Y\n%m".to_string(), "x".repeat(129)] {
            let refused = DisplayPreferences {
                date: bad,
                as_stored: true,
                ..changed.clone()
            };
            assert!(reopened.set_display_preferences(&refused).is_err());
        }
        assert_eq!(reopened.get_display_preferences().unwrap(), changed);
        // The grid's other preferences are kept apart.
        assert_eq!(
            reopened.get_grid_preferences().unwrap(),
            GridPreferences::default()
        );
    }

    #[test]
    fn copy_preferences_start_at_their_defaults_and_persist() {
        let path = temp("copy");
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_copy_preferences().unwrap(),
            CopyPreferences::default()
        );
        // Every field is saved on its own: each one changed from its default.
        let changed = CopyPreferences {
            delimiter: ";".into(),
            quote: String::new(),
            row_delimiter: "crlf".into(),
            null_text: "NULL".into(),
            header: false,
            row_numbers: true,
        };
        store.set_copy_preferences(&changed).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_copy_preferences().unwrap(), changed);
        // What could not be read back is refused, and nothing of it is saved.
        for refused in [
            CopyPreferences {
                delimiter: String::new(),
                ..changed.clone()
            },
            CopyPreferences {
                delimiter: ";;".into(),
                ..changed.clone()
            },
            CopyPreferences {
                quote: "''".into(),
                ..changed.clone()
            },
            CopyPreferences {
                row_delimiter: "cr".into(),
                ..changed.clone()
            },
            CopyPreferences {
                null_text: "a\nb".into(),
                ..changed.clone()
            },
            CopyPreferences {
                null_text: "x".repeat(129),
                header: true,
                ..changed.clone()
            },
        ] {
            assert!(
                reopened.set_copy_preferences(&refused).is_err(),
                "{refused:?}"
            );
        }
        assert_eq!(reopened.get_copy_preferences().unwrap(), changed);
        // The grid's preferences are kept apart.
        assert_eq!(
            reopened.get_grid_preferences().unwrap(),
            GridPreferences::default()
        );
    }

    #[test]
    fn grid_preferences_start_at_their_defaults_and_persist() {
        let path = temp("grid");
        let store = Store::open(&path).unwrap();
        let defaults = GridPreferences::default();
        assert_eq!(store.get_grid_preferences().unwrap(), defaults);
        assert!(defaults.zebra && defaults.null_text && !defaults.bool_checkboxes);
        assert_eq!(defaults.result_tabs, 20);
        // Every field is saved on its own: each one changed from its default.
        let changed = GridPreferences {
            viewer_wrap: false,
            zebra: false,
            null_text: false,
            bool_checkboxes: true,
            result_tabs: 5,
            colorize: true,
            header_type: false,
            header_comment: true,
            font_size: 15,
        };
        for font_size in [MIN_GRID_FONT - 1, MAX_GRID_FONT + 1] {
            let out_of_range = GridPreferences {
                font_size,
                ..changed
            };
            assert!(store.set_grid_preferences(&out_of_range).is_err());
        }
        assert!(!defaults.colorize && defaults.header_type && !defaults.header_comment);
        assert_eq!(defaults.font_size, 12);
        // A number of result tabs out of range is refused, and nothing is saved.
        for result_tabs in [0, MAX_RESULT_TABS + 1] {
            let out_of_range = GridPreferences {
                result_tabs,
                ..changed
            };
            assert!(store.set_grid_preferences(&out_of_range).is_err());
        }
        assert_eq!(store.get_grid_preferences().unwrap(), defaults);
        store.set_grid_preferences(&changed).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_grid_preferences().unwrap(), changed);
        // The explorer's preferences are kept apart.
        assert_eq!(
            reopened.get_explorer_preferences().unwrap(),
            ExplorerPreferences::default()
        );
        let one = GridPreferences {
            null_text: true,
            ..changed
        };
        reopened.set_grid_preferences(&one).unwrap();
        assert_eq!(reopened.get_grid_preferences().unwrap(), one);
    }

    #[test]
    fn script_preferences_start_at_their_defaults_and_persist() {
        let path = temp("script");
        let store = Store::open(&path).unwrap();
        let defaults = ScriptPreferences::default();
        assert_eq!(store.get_script_preferences().unwrap(), defaults);
        assert_eq!(defaults.on_error, ScriptErrorMode::Stop);
        assert!(!defaults.notify, "notifications are opt-in");
        for on_error in [
            ScriptErrorMode::Continue,
            ScriptErrorMode::Ask,
            ScriptErrorMode::Stop,
        ] {
            let changed = ScriptPreferences {
                on_error,
                notify: true,
                notify_after_seconds: 120,
                confirm_delay_seconds: 3,
            };
            store.set_script_preferences(&changed).unwrap();
            assert_eq!(store.get_script_preferences().unwrap(), changed);
        }
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened
                .get_script_preferences()
                .unwrap()
                .notify_after_seconds,
            120
        );
        // A threshold out of range never reaches the store.
        for bad in [0, ScriptPreferences::MAX_NOTIFY_AFTER + 1] {
            let wrong = ScriptPreferences {
                notify_after_seconds: bad,
                ..defaults
            };
            assert!(reopened.set_script_preferences(&wrong).is_err());
        }
        assert_eq!(
            reopened
                .get_script_preferences()
                .unwrap()
                .notify_after_seconds,
            120
        );
        assert_eq!(
            reopened.get_grid_preferences().unwrap(),
            GridPreferences::default()
        );
        // The pause before "Run anyway": none by default, and at most a few seconds.
        assert_eq!(defaults.confirm_delay_seconds, 0);
        assert_eq!(
            reopened
                .get_script_preferences()
                .unwrap()
                .confirm_delay_seconds,
            3
        );
        let wrong = ScriptPreferences {
            confirm_delay_seconds: ScriptPreferences::MAX_CONFIRM_DELAY + 1,
            ..defaults
        };
        assert!(reopened.set_script_preferences(&wrong).is_err());
        // A value written by hand out of range reads as the default.
        reopened
            .set_personal_setting("script.confirm_delay", "3600")
            .unwrap();
        assert_eq!(
            reopened
                .get_script_preferences()
                .unwrap()
                .confirm_delay_seconds,
            0
        );
    }

    #[test]
    fn a_moved_connection_keeps_its_settings_and_never_takes_another_s() {
        let path = temp("move-settings");
        let mut store = Store::open(&path).unwrap();
        let filter = ExplorerFilter {
            schema_include: vec!["sales".into()],
            ..ExplorerFilter::default()
        };
        let bookmark = |connection: &str| Bookmark {
            connection: connection.into(),
            schema: "public".into(),
            name: "orders".into(),
            kind: "table".into(),
            detail: String::new(),
        };
        let tabs = |connection: &str| SavedTabs {
            tabs: vec![SavedTab {
                text: "select 1".into(),
                connection: Some(connection.into()),
                ..SavedTab::default()
            }],
            active: Some(0),
        };
        // A repository connection in /repo, and the same identifier in another repository.
        for workspace in ["/repo", "/other"] {
            store
                .set_explorer_filter(workspace, "eu/orders", &filter)
                .unwrap();
            store
                .add_bookmark(workspace, &bookmark("eu/orders"))
                .unwrap();
            store
                .set_history_enabled(workspace, "eu/orders", false)
                .unwrap();
            store.save_open_tabs(workspace, &tabs("eu/orders")).unwrap();
        }
        // An older connection left a history setting under the new identifier.
        store
            .set_history_enabled("/repo", "prod/orders", true)
            .unwrap();

        store
            .move_connection_settings(Some("/repo"), "eu/orders", "prod/orders")
            .unwrap();
        assert_eq!(
            store.explorer_filter("/repo", "prod/orders").unwrap(),
            filter
        );
        assert_eq!(
            store.explorer_filter("/repo", "eu/orders").unwrap(),
            ExplorerFilter::default()
        );
        assert_eq!(store.bookmarks("/repo").unwrap(), [bookmark("prod/orders")]);
        assert!(
            !store.history_enabled("/repo", "prod/orders").unwrap(),
            "history switched off stays off"
        );
        assert_eq!(store.open_tabs("/repo").unwrap(), tabs("prod/orders"));
        // Another repository's connection of the same identifier is someone else's.
        assert_eq!(
            store.explorer_filter("/other", "eu/orders").unwrap(),
            filter
        );
        assert_eq!(store.bookmarks("/other").unwrap(), [bookmark("eu/orders")]);
        assert!(!store.history_enabled("/other", "eu/orders").unwrap());
        assert_eq!(store.open_tabs("/other").unwrap(), tabs("eu/orders"));

        // A personal connection is the same in every workspace: all of them follow.
        for workspace in ["/repo", ""] {
            store
                .set_explorer_filter(workspace, "~/local", &filter)
                .unwrap();
            store
                .set_history_enabled(workspace, "~/local", false)
                .unwrap();
        }
        // A stale "off" under the new identifier, from an older connection, goes with it.
        store.set_history_enabled("", "~/dev/local", false).unwrap();
        store.set_history_enabled("/repo", "~/old", false).unwrap();
        store
            .move_connection_settings(None, "~/old", "~/dev/old")
            .unwrap();
        assert!(!store.history_enabled("/repo", "~/dev/old").unwrap());
        store
            .move_connection_settings(None, "~/local", "~/dev/local")
            .unwrap();
        for workspace in ["/repo", ""] {
            assert_eq!(
                store.explorer_filter(workspace, "~/dev/local").unwrap(),
                filter
            );
            assert!(!store.history_enabled(workspace, "~/dev/local").unwrap());
            assert!(store.history_enabled(workspace, "~/local").unwrap());
        }
        // Moving one with no settings clears what an older one left under the new name.
        store.set_history_enabled("", "~/target", false).unwrap();
        store
            .move_connection_settings(None, "~/plain", "~/target")
            .unwrap();
        assert!(store.history_enabled("", "~/target").unwrap());
    }

    #[test]
    fn bookmarks_are_kept_per_workspace_once_each() {
        let path = temp("bookmarks");
        let store = Store::open(&path).unwrap();
        let orders = Bookmark {
            connection: "shop".into(),
            schema: "public".into(),
            name: "orders".into(),
            kind: "table".into(),
            detail: String::new(),
        };
        let total = Bookmark {
            name: "total".into(),
            kind: "function".into(),
            detail: "integer".into(),
            ..orders.clone()
        };
        store.add_bookmark("/repo", &orders).unwrap();
        store.add_bookmark("/repo", &orders).unwrap();
        store.add_bookmark("/repo", &total).unwrap();
        store.add_bookmark("", &orders).unwrap();
        assert_eq!(
            store.bookmarks("/repo").unwrap(),
            [orders.clone(), total.clone()]
        );
        assert_eq!(store.bookmarks("").unwrap(), std::slice::from_ref(&orders));
        assert!(store.bookmarks("/other").unwrap().is_empty());
        // An overload with other arguments is another bookmark.
        let other = Bookmark {
            detail: "text".into(),
            ..total.clone()
        };
        assert!(!store.remove_bookmark("/repo", &other).unwrap());
        assert!(store.remove_bookmark("/repo", &total).unwrap());
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.bookmarks("/repo").unwrap(), [orders]);
    }

    #[test]
    fn connection_name_pattern_is_unset_until_chosen_and_persistent() {
        let path = temp("connection-name-pattern");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_connection_name_pattern().unwrap(), None);
        store
            .set_connection_name_pattern(Some("{engine} {host}"))
            .unwrap();
        store.set_theme("dark").unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.get_connection_name_pattern().unwrap().as_deref(),
            Some("{engine} {host}")
        );
        reopened.set_connection_name_pattern(Some("")).unwrap();
        assert_eq!(
            reopened.get_connection_name_pattern().unwrap().as_deref(),
            Some("")
        );
        reopened.set_connection_name_pattern(None).unwrap();
        assert_eq!(reopened.get_connection_name_pattern().unwrap(), None);
        assert_eq!(reopened.get_theme().unwrap(), "dark");
    }

    #[test]
    fn update_check_is_on_until_turned_off_and_persistent() {
        let path = temp("update-check");
        let store = Store::open(&path).unwrap();
        assert!(store.get_update_check().unwrap());
        store.set_update_check(false).unwrap();
        assert!(!store.get_update_check().unwrap());
        store.set_theme("dark").unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(!reopened.get_update_check().unwrap());
        assert_eq!(reopened.get_theme().unwrap(), "dark");
        reopened.set_update_check(true).unwrap();
        assert!(reopened.get_update_check().unwrap());
    }

    #[test]
    fn theme_is_validated_persistent_and_independent_of_language() {
        let path = temp("theme");
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_theme().unwrap(), "system");
        store.set_language("es").unwrap();
        store.set_theme("dark").unwrap();
        for invalid in ["", "Dark", "blue", " light", "dark' OR 1=1 --"] {
            assert!(store.set_theme(invalid).is_err(), "{invalid}");
            assert_eq!(store.get_theme().unwrap(), "dark");
        }
        // The database rejects invalid values even if they bypass the Rust validation.
        for sql in [
            "update personal_settings set value = 'blue' where key = 'theme'",
            "insert or replace into personal_settings (key, value) values ('theme', 'blue')",
        ] {
            assert!(store.conn.execute(sql, []).is_err(), "{sql}");
        }
        store.set_theme("light").unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_theme().unwrap(), "light");
        assert_eq!(reopened.get_language().unwrap(), "es");
    }

    #[test]
    fn theme_migration_preserves_existing_settings() {
        let path = temp("theme-upgrade");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            for migration in &MIGRATIONS[..3] {
                tx.execute_batch(migration).unwrap();
            }
            tx.pragma_update(None, "user_version", 3).unwrap();
            tx.execute(
                "insert into personal_settings (key, value) values ('language', 'es')",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get_language().unwrap(), "es");
        assert_eq!(store.get_theme().unwrap(), "system");
    }

    #[test]
    fn mcp_settings_start_permissive_as_before_and_persist() {
        let path = temp("mcp-settings");
        let store = Store::open(&path).unwrap();
        assert!(!store.mcp_read_only().unwrap());
        assert_eq!(store.mcp_output_format().unwrap(), ToolOutput::Json);
        store.set_mcp_read_only(true).unwrap();
        store.set_mcp_output_format(ToolOutput::Toon).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.mcp_read_only().unwrap());
        assert_eq!(reopened.mcp_output_format().unwrap(), ToolOutput::Toon);
        reopened.set_mcp_read_only(false).unwrap();
        assert!(!reopened.mcp_read_only().unwrap());
    }

    #[test]
    fn mcp_read_only_cannot_be_switched_off_by_a_strange_value() {
        let path = temp("mcp-read-only-values");
        let store = Store::open(&path).unwrap();
        store.set_mcp_read_only(true).unwrap();
        store.set_mcp_output_format(ToolOutput::Json).unwrap();
        // The database refuses any other value, through any statement.
        for sql in [
            "update personal_settings set value = 'no' where key = 'mcp.read_only'",
            "update personal_settings set value = 'OFF' where key = 'mcp.read_only'",
            "update personal_settings set value = '' where key = 'mcp.read_only'",
            "insert or replace into personal_settings (key, value) values ('mcp.read_only', 'false')",
            "update personal_settings set value = 'yaml' where key = 'mcp.output_format'",
            "insert into personal_settings (key, value) values ('mcp.output_format', 'markdown')",
        ] {
            assert!(store.conn.execute(sql, []).is_err(), "{sql}");
        }
        assert!(store.mcp_read_only().unwrap());
        // Even if one got in (an older or damaged database), only `off` lets agents write.
        store
            .conn
            .execute_batch(
                "drop trigger personal_settings_mcp_update;
                 update personal_settings set value = 'false' where key = 'mcp.read_only';",
            )
            .unwrap();
        assert!(store.mcp_read_only().unwrap());
    }

    fn call(session: &str, tool: &str, statement: &str, decision: Decision) -> AuditEntry {
        let mut entry = AuditEntry::now("claude-code", "~/shop", statement, decision);
        entry.session = Some(session.into());
        entry.tool = Some(tool.into());
        entry
    }

    #[test]
    fn audit_events_are_filtered_counted_and_grouped_by_session() {
        let path = temp("audit-activity");
        let store = Store::open(&path).unwrap();
        // An entry from before sessions were recorded.
        store
            .record(&AuditEntry::now("old", "c", "select 0", Decision::Executed))
            .unwrap();
        store
            .record(&call("s1", "run_query", "select 1", Decision::Executed))
            .unwrap();
        store
            .record(&call(
                "s1",
                "execute_write",
                "drop table t",
                Decision::Rejected,
            ))
            .unwrap();
        store
            .record(&call(
                "s2",
                "run_query",
                "select 100% of_it",
                Decision::Failed,
            ))
            .unwrap();
        store
            .record(&call("s2", "list_objects", "schemas", Decision::Executed))
            .unwrap();

        let all = AuditFilter::default();
        assert_eq!(store.audit_count(&all).unwrap(), 5);
        let newest = store.audit_events(&all, 2, 0).unwrap();
        assert_eq!(newest[0].entry.statement, "schemas");
        assert_eq!(newest[1].entry.statement, "select 100% of_it");
        assert_eq!(store.audit_events(&all, 2, 4).unwrap().len(), 1);

        let by = |filter: AuditFilter| {
            store
                .audit_events(&filter, 100, 0)
                .unwrap()
                .into_iter()
                .map(|r| r.entry.statement)
                .collect::<Vec<_>>()
        };
        // Search is literal: `%` and `_` are not wildcards.
        assert_eq!(
            by(AuditFilter {
                search: "100%".into(),
                ..all.clone()
            }),
            ["select 100% of_it"]
        );
        assert_eq!(
            by(AuditFilter {
                search: "_".into(),
                ..all.clone()
            }),
            ["select 100% of_it"]
        );
        assert_eq!(
            by(AuditFilter {
                search: "DROP Table".into(),
                ..all.clone()
            }),
            ["drop table t"]
        );
        assert_eq!(
            by(AuditFilter {
                tool: Some("run_query".into()),
                ..all.clone()
            }),
            ["select 100% of_it", "select 1"]
        );
        assert_eq!(
            by(AuditFilter {
                decision: Some(Decision::Rejected),
                ..all.clone()
            }),
            ["drop table t"]
        );
        assert_eq!(
            by(AuditFilter {
                session: Some("s1".into()),
                ..all.clone()
            }),
            ["drop table t", "select 1"]
        );
        assert_eq!(
            store
                .audit_count(&AuditFilter {
                    session: Some("s2".into()),
                    ..all.clone()
                })
                .unwrap(),
            2
        );
        assert_eq!(
            store.audit_tools().unwrap(),
            ["execute_write", "list_objects", "run_query"]
        );
        let sessions = store.audit_sessions(10).unwrap();
        assert_eq!(
            sessions.len(),
            2,
            "entries without a session belong to none"
        );
        assert_eq!(sessions[0].session, "s2");
        assert_eq!(
            (sessions[0].calls, sessions[0].refused, sessions[0].failed),
            (2, 0, 1)
        );
        assert_eq!(
            (sessions[1].calls, sessions[1].refused, sessions[1].failed),
            (2, 1, 0)
        );
        assert!(sessions[1].started_ms <= sessions[1].ended_ms);
    }

    #[test]
    fn clearing_the_audit_log_leaves_a_record_and_keeps_it_append_only() {
        let path = temp("audit-clear");
        let mut store = Store::open(&path).unwrap();
        for n in 0..3 {
            store
                .record(&call(
                    "s1",
                    "run_query",
                    &format!("select {n}"),
                    Decision::Executed,
                ))
                .unwrap();
        }
        assert_eq!(store.clear_audit("SpelaionDB").unwrap(), 3);
        let left = store.audit_events(&AuditFilter::default(), 10, 0).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].entry.decision, Decision::Cleared);
        assert_eq!(left[0].entry.detail.as_deref(), Some("3 entries cleared"));
        assert_eq!(left[0].entry.client, "SpelaionDB");
        // The protections are back: the record of the clearing cannot go either.
        assert!(store.conn.execute("delete from audit", []).is_err());
        assert!(
            store
                .conn
                .execute("update audit set detail = 'x'", [])
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("delete from audit where decision = 'cleared'", [])
                .is_err()
        );
        // A second clear removes that record and leaves its own.
        assert_eq!(store.clear_audit("SpelaionDB").unwrap(), 1);
        assert_eq!(store.audit_count(&AuditFilter::default()).unwrap(), 1);
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(reopened.conn.execute("delete from audit", []).is_err());
    }

    #[test]
    fn the_activity_migration_keeps_the_old_audit_log() {
        let path = temp("audit-upgrade");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            // Every migration before the activity's (the ninth).
            let before = 8;
            for migration in &MIGRATIONS[..before] {
                tx.execute_batch(migration).unwrap();
            }
            tx.pragma_update(None, "user_version", before as i64)
                .unwrap();
            tx.execute(
                "insert into audit (at_ms, client, connection, statement, decision)
                 values (1, 'old', 'c', 'select 1', 'executed')",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let store = Store::open(&path).unwrap();
        let old = &store.recent(1).unwrap()[0];
        assert_eq!(old.statement, "select 1");
        assert_eq!((old.session.as_deref(), old.tool.as_deref()), (None, None));
        assert!(store.conn.execute("delete from audit", []).is_err());
    }
}
