//! Transactions you can see and control (ADR-031).
//!
//! An editor session has a [`TransactionStatus`]: auto-commit or manual commit, whether its
//! connection type asks for smart commit, and whether a transaction is open. The state always
//! comes from the driver ([`Connection::transaction_state`]), read after every statement, so it
//! stays true after a `BEGIN` the user typed, an implicit commit or an error that rolled the
//! transaction back. In manual commit the core opens a transaction with the engine's own
//! `BEGIN` before a statement that runs while none is open; smart commit switches an
//! auto-commit session to manual right before its first statement that is not a read.
//!
//! [`Connection::transaction_state`]: spelaion_driver_api::Connection::transaction_state

use spelaion_config::Safeguards;
use spelaion_driver_api::Notice;
pub use spelaion_driver_api::TransactionState;

use crate::classify::classify_read_only;

/// What an editor session shows about its transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionStatus {
    /// The session follows its transaction: an editor session whose engine can tell whether
    /// one is open. Otherwise it only runs in auto-commit, as before ADR-031.
    pub supported: bool,
    /// Each statement commits as it runs. Off: changes wait for Commit.
    pub auto_commit: bool,
    /// The connection type's smart commit: the first statement that is not a read switches
    /// the session to manual commit.
    pub smart_commit: bool,
    /// Manual commit was turned on by smart commit: when the transaction ends, the session
    /// returns to auto-commit.
    pub smart_switched: bool,
    /// What the driver last said; `None` while it is not followed.
    pub state: Option<TransactionState>,
    /// Statements run in the open transaction (transaction control and internal metadata
    /// reads are not counted).
    pub statements: u32,
}

impl Default for TransactionStatus {
    fn default() -> Self {
        Self {
            supported: false,
            auto_commit: true,
            smart_commit: false,
            smart_switched: false,
            state: None,
            statements: 0,
        }
    }
}

impl TransactionStatus {
    /// A transaction is open with work that a Commit would keep.
    pub fn is_open(&self) -> bool {
        self.state == Some(TransactionState::Open)
    }

    /// A transaction is open, also one that can only be rolled back.
    pub fn in_transaction(&self) -> bool {
        matches!(
            self.state,
            Some(TransactionState::Open | TransactionState::Failed)
        )
    }

    /// Takes the driver's answer: when no transaction is open any more, the count restarts and
    /// a session that smart commit switched returns to auto-commit.
    pub(crate) fn observe(&mut self, state: TransactionState) {
        self.state = Some(state);
        if state == TransactionState::Idle {
            self.statements = 0;
            if self.smart_switched {
                self.smart_switched = false;
                self.auto_commit = true;
            }
        }
    }
}

/// Why a transaction action was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransactionError {
    #[error("a script is running on this tab: wait for it to finish or cancel it")]
    Busy,
    #[error(
        "this connection cannot tell whether a transaction is open, so it only runs in auto-commit"
    )]
    Unsupported,
    #[error("a transaction is open: commit or roll it back first")]
    Open,
    #[error("no transaction is open")]
    NotOpen,
    #[error("{0}")]
    Failed(String),
}

/// What a Commit or Rollback said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionOutcome {
    pub status: TransactionStatus,
    /// The engine's notices and warnings, and what the core noticed (a failed transaction
    /// that a Commit could only roll back).
    pub notices: Vec<Notice>,
}

/// The starting status of an editor session under its connection type's safeguards.
pub(crate) fn initial_status(
    safeguards: &Safeguards,
    state: TransactionState,
) -> TransactionStatus {
    TransactionStatus {
        supported: true,
        auto_commit: !safeguards.manual_commit,
        smart_commit: safeguards.smart_commit,
        smart_switched: false,
        state: Some(state),
        statements: 0,
    }
}

/// How a statement relates to transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatementKind {
    /// `BEGIN`, `START TRANSACTION`…: run as written.
    Begin,
    /// `COMMIT`, `ROLLBACK`: run as written, and they end the transaction on purpose.
    End,
    /// `SAVEPOINT`, `RELEASE`, `SET TRANSACTION`…: run as written.
    Control,
    /// Refused by the engine inside a transaction: never preceded by the core's `BEGIN`.
    OutsideTransaction,
    /// Anything else.
    Other,
}

impl StatementKind {
    /// The core never opens a transaction before it, nor counts it.
    pub(crate) fn is_control(self) -> bool {
        match self {
            StatementKind::Begin | StatementKind::End | StatementKind::Control => true,
            StatementKind::OutsideTransaction | StatementKind::Other => false,
        }
    }
}

/// The engine's statement that opens a transaction.
pub(crate) fn begin_statement(engine: &str) -> &'static str {
    match engine {
        // In T-SQL a lone BEGIN opens a block, not a transaction.
        "sqlserver" => "BEGIN TRANSACTION",
        "duckdb" => "BEGIN TRANSACTION",
        "mysql" => "START TRANSACTION",
        _ => "BEGIN",
    }
}

