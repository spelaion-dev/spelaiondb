//! Statement classification for read-only mode (ADR-008, layer 4).
//!
//! Allow list: only what is recognised as a read passes; whatever cannot be parsed is
//! rejected. Functions that reach outside the database (files, other servers, the session:
//! [`reaches_outside`]) are refused by name. It is not the only barrier: a user function that
//! writes looks like a read. That is why it also runs in a read-only (or always rolled back)
//! transaction and a read-only database user is recommended.

use std::ops::ControlFlow;

use sqlparser::ast::{
    Expr, ObjectName, Query, SetExpr, Statement, Value as SqlValue, Visit, Visitor,
};
use sqlparser::parser::Parser;

use crate::guard::sql_dialect;

/// Kind of permitted read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadKind {
    /// `SELECT`, `VALUES`, `WITH … SELECT`.
    Query,
    /// `EXPLAIN` without `ANALYZE`.
    Explain,
    /// `SHOW …`.
    Show,
}

/// Is `sql` (a single statement) a read? `Err` explains why not.
pub fn classify_read_only(engine: &str, sql: &str) -> Result<ReadKind, String> {
    let dialect = sql_dialect(engine);
    let statements = Parser::parse_sql(dialect.as_ref(), sql)
        .map_err(|e| format!("could not parse the statement: {e}"))?;
    let statement = match statements.as_slice() {
        [one] => one,
        [] => return Err("there is no statement".into()),
        _ => return Err("only one statement per query is allowed".into()),
    };

    let kind = match statement {
        Statement::Query(query) => check_query(query).map(|()| ReadKind::Query),
        Statement::Explain {
            analyze,
            statement: inner,
            options,
            ..
        } => {
            let analyze_option = options.iter().flatten().any(|o| {
                o.name.value.eq_ignore_ascii_case("analyze")
                    && !matches!(&o.arg, Some(Expr::Value(v)) if matches!(v.value, SqlValue::Boolean(false)))
            });
            if *analyze || analyze_option {
                return Err(
                    "EXPLAIN ANALYZE runs the statement; use EXPLAIN without ANALYZE".into(),
                );
            }
            match inner.as_ref() {
                Statement::Query(query) => check_query(query).map(|()| ReadKind::Explain),
                other => Err(format!(
                    "EXPLAIN of a statement that is not a read ({})",
                    kind_name(other)
                )),
            }
        }
        Statement::ShowVariable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowTables { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowCharset(_) => Ok(ReadKind::Show),
        other => Err(format!("{} is not a read statement", kind_name(other))),
    }?;

    // Also, no statement nested anywhere in the tree (a safety net in case a new form of the
    // grammar escapes the checks above).
    let mut nested = NestedStatements { seen: 0 };
    if let ControlFlow::Break(kind) = statement.visit(&mut nested) {
        return Err(format!("contains a nested {kind} statement"));
    }
    if let Some(reason) = reaches_outside(engine, std::slice::from_ref(statement)) {
        return Err(reason);
    }
    Ok(kind)
}

/// MySQL and MariaDB may execute comment bodies differently depending on server version.
/// Reject the markers even inside quoted text: SQL modes change quote/escape semantics, so
/// attempting to distinguish literals here could let the parser and server disagree again.
pub(crate) fn reject_mysql_executable_comments(engine: &str, sql: &str) -> Result<(), String> {
    if engine == "mysql"
        && (sql.as_bytes().windows(3).any(|w| w == b"/*!")
            || sql
                .as_bytes()
                .windows(4)
                .any(|w| w[0..2] == *b"/*" && w[2].eq_ignore_ascii_case(&b'm') && w[3] == b'!'))
    {
        return Err(
            "MySQL and MariaDB executable comment markers are not allowed for agents".into(),
        );
    }
    Ok(())
}

/// Functions that reach outside the database: files on the server, other servers, environment
/// variables, or locks and settings that outlive the statement. Matched by their last name
/// part, case-insensitively, as calls and as table functions.
fn outside_functions(engine: &str) -> &'static [&'static str] {
    match engine {
        "postgres" => &[
            "pg_read_file",
            "pg_read_binary_file",
            "pg_ls_dir",
            "pg_stat_file",
            "pg_ls_logdir",
            "pg_ls_waldir",
            "pg_ls_tmpdir",
            "pg_ls_archive_statusdir",
            "lo_import",
            "lo_export",
            "dblink",
            "dblink_exec",
            "dblink_connect",
            "dblink_connect_u",
            "dblink_send_query",
            "pg_terminate_backend",
            "pg_cancel_backend",
            "pg_reload_conf",
            "pg_rotate_logfile",
            "set_config",
            "pg_advisory_lock",
            "pg_advisory_lock_shared",
            "pg_try_advisory_lock",
            "pg_try_advisory_lock_shared",
        ],
        "sqlite" => &[
            "load_extension",
            "readfile",
            "writefile",
            "edit",
            "fts3_tokenizer",
        ],
        "mysql" => &[
            "load_file",
            "get_lock",
            "release_lock",
            "release_all_locks",
            "sys_exec",
            "sys_eval",
        ],
        "sqlserver" => &["openrowset", "opendatasource", "openquery"],
        // The engine is the main barrier (`enable_external_access = false`); these are refused
        // before reaching it, with a clearer message.
        "duckdb" => &[
            "read_csv",
            "read_csv_auto",
            "sniff_csv",
            "read_parquet",
            "parquet_scan",
            "parquet_metadata",
            "parquet_schema",
            "read_json",
            "read_json_auto",
            "read_json_objects",
            "read_ndjson",
            "read_ndjson_auto",
            "read_ndjson_objects",
            "read_text",
            "read_blob",
            "read_xlsx",
            "glob",
            "getenv",
            "sqlite_scan",
            "postgres_scan",
            "mysql_scan",
            "iceberg_scan",
            "delta_scan",
        ],
        _ => &[],
    }
}

