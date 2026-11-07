//! The grid's core features against every engine: SQLite and DuckDB always, PostgreSQL,
//! MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without them they
//! are skipped, which does not count as tested).
//!
//! - "Filter by this value" in a table data tab writes a condition the engine runs: it must
//!   keep the same rows as the same filter applied by the core to the loaded result.
//! - A column's values copied as an `IN` list, and column names copied quoted or qualified,
//!   must be SQL the engine reads back.
//! - Find works on the stored result whatever engine produced it.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    ColumnFilter, CoreEvent, FilterOp, FindQuery, NameStyle, ResultId, ResultStore, Session,
    add_condition, column_in_list, column_names, count_table_rows, qualified_name, value_condition,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// Every engine available here, with the schema its tables go in.
async fn engines() -> Vec<(&'static str, Session, &'static str)> {
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        (
            "sqlite",
            session(&spelaion_driver_sqlite::SqliteDriver::default(), memory()).await,
            "main",
        ),
        (
            "duckdb",
            session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await,
            "main",
        ),
    ];
    let login =
        |host: String, port: u16, user: &str, password: &str, database: &str| ConnectParams {
            host,
            port,
            user: user.into(),
            password: Some(Secret::new(password)),
            database: database.into(),
            ..Default::default()
        };
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        let params = login(host, port, "spelaion", "spelaion", "spelaion");
        out.push((
            "postgres",
            session(&spelaion_driver_postgres::PostgresDriver, params).await,
            "public",
        ));
    }
    for (label, var, driver) in [
        (
            "mysql",
            "SPELAION_TEST_MYSQL",
            spelaion_driver_mysql::MysqlDriver::mysql(),
        ),
        (
            "mariadb",
            "SPELAION_TEST_MARIADB",
            spelaion_driver_mysql::MysqlDriver::mariadb(),
        ),
    ] {
        if let Some((host, port)) = server(var) {
            let params = login(host, port, "spelaion", "spelaion", "spelaion");
            out.push((label, session(&driver, params).await, "spelaion"));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push((
            "sqlserver",
            session(&spelaion_driver_sqlserver::SqlServerDriver, params).await,
            "dbo",
        ));
    }
    out
}

/// Runs each statement on its own; panics on a failure unless `tolerate`. Returns the result
/// of the last statement that produced one (kept in the session's store).
async fn run(
    label: &str,
    session: &Session,
    statements: &[&str],
    tolerate: bool,
) -> Option<ResultId> {
    let mut last = None;
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            match event {
                CoreEvent::StatementFailed { error, .. } if !tolerate => {
                    panic!("{label}: {statement}\n→ {}", error.message)
                }
                CoreEvent::ResultStarted { result, .. } => last = Some(*result),
                _ => {}
            }
        }
    }
    last
}

/// The table every check reads: text with quotes, wildcards and brackets, decimals, booleans
/// and NULLs.
async fn items(label: &str, session: &Session, table: &str) -> ResultId {
    let (t, f, boolean) = match label {
        "sqlserver" => ("1", "0", "BIT"),
        _ => ("TRUE", "FALSE", "BOOLEAN"),
    };
    run(label, session, &[&format!("DROP TABLE {table}")], true).await;
    let create = format!(
        "CREATE TABLE {table} (id INT PRIMARY KEY, name VARCHAR(40), price DECIMAL(10, 2), ok {boolean})"
    );
    let insert = format!(
        "INSERT INTO {table} (id, name, price, ok) VALUES \
         (1, 'Ana', 10.50, {t}), (2, 'O''Brien', 20.00, {f}), (3, '50% off', NULL, {t}), \
         (4, 'x[y]z', 7.00, {f}), (5, NULL, 5.25, NULL), (6, 'ANA maria', 10.50, {f})"
    );
    run(label, session, &[&create, &insert], false).await;
    run(
        label,
        session,
        &[&format!(
            "SELECT id, name, price, ok FROM {table} ORDER BY id"
        )],
        false,
    )
    .await
    .unwrap_or_else(|| panic!("{label}: no result"))
}

const ID: usize = 0;
const NAME: usize = 1;
const PRICE: usize = 2;
const OK: usize = 3;

