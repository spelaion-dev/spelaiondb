//! Data imports (ADR-038) on every engine: PostgreSQL (`SPELAION_TEST_PG`), MySQL and MariaDB
//! (`SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB`), SQL Server (`SPELAION_TEST_MSSQL`), and
//! DuckDB and SQLite (always). Each engine loads a CSV of a million rows into a new table and
//! into an existing one, keeps a value made of quotes and SQL as plain text, is cancelled half
//! way (only whole committed batches stay), stops at a bad row with the batch before it
//! committed and its own batch rolled back, and refuses read-only connections and unconfirmed
//! plans. ClickHouse (`SPELAION_TEST_CLICKHOUSE`), which has no transactions, loads into a table
//! of its own put in place at the end (ADR-066): all of an import or nothing, and no appends.

use crate::engines;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    ColumnKind, ColumnMapping, CoreEvent, IfExists, ImportError, ImportProgress, ImportRequest,
    ImportSource, ImportTarget, JobCancel, ParseOptions, ResultStore, Risk, Session, plan_import,
    preview, run_import,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

const ROWS: u64 = 1_000_000;
/// A value that would break out of a statement if it were ever pasted into SQL text.
const TRICKY: &str =
    "Robert'); DROP TABLE students; -- \"quoted\" \\back\\slash `tick` $$ ; ' OR '1'='1";
const TRICKY_ID: u64 = 7;

/// `id,name,amount,day,flag` with a million rows, written once per test process.
fn million_csv() -> PathBuf {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("spelaion-import-{}.csv", std::process::id()));
        let mut out = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        writeln!(out, "id,name,amount,day,flag").unwrap();
        let tricky = format!("\"{}\"", TRICKY.replace('"', "\"\""));
        for id in 1..=ROWS {
            let name = if id == TRICKY_ID {
                tricky.clone()
            } else if id % 1000 == 0 {
                // Now and then a quoted value with the separator and a line break.
                format!("\"name, {id}\nsecond line\"")
            } else {
                format!("name {id}")
            };
            let flag = if id % 3 == 0 { "true" } else { "false" };
            writeln!(
                out,
                "{id},{name},{}.{:02},2024-{:02}-{:02},{flag}",
                id % 10_000,
                id % 100,
                id % 12 + 1,
                id % 28 + 1
            )
            .unwrap();
        }
        out.flush().unwrap();
        path
    })
    .clone()
}

/// A small CSV whose row `bad` has no id, for an `id` column that is `NOT NULL`.
fn bad_csv(label: &str, rows: u64, bad: u64) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "spelaion-import-bad-{}-{}.csv",
        label.to_lowercase().replace(' ', "-"),
        std::process::id()
    ));
    let mut text = String::from("id,name\n");
    for id in 1..=rows {
        if id == bad {
            text.push_str(",x\n");
        } else {
            text.push_str(&format!("{id},n{id}\n"));
        }
    }
    std::fs::write(&path, text).unwrap();
    path
}

struct Engine {
    label: &'static str,
    session: Session,
    schema: &'static str,
    /// The type of a text column of the existing table.
    text_type: &'static str,
}

async fn run(session: &Session, script: &str) {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
        "{script}: {events:?}"
    );
}

/// The first value of the first row of `sql`.
async fn scalar(session: &Session, sql: &str) -> Value {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{sql}: {events:?}"));
    let stored = session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    Value::from_array(
        batch.column(0).as_ref(),
        0,
        stored.schema().columns[0].logical,
    )
    .unwrap()
}

async fn count(session: &Session, table: &str) -> u64 {
    match scalar(session, &format!("select count(*) from {table}")).await {
        Value::Int(n) => n as u64,
        other => other.to_string().parse().unwrap(),
    }
}

fn request(
    path: PathBuf,
    target: ImportTarget,
    columns: Vec<ColumnMapping>,
    commit_every: u64,
) -> ImportRequest {
    ImportRequest {
        source: ImportSource::File(path),
        options: ParseOptions::default(),
        target,
        columns,
        commit_every,
    }
}

fn nothing(_: ImportProgress) {}

const NONE: Safeguards = Safeguards {
    confirm_execution: false,
    confirm_data_changes: false,
    close_idle_after: None,
    close_idle_transaction_after: None,
    manual_commit: false,
    smart_commit: false,
};

