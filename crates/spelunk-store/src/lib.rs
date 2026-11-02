//! SpelunkDB local storage (ADR-011): a SQLite database in the personal directory.
//!
//! Query history and the MCP audit log. Snippets and the schema cache will come here
//! with their own migrations.

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
    alter table history add column cancelled integer not null default 0 check (cancelled in (0, 1));"
];

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
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            at_ms,
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

/// Local database. Used by the app and `spelunk mcp` at the same time: WAL mode and waiting on locks.
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
                "the local database is from a newer version of SpelunkDB (schema {current})"
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
        self.conn.execute(
            "insert into personal_settings (key, value) values ('language', ?1)
             on conflict (key) do update set value = excluded.value",
            [language],
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
            std::env::temp_dir().join(format!("spelunk-store-{name}-{}.db", std::process::id()));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        path
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
            "spelunk-store-language-users-{}-{nonce}",
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
}
