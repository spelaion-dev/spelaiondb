//! Statement classification for read-only mode (ADR-008, layer 4).
//!
//! Allow list: only what is recognised as a read passes; whatever cannot be parsed is
//! rejected. It is not the only barrier: a function with side effects
//! (`pg_terminate_backend`, a user function that writes) looks like a read. That is why it
//! also runs in a read-only transaction and a read-only database user is recommended.

use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Query, SetExpr, Statement, Value as SqlValue, Visit, Visitor};
use sqlparser::dialect::{
    Dialect, DuckDbDialect, GenericDialect, MsSqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;

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
    let dialect: Box<dyn Dialect> = match engine {
        "postgres" => Box::new(PostgreSqlDialect {}),
        "sqlite" => Box::new(SQLiteDialect {}),
        // Only to tell reads from writes for the "production" confirmation: the MCP's
        // read-only mode does not serve these engines (`readonly.rs`).
        "sqlserver" => Box::new(MsSqlDialect {}),
        "duckdb" => Box::new(DuckDbDialect {}),
        _ => Box::new(GenericDialect {}),
    };
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
        Statement::ShowVariable { .. } => Ok(ReadKind::Show),
        other => Err(format!("{} is not a read statement", kind_name(other))),
    }?;

    // Also, no statement nested anywhere in the tree (a safety net in case a new form of the
    // grammar escapes the checks above).
    let mut nested = NestedStatements { seen: 0 };
    if let ControlFlow::Break(kind) = statement.visit(&mut nested) {
        return Err(format!("contains a nested {kind} statement"));
    }
    Ok(kind)
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
}
