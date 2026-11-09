//! The data search (ADR-043) against every engine: SQLite and DuckDB always, PostgreSQL,
//! MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without them they are
//! skipped, which does not count as tested).
//!
//! - Text matches with and without case, and the value is plain text: `%`, `_`, `[` and quotes
//!   mean nothing, and a value written like SQL changes nothing.
//! - Numeric, large text and binary columns are searched only when asked.
//! - Each hit's filter keeps exactly its row, with a primary key and without one.
//! - A table with nothing to search, and one that fails, are reported and the search goes on.
//! - The column catalog of metadata search (`list_columns`) lists every table's columns.
//! - A search over 50 tables stops when cancelled mid-way, and the session is still usable.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, DataSearchEvent, DataSearchOptions, DataSearchRequest, DataSearchSummary,
    ExportCancel, ResultStore, SearchHit, SearchTable, Session, TableOutcome, TableStatus,
    count_table_rows, data_search,
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

/// Runs each statement on its own; panics on a failure unless `tolerate`.
async fn run(label: &str, session: &Session, statements: &[String], tolerate: bool) {
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

/// The engine's types for the tables: large text (or plain text where the engine has no large
/// type) and binary, and how to write the bytes of `xx secret`.
fn types(label: &str) -> (&'static str, bool, &'static str, &'static str) {
    match label {
        "postgres" => ("JSON", true, "BYTEA", "decode('787820736563726574', 'hex')"),
        "mysql" | "mariadb" => ("TEXT", true, "VARBINARY(64)", "X'787820736563726574'"),
        "sqlserver" => ("NTEXT", true, "VARBINARY(64)", "0x787820736563726574"),
        "duckdb" => ("VARCHAR", false, "BLOB", "unhex('787820736563726574')"),
        _ => ("TEXT", false, "BLOB", "X'787820736563726574'"),
    }
}

async fn tables(label: &str, session: &Session) {
    let (large, _, binary, secret) = types(label);
    // PostgreSQL's JSON column needs JSON text.
    let note = |text: &str| {
        if label == "postgres" {
            format!("'\"{text}\"'")
        } else {
            format!("'{text}'")
        }
    };
    let drops = ["ds_people", "ds_log", "ds_dates"]
        .map(|t| format!("DROP TABLE {t}"))
        .to_vec();
    run(label, session, &drops, true).await;
    run(
        label,
        session,
        &[
            format!(
                "CREATE TABLE ds_people (id INT PRIMARY KEY, name VARCHAR(60), note {large}, \
                 amount DECIMAL(10, 2), photo {binary}, born DATE)"
            ),
            format!(
                "INSERT INTO ds_people (id, name, note, amount, photo, born) VALUES \
                 (1, 'Ana Smith', {}, 10.50, {secret}, NULL)",
                note("quiet")
            ),
            format!(
                "INSERT INTO ds_people (id, name, note, amount) VALUES \
                 (2, 'BOB', {}, 42.00)",
                note("contains needle deep inside")
            ),
            "INSERT INTO ds_people (id, name, amount) VALUES (3, 'it''s %_[x]', 7.25)".into(),
            "INSERT INTO ds_people (id, amount) VALUES (4, 42)".into(),
            "CREATE TABLE ds_log (msg VARCHAR(60), n INT)".into(),
            "INSERT INTO ds_log (msg, n) VALUES ('needle one', 1)".into(),
            "INSERT INTO ds_log (msg, n) VALUES ('other', 2)".into(),
            "CREATE TABLE ds_dates (d DATE)".into(),
        ],
        false,
    )
    .await;
}

fn t(schema: &str, table: &str) -> SearchTable {
    SearchTable {
        schema: schema.into(),
        table: table.into(),
    }
}

/// Runs a search and collects every table's outcome.
async fn search(
    session: &Session,
    value: &str,
    tables: Vec<SearchTable>,
    options: DataSearchOptions,
) -> (DataSearchSummary, Vec<TableOutcome>) {
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let seen = outcomes.clone();
    let progress = move |event: DataSearchEvent| {
        if let DataSearchEvent::Finished(outcome) = event {
            seen.lock().unwrap().push(outcome);
        }
    };
    let request = DataSearchRequest {
        value: value.into(),
        tables,
        options,
    };
    let summary = data_search(session, &request, &ExportCancel::new(), &progress)
        .await
        .unwrap_or_else(|e| panic!("{value}: {e}"));
    let outcomes = outcomes.lock().unwrap().clone();
    (summary, outcomes)
}

fn hits_of<'a>(outcomes: &'a [TableOutcome], table: &str) -> &'a [SearchHit] {
    match &outcomes
        .iter()
        .find(|o| o.table == table)
        .unwrap_or_else(|| panic!("{table} not searched"))
        .status
    {
        TableStatus::Searched { hits, .. } => hits,
        other => panic!("{table}: {other:?}"),
    }
}

