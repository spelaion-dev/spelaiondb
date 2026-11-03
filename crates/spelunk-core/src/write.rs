//! Writes from agents (ADR-019), on connections whose user allowed them. The barriers live
//! here, not in the MCP server: one statement the parser understands, never a read (that is
//! `run_read_only`), never destructive (`DROP`, `TRUNCATE`, `DELETE`/`UPDATE` without
//! `WHERE`), never transaction control nor other files. Each write runs in a transaction
//! of its own, with a time limit, and is committed only if it succeeds.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelunk_driver_api::{Dialect, DriverError, ExecEvent, ExecOptions};
use sqlparser::ast::{Statement, visit_statements};
use sqlparser::parser::Parser;

use crate::Session;
use crate::classify::classify_read_only;
use crate::guard::{Risk, destructive, sql_dialect};

#[derive(Debug, Clone)]
pub struct WriteLimits {
    pub timeout: Duration,
}

impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WriteResult {
    /// `None` for statements without a count (DDL).
    pub rows_affected: Option<u64>,
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A barrier refused it: nothing was sent to the server.
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("exceeded the time limit of {} s", .0.as_secs())]
    Timeout(Duration),
    #[error("{0}")]
    Database(#[from] DriverError),
    #[error("writes from agents are not available for {0} yet")]
    Unsupported(String),
}

/// Margin of the client timer over `statement_timeout` in PostgreSQL (as for reads).
const CLIENT_GRACE: Duration = Duration::from_secs(2);

/// Can an agent run `sql` as a write? `Err` says why not, in words for the agent.
pub fn check_write(dialect: &dyn Dialect, sql: &str) -> Result<(), String> {
    let engine = dialect.name();
    if dialect.split_statements(sql).len() != 1 {
        return Err("exactly one statement per call".into());
    }
    if classify_read_only(engine, sql).is_ok() {
        return Err("this is a read: use run_query".into());
    }
    if let Some(risk) = destructive(engine, sql) {
        return Err(match risk {
            Risk::Drop => "DROP is never run for agents".into(),
            Risk::Truncate => "TRUNCATE is never run for agents".into(),
            Risk::DeleteAll => "DELETE without WHERE is never run for agents".into(),
            Risk::UpdateAll => "UPDATE without WHERE is never run for agents".into(),
            Risk::UncheckedWrite | Risk::ProductionWrite => {
                "the statement could not be read, so its WHERE was not checked".into()
            }
        });
    }
    let parser = sql_dialect(engine);
    let statements = Parser::parse_sql(parser.as_ref(), sql)
        .map_err(|e| format!("the statement could not be read: {e}"))?;
    let mut refused = None;
    let _ = visit_statements(&statements, |statement| {
        refused = match statement {
            Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::Savepoint { .. }
            | Statement::ReleaseSavepoint { .. } => {
                Some("each write runs in a transaction of its own: no BEGIN, COMMIT or ROLLBACK")
            }
            Statement::AttachDatabase { .. }
            | Statement::AttachDuckDBDatabase { .. }
            | Statement::DetachDuckDBDatabase { .. }
            | Statement::Copy { .. }
            | Statement::Install { .. }
            | Statement::Load { .. } => {
                Some("statements that reach other files are never run for agents")
            }
            _ => None,
        };
        if refused.is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    match refused {
        Some(reason) => Err(reason.into()),
        None => Ok(()),
    }
}

impl Session {
    /// Runs one write statement from an agent in a transaction of its own (ADR-019).
    pub async fn run_write(
        &self,
        sql: &str,
        limits: &WriteLimits,
    ) -> Result<WriteResult, WriteError> {
        let engine = self.dialect().name();
        if !matches!(engine, "postgres" | "sqlite") {
            return Err(WriteError::Unsupported(engine.into()));
        }
        let sql = sql.trim().trim_end_matches(';').trim();
        check_write(self.dialect().as_ref(), sql).map_err(WriteError::Rejected)?;
        let started = Instant::now();
        let run = async {
            self.control("begin").await?;
            let result = async {
                if engine == "postgres" {
                    let timeout_ms = limits.timeout.as_millis().min(i32::MAX as u128);
                    self.control(&format!("set local statement_timeout = {timeout_ms}"))
                        .await?;
                }
                self.affected(sql).await
            }
            .await;
            match result {
                Ok(rows_affected) => {
                    self.control("commit").await?;
                    Ok(rows_affected)
                }
                Err(e) => {
                    // An error leaves the transaction aborted (PostgreSQL) or open: undone.
                    let _ = self.control("rollback").await;
                    Err(e)
                }
            }
        };
        let client_timeout = match engine {
            "postgres" => limits.timeout + CLIENT_GRACE,
            _ => limits.timeout,
        };
        let rows_affected = match tokio::time::timeout(client_timeout, run).await {
            // `statement_timeout` cancels the statement on the server.
            Ok(Err(DriverError::Cancelled)) => return Err(WriteError::Timeout(limits.timeout)),
            Ok(result) => result?,
            Err(_) => {
                let _ = self.cancel().await;
                self.rollback_until_clean().await;
                return Err(WriteError::Timeout(limits.timeout));
            }
        };
        Ok(WriteResult {
            rows_affected,
            elapsed: started.elapsed(),
        })
    }

    /// Runs a control statement and discards what it returns.
    async fn control(&self, sql: &str) -> Result<(), DriverError> {
        self.affected(sql).await.map(|_| ())
    }

    /// Runs a statement and returns its rows affected; any rows it returns are dropped.
    async fn affected(&self, sql: &str) -> Result<Option<u64>, DriverError> {
        let mut events = self
            .connection()
            .execute(sql, &ExecOptions::default())
            .await?;
        let mut rows_affected = None;
        while let Some(event) = events.next().await {
            if let ExecEvent::Done { rows_affected: n } = event? {
                rows_affected = n;
            }
        }
        Ok(rows_affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_driver_postgres::PostgresDialect;
    use spelunk_driver_sqlite::SqliteDialect;

    #[test]
    fn writes_that_hurt_or_escape_are_refused() {
        for (sql, why) in [
            ("drop table orders", "DROP"),
            ("truncate orders", "TRUNCATE"),
            ("delete from orders", "DELETE without WHERE"),
            ("update orders set total = 0", "UPDATE without WHERE"),
            (
                "with gone as (delete from orders returning *) insert into log select * from gone",
                "DELETE without WHERE",
            ),
            ("begin", "transaction"),
            ("commit", "transaction"),
            ("select * from orders", "run_query"),
            (
                "insert into a values (1); insert into a values (2)",
                "one statement",
            ),
            ("insert into a values (1); drop table a", "one statement"),
            ("delete frm orders where id = 1", "could not be read"),
        ] {
            let err = check_write(&PostgresDialect, sql).unwrap_err();
            assert!(err.contains(why), "{sql}: {err}");
        }
        assert!(
            check_write(&PostgresDialect, "copy orders to '/tmp/x'")
                .unwrap_err()
                .contains("other files")
        );
        assert!(
            check_write(&SqliteDialect, "attach 'x.db' as x")
                .unwrap_err()
                .contains("other files")
        );
    }

    #[test]
    fn ordinary_writes_pass() {
        for sql in [
            "insert into orders (id, total) values (1, 10)",
            "update orders set total = 0 where id = 1",
            "delete from orders where id = 1",
            "create table notes (id int primary key, body text)",
            "alter table notes add column at timestamp",
            "create index notes_at on notes (at)",
        ] {
            assert_eq!(check_write(&PostgresDialect, sql), Ok(()), "{sql}");
        }
    }
}
