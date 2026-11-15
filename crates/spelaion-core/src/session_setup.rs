//! How a new session starts (ADR-037): on a read-only connection, the engine's own read-only
//! mode first (ADR-053), then the connection's isolation level, its default schema and its
//! setup statements, applied by [`crate::open_connection`] every time a session opens,
//! reconnecting included. Nothing the user ran is ever re-run: only this, which is part of the
//! connection, as its host or its user are.
//!
//! Where the read-only barrier applies (a read-only connection, a type that confirms data
//! changes, a session that only reads), every setup statement is checked by
//! [`classify_session_setup`] before any of them runs: one that could write refuses the
//! session, whatever "ignore errors" says.

use std::time::Duration;

use futures_util::StreamExt;
use spelaion_config::{ConnectionFile, IsolationLevel, ResolvedConnection, Safeguards};
use spelaion_driver_api::TransactionState;

use crate::classify::classify_session_setup;
use crate::objects::{active_schema, set_active_schema};
use crate::session::CoreEvent;
use crate::{Session, SessionHealth};

/// Why a session could not start as its connection says.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupError {
    /// A setup statement may not run here: nothing of the setup ran.
    #[error(
        "the session setup's statement {index} of {total} cannot run on this session: {reason}"
    )]
    Refused {
        /// 1-based.
        index: usize,
        total: usize,
        reason: String,
    },
    /// A setup statement failed, and errors are not ignored.
    #[error("the session setup's statement {index} of {total} failed: {message}")]
    Failed {
        index: usize,
        total: usize,
        message: String,
    },
    /// The engine's read-only mode could not be set on a read-only connection (ADR-053).
    #[error("could not make the session read-only: {message}")]
    ReadOnly { message: String },
    #[error("could not set the isolation level to {level}: {message}")]
    Isolation { level: String, message: String },
    #[error("could not make {schema:?} the default schema: {message}")]
    Schema { schema: String, message: String },
    /// The setup left a transaction open: the session would start inside it.
    #[error(
        "the session setup left a transaction open: end it in the setup (COMMIT) or remove its BEGIN"
    )]
    TransactionLeftOpen,
    #[error("the session setup did not finish in {} s", after.as_secs())]
    Timeout { after: Duration },
    /// The connection was lost while the session was being set up.
    #[error("the connection was lost during the session setup: {0}")]
    Lost(String),
}

/// A setup statement that failed and was skipped, because the connection ignores setup errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSetup {
    /// 1-based.
    pub index: usize,
    /// The start of the statement, to recognize it.
    pub statement: String,
    pub message: String,
}

/// What the setup of a new session did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupReport {
    /// Setup statements that ran (skipped ones included).
    pub statements: usize,
    pub skipped: Vec<SkippedSetup>,
}

/// Whether the read-only barrier applies to this connection's session setup: read-only
/// connections and types that confirm data changes (Production). Sessions that only read add
/// themselves through [`crate::OpenOptions::read_only_session`].
pub fn setup_is_restricted(resolved: &ResolvedConnection) -> bool {
    resolved.params.read_only || resolved.safeguards().confirm_data_changes
}

/// Checks every statement of `setup` without running anything: where `restricted`, each must
/// pass the read-only barrier. The connection form calls it before saving, and
/// [`apply_session_setup`] before running.
pub fn check_session_setup(
    session_dialect: &dyn spelaion_driver_api::Dialect,
    engine: &str,
    setup: &str,
    restricted: bool,
) -> Result<usize, SetupError> {
    let spans = session_dialect.split_statements(setup);
    let total = spans.len();
    if restricted {
        for (i, span) in spans.iter().enumerate() {
            classify_session_setup(engine, span.text(setup)).map_err(|reason| {
                SetupError::Refused {
                    index: i + 1,
                    total,
                    reason,
                }
            })?;
        }
    }
    Ok(total)
}