/// The ids of a `ds_people` search's hits, in order.
fn ids(hits: &[SearchHit]) -> Vec<String> {
    let mut ids: Vec<String> = hits
        .iter()
        .map(|h| {
            assert_eq!(h.key.len(), 1, "{h:?}");
            h.key[0].1.clone()
        })
        .collect();
    ids.sort();
    ids
}

fn matched(hit: &SearchHit) -> Vec<&str> {
    hit.matches.iter().map(|m| m.column.as_str()).collect()
}

#[tokio::test]
async fn data_search_finds_values_on_every_engine() {
    for (label, session, schema) in engines().await {
        tables(label, &session).await;
        let (_, has_large, _, _) = types(label);
        let people = || vec![t(schema, "ds_people")];
        let plain = DataSearchOptions::default();

        // Case insensitive by default, case sensitive when asked (even on a _ci collation).
        let (_, out) = search(&session, "ana", people(), plain.clone()).await;
        let hits = hits_of(&out, "ds_people");
        assert_eq!(ids(hits), ["1"], "{label}");
        assert_eq!(matched(&hits[0]), ["name"], "{label}");
        assert_eq!(
            hits[0].matches[0].text.as_deref(),
            Some("Ana Smith"),
            "{label}"
        );
        let sensitive = DataSearchOptions {
            case_sensitive: true,
            ..plain.clone()
        };
        let (_, out) = search(&session, "ana", people(), sensitive.clone()).await;
        assert!(hits_of(&out, "ds_people").is_empty(), "{label}");
        let (_, out) = search(&session, "Ana", people(), sensitive.clone()).await;
        assert_eq!(ids(hits_of(&out, "ds_people")), ["1"], "{label}");
        let (_, out) = search(&session, "bob", people(), plain.clone()).await;
        assert_eq!(ids(hits_of(&out, "ds_people")), ["2"], "{label}");

        // The value is plain text: wildcards and quotes match only themselves.
        for value in ["%_[", "s %", "[x]", "it's", "_"] {
            let (_, out) = search(&session, value, people(), plain.clone()).await;
            assert_eq!(ids(hits_of(&out, "ds_people")), ["3"], "{label}: {value}");
        }
        let (_, out) = search(
            &session,
            "x'); DROP TABLE ds_people; --",
            people(),
            plain.clone(),
        )
        .await;
        assert!(hits_of(&out, "ds_people").is_empty(), "{label}");
        assert_eq!(
            count_table_rows(&session, schema, "ds_people", "").await,
            Ok(4),
            "{label}"
        );

        // Numbers only when asked, by equality (42 and 42.00).
        let (_, out) = search(&session, "42", people(), plain.clone()).await;
        assert!(hits_of(&out, "ds_people").is_empty(), "{label}");
        let numbers = DataSearchOptions {
            numbers: true,
            ..plain.clone()
        };
        let (_, out) = search(&session, "42", people(), numbers.clone()).await;
        let hits = hits_of(&out, "ds_people");
        assert_eq!(ids(hits), ["2", "4"], "{label}");
        assert!(hits.iter().all(|h| matched(h) == ["amount"]), "{label}");
        let (_, out) = search(&session, "10.5", people(), numbers).await;
        assert_eq!(ids(hits_of(&out, "ds_people")), ["1"], "{label}");

        // Large text and binary columns only when asked.
        let large = DataSearchOptions {
            large: true,
            ..plain.clone()
        };
        let (_, out) = search(&session, "needle", people(), plain.clone()).await;
        let expected: &[&str] = if has_large { &[] } else { &["2"] };
        assert_eq!(ids(hits_of(&out, "ds_people")), expected, "{label}");
        let (_, out) = search(&session, "needle", people(), large.clone()).await;
        let hits = hits_of(&out, "ds_people");
        assert_eq!(ids(hits), ["2"], "{label}");
        assert_eq!(matched(&hits[0]), ["note"], "{label}");
        assert!(
            hits[0].matches[0]
                .text
                .as_deref()
                .is_some_and(|s| s.contains("needle")),
            "{label}: {hits:?}"
        );
        let (_, out) = search(&session, "secret", people(), plain.clone()).await;
        assert!(hits_of(&out, "ds_people").is_empty(), "{label}");
        let (_, out) = search(&session, "secret", people(), large).await;
        let hits = hits_of(&out, "ds_people");
        assert_eq!(ids(hits), ["1"], "{label}");
        assert_eq!(matched(&hits[0]), ["photo"], "{label}");
        assert_eq!(hits[0].matches[0].text, None, "{label}");

        // Every table in one search: each hit's filter keeps exactly its row; a table without
        // a key is filtered by the matching value; one with only dates and a missing one are
        // reported and the search goes on.
        let (summary, out) = search(
            &session,
            "o",
            vec![
                t(schema, "ds_missing"),
                t(schema, "ds_people"),
                t(schema, "ds_log"),
                t(schema, "ds_dates"),
            ],
            plain.clone(),
        )
        .await;
        assert_eq!(out.len(), 4, "{label}");
        assert!(
            matches!(out[0].status, TableStatus::Failed(_)),
            "{label}: {:?}",
            out[0]
        );
        assert_eq!(out[3].status, TableStatus::NoColumns, "{label}");
        assert_eq!(summary.tables, 4, "{label}");
        assert!(!summary.cancelled && !summary.stopped, "{label}");
        for table in ["ds_people", "ds_log"] {
            let hits = hits_of(&out, table);
            assert!(!hits.is_empty(), "{label}: {table}");
            for hit in hits {
                let filter = hit
                    .filter
                    .as_deref()
                    .unwrap_or_else(|| panic!("{label}: {hit:?}"));
                assert_eq!(
                    count_table_rows(&session, schema, table, filter).await,
                    Ok(1),
                    "{label}: {table} {filter}"
                );
            }
        }
        let log = hits_of(&out, "ds_log");
        assert_eq!(log.len(), 2, "{label}: {log:?}");
        assert!(log.iter().all(|h| h.key.is_empty()), "{label}");

        // A row limit per table, and the summary counts what was found.
        let (summary, out) = search(
            &session,
            "o",
            people(),
            DataSearchOptions {
                rows_per_table: 1,
                ..plain.clone()
            },
        )
        .await;
        match &out[0].status {
            TableStatus::Searched { hits, more, .. } => {
                assert_eq!(hits.len(), 1, "{label}");
                assert!(*more, "{label}");
            }
            other => panic!("{label}: {other:?}"),
        }
        assert_eq!(summary.hits, 1, "{label}");
        // Every statement was a read: the session is in no transaction afterwards.
        assert!(
            summary
                .statements
                .iter()
                .all(|s| s.trim_start().starts_with("SELECT")),
            "{label}: {:?}",
            summary.statements
        );
        assert_eq!(
            session.connection().transaction_state().await.ok(),
            Some(spelaion_driver_api::TransactionState::Idle),
            "{label}"
        );

        // The column catalog of metadata search: every table, its columns in order.
        let columns = session
            .connection()
            .list_columns(schema)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let people_columns: Vec<&str> = columns
            .iter()
            .filter(|c| c.table == "ds_people")
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(
            people_columns,
            ["id", "name", "note", "amount", "photo", "born"],
            "{label}"
        );
        assert!(
            columns
                .iter()
                .any(|c| c.table == "ds_log" && c.name == "msg"),
            "{label}"
        );
        assert!(
            columns
                .iter()
                .filter(|c| c.table == "ds_people")
                .all(|c| !c.native_type.is_empty()),
            "{label}: {columns:?}"
        );

        let drops = ["ds_people", "ds_log", "ds_dates"]
            .map(|t| format!("DROP TABLE {t}"))
            .to_vec();
        run(label, &session, &drops, true).await;
    }
}

