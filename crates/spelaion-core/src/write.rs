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
use spelaion_config::{Config, ConnectionEntry, ConnectionFile, Engine, Source, WritePolicy};
use spelaion_driver_api::{Dialect, DriverError, ExecEvent, ExecOptions};
use sqlparser::ast::{AlterTableOperation, Statement, visit_statements};
use sqlparser::parser::Parser;

use crate::Session;
use crate::classify::{
    classify_read_only, reaches_outside, reject_mysql_executable_comments, reject_unicode_escapes,
};
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

/// What agents may do on a connection, with every rule applied (ADR-019, ADR-033). This is
/// the one place that decides it: the MCP server refuses writes with it, and the app and
/// `list_connections` show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAccess {
    /// `execute_write` may run, through the barriers of [`check_write`].
    ReadWrite,
    ReadOnly(ReadOnlyBecause),
}

/// Why agents may only read a connection, the first reason that applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnlyBecause {
    /// The user's read-only mode for every agent (ADR-033): it wins over any connection
    /// file, a trusted repository's and Production's included.
    AllAgents,
    /// A personal read-only restriction on this connection.
    ConnectionOverride,
    /// The connection is `read_only`, for the user too.
    Connection,
    /// `[mcp] writes = "deny"`, the default.
    WritesDenied,
    /// `[mcp] writes = "confirm"`, which is not supported yet (ADR-019).
    ConfirmUnsupported,
    /// The engine cannot run agents' writes yet: each runs in a transaction of its own, and the
    /// engine has none [`Session::run_write`] can open (ClickHouse, Trino, Presto).
    EngineWithoutWrites,
}

/// What agents may do on the connection in `file`. `all_read_only` is the user's read-only
/// mode for every agent (or `spelaiondb mcp --read-only`); it can only take writes away.
pub fn agent_access(file: &ConnectionFile, all_read_only: bool) -> AgentAccess {
    agent_access_with_override(file, all_read_only, false)
}

/// The same policy with a local connection restriction. An unreadable override must be
/// passed as read-only by the caller.
pub fn agent_access_with_override(
    file: &ConnectionFile,
    all_read_only: bool,
    connection_read_only: bool,
) -> AgentAccess {
    if all_read_only {
        return AgentAccess::ReadOnly(ReadOnlyBecause::AllAgents);
    }
    if connection_read_only {
        return AgentAccess::ReadOnly(ReadOnlyBecause::ConnectionOverride);
    }
    if file.read_only {
        return AgentAccess::ReadOnly(ReadOnlyBecause::Connection);
    }
    if !runs_agent_writes(file.engine) {
        return AgentAccess::ReadOnly(ReadOnlyBecause::EngineWithoutWrites);
    }
    match file.mcp.writes {
        WritePolicy::Allow => AgentAccess::ReadWrite,
        WritePolicy::Deny => AgentAccess::ReadOnly(ReadOnlyBecause::WritesDenied),
        WritePolicy::Confirm => AgentAccess::ReadOnly(ReadOnlyBecause::ConfirmUnsupported),
    }
}

/// Workspace key for personal MCP preferences. Personal connections share their settings
/// across folders. An unresolved repository root is an unreadable preference, never the
/// personal namespace.
pub fn agent_workspace(config: &Config, source: Source) -> Option<String> {
    match source {
        Source::User => Some(String::new()),
        Source::Workspace => {
            let root = std::fs::canonicalize(config.workspace_root.as_ref()?).ok()?;
            root.to_str().map(str::to_owned)
        }
    }
}

/// What an agent sees of a connection: nothing, or the connection with what it may do there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentView {
    /// Not `[mcp] exposed = true`.
    NotExposed,
    /// A repository's connection while the repository is not trusted (ADR-022).
    UntrustedRepository,
    /// An engine with no agents' barrier yet: one whose language is not SQL (ADR-056, point
    /// 6), Oracle (ADR-059, point 2), libSQL (ADR-058) and the Flight SQL engines (ADR-041):
    /// `has_read_barrier`. Never visible, whatever `[mcp]` says.
    EngineWithoutBarrier,
    /// A DuckDB connection to a folder of files (SDB-316): agents' DuckDB barrier turns off the
    /// file access reading it needs, so it is never visible, whatever `[mcp]` says.
    FileFolder,
    Visible(AgentAccess),
}

/// Whether the core has a read barrier for `engine`, so that it offers read-only connections
/// and agents there. An engine whose language is not SQL waits for its own (ADR-056, point 6),
/// and Oracle for one that stands up to tests that try to get around it (ADR-059, point 2: a
/// function with an autonomous transaction writes from inside a `SELECT`), libSQL (ADR-058) and
/// the Flight SQL engines, which may be any server. The configuration refuses `read_only` on the
/// same engines: both read `Engine::has_read_barrier`.
pub fn has_read_barrier(engine: Engine) -> bool {
    engine.has_read_barrier()
}