/// The check the connection form runs before saving a connection file (ADR-037), with the
/// safeguards of its type: a setup that the session would refuse is refused now, with the same
/// message. Opening a session checks it again whatever was saved.
pub fn check_file_setup(file: &ConnectionFile, safeguards: &Safeguards) -> Result<(), SetupError> {
    let Some(setup) = &file.session.setup else {
        return Ok(());
    };
    let Some(dialect) = crate::dialect_of(file.engine) else {
        return Ok(());
    };
    let restricted = file.read_only || safeguards.confirm_data_changes;
    check_session_setup(dialect.as_ref(), dialect.name(), setup, restricted).map(|_| ())
}

/// The statement that makes a read-only connection's session read-only in the engine itself
/// (ADR-053), run before anything else when it opens: every transaction of the session is then
/// read-only, so a function that writes behind a `SELECT` fails too. `None` where the engine has
/// no such mode for a session (SQL Server: the core's check is the barrier there) or the
/// connection already opens its file read-only (SQLite, DuckDB).
pub fn read_only_statement(engine: &str) -> Option<&'static str> {
    match engine {
        "postgres" => Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"),
        "mysql" | "mariadb" => Some("SET SESSION TRANSACTION READ ONLY"),
        _ => None,
    }
}

/// The statement that sets the session's isolation level, by engine; `None` where the engine
/// has a single level (the configuration refuses one there).
pub fn isolation_statement(engine: &str, level: IsolationLevel) -> Option<String> {
    let words = level.sql();
    match engine {
        "postgres" if level != IsolationLevel::Snapshot => Some(format!(
            "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL {words}"
        )),
        "mysql" | "mariadb" if level != IsolationLevel::Snapshot => {
            Some(format!("SET SESSION TRANSACTION ISOLATION LEVEL {words}"))
        }
        "sqlserver" => Some(format!("SET TRANSACTION ISOLATION LEVEL {words}")),
        _ => None,
    }
}

/// Sets up a session that has just opened: the engine's read-only mode on a read-only
/// connection (ADR-053), the isolation level, then the default schema, then the setup
/// statements, within the connection's connect timeout. Statements are run as the
/// core's own (not recorded in the history, never inside the manual-commit transaction).
pub async fn apply_session_setup(
    session: &Session,
    resolved: &ResolvedConnection,
    restricted: bool,
) -> Result<SetupReport, SetupError> {
    let config = &resolved.session;
    let read_only_mode =
        resolved.params.read_only && read_only_statement(session.dialect().name()).is_some();
    if !read_only_mode
        && config.setup.is_none()
        && config.schema.is_none()
        && config.isolation.is_none()
    {
        return Ok(SetupReport::default());
    }
    let after = resolved.params.connect_timeout();
    match tokio::time::timeout(after, apply(session, resolved, restricted)).await {
        Ok(result) => result,
        Err(_) => {
            let _ = session.cancel().await;
            Err(SetupError::Timeout { after })
        }
    }
}

