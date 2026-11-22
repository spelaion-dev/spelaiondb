//! Cassandra through the core (ADR-056): a script of CQL is split by the core's own splitter
//! (batches whole) and each statement sent as it is written; `USE` is refused with its place and
//! never sent; the guard reads CQL with its own lists; and the keyspace is the session's.
//!
//! Needs `SPELAION_TEST_CASSANDRA=host:port` (`testing/docker`, 127.0.0.1:19142) and Cassandra's
//! plugin (pinned, or `SPELAION_DEV_PLUGIN_CASSANDRA`). Without them it is skipped, which does
//! not count as tested.

use crate::engines;

use std::sync::Arc;

use arrow::array::{Array, Int64Array, StringArray};
use futures_util::StreamExt;
use spelaion_config::{Engine, Safeguards};
use spelaion_core::{CoreEvent, ResultId, ResultStore, Risk, Session, script_risks};
use spelaion_driver_api::{ConnectParams, Driver};

fn params() -> Option<ConnectParams> {
    let Ok(addr) = std::env::var("SPELAION_TEST_CASSANDRA") else {
        eprintln!("SPELAION_TEST_CASSANDRA not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        connect_timeout: Some(std::time::Duration::from_secs(30)),
        ..Default::default()
    })
}

async fn session() -> Option<(Session, Arc<ResultStore>)> {
    let params = params()?;
    let driver = engines::driver(Engine::Cassandra);
    let conn = driver.connect(&params).await.expect("connecting");
    let results = Arc::new(ResultStore::new());
    let dialect = spelaion_dialect::for_engine(driver.engine()).expect("a dialect");
    Some((Session::new(conn, dialect, results.clone()), results))
}

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
async fn a_script_of_cql_runs_each_statement_and_a_batch_whole() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((session, results)) = session().await else {
        return;
    };
    let setup = "DROP KEYSPACE IF EXISTS spelaion_core_cql;\n\
                 CREATE KEYSPACE spelaion_core_cql WITH replication = \
                 {'class': 'SimpleStrategy', 'replication_factor': 1};";
    let ran = run(&session, setup).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    session
        .connection()
        .select_database("spelaion_core_cql")
        .await
        .unwrap();
    let script = "CREATE TABLE people (id int PRIMARY KEY, name text);\n\
                  -- two rows in one batch; its semicolons stay inside it\n\
                  BEGIN BATCH\n\
                    INSERT INTO people (id, name) VALUES (1, 'Ana; María');\n\
                    INSERT INTO people (id, name) VALUES (2, 'Luis');\n\
                  APPLY BATCH;\n\
                  SELECT name FROM people WHERE id = 1;";
    let ran = run(&session, script).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    let last = *ran.results.last().expect("the select's result");
    assert_eq!(texts(&results, last, "name"), [Some("Ana; María".into())]);
    assert_eq!(
        spelaion_core::active_schema(&session).await,
        Ok(Some("spelaion_core_cql".into()))
    );
}

#[tokio::test]
async fn use_is_refused_in_place_and_never_sent() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((session, _results)) = session().await else {
        return;
    };
    let script = "SELECT release_version FROM system.local;\nUSE system;";
    let ran = run(&session, script).await;
    let (index, message, offset) = ran.errors.first().expect("refused").clone();
    assert_eq!(index, 1);
    assert!(
        message.contains("choose the keyspace above the editor"),
        "{message}"
    );
    assert_eq!(offset, Some(script.find("USE").unwrap()));
    // The session's keyspace did not move.
    assert_ne!(
        session
            .connection()
            .current_database()
            .await
            .unwrap()
            .as_deref(),
        Some("system")
    );
}

#[test]
fn the_guard_reads_cql_with_its_own_lists() {
    let _turn = crate::serial::file_turn(module_path!());
    let dialect = spelaion_dialect::for_engine("cassandra").expect("CQL");
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
    assert_eq!(risk("DROP TABLE t", &plain), [Risk::Drop]);
    assert_eq!(risk("drop keyspace shop", &plain), [Risk::Drop]);
    assert_eq!(risk("TRUNCATE t", &plain), [Risk::DeleteAll]);
    assert_eq!(risk("ALTER TABLE t DROP c", &plain), [Risk::Drop]);
    for quiet in [
        "SELECT * FROM t",
        "INSERT INTO t (a) VALUES (1)",
        "GRANT SELECT ON t TO r",
    ] {
        assert!(risk(quiet, &plain).is_empty(), "{quiet}");
    }
    // On Production a write and what the core cannot tell ask, never a read; a DELETE needs
    // no WHERE warning of SQL's (CQL refuses a DELETE without one).
    for asks in [
        "INSERT INTO t (a) VALUES (1)",
        "DELETE FROM t WHERE a = 1",
        "BEGIN BATCH INSERT INTO t (a) VALUES (1); APPLY BATCH",
        "GRANT SELECT ON t TO r",
    ] {
        assert_eq!(risk(asks, &production), [Risk::DataChange], "{asks}");
    }
    assert!(risk("SELECT * FROM t", &production).is_empty());
    // A word in a string or a comment is not the statement's.
    assert!(risk("SELECT 'DROP TABLE t' FROM t /* TRUNCATE */", &plain).is_empty());
}
