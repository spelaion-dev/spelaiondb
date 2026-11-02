//! Sessions over in-memory SQLite: they always run, with nothing external.

use std::sync::Arc;

use futures_util::StreamExt;
use spelunk_core::{CoreEvent, ResultStore, Session};
use spelunk_driver_api::{ConnectParams, Driver};
use spelunk_driver_sqlite::SqliteDriver;

async fn session() -> Session {
    let driver = SqliteDriver::default();
    let conn = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .expect("opening :memory:");
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

#[tokio::test]
async fn trigger_bodies_run_as_one_statement() {
    let session = session().await;
    let script = "create table t (x int);
                  create table log (msg text);
                  create trigger t_ins after insert on t begin
                      insert into log values (case when new.x > 0 then 'pos' else 'neg' end);
                  end;
                  insert into t values (1), (-1);
                  select msg from log order by msg";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    assert!(
        matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished {
                executed: 5,
                failed: false
            })
        ),
        "{events:#?}"
    );
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        session
            .results()
            .get(result)
            .unwrap()
            .read()
            .unwrap()
            .rows(),
        2
    );
}

#[tokio::test]
async fn syntax_errors_point_into_the_script() {
    let session = session().await;
    let script = "select 1;\nselect * form t";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    let error = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::StatementFailed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("should fail");
    let offset = error.script_offset.expect("position");
    assert!(
        script[offset..].starts_with("form"),
        "points at {:?}",
        &script[offset..]
    );
}
