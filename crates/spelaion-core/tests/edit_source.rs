//! Edits are written only through the session that read the rows, in the database it read
//! them in (SDB-137). SQLite and DuckDB always; PostgreSQL, MySQL, MariaDB and SQL Server with
//! their `SPELAION_TEST_*` variables (without them they are skipped, which does not count as
//! tested).
//!
//! Each engine has two sessions on two databases that hold the same table with the same key,
//! sharing one result store as the app's tabs do. A result read on the first is not editable
//! on the second, and its edit statements and file loads are refused there; so they are on a
//! new session to the first database (a reconnected tab) and, on SQL Server, DuckDB, MySQL and
//! MariaDB, on the first session once `USE` moved it to the other database. Nothing is written
//! anywhere until the first session, back in its database, applies the edit to its own table.

mod engines;

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CellEdit, CellInput, CoreEvent, EditError, PendingEdit, ResultId, ResultStore, Session,
    apply_edits, edit_statements, editable_query_table, file_change,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

const TABLE: &str = "edit_source";
const QUERY: &str = "select id, name from edit_source";

/// Two sessions of one engine, on two databases holding `TABLE`.
struct Pair {
    label: &'static str,
    driver: Arc<dyn Driver>,
    first: ConnectParams,
    second: ConnectParams,
    /// The schema `TABLE` is in, in both databases.
    schema: &'static str,
    /// Statements run on the first session before the table is created in each database.
    prepare: Vec<String>,
    /// `USE` statements that move the first session to the second database and back, where the
    /// engine has them (not PostgreSQL or SQLite).
    switch: Option<(String, String)>,
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spelaion-edit-source-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// An empty file: SQLite opens only a file that exists.
fn empty_file(name: &str) -> PathBuf {
    let path = scratch(name);
    std::fs::write(&path, b"").unwrap();
    path
}

fn file(path: &std::path::Path) -> ConnectParams {
    ConnectParams {
        database: path.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

fn login(host: &str, port: u16, user: &str, password: &str, database: &str) -> ConnectParams {
    ConnectParams {
        host: host.into(),
        port,
        user: user.into(),
        password: Some(Secret::new(password)),
        database: database.into(),
        ..Default::default()
    }
}

async fn pairs() -> Vec<Pair> {
    let duck_a = scratch("a.duckdb");
    let duck_b = scratch("b.duckdb");
    for path in [&duck_a, &duck_b] {
        engines::create_database(spelaion_config::Engine::Duckdb, path).await;
    }
    let mut out = vec![
        Pair {
            label: "sqlite",
            driver: engines::sqlite(),
            first: file(&empty_file("a.sqlite")),
            second: file(&empty_file("b.sqlite")),
            schema: "main",
            prepare: vec![],
            switch: None,
        },
        Pair {
            label: "duckdb",
            driver: engines::duckdb(),
            first: file(&duck_a),
            second: file(&duck_b),
            schema: "main",
            prepare: vec![],
            // The first session attaches a third file holding the table too, to `USE` it.
            switch: Some(("use c.main".into(), "use a.main".into())),
        },
    ];
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        out.push(Pair {
            label: "postgres",
            driver: engines::postgres(),
            first: login(&host, port, "spelaion", "spelaion", "spelaion"),
            second: login(&host, port, "spelaion", "spelaion", "spelaion_gis"),
            schema: "public",
            prepare: vec![],
            switch: None,
        });
    }
    for (label, var, driver) in [
        ("mysql", "SPELAION_TEST_MYSQL", engines::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", engines::mariadb()),
    ] {
        if let Some((host, port)) = server(var) {
            out.push(Pair {
                label,
                driver,
                // Root, to create the second database.
                first: login(&host, port, "root", "spelaion-root", "spelaion"),
                second: login(&host, port, "root", "spelaion-root", "spelaion_edit_source"),
                schema: "",
                prepare: vec!["create database if not exists spelaion_edit_source".into()],
                switch: Some(("use spelaion_edit_source".into(), "use spelaion".into())),
            });
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let tls = |params: ConnectParams| ConnectParams {
            tls: TlsMode::Require,
            ..params
        };
        out.push(Pair {
            label: "sqlserver",
            driver: engines::sqlserver(),
            first: tls(login(&host, port, "sa", "Spelaion-test-1", "tempdb")),
            second: tls(login(&host, port, "sa", "Spelaion-test-1", "spelaion")),
            schema: "dbo",
            prepare: vec!["if db_id('spelaion') is null create database spelaion".into()],
            switch: Some(("use spelaion".into(), "use tempdb".into())),
        });
    }
    out
}

async fn session(pair: &Pair, params: &ConnectParams, store: &Arc<ResultStore>) -> Session {
    let conn = pair
        .driver
        .connect(params)
        .await
        .unwrap_or_else(|e| panic!("{}: connecting: {e}", pair.label));
    Session::new(
        conn,
        spelaion_dialect::for_engine(pair.driver.engine()).expect("a dialect"),
        store.clone(),
    )
}

/// Runs `sql`, panicking on a failure; returns its first result, if any.
async fn run(label: &str, session: &Session, sql: &str) -> Option<ResultId> {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let mut first = None;
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => {
                panic!("{label}: {sql}\n→ {}", error.message)
            }
            CoreEvent::ResultStarted { result, .. } => {
                first.get_or_insert(*result);
            }
            _ => {}
        }
    }
    first
}

/// The name of row 1 of `TABLE`, read through `session`.
async fn name(label: &str, session: &Session) -> String {
    let id = run(label, session, "select name from edit_source where id = 1")
        .await
        .expect("a result");
    let stored = session.results().get(id).expect("kept");
    let stored = stored.read().unwrap();
    match stored.value_at(0, 0) {
        Some(Value::Text(text)) => text,
        other => panic!("{label}: name is {other:?}"),
    }
}

/// The table's description, through `session` (its own catalog, as the app reads it).
async fn table(pair: &Pair, session: &Session) -> spelaion_driver_api::TableInfo {
    let schema = if pair.schema.is_empty() {
        session
            .connection()
            .current_database()
            .await
            .unwrap()
            .expect("a database")
    } else {
        pair.schema.to_string()
    };
    session
        .connection()
        .describe_table(&schema, TABLE)
        .await
        .unwrap_or_else(|e| panic!("{}: describe: {e}", pair.label))
}

/// Asserts that `result` is not editable through `session` and that its edit statements and
/// a file loaded into one of its cells are refused there.
async fn refused(
    pair: &Pair,
    session: &Session,
    result: ResultId,
    file: &std::path::Path,
    why: &str,
) {
    let label = pair.label;
    assert_eq!(
        editable_query_table(session, result, QUERY, false).await,
        Ok(None),
        "{label}: {why}: editable"
    );
    let table = table(pair, session).await;
    let target = session.edit_target().await.unwrap();
    let stored = session.results().get(result).expect("kept");
    let stored = stored.read().unwrap();
    assert_eq!(
        edit_statements(&target, &table, &stored, &stored.pending().changes()),
        Err(EditError::OtherSource),
        "{label}: {why}: edit statements"
    );
    let change = file_change(&stored, 0, 1, file).unwrap();
    assert_eq!(
        edit_statements(&target, &table, &stored, &[change]),
        Err(EditError::OtherSource),
        "{label}: {why}: file load"
    );
}

#[tokio::test]
async fn edits_are_written_only_where_the_rows_were_read() {
    let loaded = scratch("loaded.txt");
    std::fs::write(&loaded, "from a file").unwrap();
    for pair in pairs().await {
        let label = pair.label;
        let store = Arc::new(ResultStore::new());
        let first = session(&pair, &pair.first, &store).await;
        for statement in &pair.prepare {
            run(label, &first, statement).await;
        }
        let second = session(&pair, &pair.second, &store).await;
        for (session, value) in [(&first, "first"), (&second, "second")] {
            run(label, session, "drop table if exists edit_source").await;
            run(
                label,
                session,
                "create table edit_source (id int primary key, name varchar(40))",
            )
            .await;
            run(
                label,
                session,
                &format!("insert into edit_source values (1, '{value}')"),
            )
            .await;
        }

        // Read on the first session: editable there, with one change typed into the grid.
        let result = run(label, &first, QUERY).await.expect("a result");
        assert!(
            editable_query_table(&first, result, QUERY, false)
                .await
                .unwrap()
                .is_some(),
            "{label}: editable where it was read"
        );
        {
            let stored = first.results().get(result).unwrap();
            let mut stored = stored.write().unwrap();
            stored
                .edit_pending(PendingEdit::Type(CellEdit {
                    row: 0,
                    column: 1,
                    value: Some(CellInput::Text("changed".into())),
                }))
                .unwrap();
        }

        // Another connection's session, to a database with the same table and key.
        refused(&pair, &second, result, &loaded, "another database").await;
        // A new session to the same database: a reconnected tab.
        let reconnected = session(&pair, &pair.first, &store).await;
        refused(&pair, &reconnected, result, &loaded, "a new session").await;
        drop(reconnected);
        // The same session, moved to the other database.
        if let Some((there, back)) = &pair.switch {
            if label == "duckdb" {
                let attached = scratch("c.duckdb").to_string_lossy().replace('\'', "''");
                run(label, &first, &format!("attach '{attached}' as c")).await;
                run(
                    label,
                    &first,
                    "create table c.main.edit_source (id int primary key, name varchar(40))",
                )
                .await;
                run(
                    label,
                    &first,
                    "insert into c.main.edit_source values (1, 'c')",
                )
                .await;
            }
            run(label, &first, there).await;
            refused(&pair, &first, result, &loaded, "after USE").await;
            run(label, &first, back).await;
            if label == "duckdb" {
                assert_eq!(name(label, &first).await, "first", "{label}: back in a");
            }
        }
        assert_eq!(
            name(label, &first).await,
            "first",
            "{label}: nothing written"
        );
        assert_eq!(
            name(label, &second).await,
            "second",
            "{label}: nothing written"
        );

        // Back where the rows were read, the edit is written to that table only.
        let table = table(&pair, &first).await;
        let target = first.edit_target().await.unwrap();
        let statements = {
            let stored = first.results().get(result).unwrap();
            let stored = stored.read().unwrap();
            edit_statements(&target, &table, &stored, &stored.pending().changes())
                .unwrap_or_else(|e| panic!("{label}: {e}"))
        };
        assert_eq!(apply_edits(&first, &statements).await, Ok(1), "{label}");
        assert_eq!(name(label, &first).await, "changed", "{label}");
        assert_eq!(
            name(label, &second).await,
            "second",
            "{label}: the other database"
        );

        run(label, &first, "drop table edit_source").await;
        run(label, &second, "drop table edit_source").await;
    }
}

/// A statement that changes database while it runs (a `USE` in a SQL Server batch) leaves its
/// rows without a known database: they are never editable.
#[tokio::test]
async fn rows_read_across_a_use_are_not_editable() {
    let Some((host, port)) = server("SPELAION_TEST_MSSQL") else {
        return;
    };
    let driver = engines::sqlserver();
    let params = ConnectParams {
        tls: TlsMode::Require,
        ..login(&host, port, "sa", "Spelaion-test-1", "tempdb")
    };
    let conn = driver.connect(&params).await.unwrap();
    let session = Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    run(
        "sqlserver",
        &session,
        "drop table if exists edit_source_batch",
    )
    .await;
    run(
        "sqlserver",
        &session,
        "create table edit_source_batch (id int primary key, name varchar(40))",
    )
    .await;
    let sql = "select id, name from edit_source_batch";
    let read = run("sqlserver", &session, sql).await.expect("a result");
    assert!(
        editable_query_table(&session, read, sql, false)
            .await
            .unwrap()
            .is_some()
    );
    let across = run(
        "sqlserver",
        &session,
        "use tempdb select id, name from edit_source_batch",
    )
    .await
    .expect("a result");
    assert_eq!(
        editable_query_table(&session, across, sql, false).await,
        Ok(None),
        "a batch that ran USE"
    );
    run("sqlserver", &session, "drop table edit_source_batch").await;
}
