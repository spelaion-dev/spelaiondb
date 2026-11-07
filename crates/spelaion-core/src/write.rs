//! Writes from agents (ADR-019, ADR-020), on connections whose user allowed them. The
//! barriers live here, not in the MCP server: one statement the parser understands, never a
//! read (that is `run_read_only`), never destructive (`DROP`, `TRUNCATE`, `DELETE`/`UPDATE`
//! without `WHERE`), and only kinds on an allow list: data changes and `CREATE`/`ALTER` of
//! tables, views, indexes, schemas and sequences. Never transaction control, procedures,
//! permissions, settings, other files nor other servers. Each write runs in a transaction of
//! its own, with a time limit, and is committed only if it succeeds.

use std::ops::ControlFlow;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_driver_api::{Dialect, DriverError, ExecEvent, ExecOptions};
use sqlparser::ast::{AlterTableOperation, Statement, visit_statements};
use sqlparser::parser::Parser;

use crate::Session;
use crate::classify::{classify_read_only, reaches_outside, reject_mysql_executable_comments};
use crate::guard::{Risk, destructive, sql_dialect};
use crate::readonly::DUCKDB_NO_EXTERNAL_ACCESS;

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
    reject_mysql_executable_comments(engine, sql)?;
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
            Risk::UncheckedWrite | Risk::DataChange | Risk::Execution => {
                "the statement could not be read, so its WHERE was not checked".into()
            }
        });
    }
    let parser = sql_dialect(engine);
    let statements = Parser::parse_sql(parser.as_ref(), sql)
        .map_err(|e| format!("the statement could not be read: {e}"))?;
    let mut refused = None;
    let _ = visit_statements(&statements, |statement| {
        refused = allowed(statement).err();
        if refused.is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    if let Some(reason) = refused {
        return Err(reason);
    }
    match reaches_outside(engine, &statements) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// The allow list, checked on the statement and on every statement nested in it.
fn allowed(statement: &Statement) -> Result<(), String> {
    match statement {
        Statement::CreateSchema {
            or_replace: true, ..
        } => Err("replacing an existing schema is never run for agents".into()),
        Statement::CreateTable(table) if table.or_replace => {
            Err("replacing an existing table is never run for agents".into())
        }
        Statement::CreateView(view) if view.materialized && (view.or_replace || view.or_alter) => {
            Err("replacing a materialized view is never run for agents".into())
        }
        Statement::AlterTable(table) => {
            if table.location.is_some()
                || table.operations.is_empty()
                || table.operations.iter().any(|operation| {
                    !matches!(
                        operation,
                        AlterTableOperation::AddColumn { .. }
                            | AlterTableOperation::AddConstraint { .. }
                            | AlterTableOperation::RenameColumn { .. }
                            | AlterTableOperation::RenameTable { .. }
                    )
                })
            {
                Err("ALTER TABLE operation is not allowed for agents".into())
            } else {
                Ok(())
            }
        }
        Statement::Insert { .. }
        | Statement::Update { .. }
        | Statement::Delete { .. }
        | Statement::Merge { .. }
        | Statement::Query { .. }
        | Statement::CreateTable { .. }
        | Statement::CreateView { .. }
        | Statement::CreateIndex { .. }
        | Statement::CreateSchema { .. }
        | Statement::CreateSequence { .. }
        | Statement::AlterIndex { .. }
        | Statement::AlterView { .. }
        | Statement::Comment { .. } => Ok(()),
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => {
            Err("each write runs in a transaction of its own: no BEGIN, COMMIT or ROLLBACK".into())
        }
        Statement::AttachDatabase { .. }
        | Statement::AttachDuckDBDatabase { .. }
        | Statement::DetachDuckDBDatabase { .. }
        | Statement::Copy { .. }
        | Statement::Install { .. }
        | Statement::Load { .. } => {
            Err("statements that reach other files are never run for agents".into())
        }
        other => {
            let text = other.to_string();
            let kind = text.split_whitespace().next().unwrap_or("this statement");
            Err(format!(
                "{} is not run for agents: only INSERT, UPDATE, DELETE, MERGE and CREATE or ALTER of tables, views, indexes, schemas and sequences",
                kind.to_ascii_uppercase()
            ))
        }
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
        let begin = match engine {
            "postgres" | "sqlite" => "begin",
            "mysql" => "start transaction",
            "sqlserver" | "duckdb" => "begin transaction",
            other => return Err(WriteError::Unsupported(other.into())),
        };
        let sql = sql.trim().trim_end_matches(';').trim();
        check_write(self.dialect().as_ref(), sql).map_err(WriteError::Rejected)?;
        let started = Instant::now();
        let run = async {
            if engine == "duckdb" {
                self.control(DUCKDB_NO_EXTERNAL_ACCESS).await?;
            }
            self.control(begin).await?;
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
                    let _ = self.control(self.rollback_statement()).await;
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
    use spelaion_driver_duckdb::DuckdbDialect;
    use spelaion_driver_mysql::MysqlDialect;
    use spelaion_driver_postgres::PostgresDialect;
    use spelaion_driver_sqlite::SqliteDialect;
    use spelaion_driver_sqlserver::SqlServerDialect;

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
            ("grant all on orders to public", "GRANT is not run"),
            ("call do_stuff()", "CALL is not run"),
            ("set session_replication_role = replica", "SET is not run"),
            (
                "create function f() returns int as 'select 1' language sql",
                "CREATE is not run",
            ),
            (
                "insert into stolen select pg_read_file('/etc/passwd')",
                "outside the database",
            ),
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
        for (dialect, sql, why) in [
            (
                &MysqlDialect as &dyn Dialect,
                "insert into t select load_file('/etc/passwd')",
                "outside the database",
            ),
            (&MysqlDialect, "lock tables t write", "not run"),
            (&MysqlDialect, "load data infile 'x' into table t", ""),
            (
                &SqlServerDialect,
                "exec sp_configure 'xp_cmdshell', 1",
                "not run",
            ),
            (
                &SqlServerDialect,
                "insert into t select * from other.db.dbo.t",
                "another server",
            ),
            (
                &SqlServerDialect,
                "insert into t select * from openrowset('SQLNCLI', 'x', 'select 1')",
                "outside the database",
            ),
            (&DuckdbDialect, "copy t to 'out.csv'", "other files"),
            (&DuckdbDialect, "install httpfs", "other files"),
            (
                &DuckdbDialect,
                "create table t as select * from read_csv('/etc/passwd')",
                "outside the database",
            ),
            (
                &DuckdbDialect,
                "create secret s (type s3, key_id 'x')",
                "not run",
            ),
        ] {
            let err = check_write(dialect, sql).unwrap_err();
            assert!(err.contains(why), "{sql}: {err}");
        }
    }

    #[test]
    fn mysql_executable_comments_cannot_hide_write_predicates() {
        for sql in [
            "delete from orders /*!99999 where id = 1 */",
            "update orders set total = 0 /*!99999 where id = 1 */",
            "delete from orders /*M!999999 where id = 1 */",
            "update orders set total = 0 /*m!999999 where id = 1 */",
            "delete from orders /*! where id = 1 */",
            "insert into orders values (1, '/*!')",
        ] {
            let err = check_write(&MysqlDialect, sql).unwrap_err();
            assert!(err.contains("executable comment"), "{sql}: {err}");
        }
        for sql in [
            "delete from orders /* ordinary comment */ where id = 1",
            "update orders set total = 0 where id = 1",
        ] {
            assert_eq!(check_write(&MysqlDialect, sql), Ok(()), "{sql}");
        }
    }

    #[test]
    fn destructive_table_ddl_is_refused() {
        for (dialect, sql) in [
            (
                &DuckdbDialect as &dyn Dialect,
                "create or replace table t (id int)",
            ),
            (&MysqlDialect, "create or replace table t (id int)"),
            (&MysqlDialect, "create or replace schema important"),
            (&MysqlDialect, "create or replace database important"),
            (&DuckdbDialect, "create or replace schema important"),
            (&PostgresDialect, "alter table t drop column body"),
            (&SqliteDialect, "alter table t drop column body"),
            (&MysqlDialect, "alter table t drop column body"),
            (&SqlServerDialect, "alter table t drop column body"),
            (&DuckdbDialect, "alter table t drop column body"),
            (
                &PostgresDialect,
                "alter table t add column a int, drop column body",
            ),
            (&MysqlDialect, "alter table t modify column body varchar(1)"),
            (&MysqlDialect, "alter table t drop partition p0"),
            (
                &PostgresDialect,
                "alter table t alter column body type varchar(1)",
            ),
            (&PostgresDialect, "alter table t disable row level security"),
        ] {
            assert!(check_write(dialect, sql).is_err(), "must reject: {sql}");
        }
        let error = check_write(&MysqlDialect, "create or replace schema important").unwrap_err();
        assert!(error.contains("replacing an existing schema"), "{error}");
        for (dialect, sql) in [
            (
                &MysqlDialect as &dyn Dialect,
                "alter table t add column added int",
            ),
            (&SqliteDialect, "alter table t add column added int"),
            (
                &PostgresDialect,
                "alter table t rename column body to contents",
            ),
            (&DuckdbDialect, "create table t (id int)"),
        ] {
            assert_eq!(check_write(dialect, sql), Ok(()), "{sql}");
        }
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
            "comment on table notes is 'from an agent'",
            "create view recent as select * from notes",
        ] {
            assert_eq!(check_write(&PostgresDialect, sql), Ok(()), "{sql}");
        }
        for (dialect, sql) in [
            (
                &MysqlDialect as &dyn Dialect,
                "insert into `orders` (`id`) values (1)",
            ),
            (
                &MysqlDialect,
                "update orders set total = 0 where id = 1 limit 1",
            ),
            (&SqlServerDialect, "insert into dbo.orders (id) values (1)"),
            (
                &SqlServerDialect,
                "update dbo.orders set total = 0 where id = 1",
            ),
            (
                &DuckdbDialect,
                "create table t as select * from range(3) r(i)",
            ),
            (&DuckdbDialect, "insert into t select * from range(3) r(i)"),
        ] {
            assert_eq!(check_write(dialect, sql), Ok(()), "{sql}");
        }
    }
}