/// Why `statements` reach outside the database, if they do (see [`outside_functions`]). In
/// SQL Server a four-part name (`server.database.schema.table`) goes to a linked server.
pub(crate) fn reaches_outside(engine: &str, statements: &[Statement]) -> Option<String> {
    let mut visitor = Outside {
        functions: outside_functions(engine),
        linked_servers: engine == "sqlserver",
    };
    statements
        .iter()
        .find_map(|statement| match statement.visit(&mut visitor) {
            ControlFlow::Break(reason) => Some(reason),
            ControlFlow::Continue(()) => None,
        })
}

struct Outside {
    functions: &'static [&'static str],
    linked_servers: bool,
}

impl Outside {
    fn check(&self, name: &ObjectName) -> ControlFlow<String> {
        let last = name
            .0
            .last()
            .and_then(|part| part.as_ident())
            .map(|ident| ident.value.to_ascii_lowercase());
        if let Some(last) = last
            && self.functions.contains(&last.as_str())
        {
            return ControlFlow::Break(format!(
                "{last}() reaches outside the database (files, other servers or the session)"
            ));
        }
        ControlFlow::Continue(())
    }
}

impl Visitor for Outside {
    type Break = String;

    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<String> {
        if self.linked_servers && relation.0.len() >= 4 {
            return ControlFlow::Break(format!(
                "{relation} names another server (a linked server)"
            ));
        }
        // `FROM read_csv('…')` is a relation with arguments.
        self.check(relation)
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<String> {
        match expr {
            Expr::Function(function) => self.check(&function.name),
            _ => ControlFlow::Continue(()),
        }
    }
}

fn check_query(query: &Query) -> Result<(), String> {
    if !query.locks.is_empty() {
        return Err("SELECT … FOR UPDATE/SHARE locks rows".into());
    }
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            check_query(&cte.query)?;
        }
    }
    check_set_expr(&query.body)
}