#[tokio::test]
async fn a_search_over_fifty_tables_stops_when_cancelled() {
    for (label, session, schema) in engines().await {
        let names: Vec<String> = (0..50).map(|i| format!("ds_many_{i:02}")).collect();
        let mut setup = Vec::new();
        for name in &names {
            setup.push(format!("DROP TABLE {name}"));
        }
        run(label, &session, &setup, true).await;
        let mut create = Vec::new();
        for name in &names {
            create.push(format!(
                "CREATE TABLE {name} (id INT PRIMARY KEY, label VARCHAR(40))"
            ));
            create.push(format!(
                "INSERT INTO {name} (id, label) VALUES (1, 'needle'), (2, 'hay')"
            ));
        }
        run(label, &session, &create, false).await;

        let cancel = Arc::new(ExportCancel::new());
        cancel.attach(&session);
        let started = Arc::new(Mutex::new(0usize));
        let finished = Arc::new(Mutex::new(Vec::new()));
        let progress = {
            let cancel = cancel.clone();
            let started = started.clone();
            let finished = finished.clone();
            move |event: DataSearchEvent| match event {
                DataSearchEvent::Started { index, total, .. } => {
                    assert_eq!(total, 50);
                    *started.lock().unwrap() = index + 1;
                    // Cancel while the eleventh table is being searched.
                    if index == 10 {
                        let cancel = cancel.clone();
                        tokio::spawn(async move { cancel.cancel().await });
                    }
                }
                DataSearchEvent::Finished(outcome) => finished.lock().unwrap().push(outcome),
            }
        };
        let request = DataSearchRequest {
            value: "needle".into(),
            tables: names.iter().map(|n| t(schema, n)).collect(),
            options: DataSearchOptions::default(),
        };
        let summary = data_search(&session, &request, &cancel, &progress)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert!(summary.cancelled, "{label}");
        let started = *started.lock().unwrap();
        assert!((11..50).contains(&started), "{label}: {started} started");
        let finished = finished.lock().unwrap().clone();
        assert!(finished.len() < 50, "{label}: {}", finished.len());
        for outcome in &finished[..10] {
            match &outcome.status {
                TableStatus::Searched { hits, .. } => assert_eq!(hits.len(), 1, "{label}"),
                other => panic!("{label}: {other:?}"),
            }
        }
        // The session is still usable afterwards.
        assert_eq!(
            count_table_rows(&session, schema, &names[0], "").await,
            Ok(2),
            "{label}"
        );
        let drops: Vec<String> = names.iter().map(|n| format!("DROP TABLE {n}")).collect();
        run(label, &session, &drops, true).await;
    }
}

