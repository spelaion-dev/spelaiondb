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

/// The first cell of a query's result, as text.
async fn first_cell(label: &str, session: &Session, sql: &str) -> String {
    let id = run(label, session, &[sql], false)
        .await
        .unwrap_or_else(|| panic!("{label}: {sql}: no result"));
    let stored = session.results().get(id).unwrap();
    let stored = stored.read().unwrap();
    match stored.value_at(0, 0).unwrap() {
        spelaion_driver_api::Value::Null => "NULL".into(),
        other => other.to_string(),
    }
}

/// MySQL and MariaDB read a backslash in a string as an escape, unless the session's sql_mode
/// has NO_BACKSLASH_ESCAPES (SDB-161). In both modes: the editor splits a script where the
/// server would, "filter by this value" and an edit keyed by a value that holds a backslash
/// hit exactly that row, and a copied INSERT loads the value unchanged.
#[tokio::test]
async fn mysql_and_mariadb_read_backslashes_as_the_sessions_sql_mode_does() {
    for (label, session, schema) in engines().await {
        if label != "mysql" && label != "mariadb" {
            continue;
        }
        let base = first_cell(label, &session, "select @@session.sql_mode").await;
        let base = if base == "NULL" { String::new() } else { base };
        for no_backslash_escapes in [false, true] {
            let mode = if no_backslash_escapes {
                format!("{base},NO_BACKSLASH_ESCAPES")
                    .trim_start_matches(',')
                    .to_string()
            } else {
                base.clone()
            };
            let case = format!(
                "{label} ({})",
                if no_backslash_escapes {
                    "NO_BACKSLASH_ESCAPES"
                } else {
                    "default"
                }
            );
            // The statement names sql_mode: the session reads it again afterwards.
            run(
                &case,
                &session,
                &[&format!("SET SESSION sql_mode = '{mode}'")],
                false,
            )
            .await;
            run(&case, &session, &["DROP TABLE IF EXISTS ge_mode"], false).await;
            run(
                &case,
                &session,
                &["CREATE TABLE ge_mode (k VARCHAR(20) PRIMARY KEY, n INT)"],
                false,
            )
            .await;

            // The script as the server reads it: two statements, the value `C:\`.
            let script = if no_backslash_escapes {
                r"insert into ge_mode values ('C:\', 1); insert into ge_mode values ('C:', 2);"
            } else {
                r"insert into ge_mode values ('C:\\', 1); insert into ge_mode values ('C:', 2);"
            };
            let dialect = session.dialect();
            assert_eq!(dialect.split_statements(script).len(), 2, "{case}");
            let events: Vec<CoreEvent> = session.run_script(script).collect().await;
            let finished = events
                .iter()
                .filter(|e| matches!(e, CoreEvent::StatementFinished { .. }))
                .count();
            assert_eq!(finished, 2, "{case}: {events:?}");
            assert_eq!(
                first_cell(&case, &session, "select k from ge_mode where n = 1").await,
                r"C:\",
                "{case}"
            );

            // "Filter by this value" on the value with a backslash.
            let id = run(
                &case,
                &session,
                &["select k, n from ge_mode order by n"],
                false,
            )
            .await
            .unwrap();
            let stored = session.results().get(id).unwrap();
            let (meta, value) = {
                let r = stored.read().unwrap();
                (r.schema().columns[0].clone(), r.value_at(0, 0).unwrap())
            };
            let condition = value_condition(
                dialect.as_ref(),
                &meta.name,
                meta.logical,
                FilterOp::Eq,
                &value,
            )
            .unwrap();
            let filter = add_condition(dialect.as_ref(), "", &condition).unwrap();
            let rows = count_table_rows(&session, schema, "ge_mode", &filter)
                .await
                .unwrap_or_else(|e| panic!("{case}: {filter}\n→ {e}"));
            assert_eq!(rows, 1, "{case}: {filter}");

            // An edit of the row keyed by `C:\` changes that row only.
            let table = session
                .connection()
                .describe_table(schema, "ge_mode")
                .await
                .unwrap();
            let edit_target = session.edit_target().await.unwrap();
            let statements = spelaion_core::edit_statements(
                &edit_target,
                &table,
                &stored.read().unwrap(),
                &[spelaion_core::RowChange::Update {
                    row: 0,
                    values: vec![(1, spelaion_core::CellInput::Text("10".into()))],
                }],
            )
            .unwrap();
            assert_eq!(
                spelaion_core::apply_edits(&session, &statements).await,
                Ok(1),
                "{case}"
            );
            assert_eq!(
                first_cell(&case, &session, "select sum(n) from ge_mode").await,
                "12",
                "{case}"
            );

            // A copied INSERT of the rows loads them unchanged.
            let mut copied = Vec::new();
            spelaion_core::write_result(
                &stored.read().unwrap(),
                &spelaion_core::ExportOptions {
                    format: spelaion_core::ExportFormat::SqlInsert {
                        table: "ge_mode".into(),
                    },
                    rows: 0..2,
                    columns: vec![0, 1],
                    header: false,
                    csv: spelaion_core::CsvOptions::default(),
                },
                dialect.as_ref(),
                &mut copied,
            )
            .unwrap();
            let copied = String::from_utf8(copied).unwrap();
            run(&case, &session, &["DELETE FROM ge_mode"], false).await;
            let events: Vec<CoreEvent> = session.run_script(&copied).collect().await;
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
                "{case}: {copied}\n{events:?}"
            );
            assert_eq!(
                first_cell(&case, &session, "select k from ge_mode where n = 1").await,
                r"C:\",
                "{case}: {copied}"
            );
            run(&case, &session, &["DROP TABLE ge_mode"], false).await;
        }
        run(
            label,
            &session,
            &[&format!("SET SESSION sql_mode = '{base}'")],
            false,
        )
        .await;
    }
}

