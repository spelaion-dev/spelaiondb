//! Redis through the core (ADR-056): a script of command lines is split and read by the core,
//! each line sent to the plugin as its argument list, its reply stored as rows; what the core
//! cannot read, or will not send, is refused with its place and never sent; the guard reads the
//! command, not the text; and the database is the session's, never a `SELECT`.
//!
//! Needs `SPELAION_TEST_REDIS=host:port` (`testing/docker`, 127.0.0.1:16379, password
//! `spelaion`) and Redis's plugin (pinned, or `SPELAION_DEV_PLUGIN_REDIS`). Without them it is
//! skipped, which does not count as tested.

use crate::engines;

use std::sync::Arc;

use arrow::array::{Array, Int64Array, StringArray};
use futures_util::StreamExt;
use spelaion_config::{Engine, Safeguards};
use spelaion_core::{CoreEvent, ResultId, ResultStore, Risk, Session, script_risks};
use spelaion_driver_api::{ConnectParams, Driver, Secret};

fn params(database: &str) -> Option<ConnectParams> {
    let Ok(addr) = std::env::var("SPELAION_TEST_REDIS") else {
        eprintln!("SPELAION_TEST_REDIS not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        password: Some(Secret::new("spelaion")),
        database: database.into(),
        ..Default::default()
    })
}

async fn session(database: &str) -> Option<(Session, Arc<ResultStore>)> {
    let params = params(database)?;
    let driver = engines::driver(Engine::Redis);
    let conn = driver.connect(&params).await.expect("connecting");
    let results = Arc::new(ResultStore::new());
    let dialect = spelaion_dialect::for_engine(driver.engine()).expect("a dialect");
    Some((Session::new(conn, dialect, results.clone()), results))
}

/// What a script did: each statement's result and error, in order.
#[derive(Default, Debug)]
struct Ran {
    results: Vec<ResultId>,
    errors: Vec<(usize, String, Option<usize>)>,
}

async fn run(session: &Session, script: &str) -> Ran {
    let mut ran = Ran::default();
    let mut events = session.run_script(script);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => ran.results.push(result),
            CoreEvent::StatementFailed { index, error } => {
                ran.errors.push((index, error.message, error.script_offset));
            }
            _ => {}
        }
    }
    ran
}

/// Column `name` of every row of a stored result, as text.
fn texts(results: &ResultStore, id: ResultId, name: &str) -> Vec<Option<String>> {
    let stored = results.get(id).expect("stored");
    let stored = stored.read().unwrap();
    let index = stored
        .schema()
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {name}"));
    let rows = stored.rows();
    let mut out = Vec::new();
    for batch in stored.window(0, rows) {
        let column = batch.column(index);
        for i in 0..column.len() {
            out.push(if column.is_null(i) {
                None
            } else if let Some(text) = column.as_any().downcast_ref::<StringArray>() {
                Some(text.value(i).to_string())
            } else if let Some(n) = column.as_any().downcast_ref::<Int64Array>() {
                Some(n.value(i).to_string())
            } else {
                Some(format!("{:?}", column.slice(i, 1)))
            });
        }
    }
    out
}

#[tokio::test]
async fn a_script_of_command_lines_runs_each_as_its_arguments() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((session, results)) = session("5").await else {
        return;
    };
    let script = r#"
# The demo's keys
DEL spelaion:core:user spelaion:core:list
HSET spelaion:core:user name "Ana María" city 'Ávila'
RPUSH spelaion:core:list a "b c"
HGETALL spelaion:core:user
LRANGE spelaion:core:list 0 -1
"#;
    let ran = run(&session, script).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(ran.results.len(), 5);
    let hash = ran.results[3];
    let mut pairs: Vec<_> = texts(&results, hash, "field")
        .into_iter()
        .zip(texts(&results, hash, "value"))
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        [
            (Some("city".into()), Some("Ávila".into())),
            (Some("name".into()), Some("Ana María".into())),
        ]
    );
    assert_eq!(
        texts(&results, ran.results[4], "value"),
        [Some("a".into()), Some("b c".into())]
    );
}

#[tokio::test]
async fn what_the_core_will_not_send_is_refused_in_place() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((session, _results)) = session("5").await else {
        return;
    };
    let script = "SET spelaion:core:k 1\nSELECT 3\nGET spelaion:core:k\n";
    let ran = run(&session, script).await;
    let (index, message, offset) = ran.errors.first().expect("refused").clone();
    assert_eq!(index, 1);
    assert!(
        message.contains("SpelaionDB could not read this statement"),
        "{message}"
    );
    assert!(
        message.contains("choose the database above the editor"),
        "{message}"
    );
    assert_eq!(offset, Some(script.find("SELECT").unwrap()));
    // A quote that never closes is refused at the quote.
    let script = "GET \"open";
    let ran = run(&session, script).await;
    assert_eq!(ran.errors[0].2, Some(4), "{:?}", ran.errors);
    // A password typed after AUTH is never repeated in what comes back.
    let ran = run(&session, "AUTH default hunter2").await;
    assert!(!ran.errors[0].1.contains("hunter2"), "{:?}", ran.errors);
}

#[tokio::test]
async fn the_database_is_the_sessions() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((session, results)) = session("6").await else {
        return;
    };
    run(&session, "SET spelaion:core:where six").await;
    session.connection().select_database("7").await.unwrap();
    run(&session, "SET spelaion:core:where seven").await;
    let ran = run(&session, "GET spelaion:core:where").await;
    assert_eq!(
        texts(&results, ran.results[0], "value"),
        [Some("seven".into())]
    );
    assert_eq!(
        spelaion_core::active_schema(&session).await,
        Ok(Some("7".into()))
    );
}

#[test]
fn the_guard_asks_before_what_redis_would_lose() {
    let _turn = crate::serial::file_turn(module_path!());
    let dialect = spelaion_dialect::for_engine("redis").expect("Redis's language");
    let plain = Safeguards::default();
    let production = Safeguards {
        confirm_data_changes: true,
        ..Safeguards::default()
    };
    let risk = |script: &str, safeguards: &Safeguards| -> Vec<Risk> {
        script_risks(dialect.as_ref(), script, safeguards)
            .into_iter()
            .map(|r| r.risk)
            .collect()
    };
    assert_eq!(risk("FLUSHDB", &plain), [Risk::DeleteAll]);
    assert_eq!(risk("flushall async", &plain), [Risk::DeleteAll]);
    assert_eq!(risk("CONFIG SET maxmemory 1mb", &plain), [Risk::Drop]);
    assert_eq!(risk("SHUTDOWN NOSAVE", &plain), [Risk::Drop]);
    assert_eq!(risk("SCRIPT FLUSH", &plain), [Risk::Drop]);
    // A write, a read, and what the core cannot tell ask nothing by default…
    for quiet in ["SET k v", "DEL k", "GET k", "KEYS *", "EVAL \"return 1\" 0"] {
        assert!(risk(quiet, &plain).is_empty(), "{quiet}");
    }
    // …and on Production a write and the unknown ask (`KEYS` walks the whole keyspace),
    // never a read.
    for asks in [
        "SET k v",
        "DEL k",
        "KEYS *",
        "EVAL \"return 1\" 0",
        "NOSUCH x",
    ] {
        assert_eq!(risk(asks, &production), [Risk::DataChange], "{asks}");
    }
    for read in ["GET k", "HGETALL h", "SCAN 0 COUNT 100", "INFO"] {
        assert!(risk(read, &production).is_empty(), "{read}");
    }
}
