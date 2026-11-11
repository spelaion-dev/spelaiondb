//! Statement classification for read-only mode (ADR-008, layer 4).
//!
//! Allow list: only what is recognised as a read passes; whatever cannot be parsed is
//! rejected. Functions that reach outside the database (files, other servers, the session:
//! [`reaches_outside`]) are refused by name. It is not the only barrier: a user function that
//! writes looks like a read. That is why it also runs in a read-only (or always rolled back)
//! transaction and a read-only database user is recommended.

use std::ops::ControlFlow;

use sqlparser::ast::{
    ContextModifier, Expr, ObjectName, Query, Set, SetExpr, SetSessionParamKind, Statement,
    TransactionAccessMode, TransactionMode, UnaryOperator, Value as SqlValue, Visit, Visitor,
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
    read_statement(engine, statement)
}

/// [`classify_read_only`] of one statement sqlparser has already read.
fn read_statement(engine: &str, statement: &Statement) -> Result<ReadKind, String> {
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

/// May `sql` (a single statement) run as session setup where the read-only barrier applies
/// (ADR-037): on a read-only connection, on a connection whose type confirms data changes
/// (Production) and in sessions that only read (agents' reads, streaming exports)? A read
/// passes as [`classify_read_only`] decides; otherwise only session settings from an allow
/// list, with plain values (literals and names, no expressions or subqueries), and `USE`. A
/// setting that could loosen read-only mode or change how SQL is parsed (`SET
/// default_transaction_read_only`, `SET … READ WRITE`, `sql_mode`, `SET NAMES`…) is refused,
/// like anything that does not parse. `Err` explains why.
pub fn classify_session_setup(engine: &str, sql: &str) -> Result<(), String> {
    reject_mysql_executable_comments(engine, sql)?;
    let dialect = sql_dialect(engine);
    let statements = Parser::parse_sql(dialect.as_ref(), sql)
        .map_err(|e| format!("could not parse the statement: {e}"))?;
    let statement = match statements.as_slice() {
        [one] => one,
        [] => return Err("there is no statement".into()),
        _ => return Err("only one statement at a time".into()),
    };
    setup_statement(engine, statement)
}

/// [`classify_session_setup`] of one statement sqlparser has already read.
fn setup_statement(engine: &str, statement: &Statement) -> Result<(), String> {
    let set = match statement {
        Statement::Set(set) => set,
        Statement::Use(_) => {
            return match engine {
                "mysql" | "mariadb" | "sqlserver" | "duckdb" => Ok(()),
                _ => Err("USE is not a setting of this engine".into()),
            };
        }
        _ => {
            return read_statement(engine, statement)
                .map(|_| ())
                .map_err(|reason| {
                    format!("{reason}: only reads and session settings may run here")
                });
        }
    };
    let refused = || {
        format!(
            "{} could change more than this session's settings: only reads and the settings on the session setup's allow list may run here",
            kind_name(statement)
        )
    };
    match set {
        Set::SingleAssignment {
            scope,
            hivevar,
            variable,
            values,
        } => {
            if *hivevar || !session_scope(*scope) || !values.iter().all(plain_value) {
                return Err(refused());
            }
            setting_allowed(engine, variable)
                .then_some(())
                .ok_or_else(refused)
        }
        Set::MultipleAssignments { assignments } => assignments
            .iter()
            .all(|a| {
                session_scope(a.scope) && plain_value(&a.value) && setting_allowed(engine, &a.name)
            })
            .then_some(())
            .ok_or_else(refused),
        Set::SetTimeZone { value, .. } => match engine {
            "postgres" | "duckdb" if plain_value(value) => Ok(()),
            _ => Err(refused()),
        },
        Set::SetRole {
            context_modifier, ..
        } => match engine {
            "postgres" | "mysql" | "mariadb" if session_scope(*context_modifier) => Ok(()),
            _ => Err(refused()),
        },
        // Only the isolation level: never READ WRITE, nor another transaction's snapshot.
        Set::SetTransaction {
            modes, snapshot, ..
        } => {
            let isolation_only = !modes.is_empty()
                && modes
                    .iter()
                    .all(|m| matches!(m, TransactionMode::IsolationLevel(_)));
            if isolation_only && snapshot.is_none() {
                Ok(())
            } else {
                Err(refused())
            }
        }
        Set::SetSessionParam(SetSessionParamKind::Generic(param)) if engine == "sqlserver" => {
            let allowed = !param.names.is_empty()
                && param
                    .names
                    .iter()
                    .all(|name| SQLSERVER_SETTINGS.contains(&name.to_ascii_uppercase().as_str()));
            allowed.then_some(()).ok_or_else(refused)
        }
        Set::SetSessionParam(_)
        | Set::ParenthesizedAssignments { .. }
        | Set::SetSessionAuthorization(_)
        | Set::SetNames { .. }
        | Set::SetNamesDefault {} => Err(refused()),
    }
}

/// May `sql` (one statement, or one T-SQL batch) run on a read-only connection (ADR-053)?
/// The editor's scripts, query pages and plans all go through it before anything runs. Only:
/// - a read, as [`classify_read_only`] decides;
/// - a session setting or `USE`, as [`classify_session_setup`] allows;
/// - plain transaction control: `BEGIN` / `START TRANSACTION` with at most an isolation level
///   and `READ ONLY`, `COMMIT`, `ROLLBACK` (to a savepoint too), `SAVEPOINT`, `RELEASE` and
///   `SET TRANSACTION` with an isolation level or `READ ONLY`.
///
/// Anything else, `READ WRITE` in any form, MySQL's executable comments and whatever does not
/// parse are refused. A T-SQL batch may hold several statements, and each must pass; elsewhere
/// a span is one statement, as for agents. `Err` explains why.
pub fn classify_read_only_connection(engine: &str, sql: &str) -> Result<(), String> {
    if reject_mysql_executable_comments(engine, sql).is_err() {
        return Err(
            "it has a MySQL executable comment (/*! … */), which the server runs as SQL".into(),
        );
    }
    let dialect = sql_dialect(engine);
    let statements = Parser::parse_sql(dialect.as_ref(), sql)
        .map_err(|e| format!("could not parse the statement: {e}"))?;
    match (engine, statements.as_slice()) {
        // Only comments: nothing runs.
        (_, []) => Ok(()),
        (_, [one]) => read_only_connection_statement(engine, one),
        ("sqlserver", batch) => batch
            .iter()
            .try_for_each(|statement| read_only_connection_statement(engine, statement)),
        _ => Err("only one statement at a time".into()),
    }
}

/// [`classify_read_only_connection`] of one statement sqlparser has already read.
fn read_only_connection_statement(engine: &str, statement: &Statement) -> Result<(), String> {
    if plain_transaction_control(statement) {
        return Ok(());
    }
    match statement {
        Statement::Set(_) | Statement::Use(_) => setup_statement(engine, statement).map_err(|_| {
            format!(
                "{} could change more than this session's settings",
                kind_name(statement)
            )
        }),
        _ => read_statement(engine, statement).map(|_| ()),
    }
}

/// `BEGIN`, `COMMIT`, `ROLLBACK`, savepoints and `SET TRANSACTION`, never asking for `READ
/// WRITE`, another transaction's snapshot or a block of statements (T-SQL's `BEGIN … END`,
/// `BEGIN TRY`).
fn plain_transaction_control(statement: &Statement) -> bool {
    let read_only_modes = |modes: &[TransactionMode]| {
        modes.iter().all(|mode| {
            matches!(
                mode,
                TransactionMode::IsolationLevel(_)
                    | TransactionMode::AccessMode(TransactionAccessMode::ReadOnly)
            )
        })
    };
    match statement {
        Statement::StartTransaction {
            modes,
            modifier,
            statements,
            exception,
            has_end_keyword,
            ..
        } => {
            modifier.is_none()
                && statements.is_empty()
                && exception.is_none()
                && !has_end_keyword
                && read_only_modes(modes)
        }
        Statement::Commit { end, modifier, .. } => !end && modifier.is_none(),
        Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => true,
        Statement::Set(Set::SetTransaction {
            modes, snapshot, ..
        }) => !modes.is_empty() && snapshot.is_none() && read_only_modes(modes),
        _ => false,
    }
}

/// Settings of this session only: no `GLOBAL`.
fn session_scope(scope: Option<ContextModifier>) -> bool {
    matches!(
        scope,
        None | Some(ContextModifier::Session) | Some(ContextModifier::Local)
    )
}

/// A literal or a bare name (`'UTC'`, `5000`, `-1`, `app`, `DEFAULT`): nothing that runs.
fn plain_value(expr: &Expr) -> bool {
    match expr {
        Expr::Value(_) | Expr::Identifier(_) => true,
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => matches!(expr.as_ref(), Expr::Value(_)),
        _ => false,
    }
}

/// Session settings a read-only session setup may change, by engine: where names are found,
/// how values are shown, and limits. None loosens read-only mode or changes how SQL parses.
fn setup_settings(engine: &str) -> &'static [&'static str] {
    match engine {
        "postgres" => &[
            "search_path",
            "timezone",
            "datestyle",
            "intervalstyle",
            "extra_float_digits",
            "bytea_output",
            "application_name",
            "client_min_messages",
            "statement_timeout",
            "lock_timeout",
            "idle_in_transaction_session_timeout",
            "work_mem",
        ],
        "mysql" | "mariadb" => &[
            "time_zone",
            "lc_time_names",
            "max_execution_time",
            "max_statement_time",
            "div_precision_increment",
            "group_concat_max_len",
            "sql_select_limit",
            "lock_wait_timeout",
            "innodb_lock_wait_timeout",
            "net_read_timeout",
            "net_write_timeout",
            "wait_timeout",
            "interactive_timeout",
        ],
        "duckdb" => &["schema", "search_path", "timezone", "calendar"],
        _ => &[],
    }
}