pub(crate) fn commit_statement(engine: &str) -> &'static str {
    match engine {
        "sqlserver" => "COMMIT TRANSACTION",
        _ => "COMMIT",
    }
}

pub(crate) fn rollback_statement(engine: &str) -> &'static str {
    match engine {
        "sqlserver" => "ROLLBACK TRANSACTION",
        _ => "ROLLBACK",
    }
}

/// Whether smart commit switches to manual before this statement: it is not a read, by the
/// same classification that asks to confirm data changes on Production (`guard.rs`).
pub(crate) fn is_write(engine: &str, sql: &str) -> bool {
    classify_read_only(engine, sql).is_err()
}

/// How `sql` (one statement, or one T-SQL batch) relates to transactions on `engine`.
pub(crate) fn statement_kind(engine: &str, sql: &str) -> StatementKind {
    let words = leading_words(sql, 4);
    let word = |i: usize| words.get(i).map_or("", String::as_str);
    let has = |w: &str| words.iter().any(|x| x == w);
    match (engine, word(0)) {
        // T-SQL: BEGIN alone (or BEGIN TRY) opens a block.
        ("sqlserver", "begin") => {
            if matches!(word(1), "tran" | "transaction" | "distributed") {
                StatementKind::Begin
            } else {
                StatementKind::Other
            }
        }
        ("sqlserver", "commit" | "rollback") => StatementKind::End,
        ("sqlserver", "save") => StatementKind::Control,
        ("sqlserver", "create" | "alter" | "drop") if word(1) == "database" => {
            StatementKind::OutsideTransaction
        }
        ("sqlserver", "create" | "alter" | "drop")
            if word(1) == "fulltext" && word(2) == "catalog" =>
        {
            StatementKind::OutsideTransaction
        }
        ("sqlserver", "backup" | "restore" | "reconfigure") => StatementKind::OutsideTransaction,
        ("sqlserver", _) => StatementKind::Other,

        (_, "begin") => StatementKind::Begin,
        (_, "start") if word(1) == "transaction" => StatementKind::Begin,
        // `ROLLBACK TO SAVEPOINT` keeps the transaction open; the state is read after it.
        (_, "commit" | "rollback" | "end" | "abort") => StatementKind::End,
        (_, "savepoint" | "release") => StatementKind::Control,
        (_, "set")
            if word(1) == "transaction"
                || (word(1) == "session" && word(2) == "characteristics") =>
        {
            StatementKind::Control
        }
        ("mysql", "set") if words.iter().any(|w| w.contains("autocommit")) => {
            StatementKind::Control
        }
        ("mysql", "xa" | "lock" | "unlock") => StatementKind::Control,
        ("postgres", "prepare") if word(1) == "transaction" => StatementKind::End,

        ("postgres", "vacuum") => StatementKind::OutsideTransaction,
        ("postgres", "create" | "drop")
            if matches!(word(1), "database" | "tablespace" | "subscription") =>
        {
            StatementKind::OutsideTransaction
        }
        ("postgres", "alter") if word(1) == "system" || word(1) == "subscription" => {
            StatementKind::OutsideTransaction
        }
        ("postgres", "create" | "drop" | "reindex") if has("concurrently") => {
            StatementKind::OutsideTransaction
        }
        ("postgres", "reindex") if matches!(word(1), "database" | "system") => {
            StatementKind::OutsideTransaction
        }
        ("postgres", "discard") if word(1) == "all" => StatementKind::OutsideTransaction,

        ("sqlite", "vacuum" | "attach" | "detach") => StatementKind::OutsideTransaction,
        ("sqlite", "pragma") if has("journal_mode") => StatementKind::OutsideTransaction,

        ("duckdb", "vacuum" | "checkpoint" | "force") => StatementKind::OutsideTransaction,
        _ => StatementKind::Other,
    }
}

/// The first `n` words of the statement in lower case, after comments and spaces. A word is
/// letters, digits and `_`; anything else separates words.
fn leading_words(sql: &str, n: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = sql;
    while words.len() < n {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, next)| next);
        } else if let Some(after) = rest.strip_prefix('#') {
            // MySQL comment.
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if rest.is_empty() {
            break;
        } else {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '@'))
                .unwrap_or(rest.len());
            if end == 0 {
                // Punctuation (`=`, `(`, `;`…): skip it.
                let skip = rest.chars().next().map_or(1, char::len_utf8);
                rest = &rest[skip..];
                continue;
            }
            words.push(rest[..end].to_lowercase());
            rest = &rest[end..];
        }
    }
    words
}

