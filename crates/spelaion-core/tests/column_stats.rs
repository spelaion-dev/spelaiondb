//! The column statistics popover against every engine: SQLite and DuckDB always, PostgreSQL,
//! MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without them they
//! are skipped, which does not count as tested).
//!
//! - The numbers match the table, with and without the tab's filter, and a big table is
//!   sampled and says so.
//! - Each most frequent value's condition keeps exactly the rows it was counted from.
//! - A number the engine refuses for the column's type leaves the others.
//! - Every statement is a read, inside the engine's read-only transaction: the session cannot
//!   write afterwards in the same transaction, and a filter cannot change the statements.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    ColumnStats, ColumnStatsError, ColumnStatsRequest, CoreEvent, ExportCancel, ResultStore,
    Session, Stat, add_condition, column_stats, count_table_rows,
};
use spelaion_driver_api::{ConnectParams, Driver, LogicalType, Secret, TlsMode};

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
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

/// Runs each statement on its own; panics on a failure unless `tolerate`.
async fn run(label: &str, session: &Session, statements: &[&str], tolerate: bool) {
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            if let CoreEvent::StatementFailed { error, .. } = event
                && !tolerate
            {
                panic!("{label}: {statement}\n→ {}", error.message)
            }
        }
    }
}

const TABLE: &str = "cs_items";

/// Six rows: repeated names and prices, NULLs in every column but the key.
async fn items(label: &str, session: &Session) {
    let (t, f, boolean) = match label {
        "sqlserver" => ("1", "0", "BIT"),
        _ => ("TRUE", "FALSE", "BOOLEAN"),
    };
    run(label, session, &[&format!("DROP TABLE {TABLE}")], true).await;
    let create = format!(
        "CREATE TABLE {TABLE} (id INT PRIMARY KEY, name VARCHAR(40), price DECIMAL(10, 2), ok {boolean})"
    );
    let insert = format!(
        "INSERT INTO {TABLE} (id, name, price, ok) VALUES \
         (1, 'Ana', 10.50, {t}), (2, 'Bob', 20.00, {f}), (3, 'Ana', NULL, {t}), \
         (4, NULL, 20.00, {t}), (5, 'Zoe', 7.25, {f}), (6, 'Ana', 20.00, NULL)"
    );
    run(label, session, &[&create, &insert], false).await;
}

fn request(schema: &str, column: &str, logical: LogicalType, filter: &str) -> ColumnStatsRequest {
    ColumnStatsRequest {
        schema: schema.into(),
        table: TABLE.into(),
        column: column.into(),
        logical,
        filter: filter.into(),
        sample_rows: 1_000,
        top: 10,
    }
}

async fn stats(label: &str, session: &Session, request: &ColumnStatsRequest) -> ColumnStats {
    column_stats(session, request, &ExportCancel::new())
        .await
        .unwrap_or_else(|e| panic!("{label}: {} → {e}", request.column))
}

fn number(text: Option<&String>) -> f64 {
    text.and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("not a number: {text:?}"))
}

/// Each listed value's condition, added to the filter, keeps the rows it was counted from.
async fn conditions_keep_their_rows(
    label: &str,
    session: &Session,
    schema: &str,
    filter: &str,
    s: &ColumnStats,
) {
    let Stat::Value(top) = &s.top else {
        panic!("{label}: no top values: {:?}", s.top)
    };
    for value in top {
        let condition = value
            .condition
            .as_deref()
            .unwrap_or_else(|| panic!("{label}: no condition for {:?}", value.text));
        let filter = add_condition(session.dialect().as_ref(), filter, condition).unwrap();
        let kept = count_table_rows(session, schema, TABLE, &filter)
            .await
            .unwrap_or_else(|e| panic!("{label}: {filter} → {e}"));
        assert_eq!(kept as u64, value.count, "{label}: {filter}");
    }
}