/// SQL Server's `SET <option> <value>` options a read-only session setup may change.
const SQLSERVER_SETTINGS: &[&str] = &[
    "DATEFORMAT",
    "DATEFIRST",
    "LANGUAGE",
    "LOCK_TIMEOUT",
    "NOCOUNT",
    "DEADLOCK_PRIORITY",
    "TEXTSIZE",
    "ARITHABORT",
];

/// `name` (`search_path`, MySQL's `@@session.time_zone` or `@@time_zone`) is on the engine's
/// allow list, case-insensitively.
fn setting_allowed(engine: &str, name: &ObjectName) -> bool {
    let mysql = engine == "mysql" || engine == "mariadb";
    let parts: Vec<String> = name
        .0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|ident| ident.value.to_ascii_lowercase())
                .unwrap_or_default()
        })
        .collect();
    let setting = match parts.as_slice() {
        [one] if mysql => match one.strip_prefix("@@") {
            Some(rest) => rest.to_string(),
            // `@x` is a user variable, not a setting.
            None if one.starts_with('@') => return false,
            None => one.clone(),
        },
        [one] => one.clone(),
        [scope, one] if mysql && (scope == "@@session" || scope == "@@local") => one.clone(),
        _ => return false,
    };
    !setting.is_empty() && setup_settings(engine).contains(&setting.as_str())
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
    #[test]
    fn session_setup_allows_reads_and_listed_settings() {
        for (engine, sql) in [
            ("postgres", "SET search_path TO app, public"),
            ("postgres", "set search_path = \"App\", public"),
            ("postgres", "SET SESSION search_path TO app"),
            ("postgres", "SET LOCAL search_path TO app"),
            ("postgres", "SET TIME ZONE 'UTC'"),
            ("postgres", "SET timezone = 'Europe/Madrid'"),
            ("postgres", "SET DateStyle = ISO"),
            ("postgres", "SET statement_timeout = '5s'"),
            ("postgres", "SET lock_timeout = 5000"),
            ("postgres", "SET extra_float_digits = -1"),
            ("postgres", "SET ROLE reporting"),
            (
                "postgres",
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            ),
            (
                "postgres",
                "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            ),
            ("postgres", "select current_schema()"),
            ("postgres", "show search_path"),
            ("mysql", "USE shop"),
            ("mysql", "SET time_zone = '+00:00'"),
            ("mysql", "SET SESSION time_zone = '+00:00'"),
            ("mysql", "SET @@session.time_zone = '+00:00'"),
            ("mysql", "SET @@time_zone = '+00:00'"),
            ("mysql", "SET time_zone = '+00:00', lc_time_names = 'es_ES'"),
            (
                "mysql",
                "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
            ),
            ("mysql", "SET ROLE reporting"),
            ("mysql", "select 1"),
            ("sqlserver", "SET DATEFORMAT dmy"),
            ("sqlserver", "SET LANGUAGE us_english"),
            ("sqlserver", "SET LOCK_TIMEOUT 5000"),
            ("sqlserver", "SET NOCOUNT ON"),
            ("sqlserver", "USE shop"),
            ("sqlserver", "SET TRANSACTION ISOLATION LEVEL SNAPSHOT"),
            ("duckdb", "SET schema = 'app'"),
            ("duckdb", "SET search_path = 'app,main'"),
            ("duckdb", "SET TimeZone = 'UTC'"),
            ("duckdb", "USE memory"),
            ("sqlite", "select 1"),
        ] {
            assert_eq!(
                classify_session_setup(engine, sql),
                Ok(()),
                "{engine}: {sql}"
            );
        }
    }

    #[test]
    fn session_setup_refuses_writes_and_what_loosens_read_only() {
        for (engine, sql) in [
            // Writes, whatever their shape.
            ("postgres", "insert into audit values (1)"),
            ("postgres", "update t set a = 1"),
            ("postgres", "delete from t"),
            ("postgres", "create temp table t (a int)"),
            ("postgres", "create table t (a int)"),
            ("postgres", "drop table t"),
            ("postgres", "truncate t"),
            (
                "postgres",
                "with x as (delete from t returning *) select * from x",
            ),
            ("postgres", "select * into t2 from t"),
            ("postgres", "select * from t for update"),
            ("postgres", "copy t to '/tmp/x'"),
            ("postgres", "do $$ begin delete from t; end $$"),
            ("postgres", "call purge()"),
            ("postgres", "grant all on t to public"),
            ("postgres", "begin"),
            ("postgres", "commit"),
            ("postgres", "explain analyze delete from t"),
            // Several statements in one, so a write cannot ride along.
            ("postgres", "set search_path to app; drop table t"),
            // Settings that loosen read-only mode.
            ("postgres", "SET default_transaction_read_only = off"),
            ("postgres", "set transaction_read_only = off"),
            (
                "postgres",
                "SET SESSION default_transaction_read_only TO false",
            ),
            (
                "postgres",
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            ),
            ("postgres", "SET TRANSACTION READ WRITE"),
            (
                "postgres",
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ WRITE",
            ),
            ("postgres", "SET TRANSACTION SNAPSHOT '00000003-0000001B-1'"),
            ("postgres", "set session_replication_role = replica"),
            ("postgres", "RESET ALL"),
            ("postgres", "reset default_transaction_read_only"),
            ("postgres", "SET SESSION AUTHORIZATION admin"),
            (
                "postgres",
                "select set_config('default_transaction_read_only', 'off', false)",
            ),
            ("postgres", "select pg_advisory_lock(1)"),
            // Values that run something.
            ("postgres", "SET search_path TO (select 'app')"),
            ("postgres", "SET timezone = current_setting('x')"),
            ("postgres", "SET TIME ZONE (select 'UTC')"),
            // Settings that are not on the list, and unknown ones.
            ("postgres", "SET work_mem_x = 1"),
            ("postgres", "SET my.custom = 'x'"),
            ("postgres", "SET hivevar:x = 1"),
            ("postgres", "USE shop"),
            // MySQL: global scope, user variables, parsing settings, executable comments.
            ("mysql", "SET GLOBAL time_zone = '+00:00'"),
            ("mysql", "SET @@global.time_zone = '+00:00'"),
            ("mysql", "SET @x = (SELECT 1)"),
            ("mysql", "SET @x = 1"),
            ("mysql", "SET sql_mode = ''"),
            ("mysql", "SET time_zone = '+00:00', sql_mode = ''"),
            ("mysql", "SET time_zone = '+00:00', GLOBAL wait_timeout = 1"),
            ("mysql", "SET NAMES utf8mb4"),
            ("mysql", "SET SESSION TRANSACTION READ WRITE"),
            ("mysql", "SET transaction_read_only = 0"),
            ("mysql", "SET @@session.transaction_read_only = 0"),
            ("mysql", "SET autocommit = 0"),
            ("mysql", "/*!50000 DELETE FROM t */"),
            ("mysql", "SET time_zone = '+00:00' /*!, sql_mode = '' */"),
            ("mysql", "insert into t values (1)"),
            ("mysql", "select get_lock('x', 1)"),
            // SQL Server: options that write or change parsing.
            ("sqlserver", "SET IDENTITY_INSERT dbo.t ON"),
            ("sqlserver", "SET QUOTED_IDENTIFIER OFF"),
            ("sqlserver", "SET IMPLICIT_TRANSACTIONS ON"),
            ("sqlserver", "SET ROLE x"),
            ("sqlserver", "EXECUTE AS USER = 'dbo'"),
            ("sqlserver", "exec sp_configure 'x', 1"),
            ("sqlserver", "delete from t"),
            // T-SQL needs no `;` between statements: a write cannot hide after a setting.
            ("sqlserver", "SET NOCOUNT ON DELETE FROM t"),
            ("sqlserver", "SET LANGUAGE us_english\nDROP TABLE t"),
            ("postgres", "SET search_path TO app DELETE FROM t"),
            // DuckDB: external access and engine-wide settings.
            ("duckdb", "SET enable_external_access = true"),
            ("duckdb", "SET GLOBAL schema = 'x'"),
            ("duckdb", "SET memory_limit = '1GB'"),
            ("duckdb", "attach 'x.db' as x"),
            ("duckdb", "install httpfs"),
            ("duckdb", "create table t (a int)"),
            // SQLite: pragmas, attachments.
            ("sqlite", "pragma query_only = 0"),
            ("sqlite", "attach 'x.db' as x"),
            ("sqlite", "USE main"),
            ("sqlite", "insert into t values (1)"),
            // Nothing, or something that does not parse.
            ("postgres", ""),
            ("postgres", "SET SCHEMA 'app'"),
            ("postgres", "this is not sql"),
        ] {
            assert!(
                classify_session_setup(engine, sql).is_err(),
                "{engine}: {sql} should be refused"
            );
        }
    }

    #[test]
    fn session_setup_refusals_say_why() {
        let err = classify_session_setup("postgres", "delete from t").unwrap_err();
        assert!(err.contains("only reads and session settings"), "{err}");
        let err = classify_session_setup("postgres", "SET default_transaction_read_only = off")
            .unwrap_err();
        assert!(err.contains("allow list"), "{err}");
    }

    #[test]
    fn read_only_connections_run_reads_settings_and_transaction_control() {
        for (engine, sql) in [
            ("postgres", "select 1"),
            ("postgres", "  SeLeCt * FROM orders -- the rest"),
            ("postgres", "/* a comment */ select 1"),
            ("postgres", "-- only a comment"),
            (
                "postgres",
                "with recent as (select * from orders) select count(*) from recent",
            ),
            ("postgres", "explain select * from orders"),
            ("postgres", "show search_path"),
            ("postgres", "set search_path to app, public"),
            ("postgres", "SET statement_timeout = 5000"),
            ("postgres", "SET ROLE reporting"),
            ("postgres", "begin"),
            ("postgres", "BEGIN READ ONLY"),
            ("postgres", "begin isolation level serializable"),
            (
                "postgres",
                "start transaction isolation level repeatable read, read only",
            ),
            ("postgres", "commit"),
            ("postgres", "COMMIT AND CHAIN"),
            ("postgres", "rollback"),
            ("postgres", "savepoint before_reads"),
            ("postgres", "rollback to savepoint before_reads"),
            ("postgres", "release savepoint before_reads"),
            ("postgres", "set transaction read only"),
            ("postgres", "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
            ("mysql", "select * from orders limit 5"),
            ("mysql", "show tables"),
            ("mysql", "use shop"),
            ("mysql", "set time_zone = '+00:00'"),
            (
                "mysql",
                "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
            ),
            ("mysql", "start transaction"),
            ("mysql", "START TRANSACTION READ ONLY"),
            ("mysql", "commit"),
            ("mysql", "rollback"),
            ("sqlserver", "SELECT TOP 5 * FROM dbo.orders"),
            ("sqlserver", "SELECT 1; SELECT 2"),
            ("sqlserver", "USE master"),
            ("sqlserver", "SET NOCOUNT ON"),
            ("sqlserver", "BEGIN TRANSACTION"),
            ("sqlserver", "COMMIT"),
            ("sqlserver", "ROLLBACK TRANSACTION"),
            ("duckdb", "select 42"),
            ("duckdb", "use memory"),
            ("duckdb", "begin transaction"),
            ("sqlite", "select * from t"),
            ("sqlite", "begin"),
            ("sqlite", "commit"),
        ] {
            assert_eq!(
                classify_read_only_connection(engine, sql),
                Ok(()),
                "{engine}: {sql}"
            );
        }
    }

    #[test]
    fn read_only_connections_refuse_writes_however_they_are_written() {
        for (engine, sql) in [
            // Writes, in odd casing and behind comments.
            ("postgres", "insert into t values (1)"),
            ("postgres", "DeLeTe FrOm t WhErE id = 1"),
            ("postgres", "/* select */ delete from t where id = 1"),
            ("postgres", "-- select 1\nupdate t set a = 1 where id = 1"),
            (
                "postgres",
                "merge into t using s on t.id = s.id when matched then delete",
            ),
            ("postgres", "create temp table t (a int)"),
            ("postgres", "create table t (a int)"),
            ("postgres", "alter table t add column b int"),
            ("postgres", "drop table t"),
            ("postgres", "truncate t"),
            ("postgres", "grant all on t to public"),
            ("postgres", "comment on table t is 'x'"),
            // Data-modifying CTEs, SELECT … INTO and locking reads.
            (
                "postgres",
                "with d as (delete from t returning *) select * from d",
            ),
            (
                "postgres",
                "WITH u AS (UPDATE t SET a = 1 RETURNING *) SELECT 1",
            ),
            (
                "postgres",
                "with i as (insert into t values (1) returning *) select * from i",
            ),
            ("postgres", "select * into t2 from t"),
            ("postgres", "select * from t for update"),
            ("postgres", "explain analyze delete from t"),
            ("postgres", "EXPLAIN (ANALYZE) SELECT 1"),
            ("postgres", "explain delete from t"),
            // Statements that run other statements, or write elsewhere.
            ("postgres", "do $$ begin delete from t; end $$"),
            ("postgres", "call purge()"),
            ("postgres", "prepare p as delete from t"),
            ("postgres", "execute p"),
            ("postgres", "copy t from stdin"),
            ("postgres", "copy t to '/tmp/x'"),
            ("postgres", "lock table t"),
            ("postgres", "vacuum t"),
            ("postgres", "notify channel"),
            ("postgres", "select pg_terminate_backend(1)"),
            (
                "postgres",
                "select dblink_exec('dbname=x', 'delete from t')",
            ),
            ("postgres", "select lo_import('/etc/passwd')"),
            ("postgres", "select pg_advisory_lock(1)"),
            // Turning read-only mode off, in every spelling.
            ("postgres", "begin read write"),
            ("postgres", "START TRANSACTION READ WRITE"),
            ("postgres", "begin isolation level serializable, read write"),
            (
                "postgres",
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
            ),
            ("postgres", "set transaction read write"),
            ("postgres", "SET TRANSACTION SNAPSHOT '00000003-0000001B-1'"),
            ("postgres", "SET default_transaction_read_only = off"),
            ("postgres", "set default_transaction_read_only to default"),
            (
                "postgres",
                "SET SESSION default_transaction_read_only TO false",
            ),
            ("postgres", "set transaction_read_only = off"),
            ("postgres", "RESET ALL"),
            ("postgres", "reset default_transaction_read_only"),
            ("postgres", "DISCARD ALL"),
            (
                "postgres",
                "select set_config('default_transaction_read_only', 'off', false)",
            ),
            ("postgres", "SET SESSION AUTHORIZATION admin"),
            ("postgres", "prepare transaction 'x'"),
            ("postgres", "commit prepared 'x'"),
            // Several statements in one span, or one that does not parse.
            ("postgres", "select 1; delete from t"),
            ("postgres", "begin; delete from t; commit"),
            ("postgres", "this is not sql"),
            ("postgres", "TABLE t"),
            // MySQL: executable comments, files, locks and READ WRITE.
            ("mysql", "/*!50000 DELETE FROM t */"),
            ("mysql", "select 1 /*!, (select sleep(1)) */"),
            ("mysql", "select 1 /*M!100000 , 2 */"),
            ("mysql", "SELECT * FROM t INTO OUTFILE '/tmp/x'"),
            ("mysql", "select 1 into @x"),
            ("mysql", "select * from t for update"),
            ("mysql", "replace into t values (1)"),
            (
                "mysql",
                "insert into t values (1) on duplicate key update a = 1",
            ),
            ("mysql", "load data infile '/tmp/x' into table t"),
            ("mysql", "lock tables t write"),
            ("mysql", "START TRANSACTION READ WRITE"),
            ("mysql", "SET SESSION TRANSACTION READ WRITE"),
            ("mysql", "set transaction read write"),
            ("mysql", "SET transaction_read_only = 0"),
            ("mysql", "SET @@session.transaction_read_only = OFF"),
            ("mysql", "set session tx_read_only = 0"),
            ("mysql", "SET GLOBAL read_only = 0"),
            ("mysql", "SET sql_mode = ''"),
            ("mysql", "set autocommit = 0"),
            ("mysql", "select get_lock('x', 1)"),
            ("mysql", "call purge()"),
            ("mysql", "xa start 'x'"),
            // SQL Server: a write hidden in a batch after a read, blocks and dynamic SQL.
            ("sqlserver", "SELECT 1; DELETE FROM t"),
            ("sqlserver", "SELECT 1\nDELETE FROM t"),
            ("sqlserver", "SELECT * FROM t DELETE FROM t WHERE id = 1"),
            ("sqlserver", "SELECT 1\nUPDATE t SET a = 1 WHERE id = 1"),
            ("sqlserver", "SET NOCOUNT ON DELETE FROM t"),
            ("sqlserver", "BEGIN DELETE FROM t END"),
            (
                "sqlserver",
                "BEGIN TRY DELETE FROM t END TRY BEGIN CATCH SELECT 1 END CATCH",
            ),
            ("sqlserver", "IF 1 = 1 DELETE FROM t"),
            ("sqlserver", "exec sp_who"),
            ("sqlserver", "EXEC('DELETE FROM t')"),
            ("sqlserver", "EXECUTE AS USER = 'dbo'"),
            ("sqlserver", "SELECT * INTO #copy FROM t"),
            ("sqlserver", "select * from srv.shop.dbo.t"),
            (
                "sqlserver",
                "select * from openrowset('SQLNCLI', 'x', 'select 1')",
            ),
            (
                "sqlserver",
                "update t set a = 1 output inserted.* where id = 1",
            ),
            ("sqlserver", "SET IMPLICIT_TRANSACTIONS ON"),
            ("sqlserver", "SET QUOTED_IDENTIFIER OFF"),
            ("sqlserver", "SET IDENTITY_INSERT dbo.t ON"),
            ("sqlserver", "TRUNCATE TABLE t"),
            ("sqlserver", "drop table t"),
            ("sqlserver", "BEGIN DISTRIBUTED TRANSACTION"),
            ("sqlserver", "bulk insert t from 'c:/x.csv'"),
            // DuckDB and SQLite: files, attachments, pragmas.
            ("duckdb", "attach 'x.db' as x"),
            ("duckdb", "copy t to 'x.csv'"),
            ("duckdb", "install httpfs"),
            ("duckdb", "SET enable_external_access = true"),
            ("duckdb", "checkpoint"),
            ("duckdb", "create table t (a int)"),
            ("sqlite", "pragma query_only = 0"),
            ("sqlite", "attach 'x.db' as x"),
            ("sqlite", "vacuum"),
            ("sqlite", "begin exclusive"),
            ("sqlite", "insert or replace into t values (1)"),
        ] {
            assert!(
                classify_read_only_connection(engine, sql).is_err(),
                "{engine}: {sql} should be refused"
            );
        }
    }

    #[test]
    fn read_only_connection_refusals_say_why() {
        let err = classify_read_only_connection("postgres", "delete from t").unwrap_err();
        assert!(err.contains("DELETE is not a read"), "{err}");
        let err = classify_read_only_connection(
            "postgres",
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        )
        .unwrap_err();
        assert!(err.contains("more than this session's settings"), "{err}");
        let err = classify_read_only_connection("mysql", "/*!50000 DELETE FROM t */").unwrap_err();
        assert!(err.contains("executable comment"), "{err}");
        let err = classify_read_only_connection("postgres", "this is not sql").unwrap_err();
        assert!(err.contains("could not parse"), "{err}");
    }
}
