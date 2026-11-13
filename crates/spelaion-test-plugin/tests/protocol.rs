//! The plugin protocol from end to end: `spelaion-plugin-host` (the core's side) starts
//! `spelaion-test-plugin`, a made-up engine served with `spelaion-plugin-sdk` (the plugin's
//! side), and talks to it through the `Driver` and `Connection` every engine has.
//!
//! Every test is bounded: a lost answer fails in seconds instead of hanging.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arrow::array::{Array, Int64Array, StringArray};
use futures_util::StreamExt;
use spelaion_driver_api::{
    ConnectParams, Connection, Driver, DriverError, ExecEvent, ExecOptions, InsertTarget,
    KeepAlive, LogicalType, NameFilter, ObjectKind, PlanMode, Result, Secret, TextRows, TlsMode,
    TransactionState,
};
use spelaion_plugin_host::{Plugin, REDACTED, VERSION};

const PROGRAM: &str = env!("CARGO_BIN_EXE_spelaion-test-plugin");
const PASSWORD: &str = "hunter2-s3cret";

async fn within<T>(what: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), what)
        .await
        .expect("the plugin did not answer in time")
}

fn params(database: &str) -> ConnectParams {
    ConnectParams {
        host: "db.test".into(),
        port: 5555,
        user: "app".into(),
        password: Some(Secret::new(PASSWORD)),
        database: database.into(),
        ..Default::default()
    }
}

async fn connect(plugin: &Plugin) -> Box<dyn Connection> {
    within(plugin.driver("test").connect(&params("made")))
        .await
        .expect("connecting")
}

async fn run(connection: &dyn Connection, sql: &str) -> Vec<Result<ExecEvent>> {
    let events = connection
        .execute(sql, &ExecOptions::default())
        .await
        .expect("the statement starts");
    within(events.collect()).await
}

/// What each event is, without its data.
fn shape(events: &[Result<ExecEvent>]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            Ok(ExecEvent::Columns(schema)) => format!("columns {}", schema.columns.len()),
            Ok(ExecEvent::Batch(batch)) => format!("batch {}", batch.num_rows()),
            Ok(ExecEvent::Notice(notice)) => format!("notice {}", notice.message),
            Ok(ExecEvent::Done { rows_affected }) => format!("done {rows_affected:?}"),
            Err(error) => format!("error {error}"),
        })
        .collect()
}

/// The first column of every batch, as text.
fn texts(events: &[Result<ExecEvent>]) -> Vec<String> {
    let mut out = Vec::new();
    for event in events {
        if let Ok(ExecEvent::Batch(batch)) = event {
            let column = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("a text column");
            out.extend((0..column.len()).map(|i| column.value(i).to_string()));
        }
    }
    out
}

async fn one(connection: &dyn Connection, sql: &str) -> String {
    texts(&run(connection, sql).await)
        .pop()
        .unwrap_or_else(|| panic!("{sql} returned nothing"))
}

/// A copy of the plugin's program under another name, which makes it another program (see
/// its `main`).
fn program_named(name: &str) -> PathBuf {
    let extension = std::env::consts::EXE_SUFFIX;
    let path =
        std::env::temp_dir().join(format!("spelaion-{name}-{}{extension}", std::process::id()));
    std::fs::copy(PROGRAM, &path).expect("copying the test plugin");
    path
}

// ------------------------------------------------------------------------ results

#[tokio::test]
async fn events_arrive_in_the_drivers_order_with_their_batches() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let events = connection
        .execute(
            "rows 25",
            &ExecOptions {
                batch_size: 10,
                origins: false,
            },
        )
        .await
        .unwrap();
    let events: Vec<_> = within(events.collect()).await;
    assert_eq!(
        shape(&events),
        ["columns 2", "batch 10", "batch 10", "batch 5", "done None"]
    );
    // The schema is the driver's: names, logical and native types, the engine's type id.
    let Ok(ExecEvent::Columns(schema)) = &events[0] else {
        panic!()
    };
    assert_eq!(schema.columns[0].name, "id");
    assert_eq!(schema.columns[0].logical, LogicalType::Int);
    assert_eq!(schema.columns[0].native_type, "int8");
    assert_eq!(schema.columns[0].native_id, Some(7));
    // And so are the values, in order.
    let Ok(ExecEvent::Batch(last)) = &events[3] else {
        panic!()
    };
    assert_eq!(last.schema(), schema.arrow);
    let ids = last
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(ids.values(), &[20, 21, 22, 23, 24]);
}