#[tokio::test]
async fn column_statistics_on_every_engine() {
    for (label, session, schema) in engines().await {
        items(label, &session).await;

        // Text: three names, one NULL; "Ana" three times.
        let s = stats(
            label,
            &session,
            &request(schema, "name", LogicalType::Text, ""),
        )
        .await;
        assert_eq!((s.rows, s.nulls, s.sampled), (6, 1, false), "{label}");
        assert_eq!(s.distinct, Stat::Value(3), "{label}");
        assert_eq!(
            s.range,
            Stat::Value((Some("Ana".into()), Some("Zoe".into()))),
            "{label}"
        );
        let Stat::Value(top) = &s.top else {
            panic!("{label}: {:?}", s.top)
        };
        assert_eq!(top.len(), 4, "{label}");
        assert_eq!((top[0].text.as_deref(), top[0].count), (Some("Ana"), 3));
        let mut rest: Vec<(Option<&str>, u64)> = top[1..]
            .iter()
            .map(|v| (v.text.as_deref(), v.count))
            .collect();
        rest.sort();
        assert_eq!(rest, [(None, 1), (Some("Bob"), 1), (Some("Zoe"), 1)]);
        conditions_keep_their_rows(label, &session, schema, "", &s).await;
        assert!(s.statements.len() == 5, "{label}: {:?}", s.statements);

        // Decimals: 20.00 three times; their text is each engine's.
        let s = stats(
            label,
            &session,
            &request(schema, "price", LogicalType::Decimal, ""),
        )
        .await;
        assert_eq!((s.rows, s.nulls), (6, 1), "{label}");
        assert_eq!(s.distinct, Stat::Value(3), "{label}");
        let Stat::Value((min, max)) = &s.range else {
            panic!("{label}: {:?}", s.range)
        };
        assert_eq!((number(min.as_ref()), number(max.as_ref())), (7.25, 20.0));
        let Stat::Value(top) = &s.top else {
            panic!("{label}: {:?}", s.top)
        };
        assert_eq!((number(top[0].text.as_ref()), top[0].count), (20.0, 3));
        conditions_keep_their_rows(label, &session, schema, "", &s).await;

        // Booleans: no minimum, but their counts (and their conditions, `bit` included).
        let s = stats(
            label,
            &session,
            &request(schema, "ok", LogicalType::Bool, ""),
        )
        .await;
        assert_eq!(s.range, Stat::NotForType, "{label}");
        assert_eq!(s.distinct, Stat::Value(2), "{label}");
        let Stat::Value(top) = &s.top else {
            panic!("{label}: {:?}", s.top)
        };
        assert_eq!(top[0].count, 3, "{label}");
        conditions_keep_their_rows(label, &session, schema, "", &s).await;

        // The tab's filter: ids 3 to 6.
        let filter = "id > 2";
        let s = stats(
            label,
            &session,
            &request(schema, "name", LogicalType::Text, filter),
        )
        .await;
        assert_eq!((s.rows, s.nulls), (4, 1), "{label}");
        assert_eq!(s.distinct, Stat::Value(2), "{label}");
        conditions_keep_their_rows(label, &session, schema, filter, &s).await;

        // A big table (here: more than 3 rows) is sampled: the first 3 rows read.
        let mut sampled = request(schema, "name", LogicalType::Text, "");
        sampled.sample_rows = 3;
        let s = stats(label, &session, &sampled).await;
        assert!(s.sampled, "{label}");
        assert_eq!(s.rows, 3, "{label}");
        // Exactly the sample size is not sampled.
        sampled.sample_rows = 6;
        assert!(!stats(label, &session, &sampled).await.sampled, "{label}");

        // A filter cannot change the statements: refused before anything runs.
        for filter in [
            "1 = 1) UNION SELECT name FROM cs_items --",
            "1 = 1; DELETE FROM cs_items",
        ] {
            let outcome = column_stats(
                &session,
                &request(schema, "name", LogicalType::Text, filter),
                &ExportCancel::new(),
            )
            .await;
            assert!(
                matches!(outcome, Err(ColumnStatsError::Invalid(_))),
                "{label}: {filter} → {outcome:?}"
            );
        }

        // Cancelled before it starts: nothing runs.
        let cancel = ExportCancel::new();
        cancel.cancel().await;
        assert_eq!(
            column_stats(
                &session,
                &request(schema, "name", LogicalType::Text, ""),
                &cancel
            )
            .await,
            Err(ColumnStatsError::Cancelled),
            "{label}"
        );

        // Nothing was written along the way.
        assert_eq!(
            count_table_rows(&session, schema, TABLE, "").await,
            Ok(6),
            "{label}"
        );
    }
}

/// PostgreSQL's `json` has no equality: distinct values and the most frequent ones fail, and
/// each failure stays in its own read-only transaction, so the counts and the next numbers
/// still come.
#[tokio::test]
async fn a_number_the_engine_refuses_leaves_the_others() {
    let Some((host, port)) = server("SPELAION_TEST_PG") else {
        return;
    };
    let params = ConnectParams {
        host,
        port,
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: "spelaion".into(),
        ..Default::default()
    };
    let session = session(&spelaion_driver_postgres::PostgresDriver, params).await;
    run(
        "postgres",
        &session,
        &[
            "DROP TABLE IF EXISTS cs_docs",
            "CREATE TABLE cs_docs (id INT PRIMARY KEY, doc JSON)",
            "INSERT INTO cs_docs VALUES (1, '{\"a\": 1}'), (2, NULL), (3, '[1, 2]')",
        ],
        false,
    )
    .await;
    let request = ColumnStatsRequest {
        schema: "public".into(),
        table: "cs_docs".into(),
        column: "doc".into(),
        logical: LogicalType::Json,
        filter: String::new(),
        sample_rows: 1_000,
        top: 10,
    };
    let s = column_stats(&session, &request, &ExportCancel::new())
        .await
        .unwrap();
    assert_eq!((s.rows, s.nulls), (3, 1));
    assert!(
        matches!(s.distinct, Stat::Failed(ref m) if m.contains("equality")),
        "{:?}",
        s.distinct
    );
    assert_eq!(s.range, Stat::NotForType);
    assert!(matches!(s.top, Stat::Failed(_)), "{:?}", s.top);
    // The session is usable afterwards: no transaction left aborted.
    assert_eq!(
        count_table_rows(&session, "public", "cs_docs", "").await,
        Ok(3)
    );
}