/// A statement that runs for a long time is stopped by Cancel, not waited for.
#[tokio::test]
async fn cancel_stops_the_running_statement() {
    let Some((host, port)) = server("SPELAION_TEST_PG") else {
        return;
    };
    let session = session(
        &spelaion_driver_postgres::PostgresDriver,
        ConnectParams {
            host,
            port,
            user: "spelaion".into(),
            password: Some(Secret::new("spelaion")),
            database: "spelaion".into(),
            ..Default::default()
        },
    )
    .await;
    run(
        "postgres",
        &session,
        &[
            "DROP VIEW IF EXISTS ds_slow".into(),
            "CREATE VIEW ds_slow AS SELECT 'needle' || pg_sleep(30)::text AS label".into(),
        ],
        false,
    )
    .await;
    let cancel = Arc::new(ExportCancel::new());
    cancel.attach(&session);
    let later = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        later.cancel().await;
    });
    let started = std::time::Instant::now();
    let request = DataSearchRequest {
        value: "needle".into(),
        tables: vec![t("public", "ds_slow"), t("public", "ds_slow")],
        options: DataSearchOptions::default(),
    };
    let summary = data_search(&session, &request, &cancel, &|_| {})
        .await
        .unwrap();
    assert!(summary.cancelled);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    run("postgres", &session, &["DROP VIEW ds_slow".into()], false).await;
}

/// A read-only connection (a SQLite file opened read-only) searches like any other.
#[tokio::test]
async fn a_read_only_connection_can_search() {
    let path = std::env::temp_dir().join(format!("spelaion-data-search-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let writer = session(
            &spelaion_driver_sqlite::SqliteDriver {
                create_if_missing: true,
            },
            ConnectParams {
                database: path.to_string_lossy().into_owned(),
                ..Default::default()
            },
        )
        .await;
        run(
            "sqlite",
            &writer,
            &[
                "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT)".into(),
                "INSERT INTO items VALUES (1, 'red needle'), (2, 'blue')".into(),
            ],
            false,
        )
        .await;
    }
    let reader = session(
        &spelaion_driver_sqlite::SqliteDriver::default(),
        ConnectParams {
            database: path.to_string_lossy().into_owned(),
            read_only: true,
            ..Default::default()
        },
    )
    .await;
    let (summary, out) = search(
        &reader,
        "NEEDLE",
        vec![t("main", "items")],
        DataSearchOptions::default(),
    )
    .await;
    assert_eq!(summary.hits, 1);
    assert_eq!(hits_of(&out, "items")[0].filter.as_deref(), Some("id = 1"));
    drop(reader);
    let _ = std::fs::remove_file(&path);
}
