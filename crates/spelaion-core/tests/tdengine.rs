//! TDengine through the core (ADR-056): a script of TDengine SQL is split by the core's own
//! splitter and each statement sent as it is written; `USE` is refused with its place and never
//! sent; the guard reads TDengine SQL with its own lists; and the database is the session's.
//!
//! Needs `SPELAION_TEST_TDENGINE=host:port` (`testing/docker`, 127.0.0.1:16141, user `root`,
//! password `taosdata`) and TDengine's plugin (pinned, or `SPELAION_DEV_PLUGIN_TDENGINE`).
//! Without them it is skipped, which does not count as tested.

use crate::engines;

use std::sync::Arc;

use arrow::array::{Array, Float64Array, StringArray};
use futures_util::StreamExt;
use spelaion_config::{Engine, Safeguards};
use spelaion_core::{CoreEvent, ResultId, ResultStore, Risk, Session, script_risks};
use spelaion_driver_api::{ConnectParams, Driver, Secret};

fn params() -> Option<ConnectParams> {
    let Ok(addr) = std::env::var("SPELAION_TEST_TDENGINE") else {
        eprintln!("SPELAION_TEST_TDENGINE not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: "root".into(),
        password: Some(Secret::new("taosdata")),
        connect_timeout: Some(std::time::Duration::from_secs(20)),
        ..Default::default()
    })
}

async fn session() -> Option<(Session, Arc<ResultStore>)> {
    let params = params()?;
    let driver = engines::driver(Engine::Tdengine);
    let conn = driver.connect(&params).await.expect("connecting");
    let results = Arc::new(ResultStore::new());
    let dialect = spelaion_dialect::for_engine(driver.engine()).expect("a dialect");
    Some((Session::new(conn, dialect, results.clone()), results))
}

#[derive(Default, Debug)]
struct Ran {
    results: Vec<ResultId>,
    affected: Vec<Option<u64>>,
    errors: Vec<(usize, String, Option<usize>)>,
}

async fn run(session: &Session, script: &str) -> Ran {
    let mut ran = Ran::default();
    let mut events = session.run_script(script);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => ran.results.push(result),
            CoreEvent::StatementFinished { rows_affected, .. } => ran.affected.push(rows_affected),
            CoreEvent::StatementFailed { index, error } => {
                ran.errors.push((index, error.message, error.script_offset));
            }
            _ => {}
        }
    }
    ran
}

fn cell(results: &ResultStore, id: ResultId, name: &str) -> String {
    let stored = results.get(id).expect("stored");
    let stored = stored.read().unwrap();
    let index = stored
        .schema()
        .columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {name}"));
    let batch = &stored.window(0, 1)[0];
    let column = batch.column(index);
    if let Some(text) = column.as_any().downcast_ref::<StringArray>() {
        text.value(0).to_string()
    } else if let Some(n) = column.as_any().downcast_ref::<Float64Array>() {
        n.value(0).to_string()
    } else {
        format!("{:?}", column.slice(0, 1))
    }
}

#[tokio::test]
async fn a_script_of_tdengine_sql_runs_each_statement_in_the_sessions_database() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((session, results)) = session().await else {
        return;
    };
    let setup = "DROP DATABASE IF EXISTS spelaion_core_td;\nCREATE DATABASE spelaion_core_td;";
    let ran = run(&session, setup).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    session
        .connection()
        .select_database("spelaion_core_td")
        .await
        .unwrap();
    let script = "CREATE STABLE meters (ts TIMESTAMP, current FLOAT) TAGS (location VARCHAR(32));\n\
                  -- a child table made by its first row; a semicolon in a string stays in it\n\
                  INSERT INTO d1001 USING meters TAGS ('San; Francisco') VALUES ('2024-02-29 12:00:00', 10.5);\n\
                  SELECT location, current FROM meters;";
    let ran = run(&session, script).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert!(ran.affected.contains(&Some(1)), "{:?}", ran.affected);
    let last = *ran.results.last().expect("the select's result");
    assert_eq!(cell(&results, last, "location"), "San; Francisco");
    assert_eq!(cell(&results, last, "current"), "10.5");
    assert_eq!(
        spelaion_core::active_schema(&session).await,
        Ok(Some("spelaion_core_td".into()))
    );
}

#[tokio::test]
async fn use_is_refused_in_place_and_never_sent() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((session, _results)) = session().await else {
        return;
    };
    let script = "SELECT server_version();\nUSE information_schema;";
    let ran = run(&session, script).await;
    let (index, message, offset) = ran.errors.first().expect("refused").clone();
    assert_eq!(index, 1);
    assert!(
        message.contains("choose the database above the editor"),
        "{message}"
    );
    assert_eq!(offset, Some(script.find("USE").unwrap()));
    assert_ne!(
        session
            .connection()
            .current_database()
            .await
            .unwrap()
            .as_deref(),
        Some("information_schema")
    );
}

#[test]
fn the_guard_reads_tdengine_sql_with_its_own_lists() {
    let _turn = crate::serial::file_turn(module_path!());
    let dialect = spelaion_dialect::for_engine("tdengine").expect("TDengine SQL");
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
    assert_eq!(risk("DROP DATABASE power", &plain), [Risk::Drop]);
    assert_eq!(risk("DELETE FROM meters", &plain), [Risk::DeleteAll]);
    assert_eq!(risk("KILL QUERY '3:4'", &plain), [Risk::Drop]);
    assert_eq!(risk("COMPACT DATABASE power", &plain), [Risk::Drop]);
    for quiet in [
        "SELECT * FROM meters",
        "INSERT INTO d1001 VALUES (now, 1.5)",
        "DELETE FROM d1001 WHERE ts < now",
    ] {
        assert!(risk(quiet, &plain).is_empty(), "{quiet}");
    }
    for asks in [
        "INSERT INTO d1001 VALUES (now, 1.5)",
        "CREATE USER u PASS 'x'",
    ] {
        assert_eq!(risk(asks, &production), [Risk::DataChange], "{asks}");
    }
    assert!(risk("SHOW DATABASES", &production).is_empty());
    assert!(risk("SELECT 'DROP DATABASE x' FROM t /* DELETE */", &plain).is_empty());
}