async fn apply(
    session: &Session,
    resolved: &ResolvedConnection,
    restricted: bool,
) -> Result<SetupReport, SetupError> {
    let config = &resolved.session;
    let dialect = session.dialect().clone();
    let engine = dialect.name();
    // Every statement is checked before anything runs.
    if let Some(setup) = &config.setup {
        check_session_setup(dialect.as_ref(), engine, setup, restricted)?;
    }

    // Before anything else, so the setup itself runs read-only (ADR-053).
    if resolved.params.read_only
        && let Some(statement) = read_only_statement(engine)
    {
        run_statement(session, statement)
            .await
            .map_err(|message| SetupError::ReadOnly { message })?;
        lost(session)?;
    }

    if let Some(level) = config.isolation {
        let statement =
            isolation_statement(engine, level).ok_or_else(|| SetupError::Isolation {
                level: level.as_str().into(),
                message: "this engine has a single isolation level".into(),
            })?;
        run_statement(session, &statement)
            .await
            .map_err(|message| SetupError::Isolation {
                level: level.as_str().into(),
                message,
            })?;
        lost(session)?;
    }

    if let Some(schema) = &config.schema {
        let failed = |message: String| SetupError::Schema {
            schema: schema.clone(),
            message,
        };
        set_active_schema(session, schema)
            .await
            .map_err(|e| failed(e.to_string()))?;
        lost(session)?;
        // PostgreSQL accepts a search_path naming a schema that does not exist: what the
        // session finds now says whether it took.
        let now = active_schema(session).await.map_err(failed)?;
        let same = match (&now, engine) {
            (Some(now), "postgres") => now == schema,
            (Some(now), _) => now.eq_ignore_ascii_case(schema),
            (None, _) => false,
        };
        if !same {
            return Err(failed(
                "it does not exist, or this user cannot use it".into(),
            ));
        }
    }

    let mut report = SetupReport::default();
    if let Some(setup) = &config.setup {
        let spans = dialect.split_statements(setup);
        let total = spans.len();
        for (i, span) in spans.iter().enumerate() {
            let text = span.text(setup);
            report.statements += 1;
            let result = run_statement(session, text).await;
            lost(session)?;
            match result {
                Ok(()) => {}
                Err(message) if config.ignore_setup_errors => report.skipped.push(SkippedSetup {
                    index: i + 1,
                    statement: start_of(text),
                    message,
                }),
                Err(message) => {
                    return Err(SetupError::Failed {
                        index: i + 1,
                        total,
                        message,
                    });
                }
            }
        }
    }

    // A session never starts inside a transaction the setup opened.
    match session.connection().transaction_state().await {
        Ok(TransactionState::Idle) | Err(_) => {}
        Ok(TransactionState::Open | TransactionState::Failed) => {
            return Err(SetupError::TransactionLeftOpen);
        }
    }
    lost(session)?;
    Ok(report)
}

/// Runs one statement as the core's own; rows it returns are not kept.
async fn run_statement(session: &Session, sql: &str) -> Result<(), String> {
    let mut events = session.run_generated(sql);
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            CoreEvent::ResultStarted { result, .. } => {
                session.results().remove(result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    failure.map_or(Ok(()), Err)
}

/// A lost connection ends the setup, whatever "ignore errors" says.
fn lost(session: &Session) -> Result<(), SetupError> {
    match &*session.health().borrow() {
        SessionHealth::Alive => Ok(()),
        SessionHealth::Lost { reason } => Err(SetupError::Lost(reason.clone())),
        SessionHealth::IdleClosed { .. } => Err(SetupError::Lost("the session was closed".into())),
    }
}

/// The first line of a statement, at most 80 characters.
fn start_of(statement: &str) -> String {
    let line = statement.trim().lines().next().unwrap_or_default();
    let mut out: String = line.chars().take(80).collect();
    if out.len() < line.len() || statement.trim().lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_mode_by_engine() {
        assert_eq!(
            read_only_statement("postgres"),
            Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
        );
        assert_eq!(
            read_only_statement("mysql"),
            Some("SET SESSION TRANSACTION READ ONLY")
        );
        assert_eq!(read_only_statement("sqlserver"), None);
        assert_eq!(read_only_statement("sqlite"), None);
        assert_eq!(read_only_statement("duckdb"), None);
    }

    #[test]
    fn isolation_statements_by_engine() {
        assert_eq!(
            isolation_statement("postgres", IsolationLevel::RepeatableRead).as_deref(),
            Some("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        );
        assert_eq!(
            isolation_statement("mysql", IsolationLevel::ReadCommitted).as_deref(),
            Some("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")
        );
        assert_eq!(
            isolation_statement("sqlserver", IsolationLevel::Snapshot).as_deref(),
            Some("SET TRANSACTION ISOLATION LEVEL SNAPSHOT")
        );
        assert_eq!(
            isolation_statement("postgres", IsolationLevel::Snapshot),
            None
        );
        assert_eq!(
            isolation_statement("sqlite", IsolationLevel::Serializable),
            None
        );
        assert_eq!(
            isolation_statement("duckdb", IsolationLevel::Serializable),
            None
        );
    }

    #[test]
    fn skipped_statements_are_shown_by_their_start() {
        assert_eq!(start_of("  set x = 1  "), "set x = 1");
        assert_eq!(start_of("select 1\nfrom t"), "select 1…");
        let long = "x".repeat(100);
        assert_eq!(start_of(&long).chars().count(), 81);
    }
}