#[tokio::test]
async fn filter_by_value_keeps_the_same_rows_in_the_server_and_in_the_core() {
    for (label, session, schema) in engines().await {
        let id = items(label, &session, "ge_filter").await;
        let stored = session.results().get(id).unwrap();
        let dialect = session.dialect().clone();
        // (column, operator, row whose value is used) — the cell menu's choices.
        let cases = [
            (NAME, FilterOp::Eq, 0),
            (NAME, FilterOp::NotEq, 0),
            (NAME, FilterOp::Contains, 0),
            (NAME, FilterOp::Contains, 2),
            (NAME, FilterOp::Contains, 3),
            (NAME, FilterOp::NotContains, 2),
            (NAME, FilterOp::Gt, 1),
            (NAME, FilterOp::IsNull, 4),
            (NAME, FilterOp::IsNotNull, 0),
            (PRICE, FilterOp::Eq, 0),
            (PRICE, FilterOp::Gt, 0),
            (PRICE, FilterOp::Lt, 1),
            (PRICE, FilterOp::Contains, 0),
            (OK, FilterOp::Eq, 0),
            (OK, FilterOp::NotEq, 0),
            (ID, FilterOp::Lt, 3),
        ];
        for (column, op, row) in cases {
            let (meta, value) = {
                let r = stored.read().unwrap();
                (
                    r.schema().columns[column].clone(),
                    r.value_at(row, column).unwrap(),
                )
            };
            let condition = value_condition(dialect.as_ref(), &meta.name, meta.logical, op, &value)
                .unwrap_or_else(|e| panic!("{label} {op:?} {value:?}: {e}"));
            let filter = add_condition(dialect.as_ref(), "", &condition).unwrap();
            let server = count_table_rows(&session, schema, "ge_filter", &filter)
                .await
                .unwrap_or_else(|e| panic!("{label}: {filter}\n→ {e}"));
            let core = {
                let mut r = stored.write().unwrap();
                let text = match &value {
                    spelaion_driver_api::Value::Null => String::new(),
                    other => other.to_string(),
                };
                let rows = r
                    .filter_columns(vec![ColumnFilter {
                        column,
                        op,
                        value: text,
                    }])
                    .unwrap();
                r.filter_columns(Vec::new()).unwrap();
                rows
            };
            assert_eq!(server as usize, core, "{label}: {filter}");
        }
        // Added to a filter the user typed, the page still runs.
        let first = {
            let r = stored.read().unwrap();
            value_condition(
                dialect.as_ref(),
                "name",
                r.schema().columns[NAME].logical,
                FilterOp::Contains,
                &r.value_at(0, NAME).unwrap(),
            )
            .unwrap()
        };
        let filter = add_condition(dialect.as_ref(), "id = 1 OR id = 6 OR id = 2", &first).unwrap();
        let rows = count_table_rows(&session, schema, "ge_filter", &filter)
            .await
            .unwrap_or_else(|e| panic!("{label}: {filter}\n→ {e}"));
        assert_eq!(rows, 2, "{label}: {filter}");
        run(label, &session, &["DROP TABLE ge_filter"], true).await;
    }
}

#[tokio::test]
async fn copied_in_lists_and_column_names_are_sql_the_engine_reads() {
    for (label, session, schema) in engines().await {
        let id = items(label, &session, "ge_copy").await;
        let stored = session.results().get(id).unwrap();
        let dialect = session.dialect().clone();
        for (column, name) in [(ID, "id"), (NAME, "name"), (PRICE, "price"), (OK, "ok")] {
            let list = column_in_list(&stored.read().unwrap(), column, dialect.as_ref())
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let filter = format!("{} IN {list}", dialect.quote_identifier(name));
            let rows = count_table_rows(&session, schema, "ge_copy", &filter)
                .await
                .unwrap_or_else(|e| panic!("{label}: {filter}\n→ {e}"));
            // Every row whose value is not NULL is in its list (NULL never matches IN).
            let not_null = {
                let r = stored.read().unwrap();
                (0..r.rows())
                    .filter(|&row| {
                        !matches!(
                            r.value_at(row, column),
                            Some(spelaion_driver_api::Value::Null) | None
                        )
                    })
                    .count()
            };
            assert_eq!(rows as usize, not_null, "{label}: {filter}");
        }
        let (quoted, qualified) = {
            let r = stored.read().unwrap();
            let quoted =
                column_names(&r, &[ID, NAME], NameStyle::Quoted, dialect.as_ref(), None).unwrap();
            let qualified = column_names(
                &r,
                &[NAME, PRICE],
                NameStyle::Qualified,
                dialect.as_ref(),
                Some((schema, "ge_copy")),
            )
            .unwrap();
            (quoted, qualified)
        };
        let table = qualified_name(dialect.as_ref(), schema, &["ge_copy".to_string()]);
        for names in [quoted, qualified] {
            let sql = format!("SELECT {names} FROM {table}");
            run(label, &session, &[&sql], false).await;
        }
        run(label, &session, &["DROP TABLE ge_copy"], true).await;
    }
}

#[tokio::test]
async fn find_searches_the_stored_result_of_every_engine() {
    for (label, session, _) in engines().await {
        let id = items(label, &session, "ge_find").await;
        let stored = session.results().get(id).unwrap();
        {
            let mut r = stored.write().unwrap();
            let search = |r: &spelaion_core::StoredResult, text: &str, match_case: bool| {
                let query = FindQuery {
                    text: text.into(),
                    match_case,
                    columns: vec![ID, NAME, PRICE, OK],
                };
                let found = r.find_matches(&query, &r.find_ticket()).unwrap().unwrap();
                (0..found.len())
                    .map(|i| {
                        let m = found.get(i).unwrap();
                        (m.row, m.column)
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(search(&r, "ana", false), [(0, NAME), (5, NAME)], "{label}");
            assert_eq!(search(&r, "ANA", true), [(5, NAME)], "{label}");
            assert_eq!(search(&r, "50%", false), [(2, NAME)], "{label}");
            // After a sort the matches follow the rows.
            r.sort(Some(spelaion_core::SortKey {
                column: ID,
                descending: true,
            }))
            .unwrap();
            assert_eq!(search(&r, "ana", false), [(0, NAME), (5, NAME)], "{label}");
            assert_eq!(search(&r, "brien", false), [(4, NAME)], "{label}");
        }
        run(label, &session, &["DROP TABLE ge_find"], true).await;
    }
}