#[tokio::test]
async fn several_result_sets_notices_and_row_counts() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let events = run(connection.as_ref(), "results").await;
    assert_eq!(
        shape(&events),
        [
            "columns 2",
            "batch 2",
            "columns 1",
            "batch 2",
            "notice two results",
            "done Some(2)"
        ]
    );
    // The second result is its own: a native type, with a NULL.
    let Ok(ExecEvent::Columns(schema)) = &events[2] else {
        panic!()
    };
    assert_eq!(schema.columns[0].logical, LogicalType::Native);
    assert_eq!(schema.columns[0].native_type, "geometry");
    let Ok(ExecEvent::Batch(batch)) = &events[3] else {
        panic!()
    };
    assert!(batch.column(0).is_null(1));

    // A result with no rows still has its columns; a statement with no result has none.
    assert_eq!(
        shape(&run(connection.as_ref(), "empty").await),
        ["columns 2", "done None"]
    );
    assert_eq!(
        shape(&run(connection.as_ref(), "update").await),
        ["done Some(3)"]
    );
}

#[tokio::test]
async fn a_drivers_error_arrives_whole() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    // Refused before it starts.
    let Err(error) = connection.execute("refused", &ExecOptions::default()).await else {
        panic!("it started");
    };
    let DriverError::Database {
        code,
        message,
        detail,
        hint,
        position,
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(code.as_deref(), Some("42P01"));
    assert_eq!(message, "relation \"t\" does not exist");
    assert_eq!(detail.as_deref(), Some("it never did"));
    assert_eq!(hint.as_deref(), Some("create it"));
    assert_eq!(position, Some(15));

    // Failing after some rows: the rows, then the error, then nothing.
    let events = run(connection.as_ref(), "fails").await;
    assert_eq!(events.len(), 3);
    assert!(matches!(
        &events[2],
        Err(DriverError::Database { code: Some(c), .. }) if c == "42P01"
    ));
    // The session goes on.
    assert_eq!(one(connection.as_ref(), "engine").await, "test");
}

#[tokio::test]
async fn bound_values_and_plans_take_their_own_paths() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let events = connection
        .execute_bound(
            "select $1, $2",
            vec!["a".into(), "ñ'\"".into()],
            &ExecOptions::default(),
        )
        .await
        .unwrap();
    let events: Vec<_> = within(events.collect()).await;
    assert_eq!(texts(&events), ["select $1, $2", "a", "ñ'\""]);

    let events = connection
        .execute_plan("select 1", PlanMode::Analyze { query_result: true })
        .await
        .unwrap();
    let events: Vec<_> = within(events.collect()).await;
    assert_eq!(
        texts(&events),
        ["select 1 as Analyze { query_result: true }"]
    );
    let events = connection
        .execute_plan("select 1", PlanMode::Estimated)
        .await
        .unwrap();
    let events: Vec<_> = within(events.collect()).await;
    assert_eq!(texts(&events), ["select 1 as Estimated"]);
}

#[tokio::test]
async fn a_million_rows_arrive_and_only_as_fast_as_they_are_read() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let watcher = connect(&plugin).await;
    let before: u64 = one(watcher.as_ref(), "produced").await.parse().unwrap();

    let options = ExecOptions {
        batch_size: 1_000,
        origins: false,
    };
    let mut events = connection.execute("rows 1000000", &options).await.unwrap();
    // Nobody reads: the plugin stops producing once its credit is spent, however long it is
    // left alone. A thousand batches are waiting to be made.
    let credit = within(plugin.describe()).await.unwrap().credit as u64;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let produced: u64 = one(watcher.as_ref(), "produced").await.parse().unwrap();
    let produced = produced - before;
    assert!(
        produced <= credit + 1,
        "{produced} batches were produced with a credit of {credit} and nobody reading"
    );

    // Read to the end: every row, once, in order.
    let mut rows = 0i64;
    let mut done = false;
    while let Some(event) = within(events.next()).await {
        match event.expect("no error") {
            ExecEvent::Batch(batch) => {
                let ids = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap();
                assert_eq!(ids.value(0), rows);
                rows += batch.num_rows() as i64;
            }
            ExecEvent::Done { .. } => done = true,
            ExecEvent::Columns(_) | ExecEvent::Notice(_) => {}
        }
    }
    assert_eq!(rows, 1_000_000);
    assert!(done);
}