/// Whether agents' writes can run on `engine`: it has agents' barriers, and its dialect has a
/// transaction [`Session::run_write`] opens (the same [`write_begin`] it reads), so a write is
/// committed only if it succeeds. The app's form and [`agent_access`] read it.
pub fn runs_agent_writes(engine: Engine) -> bool {
    has_read_barrier(engine)
        && crate::dialect_of(engine).is_some_and(|dialect| write_begin(dialect.name()).is_some())
}

/// The statement that opens an agent's write transaction on a dialect, or `None` where agents'
/// writes cannot run.
fn write_begin(dialect: &str) -> Option<&'static str> {
    match dialect {
        "postgres" | "sqlite" => Some("begin"),
        "mysql" => Some("start transaction"),
        "sqlserver" | "duckdb" => Some("begin transaction"),
        _ => None,
    }
}

/// What agents see of `entry` (ADR-008, ADR-022) and may do on it (ADR-019, ADR-033).
pub fn agent_view(
    entry: &ConnectionEntry,
    repository_trusted: bool,
    all_read_only: bool,
) -> AgentView {
    agent_view_with_override(entry, repository_trusted, all_read_only, false)
}

/// What agents see and may do, including a local connection restriction.
pub fn agent_view_with_override(
    entry: &ConnectionEntry,
    repository_trusted: bool,
    all_read_only: bool,
    connection_read_only: bool,
) -> AgentView {
    if !has_read_barrier(entry.file.engine) {
        return AgentView::EngineWithoutBarrier;
    }
    if entry.file.file_folder {
        return AgentView::FileFolder;
    }
    if !entry.file.mcp.exposed {
        return AgentView::NotExposed;
    }
    match entry.id.source {
        Source::Workspace if !repository_trusted => AgentView::UntrustedRepository,
        Source::Workspace | Source::User => AgentView::Visible(agent_access_with_override(
            &entry.file,
            all_read_only,
            connection_read_only,
        )),
    }
}

impl ReadOnlyBecause {
    /// Why a write on `connection` was refused, in words for the agent.
    pub fn refusal(self, connection: &str) -> String {
        match self {
            ReadOnlyBecause::AllAgents => format!(
                "connection {connection:?} is read-only: the user made every agent read-only (SpelaionDB's MCP server page)"
            ),
            ReadOnlyBecause::ConnectionOverride => format!(
                "connection {connection:?} is read-only for agents under its local MCP restriction"
            ),
            ReadOnlyBecause::Connection => {
                format!("connection {connection:?} is read-only, for agents and for the user")
            }
            ReadOnlyBecause::WritesDenied => format!(
                "connection {connection:?} is read-only for agents; the user can allow writes in its settings"
            ),
            ReadOnlyBecause::ConfirmUnsupported => format!(
                "connection {connection:?} asks to confirm each write, which is not supported yet; the user can set writes to allow or deny"
            ),
            ReadOnlyBecause::EngineWithoutWrites => format!(
                "connection {connection:?} is read-only for agents: writes from agents are not available for its engine yet"
            ),
        }
    }
}

/// Margin of the client timer over `statement_timeout` in PostgreSQL (as for reads).
const CLIENT_GRACE: Duration = Duration::from_secs(2);