fn check_set_expr(body: &SetExpr) -> Result<(), String> {
    match body {
        SetExpr::Select(select) => {
            if select.into.is_some() {
                return Err("SELECT … INTO creates a table".into());
            }
            Ok(())
        }
        SetExpr::Query(query) => check_query(query),
        SetExpr::SetOperation { left, right, .. } => {
            check_set_expr(left)?;
            check_set_expr(right)
        }
        SetExpr::Values(_) | SetExpr::Table(_) => Ok(()),
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) => {
            Err("contains a write".into())
        }
    }
}

/// Counts statements in the tree: the first is the statement itself; any other is nested.
struct NestedStatements {
    seen: usize,
}

impl Visitor for NestedStatements {
    type Break = String;

    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<String> {
        self.seen += 1;
        let is_explained_query = self.seen == 2 && matches!(statement, Statement::Query(_));
        if self.seen > 1 && !is_explained_query {
            return ControlFlow::Break(kind_name(statement));
        }
        ControlFlow::Continue(())
    }
}

/// Short name of the statement kind (`INSERT`, `DROP`…), for messages.
fn kind_name(statement: &Statement) -> String {
    let text = statement.to_string();
    let words: Vec<&str> = text.split_whitespace().take(2).collect();
    match words.as_slice() {
        [first, second]
            if ["CREATE", "DROP", "ALTER"].contains(&first.to_ascii_uppercase().as_str()) =>
        {
            format!(
                "{} {}",
                first.to_ascii_uppercase(),
                second.to_ascii_uppercase()
            )
        }
        [first, ..] => first.to_ascii_uppercase(),
        [] => "unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(sql: &str) -> Result<ReadKind, String> {
        classify_read_only("postgres", sql)
    }

    #[test]
    fn plain_reads_are_allowed() {
        for sql in [
            "select 1",
            "SELECT * FROM orders o JOIN customer c ON c.id = o.customer_id WHERE o.total > 10 ORDER BY 1 LIMIT 5",
            "with recent as (select * from orders where created > now() - interval '1 day') select count(*) from recent",
            "values (1), (2)",
            "select 1 union all select 2",
            "select (select max(id) from orders), exists (select 1 from customer)",
        ] {
            assert_eq!(pg(sql), Ok(ReadKind::Query), "{sql}");
        }
        assert_eq!(pg("explain select * from orders"), Ok(ReadKind::Explain));
        assert_eq!(pg("explain (format json) select 1"), Ok(ReadKind::Explain));
        assert_eq!(pg("show search_path"), Ok(ReadKind::Show));
        assert_eq!(
            classify_read_only("sqlite", "select * from t limit 3"),
            Ok(ReadKind::Query)
        );
    }

    #[test]
    fn mysql_executable_comment_marker_check_is_conservative() {
        for sql in [
            "select 1 /*!99999 + 1 */",
            "explain select 1 /*M!100000 + 1 */",
            "show tables /*m!100000 */",
            "select 1 /*M! , get_lock('agent_comment_test', 0) */",
            "select '/*!'",
        ] {
            let err = reject_mysql_executable_comments("mysql", sql).unwrap_err();
            assert!(err.contains("executable comment"), "{sql}: {err}");
        }
        assert_eq!(
            reject_mysql_executable_comments("mysql", "select 1 /* ordinary comment */"),
            Ok(())
        );
        assert_eq!(
            classify_read_only("postgres", "select '/*!'"),
            Ok(ReadKind::Query)
        );
    }

    #[test]
    fn writes_and_ddl_are_rejected() {
        for sql in [
            "insert into t values (1)",
            "update t set a = 1",
            "delete from t",
            "drop table t",
            "create table t (a int)",
            "alter table t add column b int",
            "truncate t",
            "grant select on t to bob",
            "copy t to '/tmp/x'",
            "vacuum",
            "set transaction read write",
            "set default_transaction_read_only = off",
            "begin",
            "commit",
            "call do_stuff()",
        ] {
            assert!(pg(sql).is_err(), "should reject: {sql}");
        }
    }

    #[test]
    fn classic_traps_are_caught() {
        // EXPLAIN ANALYZE runs the statement, in both forms.
        assert!(
            pg("explain analyze delete from t")
                .unwrap_err()
                .contains("ANALYZE")
        );
        assert!(
            pg("explain analyze select 1")
                .unwrap_err()
                .contains("ANALYZE")
        );
        assert!(
            pg("explain (analyze) select 1")
                .unwrap_err()
                .contains("ANALYZE")
        );
        assert!(
            pg("explain (analyze true, buffers) select 1")
                .unwrap_err()
                .contains("ANALYZE")
        );
        assert_eq!(
            pg("explain (analyze false) select 1"),
            Ok(ReadKind::Explain)
        );
        // A WITH can hide a write.
        assert!(pg("with d as (delete from t returning *) select * from d").is_err());
        assert!(pg("with u as (update t set a = 1 returning *) select 1").is_err());
        // SELECT INTO creates a table; FOR UPDATE locks rows.
        assert!(pg("select * into copy from t").is_err());
        assert!(pg("select * from t for update").is_err());
        // Several statements.
        assert!(
            pg("select 1; delete from t")
                .unwrap_err()
                .contains("one statement")
        );
        // Whatever is not understood does not pass.
        assert!(pg("selec 1").is_err());
        // `TABLE t` is a read, but sqlparser does not understand it: rejected (the safe choice).
        assert!(pg("table orders").is_err());
        assert!(classify_read_only("sqlite", "pragma journal_mode = delete").is_err());
        assert!(classify_read_only("sqlite", "attach 'x.db' as x").is_err());
    }

    #[test]
    fn each_engine_reads_in_its_own_dialect() {
        for (engine, sql) in [
            ("mysql", "select `name` from `products` limit 3"),
            ("mysql", "show tables"),
            ("mysql", "show create table products"),
            ("mysql", "explain format=json select 1"),
            ("sqlserver", "select top 5 [name] from dbo.products"),
            ("sqlserver", "select * from spelaion.dbo.products"),
            ("duckdb", "select * from range(10) t(i)"),
            ("duckdb", "explain select 1"),
        ] {
            assert!(classify_read_only(engine, sql).is_ok(), "{engine}: {sql}");
        }
    }

    #[test]
    fn functions_that_reach_outside_are_refused() {
        for (engine, sql) in [
            ("postgres", "select pg_read_file('/etc/passwd')"),
            (
                "postgres",
                "select * from dblink('host=x', 'select 1') as t(a int)",
            ),
            ("postgres", "select set_config('role', 'admin', false)"),
            ("postgres", "select PG_TERMINATE_BACKEND(42)"),
            ("postgres", "select pg_catalog.pg_ls_dir('.')"),
            ("postgres", "select (select pg_advisory_lock(1))"),
            ("sqlite", "select load_extension('x')"),
            ("mysql", "select load_file('/etc/passwd')"),
            ("mysql", "select get_lock('x', 10)"),
            (
                "sqlserver",
                "select * from openrowset('SQLNCLI', 'server=x', 'select 1')",
            ),
            ("sqlserver", "select * from openquery(other, 'select 1')"),
            ("sqlserver", "select * from other.spelaion.dbo.products"),
            ("duckdb", "select * from read_csv('/etc/passwd')"),
            ("duckdb", "select getenv('HOME')"),
            ("duckdb", "select count(*) from glob('*')"),
        ] {
            let err = classify_read_only(engine, sql).unwrap_err();
            assert!(
                err.contains("outside the database") || err.contains("another server"),
                "{engine}: {sql}: {err}"
            );
        }
        // The same names as plain columns or tables are fine.
        assert!(classify_read_only("duckdb", "select glob from files").is_ok());
        assert!(classify_read_only("mysql", "select * from load_file_log").is_ok());
    }
}