/// SQL Server reads `'YYYY-MM-DD hh:mm:ss'` by the session's DATEFORMAT (the login's language)
/// for `datetime` and `smalldatetime`, and wants `+hh:mm` in a `datetimeoffset` (SDB-123). The
/// literals the core writes are read the same under any language: "filter by this value"
/// finds the row it was written from, an edit keyed by a `datetime` applies, and an INSERT
/// copied under one language loads the same values under the other.
#[tokio::test]
async fn sqlserver_date_literals_mean_the_same_under_every_language() {
    for (label, session, schema) in engines().await {
        if label != "sqlserver" {
            continue;
        }
        run(label, &session, &["SET LANGUAGE us_english"], false).await;
        run(label, &session, &["DROP TABLE ge_dates"], true).await;
        run(label, &session, &["DROP TABLE ge_dates_key"], true).await;
        // Written in ISO 8601 with `T`, which every language reads alike: days 4 and 13.
        run(
            label,
            &session,
            &[
                "CREATE TABLE ge_dates (id INT PRIMARY KEY, dt DATETIME, sdt SMALLDATETIME, \
                 dt2 DATETIME2, d DATE, dto DATETIMEOFFSET)",
                "INSERT INTO ge_dates VALUES \
                 (1, '2024-03-04T10:00:00.123', '2024-03-04T10:00:00', '2024-03-04T10:00:00.123456', \
                  '2024-03-04', '2024-03-04T10:00:00.5+02:00'), \
                 (2, '2024-01-13T23:59:59.997', '2024-01-13T08:30:00', '2024-01-13T00:00:00', \
                  '2024-01-13', '2024-01-13T08:00:00-05:30')",
                "CREATE TABLE ge_dates_key (k DATETIME PRIMARY KEY, n INT)",
                "INSERT INTO ge_dates_key VALUES ('2024-03-04T10:00:00', 1), ('2024-01-13T10:00:00', 2)",
            ],
            false,
        )
        .await;
        let select = "SELECT id, dt, sdt, dt2, d, dto FROM ge_dates ORDER BY id";
        let original: Vec<Vec<String>> = {
            let id = run(label, &session, &[select], false).await.unwrap();
            let stored = session.results().get(id).unwrap();
            let r = stored.read().unwrap();
            (0..r.rows())
                .map(|row| {
                    (0..6)
                        .map(|c| r.value_at(row, c).unwrap().to_string())
                        .collect()
                })
                .collect()
        };
        for language in ["us_english", "Spanish"] {
            let case = format!("{label} ({language})");
            run(
                &case,
                &session,
                &[&format!("SET LANGUAGE {language}")],
                false,
            )
            .await;
            let dialect = session.dialect();
            let id = run(&case, &session, &[select], false).await.unwrap();
            let stored = session.results().get(id).unwrap();

            // "Filter by this value" on every date column, both rows.
            for row in 0..2 {
                for column in 1..6 {
                    let (meta, value) = {
                        let r = stored.read().unwrap();
                        (
                            r.schema().columns[column].clone(),
                            r.value_at(row, column).unwrap(),
                        )
                    };
                    let condition = value_condition(
                        dialect.as_ref(),
                        &meta.name,
                        meta.logical,
                        FilterOp::Eq,
                        &value,
                    )
                    .unwrap();
                    let filter = add_condition(dialect.as_ref(), "", &condition).unwrap();
                    let rows = count_table_rows(&session, schema, "ge_dates", &filter)
                        .await
                        .unwrap_or_else(|e| panic!("{case}: {filter}\n→ {e}"));
                    assert_eq!(rows, 1, "{case}: {filter}");
                }
            }

            // An edit of the row keyed by a datetime on the 13th.
            let key_id = run(
                &case,
                &session,
                &["SELECT k, n FROM ge_dates_key ORDER BY k"],
                false,
            )
            .await
            .unwrap();
            let keys = session.results().get(key_id).unwrap();
            let table = session
                .connection()
                .describe_table(schema, "ge_dates_key")
                .await
                .unwrap();
            let edit_target = session.edit_target().await.unwrap();
            let statements = spelaion_core::edit_statements(
                &edit_target,
                &table,
                &keys.read().unwrap(),
                &[spelaion_core::RowChange::Update {
                    row: 0,
                    values: vec![(1, spelaion_core::CellInput::Text("20".into()))],
                }],
            )
            .unwrap();
            assert_eq!(
                spelaion_core::apply_edits(&session, &statements).await,
                Ok(1),
                "{case}"
            );

            // The INSERT copied under this language, loaded under the other one.
            let mut copied = Vec::new();
            spelaion_core::write_result(
                &stored.read().unwrap(),
                &spelaion_core::ExportOptions {
                    format: spelaion_core::ExportFormat::SqlInsert {
                        table: "ge_dates".into(),
                    },
                    rows: 0..2,
                    columns: (0..6).collect(),
                    header: false,
                    csv: spelaion_core::CsvOptions::default(),
                },
                dialect.as_ref(),
                &mut copied,
            )
            .unwrap();
            let copied = String::from_utf8(copied).unwrap();
            let other = if language == "us_english" {
                "Spanish"
            } else {
                "us_english"
            };
            run(
                &case,
                &session,
                &[&format!("SET LANGUAGE {other}"), "DELETE FROM ge_dates"],
                false,
            )
            .await;
            let events: Vec<CoreEvent> = session.run_script(&copied).collect().await;
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
                "{case}: {copied}\n{events:?}"
            );
            let loaded: Vec<Vec<String>> = {
                let id = run(&case, &session, &[select], false).await.unwrap();
                let stored = session.results().get(id).unwrap();
                let r = stored.read().unwrap();
                (0..r.rows())
                    .map(|row| {
                        (0..6)
                            .map(|c| r.value_at(row, c).unwrap().to_string())
                            .collect()
                    })
                    .collect()
            };
            assert_eq!(loaded, original, "{case}: {copied}");
        }
        run(label, &session, &["SET LANGUAGE us_english"], false).await;
        run(
            label,
            &session,
            &["DROP TABLE ge_dates", "DROP TABLE ge_dates_key"],
            true,
        )
        .await;
    }
}

