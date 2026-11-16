//! MongoDB through the core (ADR-056): a script of the shell's statements is split and read by
//! the core, each statement sent to the plugin as the command document it is, its documents
//! stored as rows; what the core cannot read is refused with its place and never sent; the
//! guard reads the command, not the text; and the barriers that MongoDB has not got yet are not
//! offered (read-only connections, agents).
//!
//! Needs `SPELAION_TEST_MONGO=host:port` (`testing/docker`, 127.0.0.1:27017) and MongoDB's
//! plugin (pinned, or `SPELAION_DEV_PLUGIN_MONGODB`). Without them it is skipped, which does not
//! count as tested.

mod engines;

use std::sync::Arc;

use arrow::array::{Array, Int64Array, StringArray};
use futures_util::StreamExt;
use spelaion_config::{Engine, Safeguards};
use spelaion_core::{CoreEvent, ResultId, ResultStore, Risk, Session, script_risks};
use spelaion_driver_api::{ConnectParams, Driver, Secret};

fn params(database: &str) -> Option<ConnectParams> {
    let Ok(addr) = std::env::var("SPELAION_TEST_MONGO") else {
        eprintln!("SPELAION_TEST_MONGO not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: database.into(),
        ..Default::default()
    })
}

async fn session(database: &str) -> Option<(Session, Arc<ResultStore>)> {
    let params = params(database)?;
    let driver = engines::driver(Engine::Mongodb);
    let conn = driver.connect(&params).await.expect("connecting");
    let results = Arc::new(ResultStore::new());
    let dialect = spelaion_dialect::for_engine(driver.engine()).expect("a dialect");
    Some((Session::new(conn, dialect, results.clone()), results))
}

/// What a script did: each statement's result, rows affected and error, in order.
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