#[tokio::test]
async fn a_result_nobody_finishes_reading_stops_in_the_plugin() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let options = ExecOptions {
        batch_size: 10,
        origins: false,
    };
    let mut events = connection.execute("rows 1000000", &options).await.unwrap();
    // The columns and a batch, then the reader goes away.
    within(events.next()).await.unwrap().unwrap();
    within(events.next()).await.unwrap().unwrap();
    drop(events);
    // The plugin stops producing, and the session is free for the next statement.
    assert_eq!(one(connection.as_ref(), "engine").await, "test");
    let first: u64 = one(connection.as_ref(), "produced").await.parse().unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let later: u64 = one(connection.as_ref(), "produced").await.parse().unwrap();
    assert_eq!(first, later, "the dropped result went on being produced");
}

// ------------------------------------------------------------------------ cancel

#[tokio::test]
async fn cancel_is_obeyed_while_a_statement_runs() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let mut events = connection
        .execute("sleep", &ExecOptions::default())
        .await
        .unwrap();
    // It is running: its columns arrived and nothing else will until it is cancelled.
    assert!(matches!(
        within(events.next()).await,
        Some(Ok(ExecEvent::Columns(_)))
    ));
    let waiting = tokio::time::timeout(Duration::from_millis(200), events.next()).await;
    assert!(waiting.is_err(), "the statement ended by itself");

    within(connection.cancel_handle().cancel()).await.unwrap();
    assert!(matches!(
        within(events.next()).await,
        Some(Err(DriverError::Cancelled))
    ));
    assert!(within(events.next()).await.is_none());
    // Only that statement: the session is still there.
    assert_eq!(one(connection.as_ref(), "engine").await, "test");
}

// ------------------------------------------------------------------------ loading rows

#[tokio::test]
async fn rows_are_loaded_as_they_were_given() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let target = InsertTarget {
        schema: "public".into(),
        table: format!("t{}", std::process::id()),
        columns: vec!["a".into(), "b".into()],
    };
    let rows = vec![
        vec![Some("1".to_string()), None],
        vec![Some(String::new()), Some("ñ\n'\";--".to_string())],
        vec![None, Some("x".repeat(100_000))],
    ];
    let loaded = within(connection.insert_rows(&target, TextRows::from_rows(2, &rows).unwrap()))
        .await
        .unwrap();
    assert_eq!(loaded, 3);
    let name = format!("public.{}(a,b)", target.table);
    let inserted: Vec<String> = texts(&run(connection.as_ref(), "inserted").await)
        .into_iter()
        .filter(|row| row.starts_with(&name))
        .collect();
    let expected: Vec<String> = rows.iter().map(|row| format!("{name} {row:?}")).collect();
    assert_eq!(inserted, expected);

    // A row of another width never leaves the core.
    let one_wide = TextRows::from_rows(1, &[vec![None]]).unwrap();
    let wrong = within(connection.insert_rows(&target, one_wide)).await;
    assert!(wrong.is_err());
}

// ------------------------------------------------------------------------ the catalog