/// SQL Server's `datetime2(7)`, `datetimeoffset(7)` and `time(7)` keep 100 ns, and
/// `SYSDATETIME()` fills all seven digits (SDB-265, SDB-268). The grid shows the server's own text, every digit and the
/// value's own offset; "filter by this value" finds exactly the row it was written from when
/// another row differs only in the seventh digit; an edit keyed by such a value applies; and
/// a copied INSERT loads the same values back.
#[tokio::test]
async fn sqlserver_seventh_digit_filters_and_keys_hit_exactly_their_row() {
    for (label, session, schema) in engines().await {
        if label != "sqlserver" {
            continue;
        }
        let drop = [
            "DROP TABLE ge_seventh",
            "DROP TABLE ge_seventh_d2",
            "DROP TABLE ge_seventh_dto",
            "DROP TABLE ge_seventh_t",
        ];
        run(label, &session, &drop, true).await;
        // Now, with its seventh digit set to 3; the second row is 100 ns later, so both are in
        // the same microsecond.
        run(
            label,
            &session,
            &[
                "CREATE TABLE ge_seventh (id INT PRIMARY KEY, d2 DATETIME2(7), dto DATETIMEOFFSET(7), \
                 t TIME(7))",
                "DECLARE @d2 DATETIME2(7) = SYSDATETIME(); \
                 DECLARE @dto DATETIMEOFFSET(7) = SWITCHOFFSET(SYSDATETIMEOFFSET(), '+02:00'); \
                 SET @d2 = DATEADD(NANOSECOND, (3 - DATEPART(NANOSECOND, @d2) / 100 % 10) * 100, @d2); \
                 SET @dto = DATEADD(NANOSECOND, (3 - DATEPART(NANOSECOND, @dto) / 100 % 10) * 100, @dto); \
                 INSERT INTO ge_seventh VALUES (1, @d2, @dto, CAST(@d2 AS TIME(7))), \
                 (2, DATEADD(NANOSECOND, 100, @d2), DATEADD(NANOSECOND, 100, @dto), \
                  CAST(DATEADD(NANOSECOND, 100, @d2) AS TIME(7)))",
                "CREATE TABLE ge_seventh_d2 (k DATETIME2(7) PRIMARY KEY, n INT)",
                "INSERT INTO ge_seventh_d2 SELECT d2, id FROM ge_seventh",
                "CREATE TABLE ge_seventh_dto (k DATETIMEOFFSET(7) PRIMARY KEY, n INT)",
                "INSERT INTO ge_seventh_dto SELECT dto, id FROM ge_seventh",
                "CREATE TABLE ge_seventh_t (k TIME(7) PRIMARY KEY, n INT)",
                "INSERT INTO ge_seventh_t SELECT t, id FROM ge_seventh",
            ],
            false,
        )
        .await;
        let dialect = session.dialect();
        let cells = |id: ResultId| -> Vec<Vec<String>> {
            let stored = session.results().get(id).unwrap();
            let r = stored.read().unwrap();
            (0..r.rows())
                .map(|row| {
                    (1..4)
                        .map(|c| r.value_at(row, c).unwrap().to_string())
                        .collect()
                })
                .collect()
        };
        let server_text = run(
            label,
            &session,
            &[
                "SELECT id, CONVERT(VARCHAR(40), d2), CONVERT(VARCHAR(40), dto), \
                 CONVERT(VARCHAR(40), t) FROM ge_seventh ORDER BY id",
            ],
            false,
        )
        .await
        .unwrap();
        let server_text = cells(server_text);
        assert!(server_text[0][0].ends_with('3'), "{server_text:?}");
        assert!(server_text[0][1].ends_with("3 +02:00"), "{server_text:?}");
        assert!(server_text[0][2].ends_with('3'), "{server_text:?}");
        let select = "SELECT id, d2, dto, t FROM ge_seventh ORDER BY id";
        let id = run(label, &session, &[select], false).await.unwrap();
        assert_eq!(cells(id), server_text, "the grid shows the server's text");
        let stored = session.results().get(id).unwrap();

        // "Filter by this value": exactly the row it was written from.
        for row in 0..2 {
            for column in 1..4 {
                let (meta, value) = {
                    let r = stored.read().unwrap();
                    (
                        r.schema().columns[column].clone(),
                        r.value_at(row, column).unwrap(),
                    )
                };
                let condition = value_condition(
                    dialect.as_ref(),
                    &meta.name,
                    meta.logical,
                    FilterOp::Eq,
                    &value,
                )
                .unwrap();
                let filter = add_condition(dialect.as_ref(), "", &condition).unwrap();
                let rows = count_table_rows(&session, schema, "ge_seventh", &filter)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {filter}\n→ {e}"));
                assert_eq!(rows, 1, "{label}: {filter}");
            }
        }

        // An edit keyed by each type: the first row, and only it.
        for table_name in ["ge_seventh_d2", "ge_seventh_dto", "ge_seventh_t"] {
            let key_id = run(
                label,
                &session,
                &[&format!("SELECT k, n FROM {table_name} ORDER BY k")],
                false,
            )
            .await
            .unwrap();
            let keys = session.results().get(key_id).unwrap();
            let table = session
                .connection()
                .describe_table(schema, table_name)
                .await
                .unwrap();
            let statements = spelaion_core::edit_statements(
                dialect.as_ref(),
                &table,
                &keys.read().unwrap(),
                &[spelaion_core::RowChange::Update {
                    row: 0,
                    values: vec![(1, spelaion_core::CellInput::Text("20".into()))],
                }],
            )
            .unwrap();
            assert_eq!(
                spelaion_core::apply_edits(&session, &statements).await,
                Ok(1),
                "{table_name}: {statements:?}"
            );
            let edited = count_table_rows(&session, schema, table_name, "n = 20")
                .await
                .unwrap();
            let first = count_table_rows(&session, schema, table_name, "n = 1")
                .await
                .unwrap();
            assert_eq!((edited, first), (1, 0), "{table_name}");
        }

        // The INSERT copied from the grid loads every digit back.
        let mut copied = Vec::new();
        spelaion_core::write_result(
            &stored.read().unwrap(),
            &spelaion_core::ExportOptions {
                format: spelaion_core::ExportFormat::SqlInsert {
                    table: "ge_seventh".into(),
                },
                rows: 0..2,
                columns: (0..4).collect(),
                header: false,
                csv: spelaion_core::CsvOptions::default(),
            },
            dialect.as_ref(),
            &mut copied,
        )
        .unwrap();
        let copied = String::from_utf8(copied).unwrap();
        run(label, &session, &["DELETE FROM ge_seventh"], false).await;
        let events: Vec<CoreEvent> = session.run_script(&copied).collect().await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
            "{copied}\n{events:?}"
        );
        let id = run(label, &session, &[select], false).await.unwrap();
        assert_eq!(cells(id), server_text, "{copied}");

        // The bulk "now" writes a `datetimeoffset` with its offset, which the server reads as
        // this instant.
        let stored = session.results().get(id).unwrap();
        stored
            .write()
            .unwrap()
            .edit_pending(spelaion_core::PendingEdit::Bulk {
                block: spelaion_core::Block {
                    first_row: 0,
                    last_row: 1,
                    columns: vec![1, 2, 3],
                },
                action: spelaion_core::BulkAction::Now,
            })
            .unwrap();
        let table = session
            .connection()
            .describe_table(schema, "ge_seventh")
            .await
            .unwrap();
        let statements = {
            let stored = stored.read().unwrap();
            spelaion_core::edit_statements(
                dialect.as_ref(),
                &table,
                &stored,
                &stored.pending().changes(),
            )
            .unwrap()
        };
        assert_eq!(
            spelaion_core::apply_edits(&session, &statements).await,
            Ok(2),
            "{statements:?}"
        );
        let now = count_table_rows(
            &session,
            schema,
            "ge_seventh",
            "ABS(DATEDIFF(SECOND, dto, SYSDATETIMEOFFSET())) < 300",
        )
        .await
        .unwrap();
        assert_eq!(now, 2, "{statements:?}");
        run(label, &session, &drop, true).await;
    }
}