fn column_names(results: &ResultStore, id: ResultId) -> Vec<String> {
    let stored = results.get(id).expect("stored");
    let stored = stored.read().unwrap();
    stored
        .schema()
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect()
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
async fn a_script_of_shell_statements_runs_each_as_its_command() {
    let Some((session, results)) = session("spelaion_core_mongo").await else {
        return;
    };
    let script = r#"
db.people.drop()
db.people.insertMany([
  {_id: 1, name: "Ana", age: 31, tags: ["a"]},
  {_id: 2, name: "Luis", age: 25},
  {_id: 3, name: "Eva", age: 40, city: "Ávila"},
])
db.people.find({age: {$gt: 30}}, {name: 1, age: 1}).sort({age: -1})
db.people.updateMany({age: {$lt: 30}}, {$set: {young: true}});
db.people.countDocuments({})
db.getCollection("people").deleteOne({_id: 2})
"#;
    let ran = run(&session, script).await;
    // Dropping a collection that does not exist yet fails as the server says; the rest runs.
    let other: Vec<_> = ran.errors.iter().filter(|(i, ..)| *i != 0).collect();
    assert!(other.is_empty(), "{other:?}");
    // drop (failed or not), insertMany, find, updateMany, countDocuments, deleteOne.
    let found = ran.results[2 - usize::from(ran.errors.iter().any(|(i, ..)| *i == 0))];
    assert_eq!(column_names(&results, found), ["_id", "name", "age", "…"]);
    assert_eq!(
        texts(&results, found, "name"),
        [Some("Eva".into()), Some("Ana".into())]
    );
    assert!(
        ran.affected.contains(&Some(3)),
        "insertMany counts its documents: {:?}",
        ran.affected
    );
    assert!(ran.affected.contains(&Some(1)), "{:?}", ran.affected);
    let count = *ran.results.iter().rev().nth(1).expect("the count's result");
    assert_eq!(texts(&results, count, "n"), [Some("3".into())]);
}

#[tokio::test]
async fn what_the_core_cannot_read_is_refused_in_place_and_never_sent() {
    let Some((session, _results)) = session("spelaion_core_mongo_refused").await else {
        return;
    };
    let script = "db.t.insertOne({_id: 1})\ndb.t.find({a: Math.random()})\n";
    let ran = run(&session, script).await;
    let (index, message, offset) = ran.errors.first().expect("refused").clone();
    assert_eq!(index, 1);
    assert!(
        message.contains("SpelaionDB could not read this statement"),
        "{message}"
    );
    assert!(message.contains("JavaScript is not run"), "{message}");
    // The place is `Math`, in the script.
    let math = script.find("Math").unwrap();
    assert_eq!(offset, Some(math));
    // A statement of SQL is not MongoDB either: the core reads it, and refuses it.
    let ran = run(&session, "SELECT * FROM t").await;
    assert!(
        ran.errors[0].1.contains("starts with `db.` or `show`"),
        "{:?}",
        ran.errors
    );
}

#[tokio::test]
async fn the_current_database_is_where_a_statement_runs() {
    let Some((session, results)) = session("spelaion_core_mongo_a").await else {
        return;
    };
    run(&session, "db.here.drop()\ndb.here.insertOne({_id: 'a'})").await;
    session
        .connection()
        .select_database("spelaion_core_mongo_b")
        .await
        .unwrap();
    run(&session, "db.here.drop()\ndb.here.insertOne({_id: 'b'})").await;
    let ran = run(
        &session,
        r#"db.here.find()
db.getSiblingDB("spelaion_core_mongo_a").here.find()"#,
    )
    .await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(texts(&results, ran.results[0], "_id"), [Some("b".into())]);
    assert_eq!(texts(&results, ran.results[1], "_id"), [Some("a".into())]);
    // What the editor and the explorer take as the tab's schema, asked without SQL.
    assert_eq!(
        spelaion_core::active_schema(&session).await,
        Ok(Some("spelaion_core_mongo_b".into()))
    );
}

#[test]
fn the_guard_asks_before_what_mongodb_would_lose() {
    let dialect = spelaion_dialect::for_engine("mongodb").expect("MongoDB's language");
    let plain = Safeguards::default();
    let production = Safeguards {
        confirm_data_changes: true,
        ..Safeguards::default()
    };
    let every = Safeguards {
        confirm_execution: true,
        ..Safeguards::default()
    };
    let risk = |script: &str, safeguards: &Safeguards| -> Vec<Risk> {
        script_risks(dialect.as_ref(), script, safeguards)
            .into_iter()
            .map(|r| r.risk)
            .collect()
    };
    assert_eq!(risk("db.c.drop()", &plain), [Risk::Drop]);
    assert_eq!(risk("db.runCommand({drop: 'c'})", &plain), [Risk::Drop]);
    assert_eq!(risk("db.dropDatabase()", &plain), [Risk::Drop]);
    assert_eq!(risk("db.c.deleteMany({})", &plain), [Risk::DeleteAll]);
    assert_eq!(
        risk(
            "db.runCommand({delete: 'c', deletes: [{q: {}, limit: 0}]})",
            &plain
        ),
        [Risk::DeleteAll]
    );
    assert_eq!(
        risk("db.c.updateMany({}, {$set: {a: 1}})", &plain),
        [Risk::UpdateAll]
    );
    assert_eq!(risk("db.c.aggregate([{$out: 'd'}])", &plain), [Risk::Drop]);
    // A write with a filter, a read, and what the core cannot tell ask nothing by default…
    for quiet in [
        "db.c.deleteOne({})",
        "db.c.deleteMany({a: 1})",
        "db.c.insertOne({a: 1})",
        "db.c.find()",
        "db.runCommand({shutdown: 1})",
    ] {
        assert!(risk(quiet, &plain).is_empty(), "{quiet}");
    }
    // …and on Production a write and the unknown ask, never a read.
    assert_eq!(
        risk("db.c.insertOne({a: 1})", &production),
        [Risk::DataChange]
    );
    assert_eq!(
        risk("db.runCommand({shutdown: 1})", &production),
        [Risk::DataChange]
    );
    assert_eq!(
        risk("db.c.find({$where: 'true'})", &production),
        [Risk::DataChange]
    );
    assert_eq!(
        risk("db.c.find({a: Math.random()})", &production),
        [Risk::DataChange]
    );
    assert!(risk("db.c.find({a: 1})", &production).is_empty());
    assert_eq!(risk("db.c.find()", &every), [Risk::Execution]);
    // Each statement of a script is judged.
    let risks = script_risks(dialect.as_ref(), "db.c.find()\ndb.c.drop()", &plain);
    assert_eq!(risks.len(), 1);
    assert_eq!(risks[0].index, 1);
}