/// Can an agent run `sql` as a write? `Err` says why not, in words for the agent.
pub fn check_write(dialect: &dyn Dialect, sql: &str) -> Result<(), String> {
    let engine = dialect.name();
    reject_mysql_executable_comments(engine, sql)?;
    reject_unicode_escapes(engine, sql)?;
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
        let Some(begin) = write_begin(engine) else {
            return Err(WriteError::Unsupported(engine.into()));
        };
        let sql = sql.trim().trim_end_matches(';').trim();
        check_write(self.dialect().as_ref(), sql).map_err(WriteError::Rejected)?;
        // ADR-019's transaction of its own must hold: not on a server that accepts `BEGIN` and
        // ignores it (RisingWave, SDB-302), where a failed write could stay half applied.
        if let Some(reason) = self.atomic_writes_refusal() {
            return Err(WriteError::Rejected(format!(
                "{reason}, and each write from an agent runs in a transaction of its own"
            )));
        }
        let started = Instant::now();
        // Strings are read by the server as `check_write` read them (SDB-116).
        if engine == "mysql" {
            match tokio::time::timeout(limits.timeout, self.mysql_default_quoting()).await {
                Ok(checked) => checked.map_err(WriteError::Rejected)?,
                Err(_) => {
                    let _ = self.cancel().await;
                    return Err(WriteError::Timeout(limits.timeout));
                }
            }
        }
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
    use spelaion_dialect::DuckdbDialect;
    use spelaion_dialect::MysqlDialect;
    use spelaion_dialect::PostgresDialect;
    use spelaion_dialect::SqlServerDialect;
    use spelaion_dialect::SqliteDialect;

    #[test]
    fn postgres_writes_refuse_unicode_escapes_and_sql_given_as_text() {
        for (sql, why) in [
            (
                r#"insert into log select U&"pg_r\0065ad_file"('/etc/passwd')"#,
                "Unicode escapes",
            ),
            (
                r#"update t set v = U&"set_c\006Fnfig"('a', 'b', false) where id = 1"#,
                "Unicode escapes",
            ),
            (
                "insert into log select query_to_xml('select pg_terminate_backend(1)', true, true, '')",
                "query_to_xml",
            ),
            (
                "update t set v = (select count(*) from ts_stat('select 1::tsvector')) where id = 1",
                "ts_stat",
            ),
            (
                "insert into log select * from dblink_fetch('c', 1) as f(v text)",
                "dblink_fetch",
            ),
            (
                "update t set v = pg_advisory_unlock_all()::text where id = 1",
                "pg_advisory_unlock_all",
            ),
        ] {
            let err = check_write(&PostgresDialect, sql).expect_err(sql);
            assert!(err.contains(why), "{sql}: {err}");
        }
    }

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

    fn connection(extra: &str) -> ConnectionFile {
        let base = "schema_version = 1\nname = \"Orders\"\nengine = \"postgres\"\nhost = \"db\"\nenv = \"prod\"\n";
        toml::from_str(&format!("{base}{extra}")).unwrap()
    }

    #[test]
    fn read_only_for_every_agent_wins_over_any_connection() {
        // A connection that lets agents write, Production included (ADR-019): the user's
        // read-only mode takes the writes away, and nothing in the file brings them back.
        let writable = connection("[mcp]\nexposed = true\nwrites = \"allow\"\n");
        assert_eq!(agent_access(&writable, false), AgentAccess::ReadWrite);
        assert_eq!(
            agent_access(&writable, true),
            AgentAccess::ReadOnly(ReadOnlyBecause::AllAgents)
        );
        for extra in [
            "",
            "read_only = true\n[mcp]\nwrites = \"allow\"\n",
            "[mcp]\nwrites = \"confirm\"\n",
            "[mcp]\nwrites = \"deny\"\n",
        ] {
            assert_eq!(
                agent_access(&connection(extra), true),
                AgentAccess::ReadOnly(ReadOnlyBecause::AllAgents),
                "{extra}"
            );
        }
        assert!(
            ReadOnlyBecause::AllAgents
                .refusal("prod/orders")
                .contains("every agent read-only")
        );
    }

    #[test]
    fn without_the_switch_each_connection_decides_as_before() {
        assert_eq!(
            agent_access(&connection(""), false),
            AgentAccess::ReadOnly(ReadOnlyBecause::WritesDenied)
        );
        assert_eq!(
            agent_access(
                &connection("read_only = true\n[mcp]\nwrites = \"allow\"\n"),
                false
            ),
            AgentAccess::ReadOnly(ReadOnlyBecause::Connection)
        );
        assert_eq!(
            agent_access(&connection("[mcp]\nwrites = \"confirm\"\n"), false),
            AgentAccess::ReadOnly(ReadOnlyBecause::ConfirmUnsupported)
        );
    }

    /// SDB-325: an engine whose writes `run_write` cannot open a transaction for is read-only
    /// for agents whatever `[mcp]` says, so neither the app nor `list_connections` offers
    /// writes that would all fail.
    #[test]
    fn an_engine_without_agents_writes_is_read_only_for_agents() {
        let on = |engine: &str, extra: &str| {
            let file = format!(
                "schema_version = 1\nname = \"Events\"\nengine = \"{engine}\"\nhost = \"db\"\n{extra}"
            );
            toml::from_str::<ConnectionFile>(&file).unwrap()
        };
        let allow = "[mcp]\nexposed = true\nwrites = \"allow\"\n";
        for engine in ["clickhouse", "trino", "presto"] {
            for extra in [
                allow,
                "[mcp]\nwrites = \"deny\"\n",
                "[mcp]\nwrites = \"confirm\"\n",
            ] {
                assert_eq!(
                    agent_access(&on(engine, extra), false),
                    AgentAccess::ReadOnly(ReadOnlyBecause::EngineWithoutWrites),
                    "{engine}: {extra}"
                );
            }
            // The user's switch and the connection's own read-only mode still come first.
            assert_eq!(
                agent_access(&on(engine, allow), true),
                AgentAccess::ReadOnly(ReadOnlyBecause::AllAgents)
            );
            assert_eq!(
                agent_access(&on(engine, &format!("read_only = true\n{allow}")), false),
                AgentAccess::ReadOnly(ReadOnlyBecause::Connection)
            );
        }
        for engine in ["postgres", "mysql", "mariadb", "sqlserver"] {
            assert_eq!(
                agent_access(&on(engine, allow), false),
                AgentAccess::ReadWrite,
                "{engine}"
            );
        }
        assert!(
            ReadOnlyBecause::EngineWithoutWrites
                .refusal("events")
                .contains("not available for its engine")
        );
    }

    /// What `agent_access` says matches what `run_write` does: writes are offered exactly on the
    /// engines whose dialect has a transaction to open, and only where agents have barriers.
    #[test]
    fn agents_writes_are_offered_exactly_where_run_write_opens_a_transaction() {
        for engine in Engine::ALL {
            // Never writes where agents cannot even read.
            assert!(
                has_read_barrier(engine) || !runs_agent_writes(engine),
                "{}",
                engine.as_str()
            );
        }
        for engine in [
            Engine::Postgres,
            Engine::Mysql,
            Engine::Mariadb,
            Engine::Sqlserver,
            Engine::Sqlite,
            Engine::Duckdb,
        ] {
            assert!(runs_agent_writes(engine), "{}", engine.as_str());
        }
        for engine in [Engine::Clickhouse, Engine::Trino, Engine::Presto] {
            assert!(!runs_agent_writes(engine), "{}", engine.as_str());
        }
    }

    #[test]
    fn agents_see_only_exposed_connections_of_trusted_repositories() {
        let entry = |source, extra: &str| ConnectionEntry {
            id: spelaion_config::ConnectionId {
                source,
                path: "orders".into(),
            },
            file_path: "orders.toml".into(),
            file: connection(extra),
        };
        let writable = "[mcp]\nexposed = true\nwrites = \"allow\"\n";
        assert_eq!(
            agent_view(&entry(Source::Workspace, ""), true, false),
            AgentView::NotExposed
        );
        assert_eq!(
            agent_view(&entry(Source::Workspace, writable), false, false),
            AgentView::UntrustedRepository
        );
        assert_eq!(
            agent_view(&entry(Source::Workspace, writable), true, false),
            AgentView::Visible(AgentAccess::ReadWrite)
        );
        assert_eq!(
            agent_view(&entry(Source::Workspace, writable), true, true),
            AgentView::Visible(AgentAccess::ReadOnly(ReadOnlyBecause::AllAgents))
        );
        // A personal connection needs no trust.
        assert_eq!(
            agent_view(&entry(Source::User, writable), false, false),
            AgentView::Visible(AgentAccess::ReadWrite)
        );
    }

    #[test]
    fn an_engine_without_its_barrier_is_never_shown_to_agents() {
        // ADR-056, point 6: MongoDB has no agents' barrier yet, whatever `[mcp]` says; nor have
        // the Flight SQL engines, which run SQL on any server (ADR-041).
        for engine in [
            "mongodb",
            "redis",
            "oracle",
            "libsql",
            "cassandra",
            "flightsql",
            "dremio",
            "gizmosql",
            "hive",
            "spark",
            "kyuubi",
            "impala",
        ] {
            let connection = |extra: &str| {
                ConnectionEntry {
                id: spelaion_config::ConnectionId {
                    source: Source::User,
                    path: "shop".into(),
                },
                file_path: "shop.toml".into(),
                file: toml::from_str(&format!(
                    "schema_version = 1\nname = \"Shop\"\nengine = \"{engine}\"\nhost = \"db\"\n{extra}"
                ))
                .unwrap(),
            }
            };
            for extra in [
                "",
                "[mcp]\nexposed = true\n",
                "[mcp]\nexposed = true\nwrites = \"allow\"\n",
            ] {
                for all_read_only in [false, true] {
                    assert_eq!(
                        agent_view(&connection(extra), true, all_read_only),
                        AgentView::EngineWithoutBarrier,
                        "{engine}: {extra}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_folder_of_files_is_never_shown_to_agents() {
        // SDB-316: reading the folder needs the file access agents' DuckDB barrier turns off,
        // so neither `[mcp]` nor a trusted repository makes it visible.
        let folder = |extra: &str| {
            ConnectionEntry {
            id: spelaion_config::ConnectionId {
                source: Source::Workspace,
                path: "exports".into(),
            },
            file_path: "exports.toml".into(),
            file: toml::from_str(&format!(
                "schema_version = 1\nname = \"Exports\"\nengine = \"duckdb\"\ndatabase = \"exports\"\nfile_folder = true\n{extra}"
            ))
            .unwrap(),
        }
        };
        for extra in [
            "",
            "[mcp]\nexposed = true\n",
            "[mcp]\nexposed = true\nwrites = \"allow\"\n",
        ] {
            for all_read_only in [false, true] {
                assert_eq!(
                    agent_view(&folder(extra), true, all_read_only),
                    AgentView::FileFolder,
                    "{extra}"
                );
            }
        }
    }
}