async fn exercise(engine: Engine) {
    let Engine {
        label,
        session,
        schema,
        text_type,
    } = engine;
    let dialect = session.dialect().clone();
    let pid = std::process::id();
    let created = format!("import_new_{pid}");
    let existing = format!("import_existing_{pid}");
    let cancelled = format!("import_cancel_{pid}");
    let failing = format!("import_fail_{pid}");
    for t in [&created, &existing, &cancelled, &failing] {
        run(&session, &format!("drop table if exists {t}")).await;
    }
    let path = million_csv();

    // The preview's types, as the new table gets them.
    let shown = tokio::task::spawn_blocking({
        let path = path.clone();
        let engine = dialect.name();
        move || preview(&ImportSource::File(path), None, 1_000, engine).unwrap()
    })
    .await
    .unwrap();
    // Batches commit one by one here: the dialog offers "Commit every" (SDB-460).
    assert!(!shown.kept_whole, "{}", dialect.name());
    let kinds: Vec<ColumnKind> = shown.columns.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            ColumnKind::Integer,
            ColumnKind::Text,
            ColumnKind::Decimal,
            ColumnKind::Date,
            ColumnKind::Boolean
        ],
        "{label}"
    );
    assert!(shown.more, "{label}");
    let new_columns: Vec<ColumnMapping> = shown
        .columns
        .iter()
        .map(|c| ColumnMapping::New {
            name: c.name.clone(),
            sql_type: c.sql_type.clone(),
        })
        .collect();
    let new_target = |table: &str, if_exists| ImportTarget::New {
        schema: schema.into(),
        table: table.into(),
        if_exists,
    };

    // Read-only connections are refused in the core, before anything runs.
    let into_new = request(
        path.clone(),
        new_target(&created, IfExists::Fail),
        new_columns.clone(),
        100_000,
    );
    let refused = run_import(
        &session,
        &into_new,
        &NONE,
        true,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(refused, Err(ImportError::ReadOnly)),
        "{label}: {refused:?}"
    );
    // Production confirms data changes: the plan says so, and an unconfirmed run is refused.
    let production = Safeguards {
        confirm_data_changes: true,
        ..NONE
    };
    let plan = plan_import(&session, &into_new, &production, false)
        .await
        .unwrap();
    assert!(plan.creates_table, "{label}");
    assert_eq!(plan.statements.len(), 1, "{label}: {plan:?}");
    assert!(
        plan.statements[0].starts_with("CREATE TABLE"),
        "{label}: {plan:?}"
    );
    assert!(
        plan.risks.iter().any(|(_, r)| *r == Risk::DataChange),
        "{label}: {plan:?}"
    );
    let refused = run_import(
        &session,
        &into_new,
        &production,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(refused, Err(ImportError::NeedsConfirmation)),
        "{label}: {refused:?}"
    );
    assert!(matches!(
        plan_import(&session, &into_new, &NONE, false).await.map(|p| p.risks),
        Ok(r) if r.is_empty()
    ));

    // A million rows into a new table.
    let started = Instant::now();
    let summary = run_import(
        &session,
        &into_new,
        &production,
        false,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap_or_else(|e| panic!("{label}: {e}"));
    eprintln!(
        "{label}: {ROWS} rows into a new table in {:?}",
        started.elapsed()
    );
    assert_eq!(summary.rows, ROWS, "{label}");
    assert!(summary.created_table, "{label}");
    assert_eq!(count(&session, &created).await, ROWS, "{label}");
    let name = |table: &str, id: u64| format!("select name from {table} where id = {id}");
    assert_eq!(
        scalar(&session, &name(&created, TRICKY_ID)).await,
        Value::Text(TRICKY.into()),
        "{label}"
    );
    assert_eq!(
        scalar(&session, &name(&created, 2000)).await,
        Value::Text("name, 2000\nsecond line".into()),
        "{label}"
    );
    let flags = scalar(
        &session,
        &format!(
            "select count(*) from {created} where flag = {}",
            if label == "SQL Server" || label == "SQLite" {
                "1"
            } else {
                "true"
            }
        ),
    )
    .await;
    assert_eq!(flags.to_string(), (ROWS / 3).to_string(), "{label}");

    // The same name again: fail, or replace after confirming the DROP.
    let again = run_import(
        &session,
        &into_new,
        &NONE,
        false,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(again, Err(ImportError::TableExists(_))),
        "{label}: {again:?}"
    );
    let replace = request(
        bad_csv(label, 3, 0),
        new_target(&created, IfExists::Replace),
        vec![
            ColumnMapping::New {
                name: "id".into(),
                sql_type: "INTEGER".into(),
            },
            ColumnMapping::New {
                name: "name".into(),
                sql_type: text_type.into(),
            },
        ],
        0,
    );
    let plan = plan_import(&session, &replace, &NONE, false).await.unwrap();
    assert!(
        plan.statements[0].starts_with("DROP TABLE"),
        "{label}: {plan:?}"
    );
    assert_eq!(plan.risks, [(0, Risk::Drop)], "{label}");
    let refused = run_import(
        &session,
        &replace,
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(refused, Err(ImportError::NeedsConfirmation)),
        "{label}: {refused:?}"
    );
    assert_eq!(
        count(&session, &created).await,
        ROWS,
        "{label}: nothing was dropped"
    );
    let replaced = run_import(
        &session,
        &replace,
        &NONE,
        false,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap_or_else(|e| panic!("{label}: {e}"));
    assert_eq!(replaced.rows, 3, "{label}");
    assert_eq!(count(&session, &created).await, 3, "{label}");

    // A million rows into an existing table, columns mapped by name in another order, one
    // skipped and one added.
    run(
        &session,
        &format!(
            "create table {existing} (note {text_type}, day date, id bigint, name {text_type}, amount decimal(12,2))"
        ),
    )
    .await;
    let into_existing = request(
        path.clone(),
        ImportTarget::Existing {
            schema: schema.into(),
            table: existing.clone(),
        },
        vec![
            ColumnMapping::Existing("ID".into()),
            ColumnMapping::Existing("name".into()),
            ColumnMapping::Existing("amount".into()),
            ColumnMapping::Skip,
            ColumnMapping::New {
                name: "flag_text".into(),
                sql_type: text_type.into(),
            },
        ],
        250_000,
    );
    let plan = plan_import(&session, &into_existing, &NONE, false)
        .await
        .unwrap();
    assert!(!plan.creates_table, "{label}");
    assert_eq!(plan.statements.len(), 1, "{label}: {plan:?}");
    assert!(
        plan.statements[0].starts_with("ALTER TABLE"),
        "{label}: {plan:?}"
    );
    let started = Instant::now();
    let summary = run_import(
        &session,
        &into_existing,
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap_or_else(|e| panic!("{label}: {e}"));
    eprintln!(
        "{label}: {ROWS} rows into an existing table in {:?}",
        started.elapsed()
    );
    assert_eq!(summary.rows, ROWS, "{label}");
    assert_eq!(count(&session, &existing).await, ROWS, "{label}");
    assert_eq!(
        scalar(&session, &name(&existing, TRICKY_ID)).await,
        Value::Text(TRICKY.into()),
        "{label}"
    );
    assert_eq!(
        scalar(
            &session,
            &format!("select flag_text from {existing} where id = 3")
        )
        .await,
        Value::Text("true".into()),
        "{label}: a boolean into a text column stays as written"
    );
    assert_eq!(
        scalar(
            &session,
            &format!("select count(*) from {existing} where day is null and note is null")
        )
        .await
        .to_string(),
        ROWS.to_string(),
        "{label}"
    );

    // Cancelled half way: only whole committed batches stay.
    let cancel = Arc::new(JobCancel::new());
    let committed = Arc::new(AtomicU64::new(0));
    let watcher = tokio::spawn({
        let cancel = cancel.clone();
        let committed = committed.clone();
        async move {
            while committed.load(Ordering::SeqCst) < 200_000 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel.cancel().await;
        }
    });
    let report = {
        let committed = committed.clone();
        move |p: ImportProgress| committed.store(p.committed, Ordering::SeqCst)
    };
    let into_cancelled = request(
        path.clone(),
        new_target(&cancelled, IfExists::Fail),
        new_columns.clone(),
        100_000,
    );
    let outcome = run_import(
        &session,
        &into_cancelled,
        &NONE,
        false,
        false,
        &cancel,
        &report,
    )
    .await;
    watcher.abort();
    let Err(ImportError::Cancelled { committed }) = outcome else {
        panic!("{label}: {outcome:?}");
    };
    assert!((200_000..ROWS).contains(&committed), "{label}: {committed}");
    assert_eq!(committed % 100_000, 0, "{label}: {committed}");
    assert_eq!(count(&session, &cancelled).await, committed, "{label}");

    // A bad value: the batch before it stays, its own batch is rolled back.
    let bad = request(
        bad_csv(label, 300, 150),
        new_target(&failing, IfExists::Fail),
        vec![
            ColumnMapping::New {
                name: "id".into(),
                sql_type: "INTEGER NOT NULL".into(),
            },
            ColumnMapping::New {
                name: "name".into(),
                sql_type: text_type.into(),
            },
        ],
        100,
    );
    let outcome = run_import(
        &session,
        &bad,
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    let Err(ImportError::Failed {
        committed,
        first,
        last,
        ..
    }) = &outcome
    else {
        panic!("{label}: {outcome:?}");
    };
    assert_eq!(
        (*committed, *first, *last),
        (100, 101, 200),
        "{label}: {outcome:?}"
    );
    assert_eq!(count(&session, &failing).await, 100, "{label}");

    for t in [&created, &existing, &cancelled, &failing] {
        run(&session, &format!("drop table if exists {t}")).await;
    }
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    Some((host.into(), port.parse().unwrap()))
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_imports() {
    let _turn = crate::serial::file_turn(module_path!());
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
    let session = Session::new(
        engines::postgres().connect(&params).await.unwrap(),
        spelaion_dialect::for_engine(engines::postgres().engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    exercise(Engine {
        label: "PostgreSQL",
        session,
        schema: "public",
        text_type: "text",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_and_mariadb_import() {
    let _turn = crate::serial::file_turn(module_path!());
    for (var, driver, label) in [
        ("SPELAION_TEST_MYSQL", engines::mysql(), "MySQL"),
        ("SPELAION_TEST_MARIADB", engines::mariadb(), "MariaDB"),
    ] {
        let Some((host, port)) = server(var) else {
            continue;
        };
        let params = ConnectParams {
            host,
            port,
            user: "spelaion".into(),
            password: Some(Secret::new("spelaion")),
            database: "spelaion".into(),
            ..Default::default()
        };
        let session = Session::new(
            driver.connect(&params).await.unwrap(),
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        exercise(Engine {
            label,
            session,
            schema: "spelaion",
            text_type: "text",
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlserver_imports() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_MSSQL") else {
        return;
    };
    let connect = |database: &str| ConnectParams {
        host: host.clone(),
        port,
        user: "sa".into(),
        password: Some(Secret::new("Spelaion-test-1")),
        database: database.into(),
        tls: TlsMode::Require,
        ..Default::default()
    };
    let master = Session::new(
        engines::sqlserver()
            .connect(&connect("master"))
            .await
            .unwrap(),
        spelaion_dialect::for_engine(engines::sqlserver().engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    run(
        &master,
        "if db_id('spelaion') is null create database spelaion",
    )
    .await;
    let session = Session::new(
        engines::sqlserver()
            .connect(&connect("spelaion"))
            .await
            .unwrap(),
        spelaion_dialect::for_engine(engines::sqlserver().engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    exercise(Engine {
        label: "SQL Server",
        session,
        schema: "dbo",
        text_type: "nvarchar(max)",
    })
    .await;
}

/// Every value of the first column of `sql`.
async fn column(session: &Session, sql: &str) -> Vec<String> {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let mut values = Vec::new();
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {error:?}"),
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(*result).unwrap();
                let stored = stored.read().unwrap();
                for row in 0..stored.rows() {
                    values.push(stored.value_at(row, 0).unwrap().to_string());
                }
                drop(stored);
                session.results().remove(*result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::ResultEnded { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    values
}

/// The tables of our own an import left in the database `spelaion` (ADR-066).
async fn ours_left(session: &Session) -> Vec<String> {
    column(
        session,
        "select name from system.tables where database = 'spelaion' \
         and startsWith(name, '__spelaion_import_') order by name",
    )
    .await
}

/// ClickHouse has no transactions (ADR-066): the rows load into a table of our own and one
/// atomic rename puts it in place, so an import is all of it or nothing, and never appends.
#[tokio::test(flavor = "multi_thread")]
async fn clickhouse_imports_land_whole_or_not_at_all() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_CLICKHOUSE") else {
        return;
    };
    let driver = engines::clickhouse();
    let session = Session::new(
        driver
            .connect(&ConnectParams {
                host,
                port,
                user: "spelaion".into(),
                password: Some(Secret::new("spelaion")),
                database: "spelaion".into(),
                tls: TlsMode::Disable,
                ..Default::default()
            })
            .await
            .unwrap(),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    let pid = std::process::id();
    let created = format!("import_ch_new_{pid}");
    let cancelled = format!("import_ch_cancel_{pid}");
    let failing = format!("import_ch_fail_{pid}");
    let stamps = format!("import_ch_stamps_{pid}");
    let memory_db = format!("import_ch_memory_{pid}");
    for t in [&created, &cancelled, &failing, &stamps] {
        run(&session, &format!("drop table if exists {t}")).await;
    }
    // What earlier runs of this test left: nothing else uses the prefix in this database.
    for left in ours_left(&session).await {
        run(&session, &format!("drop table if exists spelaion.`{left}`")).await;
    }
    let path = million_csv();

    let shown = tokio::task::spawn_blocking({
        let path = path.clone();
        move || preview(&ImportSource::File(path), None, 1_000, "clickhouse").unwrap()
    })
    .await
    .unwrap();
    // Kept whole: the dialog shows no "Commit every" here (SDB-460).
    assert!(shown.kept_whole);
    let types: Vec<&str> = shown.columns.iter().map(|c| c.sql_type.as_str()).collect();
    assert_eq!(
        [types[0], types[1], types[3], types[4]],
        [
            "Nullable(Int64)",
            "Nullable(String)",
            "Nullable(Date32)",
            "Nullable(Bool)"
        ]
    );
    assert!(types[2].starts_with("Nullable(Decimal("), "{types:?}");
    let new_columns: Vec<ColumnMapping> = shown
        .columns
        .iter()
        .map(|c| ColumnMapping::New {
            name: c.name.clone(),
            sql_type: c.sql_type.clone(),
        })
        .collect();
    let new_target = |table: &str, if_exists| ImportTarget::New {
        schema: "spelaion".into(),
        table: table.into(),
        if_exists,
    };
    let into_new = request(
        path.clone(),
        new_target(&created, IfExists::Fail),
        new_columns.clone(),
        100_000,
    );

    // Read-only connections are refused in the core, before anything runs.
    let refused = run_import(
        &session,
        &into_new,
        &NONE,
        true,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(matches!(refused, Err(ImportError::ReadOnly)), "{refused:?}");

    // The plan: a table of our own, the rows into it as TabSeparated data, then the rename.
    let production = Safeguards {
        confirm_data_changes: true,
        ..NONE
    };
    let plan = plan_import(&session, &into_new, &production, false)
        .await
        .unwrap();
    assert!(plan.creates_table);
    assert_eq!(plan.statements.len(), 1, "{plan:?}");
    let create = &plan.statements[0];
    assert!(create.starts_with("CREATE TABLE"), "{create}");
    assert!(create.contains("__spelaion_import_"), "{create}");
    assert!(
        create.ends_with(") ENGINE = MergeTree ORDER BY tuple()"),
        "{create}"
    );
    assert!(
        plan.insert.contains("__spelaion_import_") && plan.insert.ends_with("FORMAT TabSeparated"),
        "{}",
        plan.insert
    );
    assert_eq!(plan.finish.len(), 1, "{plan:?}");
    assert!(
        plan.finish[0].starts_with("RENAME TABLE") && plan.finish[0].contains(&created),
        "{plan:?}"
    );
    assert!(plan.leftovers.is_empty(), "{plan:?}");
    assert!(
        plan.risks.iter().any(|(_, r)| *r == Risk::DataChange),
        "{plan:?}"
    );
    assert!(matches!(
        plan_import(&session, &into_new, &NONE, false).await.map(|p| p.risks),
        Ok(r) if r.is_empty()
    ));

    // A million rows into a new table, put in place at once.
    let started = Instant::now();
    let summary = run_import(
        &session,
        &into_new,
        &production,
        false,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    eprintln!(
        "ClickHouse: {ROWS} rows into a new table in {:?}",
        started.elapsed()
    );
    assert_eq!(summary.rows, ROWS);
    assert!(summary.created_table);
    assert_eq!(count(&session, &created).await, ROWS);
    let name = |table: &str, id: u64| format!("select name from {table} where id = {id}");
    assert_eq!(
        scalar(&session, &name(&created, TRICKY_ID)).await,
        Value::Text(TRICKY.into())
    );
    assert_eq!(
        scalar(&session, &name(&created, 2000)).await,
        Value::Text("name, 2000\nsecond line".into())
    );
    let flags = scalar(
        &session,
        &format!("select count(*) from {created} where flag = true"),
    )
    .await;
    assert_eq!(flags.to_string(), (ROWS / 3).to_string());
    assert!(ours_left(&session).await.is_empty());

    // The same name again: fail, or replace by a swap after confirming the drop of the old one.
    let again = run_import(
        &session,
        &into_new,
        &NONE,
        false,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(again, Err(ImportError::TableExists(_))),
        "{again:?}"
    );
    let small_columns = vec![
        ColumnMapping::New {
            name: "id".into(),
            sql_type: "Int64".into(),
        },
        ColumnMapping::New {
            name: "name".into(),
            sql_type: "Nullable(String)".into(),
        },
    ];
    let replace = request(
        bad_csv("ClickHouse", 3, 0),
        new_target(&created, IfExists::Replace),
        small_columns.clone(),
        0,
    );
    let plan = plan_import(&session, &replace, &NONE, false).await.unwrap();
    assert!(plan.statements[0].starts_with("CREATE TABLE"), "{plan:?}");
    assert_eq!(plan.finish.len(), 2, "{plan:?}");
    assert!(plan.finish[0].starts_with("EXCHANGE TABLES"), "{plan:?}");
    assert!(
        plan.finish[1].starts_with("DROP TABLE") && plan.finish[1].contains("__spelaion_import_"),
        "{plan:?}"
    );
    // The drop is the second statement after the insert.
    assert_eq!(plan.risks, [(plan.statements.len() + 2, Risk::Drop)]);
    let refused = run_import(
        &session,
        &replace,
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(refused, Err(ImportError::NeedsConfirmation)),
        "{refused:?}"
    );
    assert_eq!(count(&session, &created).await, ROWS, "nothing was dropped");
    let replaced = run_import(
        &session,
        &replace,
        &NONE,
        false,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(replaced.rows, 3);
    assert_eq!(count(&session, &created).await, 3);
    assert!(ours_left(&session).await.is_empty());

    // Rows are never added to an existing table: they could not be taken back.
    for target in [
        ImportTarget::Existing {
            schema: "spelaion".into(),
            table: created.clone(),
        },
        new_target(&created, IfExists::Append),
    ] {
        let append = request(
            bad_csv("ClickHouse", 3, 0),
            target,
            vec![
                ColumnMapping::Existing("id".into()),
                ColumnMapping::Existing("name".into()),
            ],
            0,
        );
        let refused = plan_import(&session, &append, &NONE, false).await;
        assert!(
            matches!(&refused, Err(ImportError::Invalid(m)) if m.contains("no transactions")),
            "{refused:?}"
        );
    }
    assert_eq!(count(&session, &created).await, 3);

    // Cancelled half way: nothing stays, neither the table nor ours.
    let cancel = Arc::new(JobCancel::new());
    let read = Arc::new(AtomicU64::new(0));
    let watcher = tokio::spawn({
        let cancel = cancel.clone();
        let read = read.clone();
        async move {
            while read.load(Ordering::SeqCst) < 200_000 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel.cancel().await;
        }
    });
    let report = {
        let read = read.clone();
        move |p: ImportProgress| {
            assert_eq!(p.committed, 0, "nothing is committed before the end");
            read.store(p.read, Ordering::SeqCst)
        }
    };
    let outcome = run_import(
        &session,
        &request(
            path.clone(),
            new_target(&cancelled, IfExists::Fail),
            new_columns.clone(),
            100_000,
        ),
        &NONE,
        false,
        false,
        &cancel,
        &report,
    )
    .await;
    watcher.abort();
    assert!(
        matches!(outcome, Err(ImportError::Cancelled { committed: 0 })),
        "{outcome:?}"
    );
    let exists = |table: &str| {
        format!(
            "select count() from system.tables where database = 'spelaion' and name = '{table}'"
        )
    };
    assert_eq!(scalar(&session, &exists(&cancelled)).await.to_string(), "0");
    assert!(ours_left(&session).await.is_empty());

    // A bad value: nothing stays.
    let outcome = run_import(
        &session,
        &request(
            bad_csv("ClickHouse", 300, 150),
            new_target(&failing, IfExists::Fail),
            small_columns.clone(),
            100,
        ),
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(outcome, Err(ImportError::Failed { committed: 0, .. })),
        "{outcome:?}"
    );
    assert_eq!(scalar(&session, &exists(&failing)).await.to_string(), "0");
    assert!(ours_left(&session).await.is_empty());

    // Timestamps with a zone go as their UTC time; booleans as written.
    let dir = folder("clickhouse");
    let stamps_csv = dir.join("stamps.csv");
    std::fs::write(
        &stamps_csv,
        "id,at,ok\n1,2024-01-02T03:04:05+02:00,true\n2,2024-06-30T23:59:59.5-01:30,false\n",
    )
    .unwrap();
    let shown = preview(
        &ImportSource::File(stamps_csv.clone()),
        None,
        10,
        "clickhouse",
    )
    .unwrap();
    assert_eq!(shown.columns[1].sql_type, "Nullable(DateTime64(6))");
    let columns = shown
        .columns
        .iter()
        .map(|c| ColumnMapping::New {
            name: c.name.clone(),
            sql_type: c.sql_type.clone(),
        })
        .collect();
    run_import(
        &session,
        &request(stamps_csv, new_target(&stamps, IfExists::Fail), columns, 0),
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        column(
            &session,
            &format!("select concat(toString(at), ' ', toString(ok)) from {stamps} order by id")
        )
        .await,
        [
            "2024-01-02 01:04:05.000000 true",
            "2024-07-01 01:29:59.500000 false"
        ]
    );

    // A table left by an import that did not end is listed, never dropped.
    let left = "__spelaion_import_00000000000000000000000000000000";
    run(
        &session,
        &format!("create table spelaion.`{left}` (x Int8) engine = Memory"),
    )
    .await;
    let plan = plan_import(
        &session,
        &request(
            bad_csv("ClickHouse", 3, 0),
            new_target(&failing, IfExists::Fail),
            small_columns.clone(),
            0,
        ),
        &NONE,
        false,
    )
    .await
    .unwrap();
    assert_eq!(plan.leftovers, [left]);
    assert_eq!(ours_left(&session).await, [left]);
    run(&session, &format!("drop table spelaion.`{left}`")).await;

    // A database that does not rename tables atomically is refused before anything runs.
    run(
        &session,
        &format!("create database if not exists {memory_db} engine = Memory"),
    )
    .await;
    let refused = plan_import(
        &session,
        &request(
            bad_csv("ClickHouse", 3, 0),
            ImportTarget::New {
                schema: memory_db.clone(),
                table: "t".into(),
                if_exists: IfExists::Fail,
            },
            small_columns,
            0,
        ),
        &NONE,
        false,
    )
    .await;
    assert!(
        matches!(&refused, Err(ImportError::Invalid(m)) if m.contains("Atomic")),
        "{refused:?}"
    );
    run(&session, &format!("drop database {memory_db}")).await;

    for t in [&created, &cancelled, &failing, &stamps] {
        run(&session, &format!("drop table if exists {t}")).await;
    }
}

fn folder(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spelaion-import-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn duckdb_imports() {
    let _turn = crate::serial::file_turn(module_path!());
    let path = folder("duckdb").join("import.duckdb");
    engines::create_database(spelaion_config::Engine::Duckdb, &path).await;
    let driver = engines::duckdb();
    let conn = driver
        .connect(&ConnectParams {
            database: path.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    exercise(Engine {
        label: "DuckDB",
        session: Session::new(
            conn,
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        ),
        schema: "main",
        text_type: "varchar",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_imports() {
    let _turn = crate::serial::file_turn(module_path!());
    let path = folder("sqlite").join("import.db");
    std::fs::File::create(&path).unwrap();
    let driver = engines::sqlite();
    let conn = driver
        .connect(&ConnectParams {
            database: path.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    exercise(Engine {
        label: "SQLite",
        session: Session::new(
            conn,
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        ),
        schema: "main",
        text_type: "text",
    })
    .await;
}

#[tokio::test]
async fn unicode_csv_values_are_previewed_and_imported_unchanged() {
    let _turn = crate::serial::file_turn(module_path!());
    let dir = folder("unicode");
    let db = dir.join("unicode.db");
    let csv = dir.join("unicode.csv");
    std::fs::File::create(&db).unwrap();
    let values = ["ITEM-Молоко 3.2%", "CODE-商品名称テスト", "2024-ééééééééé"];
    let rows = values
        .iter()
        .enumerate()
        .map(|(i, value)| format!("{},{value}\n", i + 1))
        .collect::<String>();
    std::fs::write(&csv, format!("id,name\n{rows}")).unwrap();
    let driver = engines::sqlite();
    let session = Session::new(
        driver
            .connect(&ConnectParams {
                database: db.display().to_string(),
                ..Default::default()
            })
            .await
            .unwrap(),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    let source = ImportSource::File(csv);
    let shown = preview(&source, None, 100, "sqlite").unwrap();
    assert_eq!(shown.columns[1].kind, ColumnKind::Text);
    let request = ImportRequest {
        source,
        options: shown.options,
        target: ImportTarget::New {
            schema: "main".into(),
            table: "products".into(),
            if_exists: IfExists::Fail,
        },
        columns: shown
            .columns
            .into_iter()
            .map(|c| ColumnMapping::New {
                name: c.name,
                sql_type: c.sql_type,
            })
            .collect(),
        commit_every: 0,
    };
    let summary = run_import(
        &session,
        &request,
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap();
    assert_eq!(summary.rows, values.len() as u64);
    for (i, value) in values.iter().enumerate() {
        assert_eq!(
            scalar(
                &session,
                &format!("select name from products where id = {}", i + 1)
            )
            .await,
            Value::Text((*value).into())
        );
    }
}

/// Pasted text (a Markdown table) into a new SQLite table, and a view that is not a target.
#[tokio::test]
async fn pasted_tables_and_views() {
    let _turn = crate::serial::file_turn(module_path!());
    let path = folder("paste").join("paste.db");
    std::fs::File::create(&path).unwrap();
    let driver = engines::sqlite();
    let session = Session::new(
        driver
            .connect(&ConnectParams {
                database: path.display().to_string(),
                ..Default::default()
            })
            .await
            .unwrap(),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    let text = "| id | city |\n|---|---|\n| 1 | Zaragoza |\n| 2 | O'Higgins \\| Chile |\n";
    let shown = preview(&ImportSource::Text(text.into()), None, 100, "sqlite").unwrap();
    let request = ImportRequest {
        source: ImportSource::Text(text.into()),
        options: shown.options.clone(),
        target: ImportTarget::New {
            schema: "main".into(),
            table: shown.suggested_table.clone(),
            if_exists: IfExists::Fail,
        },
        columns: shown
            .columns
            .iter()
            .map(|c| ColumnMapping::New {
                name: c.name.clone(),
                sql_type: c.sql_type.clone(),
            })
            .collect(),
        commit_every: 0,
    };
    let summary = run_import(
        &session,
        &request,
        &NONE,
        false,
        false,
        &JobCancel::new(),
        &nothing,
    )
    .await
    .unwrap();
    assert_eq!((summary.table.as_str(), summary.rows), ("imported", 2));
    assert_eq!(
        scalar(&session, "select city from imported where id = 2").await,
        Value::Text("O'Higgins | Chile".into())
    );
    run(
        &session,
        "create view imported_view as select * from imported",
    )
    .await;
    let into_view = ImportRequest {
        target: ImportTarget::Existing {
            schema: "main".into(),
            table: "imported_view".into(),
        },
        columns: vec![ColumnMapping::Existing("id".into()), ColumnMapping::Skip],
        ..request
    };
    let refused = plan_import(&session, &into_view, &NONE, false).await;
    assert!(
        matches!(&refused, Err(ImportError::Invalid(m)) if m.contains("view")),
        "{refused:?}"
    );
    // A typed column type that tries to end the statement never reaches the database.
    let sneaky = ImportRequest {
        target: ImportTarget::New {
            schema: "main".into(),
            table: "sneaky".into(),
            if_exists: IfExists::Fail,
        },
        columns: vec![
            ColumnMapping::New {
                name: "id".into(),
                sql_type: "int); drop table imported; --".into(),
            },
            ColumnMapping::Skip,
        ],
        ..into_view
    };
    let refused = run_import(
        &session,
        &sneaky,
        &NONE,
        false,
        true,
        &JobCancel::new(),
        &nothing,
    )
    .await;
    assert!(
        matches!(refused, Err(ImportError::Invalid(_))),
        "{refused:?}"
    );
    assert_eq!(count(&session, "imported").await, 2);
}