/// MySQL and MariaDB legacy zero dates after the first 10,000 rows (SDB-122): their date
/// columns are text with their native type, so the whole result arrives; sorting puts the zero
/// dates first, "filter by this value" finds exactly their rows, and a streaming export
/// finishes with every row and the server's text.
#[tokio::test]
async fn mysql_zero_dates_far_down_sort_filter_and_export() {
    for (label, session, schema) in engines().await {
        if label != "mysql" && label != "mariadb" {
            continue;
        }
        run(label, &session, &["SET SESSION sql_mode = ''"], false).await;
        run(label, &session, &["DROP TABLE IF EXISTS ge_zero"], false).await;
        let digits = "(select 0 n union all select 1 union all select 2 union all select 3 \
                      union all select 4 union all select 5 union all select 6 union all \
                      select 7 union all select 8 union all select 9)";
        run(
            label,
            &session,
            &[
                "CREATE TABLE ge_zero (id INT PRIMARY KEY, d DATE, dt DATETIME)",
                &format!(
                    "INSERT INTO ge_zero (id, d, dt) \
                     SELECT a.n + b.n * 10 + c.n * 100 + e.n * 1000 + 1, '2024-01-15', \
                            '2024-01-15 10:20:30' \
                     FROM {digits} a CROSS JOIN {digits} b CROSS JOIN {digits} c \
                     CROSS JOIN {digits} e"
                ),
                "INSERT INTO ge_zero VALUES (10001, '0000-00-00', '0000-00-00 00:00:00')",
            ],
            false,
        )
        .await;
        let id = run(
            label,
            &session,
            &["SELECT id, d, dt FROM ge_zero ORDER BY id"],
            false,
        )
        .await
        .unwrap_or_else(|| panic!("{label}: no result"));
        let stored = session.results().get(id).unwrap();
        let dialect = session.dialect();
        {
            let mut r = stored.write().unwrap();
            assert_eq!(r.rows(), 10_001, "{label}");
            let last = (
                r.value_at(10_000, 1).unwrap(),
                r.value_at(10_000, 2).unwrap(),
            );
            assert_eq!(
                (last.0.to_string(), last.1.to_string()),
                ("0000-00-00".to_string(), "0000-00-00 00:00:00".to_string()),
                "{label}"
            );
            // Sorting by the date: the zero date comes first, the server's text kept.
            r.sort(Some(spelaion_core::SortKey {
                column: 1,
                descending: false,
            }))
            .unwrap();
            let first = r.value_at(0, 1).unwrap().to_string();
            assert_eq!(first, "0000-00-00", "{label}");
            r.sort(None).unwrap();
        }
        // "Filter by this value" on the zero date, and on an ordinary date.
        for (row, expected) in [(10_000, 1), (0, 10_000)] {
            for column in [1, 2] {
                let (meta, value) = {
                    let r = stored.read().unwrap();
                    (
                        r.schema().columns[column].clone(),
                        r.value_at(row, column).unwrap(),
                    )
                };
                let condition = value_condition(
                    dialect.as_ref(),
                    &meta.name,
                    meta.logical,
                    FilterOp::Eq,
                    &value,
                )
                .unwrap();
                let filter = add_condition(dialect.as_ref(), "", &condition).unwrap();
                let rows = count_table_rows(&session, schema, "ge_zero", &filter)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {filter}\n→ {e}"));
                assert_eq!(rows, expected, "{label}: {filter}");
            }
        }
        // A streaming export of the table finishes, every row with the server's text.
        let dir =
            std::env::temp_dir().join(format!("spelaion-ge-zero-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("zero.csv");
        let export = spelaion_core::StreamExport {
            format: spelaion_core::ExportFormat::Csv,
            header: true,
            csv: spelaion_core::CsvOptions {
                delimiter: ',',
                null_text: "NULL".into(),
                escape_formulas: false,
            },
            encoding: spelaion_core::TextEncoding::Utf8,
            columns: None,
            batch_size: spelaion_core::DEFAULT_BATCH_ROWS,
        };
        let nothing = |_: spelaion_core::ExportProgress| {};
        let summary = spelaion_core::export_query(
            &session,
            "SELECT id, d, dt FROM ge_zero ORDER BY id",
            &spelaion_config::Safeguards::default(),
            false,
            &path,
            &export,
            &spelaion_core::ExportCancel::new(),
            &nothing,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: export: {e}"));
        assert_eq!(summary.rows, 10_001, "{label}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 10_002, "{label}");
        assert_eq!(
            text.lines().last(),
            Some("10001,0000-00-00,0000-00-00 00:00:00"),
            "{label}"
        );
        assert_eq!(
            text.lines().nth(1),
            Some("1,2024-01-15,2024-01-15 10:20:30"),
            "{label}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        run(label, &session, &["DROP TABLE ge_zero"], false).await;
    }
}