#[tokio::test]
async fn the_catalog_and_the_session_calls_answer_as_the_driver_does() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let c = connection.as_ref();
    assert_eq!(within(c.server_version()).await.unwrap(), "made-up 1.0");
    assert_eq!(
        within(c.keep_alive()).await.unwrap(),
        KeepAlive::InTransaction
    );
    assert_eq!(
        within(c.transaction_state()).await.unwrap(),
        TransactionState::Failed
    );
    assert_eq!(
        within(c.current_database()).await.unwrap().as_deref(),
        Some("made")
    );
    // The default of a driver that keeps no context.
    assert_eq!(within(c.database_context()).await.unwrap(), None);
    assert_eq!(within(c.list_databases()).await.unwrap(), ["made", "up"]);
    within(c.select_database("made")).await.unwrap();
    assert!(within(c.select_database("nope")).await.is_err());

    let schemas = within(c.list_schemas()).await.unwrap();
    assert_eq!(schemas.len(), 2);
    assert!(schemas[1].system);
    let filter = NameFilter {
        include: vec!["s*".into()],
        exclude: Vec::new(),
    };
    let narrowed = within(c.list_schemas_matching(&filter)).await.unwrap();
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].name, "sys");

    let objects = within(c.list_objects("main")).await.unwrap();
    assert_eq!(objects[0].kind, ObjectKind::Table);
    assert_eq!(objects[0].estimated_rows, Some(42));
    assert_eq!(objects[1].kind, ObjectKind::View);
    let filter = NameFilter {
        include: Vec::new(),
        exclude: vec!["people".into()],
    };
    let narrowed = within(c.list_objects_matching("main", &filter))
        .await
        .unwrap();
    assert_eq!(narrowed.len(), 1);

    let table = within(c.describe_table("main", "people")).await.unwrap();
    assert_eq!(table.primary_key, ["id"]);
    assert_eq!(table.columns[0].default.as_deref(), Some("nextval('s')"));
    // The trait's default, computed in the plugin from the driver's own answers.
    let columns = within(c.list_columns("main")).await;
    assert!(columns.is_err(), "the view has no description: {columns:?}");

    // What a driver does not support says so with the driver's own words.
    let refused = within(c.relation_definition("main", "people"))
        .await
        .unwrap_err();
    assert_eq!(refused.to_string(), "this engine has no table definitions");
    assert!(
        within(c.list_schema_objects("main"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(within(c.list_server_objects()).await.unwrap().is_empty());
}

#[tokio::test]
async fn connect_carries_what_the_driver_needs_and_nothing_else() {
    let plugin = Plugin::new(PROGRAM);
    let mut params = params("made");
    params.tls = TlsMode::VerifyFull;
    params.connect_addr = Some("127.0.0.1:50211".parse().unwrap());
    params.read_only = true;
    params.application_name = Some("SpelaionDB".into());
    params.connect_timeout = Some(Duration::from_secs(7));
    params.ca_certificate = Some(PathBuf::from("ca.pem"));
    let connection = within(plugin.driver("test2").connect(&params))
        .await
        .unwrap();
    let seen = one(connection.as_ref(), "params").await;
    assert_eq!(
        seen,
        "db.test:5555 user=app tls=VerifyFull addr=Some(127.0.0.1:50211) read_only=true \
         app=Some(\"SpelaionDB\") timeout=Some(7s) ca=Some(\"ca.pem\")"
    );
    assert_eq!(one(connection.as_ref(), "engine").await, "test2");
    assert!(connection.capabilities().insert_rows);
    assert!(!connection.capabilities().editable_results);
}

// ------------------------------------------------------------------------ one process

#[tokio::test]
async fn one_process_serves_every_session_of_its_engines() {
    let plugin = Plugin::new(PROGRAM);
    assert!(
        !within(plugin.is_running()).await,
        "nothing starts it early"
    );
    let a = connect(&plugin).await;
    let b = connect(&plugin).await;
    let other_engine = within(plugin.driver("test2").connect(&params("made")))
        .await
        .unwrap();
    let pid = one(a.as_ref(), "pid").await;
    assert_eq!(one(b.as_ref(), "pid").await, pid);
    assert_eq!(one(other_engine.as_ref(), "pid").await, pid);
    assert_ne!(pid, std::process::id().to_string());

    // Sessions run at the same time: one waits, the other answers.
    let mut sleeping = a.execute("sleep", &ExecOptions::default()).await.unwrap();
    within(sleeping.next()).await.unwrap().unwrap();
    assert_eq!(one(b.as_ref(), "engine").await, "test");
    within(a.cancel_handle().cancel()).await.unwrap();

    // Closing a session closes it in the plugin.
    assert_eq!(one(b.as_ref(), "open").await, "3");
    drop(other_engine);
    drop(sleeping);
    drop(a);
    for _ in 0..50 {
        if one(b.as_ref(), "open").await == "1" {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the closed sessions are still open in the plugin");
}

/// A plugin is shared by a whole program, so it belongs to no caller's runtime: the one that
/// started it may end (every test of the core has its own) and the others go on.
#[test]
fn a_plugin_outlives_the_runtime_that_started_it() {
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let plugin = Plugin::new(PROGRAM);

    let first = runtime();
    let (kept, pid) = first.block_on(async {
        let connection = connect(&plugin).await;
        let pid = one(connection.as_ref(), "pid").await;
        (connection, pid)
    });
    drop(first);

    let second = runtime();
    second.block_on(async {
        assert!(
            within(plugin.is_running()).await,
            "it stopped with a runtime"
        );
        // The session opened from the first runtime still answers, and so does a new one, from
        // the same process.
        assert_eq!(one(kept.as_ref(), "pid").await, pid);
        let other = connect(&plugin).await;
        assert_eq!(one(other.as_ref(), "pid").await, pid);
        assert_eq!(one(other.as_ref(), "open").await, "2");
    });
}

#[tokio::test]
async fn a_connection_given_up_is_not_left_open_in_the_plugin() {
    let plugin = Plugin::new(PROGRAM);
    let watcher = connect(&plugin).await;
    // The driver takes ten minutes to connect; the core stops waiting.
    let driver = plugin.driver("test");
    let slow = params("slow");
    let attempt = tokio::spawn(async move { driver.connect(&slow).await.map(|_| ()) });
    for _ in 0..100 {
        if one(watcher.as_ref(), "connecting").await == "1" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(one(watcher.as_ref(), "connecting").await, "1");
    attempt.abort();
    // The attempt stops in the plugin there and then (a login the user stopped is not left
    // open until the server answers), not when the ten minutes are over.
    let mut connecting = String::new();
    for _ in 0..100 {
        connecting = one(watcher.as_ref(), "connecting").await;
        if connecting == "0" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(connecting, "0", "the driver is still connecting");
    // And one that fails leaves nothing either.
    let refused = within(plugin.driver("test").connect(&params("refuse"))).await;
    let Err(DriverError::Connection(message)) = refused else {
        panic!("it connected");
    };
    assert_eq!(message, "no database for app");
    assert_eq!(one(watcher.as_ref(), "open").await, "1");
}

#[tokio::test]
async fn what_a_plugin_cannot_serve_is_refused_by_the_core() {
    let plugin = Plugin::new(PROGRAM);
    let Err(error) = within(plugin.driver("postgres").connect(&params("made"))).await else {
        panic!("it connected");
    };
    assert!(
        error.to_string().contains("does not serve the engine"),
        "{error}"
    );
    // A program that does not exist.
    let missing = Plugin::new("spelaion-no-such-plugin");
    let Err(error) = within(missing.driver("test").connect(&params("made"))).await else {
        panic!("it connected");
    };
    assert!(
        error.to_string().contains("could not be started"),
        "{error}"
    );
}

/// An engine whose databases are files has them created by its driver: through a plugin too.
#[tokio::test]
async fn a_file_database_is_created_by_the_plugins_driver() {
    let plugin = Plugin::new(PROGRAM);
    let driver = plugin.driver("test");
    within(driver.create_database("new.db")).await.unwrap();

    // An existing file is never touched: the driver's refusal arrives as it wrote it.
    let error = within(driver.create_database("new.db")).await.unwrap_err();
    assert!(
        matches!(&error, DriverError::Other(message) if message.contains("already exists")),
        "{error:?}"
    );
    let connection = connect(&plugin).await;
    assert_eq!(
        texts(&run(connection.as_ref(), "created").await),
        ["new.db"]
    );

    // An engine that is not a file refuses, and one the plugin does not serve is not asked.
    let error = within(plugin.driver("test2").create_database("x"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is not a file"), "{error}");
    let error = within(plugin.driver("postgres").create_database("x"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("does not serve the engine"),
        "{error}"
    );
}

// ------------------------------------------------------------------------ versions

#[tokio::test]
async fn a_plugin_of_another_version_is_refused_with_both_versions() {
    for name in ["other-version", "other-version-claims"] {
        let program = program_named(name);
        let plugin = Plugin::new(&program);
        let Err(error) = within(plugin.driver("test").connect(&params("made"))).await else {
            panic!("{name} connected");
        };
        let message = error.to_string();
        assert!(message.contains("speaks version 2.0"), "{name}: {message}");
        assert!(
            message.contains(&format!("this SpelaionDB speaks version {VERSION}")),
            "{name}: {message}"
        );
        assert!(!within(plugin.is_running()).await);
        let _ = std::fs::remove_file(program);
    }
}

#[tokio::test]
async fn a_program_that_is_not_a_plugin_is_refused() {
    let program = program_named("not-a-plugin");
    let plugin = Plugin::new(&program);
    let Err(error) = within(plugin.driver("test").connect(&params("made"))).await else {
        panic!("it connected");
    };
    let message = error.to_string();
    assert!(
        message.contains("may not speak the plugin protocol"),
        "{message}"
    );
    let _ = std::fs::remove_file(program);
}

// ------------------------------------------------------------------------ a plugin that stops

#[tokio::test]
async fn a_plugin_that_dies_loses_its_sessions_and_nothing_is_run_again() {
    let plugin = Plugin::new(PROGRAM);
    let doomed = connect(&plugin).await;
    let bystander = connect(&plugin).await;
    let first_pid = one(bystander.as_ref(), "pid").await;
    assert!(!doomed.is_closed());

    let events = run(doomed.as_ref(), "crash").await;
    // What had arrived, then the loss: an error, and the end.
    assert_eq!(shape(&events)[..2], ["columns 2", "batch 1"]);
    let Some(Err(DriverError::Connection(message))) = events.last() else {
        panic!("{:?}", shape(&events));
    };
    assert!(message.contains("stopped"), "{message}");

    // Every session of that process is lost, and says so without being asked.
    assert!(doomed.is_closed());
    assert!(bystander.is_closed());
    assert!(!within(plugin.is_running()).await);
    let after = bystander.execute("engine", &ExecOptions::default()).await;
    assert!(matches!(after, Err(DriverError::Connection(_))));
    assert!(matches!(
        within(bystander.server_version()).await,
        Err(DriverError::Connection(_))
    ));

    // The next connection starts the plugin again; the old sessions stay lost.
    let fresh = connect(&plugin).await;
    assert_ne!(one(fresh.as_ref(), "pid").await, first_pid);
    assert!(bystander.is_closed());
    assert!(!fresh.is_closed());
}

#[tokio::test]
async fn output_that_is_not_the_protocol_ends_the_plugin() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let events = connection.execute("print", &ExecOptions::default()).await;
    // Whether the text arrived before the answer or after, the session is lost.
    if let Ok(events) = events {
        let _: Vec<_> = within(events.collect()).await;
    }
    for _ in 0..100 {
        if connection.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(connection.is_closed());
    assert!(matches!(
        within(connection.server_version()).await,
        Err(DriverError::Connection(_))
    ));
}

#[tokio::test]
async fn a_driver_that_panics_still_answers() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let failed = within(connection.execute("panic", &ExecOptions::default())).await;
    let Err(error) = failed else {
        panic!("it started");
    };
    assert!(error.to_string().contains("the plugin failed"), "{error}");
    // The plugin and the session are still there.
    assert_eq!(one(connection.as_ref(), "engine").await, "test");
}

#[tokio::test]
async fn a_connection_the_server_closed_is_known_to_be_closed() {
    let plugin = Plugin::new(PROGRAM);
    // Found out by a statement.
    let connection = connect(&plugin).await;
    let failed = connection
        .execute("drop-connection", &ExecOptions::default())
        .await;
    assert!(matches!(failed, Err(DriverError::Connection(_))));
    assert!(
        connection.is_closed(),
        "known by the time the error is read"
    );
    // Found out while idle.
    let idle = connect(&plugin).await;
    run(idle.as_ref(), "close-later").await;
    assert!(!idle.is_closed());
    for _ in 0..100 {
        if idle.is_closed() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(idle.is_closed());
    // Other sessions are not: the plugin is fine.
    let other = connect(&plugin).await;
    assert!(!other.is_closed());
}

#[tokio::test]
async fn stopping_a_plugin_is_like_its_death() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    within(plugin.stop()).await;
    assert!(connection.is_closed());
    assert!(!within(plugin.is_running()).await);
    assert!(
        connection
            .execute("engine", &ExecOptions::default())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn the_plugin_ends_when_its_input_closes() {
    let mut child = std::process::Command::new(PROGRAM)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Closing its standard input is all it takes: no message, no signal.
    drop(child.stdin.take());
    for _ in 0..200 {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let _ = child.kill();
    panic!("the plugin outlived its standard input");
}

// ------------------------------------------------------------------------ secrets

#[tokio::test]
async fn the_password_is_in_no_argument_and_no_environment_variable() {
    let plugin = Plugin::new(PROGRAM);
    let connection = connect(&plugin).await;
    let started = texts(&run(connection.as_ref(), "started").await);
    // It was started with no arguments at all.
    assert!(
        !started.iter().any(|line| line.starts_with("arg ")),
        "{started:?}"
    );
    // And the password it was given is not in its environment.
    assert!(started.iter().any(|line| line.starts_with("env ")));
    assert!(
        !started.iter().any(|line| line.contains(PASSWORD)),
        "the password is in the plugin's environment"
    );
}

#[tokio::test]
async fn a_plugin_that_repeats_the_password_is_not_heard_saying_it() {
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = lines.clone();
    let plugin = Plugin::new(PROGRAM).with_log(Arc::new(move |line: &str| {
        sink.lock().unwrap().push(line.to_string());
    }));

    // In the error of a login that fails.
    let Err(error) = within(plugin.driver("test").connect(&params("leak"))).await else {
        panic!("it connected");
    };
    let message = error.to_string();
    assert!(!message.contains(PASSWORD), "{message}");
    assert!(message.contains(REDACTED), "{message}");

    // In a notice, in every field of an error, and in what it writes to standard error.
    let connection = connect(&plugin).await;
    let events = run(connection.as_ref(), "leak").await;
    let all = format!("{events:?}");
    assert!(!all.contains(PASSWORD), "{all}");
    let Ok(ExecEvent::Notice(notice)) = &events[0] else {
        panic!("{all}");
    };
    assert_eq!(notice.message, format!("your password is {REDACTED}"));
    let Err(DriverError::Database {
        code,
        message,
        detail,
        hint,
        position,
    }) = &events[1]
    else {
        panic!("{all}");
    };
    assert_eq!(code.as_deref(), Some(REDACTED));
    assert_eq!(message, &format!("bad statement near {REDACTED}"));
    assert_eq!(
        detail.as_deref(),
        Some(format!("detail {REDACTED}").as_str())
    );
    assert_eq!(hint.as_deref(), Some(format!("hint {REDACTED}").as_str()));
    assert_eq!(*position, Some(3));

    // The log line arrives, without the password.
    for _ in 0..100 {
        if !lines.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let lines = lines.lock().unwrap().clone();
    assert_eq!(
        lines,
        [format!("debug: connected with password {REDACTED}")]
    );
}

// ------------------------------------------------------------------------ conformance

/// The set every plugin runs against itself (ADR-042), run against this one.
#[tokio::test]
async fn the_test_plugin_conforms() {
    use spelaion_plugin_host::conformance::{Target, assert_conforms, check};
    let target = Target {
        program: PROGRAM.into(),
        engine: "test".into(),
        params: params("made"),
        one_row: "rows 1".into(),
        refused: "refused".into(),
        slow: Some("sleep".into()),
    };
    assert_conforms(&target).await;

    // And it does find what is wrong: results that are not one row, statements that are not
    // refused, one that ends by itself instead of waiting to be cancelled.
    let wrong = Target {
        one_row: "rows 3".into(),
        refused: "rows 1".into(),
        slow: Some("update".into()),
        engine: "test".into(),
        program: PROGRAM.into(),
        params: params("made"),
    };
    let failures = check(&wrong).await.join(
        "
",
    );
    assert!(failures.contains("returned 3 rows, not one"), "{failures}");
    assert!(failures.contains("was not refused"), "{failures}");
    assert!(
        failures.contains("without saying it was cancelled"),
        "{failures}"
    );
    // A program that is not a plugin, and an engine it does not serve.
    let not_served = Target {
        engine: "postgres".into(),
        ..target
    };
    let failures = check(&not_served).await.join(
        "
",
    );
    assert!(failures.contains("not \"postgres\""), "{failures}");
}
