//! SpelaionDB local storage (ADR-011): a SQLite database in the personal directory.
//!
//! Query history, saved queries (snippets) and the MCP audit log. The schema cache will
//! come here with its own migration.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

#[derive(Debug, thiserror::Error)]
#[error("local store: {0}")]
pub struct StoreError(String);

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError(e.to_string())
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
    );"
];

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
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::Executed => "executed",
            Decision::Rejected => "rejected",
            Decision::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "executed" => Some(Decision::Executed),
            "rejected" => Some(Decision::Rejected),
            "failed" => Some(Decision::Failed),
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

    /// "Show only": the pattern becomes the only included one of a kind, and stops being
    /// excluded.
    pub fn show_only(&mut self, kind: FilterKind, pattern: &str) {
        let (include, exclude) = self.lists_of(kind);
        *include = vec![pattern.to_string()];
        exclude.retain(|p| p != pattern);
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
            return Err(StoreError(
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
        }
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError(e.to_string()))?;
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
        let current: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let current = usize::try_from(current).unwrap_or(usize::MAX);
        if current > MIGRATIONS.len() {
            return Err(StoreError(format!(
                "the local database is from a newer version of SpelaionDB (schema {current})"
            )));
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn record(&self, entry: &AuditEntry) -> Result<(), StoreError> {
        self.conn.execute(
            "insert into audit (at_ms, client, connection, statement, decision, rows, duration_ms, detail)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                entry.at_ms,
                entry.client,
                entry.connection,
                entry.statement,
                entry.decision.as_str(),
                entry.rows,
                entry.duration_ms,
                entry.detail
            ],
        )?;
        Ok(())
    }

    /// Latest entries, from newest to oldest.
    pub fn recent(&self, limit: usize) -> Result<Vec<AuditEntry>, StoreError> {
        let mut stmt = self.conn.prepare(
            "select at_ms, client, connection, statement, decision, rows, duration_ms, detail
             from audit order by id desc limit ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            let decision: String = r.get(4)?;
            Ok(AuditEntry {
                at_ms: r.get(0)?,
                client: r.get(1)?,
                connection: r.get(2)?,
                statement: r.get(3)?,
                decision: Decision::parse(&decision).unwrap_or(Decision::Failed),
                rows: r.get(5)?,
                duration_ms: r.get(6)?,
                detail: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
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
            "select id, workspace, name, statement, connection, updated_ms from snippets
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

    /// Personal interface language, shared by every workspace in this user directory.
    /// `system` follows the operating system; `en` and `es` are explicit overrides.
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
        } = *preferences;
        self.set_flag("explorer.show_system", show_system)?;
        self.set_flag("explorer.sort_alphabetically", sort_alphabetically)?;
        self.set_flag("explorer.sort_ignore_case", sort_ignore_case)?;
        self.set_flag("explorer.folders_first", folders_first)?;
        self.set_flag("explorer.connected_only", connected_only)?;
        self.set_flag("explorer.server_side_filters", server_side_filters)
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
            })
        };
        if search.is_empty() {
            let mut stmt = self.conn.prepare(
                "select id, at_ms, workspace, connection, statement, duration_ms, rows, error, cancelled
                 from history where (workspace = ?1 or workspace = '')
                 order by at_ms desc, id desc limit ?2 offset ?3",
            )?;
            let rows = stmt.query_map(params![workspace, limit, offset], read)?;
            Ok(rows.collect::<Result<_, _>>()?)
        } else {
            let phrase = format!("\"{}\"*", search.replace('"', "\"\""));
            let mut stmt = self.conn.prepare(
                "select h.id, h.at_ms, h.workspace, h.connection, h.statement,
                        h.duration_ms, h.rows, h.error, h.cancelled
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
        Err(StoreError(
            "unsupported language; expected system, en or es".into(),
        ))
    }
}

fn validate_theme(theme: &str) -> Result<(), StoreError> {
    if matches!(theme, "system" | "light" | "dark") {
        Ok(())
    } else {
        Err(StoreError(
            "unsupported theme; expected system, light or dark".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history_entry(
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

    fn temp(name: &str) -> std::path::PathBuf {
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
        filter.show_only(FilterKind::Objects, "orders");
        assert_eq!(filter.object_include, ["orders"]);
        assert_eq!(filter.object_exclude, ["audit"]);
        filter.hide(FilterKind::Columns, "password");
        filter.show_only(FilterKind::Schemas, "public");
        assert_eq!(filter.column_exclude, ["password"]);
        assert_eq!(filter.schema_include, ["public"]);
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
        assert!(!defaults.show_system && !defaults.folders_first && !defaults.connected_only);
        // Every field is saved on its own: each one flipped from its default.
        let changed = ExplorerPreferences {
            show_system: true,
            sort_alphabetically: false,
            sort_ignore_case: false,
            folders_first: true,
            connected_only: true,
            server_side_filters: false,
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
}