/// The message for a transaction that a statement ended without being `COMMIT` or `ROLLBACK`.
pub(crate) fn ended_by_statement(engine: &str, failed: bool) -> Notice {
    let message = if failed {
        "The open transaction was rolled back by this error.".to_string()
    } else if engine == "mysql" {
        "The open transaction was committed by this statement: MySQL and MariaDB commit before DDL and before a new transaction.".to_string()
    } else {
        "The open transaction was committed by this statement.".to_string()
    };
    Notice {
        severity: "WARNING".into(),
        message,
    }
}

/// The notice of a Commit on a transaction that had failed: the engine only rolls it back.
pub(crate) fn failed_commit_notice() -> Notice {
    Notice {
        severity: "WARNING".into(),
        message:
            "The transaction had failed, so the server rolled it back instead of committing it."
                .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_control_is_recognised_per_engine() {
        use StatementKind::*;
        let cases = [
            ("postgres", "begin", Begin),
            (
                "postgres",
                "  -- why\nBEGIN ISOLATION LEVEL SERIALIZABLE",
                Begin,
            ),
            ("postgres", "start transaction read only", Begin),
            ("postgres", "COMMIT", End),
            ("postgres", "end", End),
            ("postgres", "abort", End),
            ("postgres", "rollback to savepoint a", End),
            ("postgres", "savepoint a", Control),
            ("postgres", "release savepoint a", Control),
            (
                "postgres",
                "set transaction isolation level serializable",
                Control,
            ),
            (
                "postgres",
                "set session characteristics as transaction read only",
                Control,
            ),
            ("postgres", "prepare transaction 'x'", End),
            ("postgres", "prepare plan as select 1", Other),
            ("postgres", "set search_path = public", Other),
            ("postgres", "vacuum analyze t", OutsideTransaction),
            ("postgres", "create database x", OutsideTransaction),
            (
                "postgres",
                "create index concurrently i on t (a)",
                OutsideTransaction,
            ),
            (
                "postgres",
                "create unique index concurrently i on t (a)",
                OutsideTransaction,
            ),
            ("postgres", "create index i on t (a)", Other),
            (
                "postgres",
                "alter system set work_mem = '1MB'",
                OutsideTransaction,
            ),
            ("postgres", "reindex database x", OutsideTransaction),
            ("postgres", "reindex table t", Other),
            ("mysql", "begin", Begin),
            ("mysql", "start transaction with consistent snapshot", Begin),
            ("mysql", "set autocommit = 0", Control),
            ("mysql", "set @@session.autocommit=1", Control),
            ("mysql", "lock tables t write", Control),
            ("mysql", "# note\nupdate t set a = 1", Other),
            ("sqlserver", "begin tran", Begin),
            ("sqlserver", "BEGIN TRANSACTION t1", Begin),
            ("sqlserver", "begin\n  update t set a = 1\nend", Other),
            (
                "sqlserver",
                "begin try select 1 end try begin catch end catch",
                Other,
            ),
            ("sqlserver", "commit", End),
            ("sqlserver", "rollback tran", End),
            ("sqlserver", "save transaction s", Control),
            ("sqlserver", "create database x", OutsideTransaction),
            (
                "sqlserver",
                "backup database x to disk = 'x'",
                OutsideTransaction,
            ),
            ("sqlserver", "end", Other),
            ("sqlite", "begin immediate", Begin),
            ("sqlite", "end transaction", End),
            ("sqlite", "vacuum", OutsideTransaction),
            ("sqlite", "attach 'x.db' as x", OutsideTransaction),
            ("sqlite", "pragma journal_mode = wal", OutsideTransaction),
            ("sqlite", "pragma foreign_keys", Other),
            ("duckdb", "begin transaction", Begin),
            ("duckdb", "checkpoint", OutsideTransaction),
            ("duckdb", "insert into t values (1)", Other),
        ];
        for (engine, sql, kind) in cases {
            assert_eq!(statement_kind(engine, sql), kind, "{engine}: {sql}");
        }
    }

    #[test]
    fn smart_commit_sees_writes_not_reads() {
        assert!(!is_write("postgres", "select * from t"));
        assert!(is_write("postgres", "update t set a = 1 where id = 2"));
        assert!(is_write("postgres", "create table t (a int)"));
        assert!(is_write("mysql", "insert into t values (1)"));
        assert!(is_write("sqlserver", "delete from t where id = 1"));
    }

    #[test]
    fn a_smart_switch_ends_with_its_transaction() {
        let mut status = TransactionStatus {
            supported: true,
            auto_commit: false,
            smart_commit: true,
            smart_switched: true,
            state: Some(TransactionState::Open),
            statements: 2,
        };
        status.observe(TransactionState::Open);
        assert!(!status.auto_commit && status.statements == 2);
        status.observe(TransactionState::Idle);
        assert!(status.auto_commit && !status.smart_switched && status.statements == 0);
        // A manual mode the user chose stays manual.
        let mut manual = TransactionStatus {
            smart_switched: false,
            auto_commit: false,
            ..status
        };
        manual.observe(TransactionState::Idle);
        assert!(!manual.auto_commit);
    }
}
