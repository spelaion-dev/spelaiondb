//! "Read-only for this session" (SDB-395): an editor tab's session made read-only on request,
//! for a demo or a careful look at production, until the tab reconnects or the user turns it
//! off.
//!
//! It is the barrier of a read-only connection (ADR-053), never a lighter one: the session
//! refuses, before anything is sent, every script that is not a read, a session setting or
//! transaction control ([`Session::is_read_only`], which `guard::check_run` and the session's
//! own runs read), and every write path the app gives the session's read-only flag (table
//! edits, object actions, imports, ending processes) refuses too. Where the engine has a
//! read-only mode for a whole session that can be undone (PostgreSQL, MySQL, MariaDB), it is
//! set as well and read back, so a function that writes behind a `SELECT` fails in the engine.
//! On the other engines the core's check is the barrier, as on a read-only connection to SQL
//! Server.
//!
//! It only ever adds: a read-only connection stays read-only whatever is asked here, an
//! engine without a read barrier of ours gets no read-only mode this way either, and nothing
//! changes while a transaction is open (the work already done in it is the user's to commit
//! or roll back first).

use futures_util::StreamExt;

use crate::{CoreEvent, OpenConnection, Session, SessionHealth};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionReadOnlyError {
    #[error("the connection is read-only: its sessions cannot be made writable")]
    ConnectionReadOnly,
    #[error("{engine} has no read-only mode in SpelaionDB")]
    NoBarrier { engine: String },
    #[error("read-only mode is not available on this server: {0}")]
    ServerRefuses(String),
    #[error("a transaction is open: commit or roll it back first")]
    TransactionOpen,
    #[error("the session's connection was lost")]
    Lost,
    #[error("could not change the session's read-only mode: {0}")]
    Engine(String),
}

/// The engine's own statement that makes a whole session read-only or writable again, where
/// the core sets one for this (`None`: the core's check is the barrier alone).
fn engine_statement(dialect: &str, read_only: bool) -> Option<&'static str> {
    match (dialect, read_only) {
        ("postgres", true) => Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"),
        ("postgres", false) => Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"),
        ("mysql" | "mariadb", true) => Some("SET SESSION TRANSACTION READ ONLY"),
        ("mysql" | "mariadb", false) => Some("SET SESSION TRANSACTION READ WRITE"),
        _ => None,
    }
}

/// Makes `open`'s session read-only for as long as it lives (`read_only`), or takes that back.
/// Turning it off never makes a read-only connection's session writable.
pub async fn set_session_read_only(
    open: &OpenConnection,
    read_only: bool,
) -> Result<(), SessionReadOnlyError> {
    let session = &open.session;
    if read_only == session.is_session_read_only() {
        return Ok(());
    }
    if !read_only && session.is_connection_read_only() {
        return Err(SessionReadOnlyError::ConnectionReadOnly);
    }
    let engine = open.resolved.engine;
    if read_only && !engine.has_read_barrier() {
        return Err(SessionReadOnlyError::NoBarrier {
            engine: engine.as_str().into(),
        });
    }
    if read_only
        && let Some(reason) = session
            .server_family()
            .and_then(crate::ServerFamily::read_only_sessions_refusal)
    {
        return Err(SessionReadOnlyError::ServerRefuses(reason));
    }
    if session.transaction_status().in_transaction() {
        return Err(SessionReadOnlyError::TransactionOpen);
    }
    match &*session.health().borrow() {
        SessionHealth::Alive => {}
        SessionHealth::Lost { .. } | SessionHealth::IdleClosed { .. } => {
            return Err(SessionReadOnlyError::Lost);
        }
    }
    let dialect = session.dialect().name().to_string();
    // A read-only connection's session already has the engine's mode on: leave it.
    let statement = (!session.is_connection_read_only())
        .then(|| engine_statement(&dialect, read_only))
        .flatten();
    // The core's check first, so nothing writable is ever sent while the engine's mode is set.
    if read_only {
        session.mark_session_read_only(true);
    }
    if let Some(statement) = statement {
        let outcome = match run_statement(session, statement).await {
            Err(message) => Err(SessionReadOnlyError::Engine(message)),
            Ok(())
                if read_only
                    && !crate::server_family::read_only_confirmed(
                        session.connection().as_ref(),
                        &dialect,
                    )
                    .await =>
            {
                // The server accepted the statement and stayed writable: no barrier of the
                // engine's to rely on, and no half-made read-only session either.
                let _ = run_statement(
                    session,
                    engine_statement(&dialect, false).unwrap_or_default(),
                )
                .await;
                Err(SessionReadOnlyError::ServerRefuses(
                    "it accepted the session's read-only mode and its session is still writable"
                        .into(),
                ))
            }
            Ok(()) => Ok(()),
        };
        if let Err(error) = outcome {
            // Turning it on failed: the session is as it was. Turning it off failed: the
            // engine may still be read-only, so the session stays marked read-only too.
            if read_only {
                session.mark_session_read_only(false);
            }
            return Err(error);
        }
    }
    if !read_only {
        session.mark_session_read_only(false);
    }
    Ok(())
}

/// Runs one statement the core wrote, as the session setup does.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_mode_is_set_and_undone_only_where_it_can_be() {
        assert_eq!(
            engine_statement("postgres", true),
            Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
        );
        assert_eq!(
            engine_statement("postgres", false),
            Some("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE")
        );
        assert_eq!(
            engine_statement("mysql", true),
            Some("SET SESSION TRANSACTION READ ONLY")
        );
        assert_eq!(
            engine_statement("mysql", false),
            Some("SET SESSION TRANSACTION READ WRITE")
        );
        // ClickHouse's `readonly` cannot be set twice nor taken back in a session; SQL Server
        // has no session mode; SQLite and DuckDB open their file once.
        for dialect in ["clickhouse", "sqlserver", "sqlite", "duckdb", "trino"] {
            assert_eq!(engine_statement(dialect, true), None, "{dialect}");
            assert_eq!(engine_statement(dialect, false), None, "{dialect}");
        }
    }
}
