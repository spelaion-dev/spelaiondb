//! Sessions against a real PostgreSQL. Needs `SPELUNK_TEST_PG=host:port`
//! (see `crates/spelunk-driver-postgres/tests/postgres.rs`).

use std::sync::Arc;

use futures_util::StreamExt;
use spelunk_core::{CoreEvent, ResultStore, Session};
use spelunk_driver_api::{ConnectParams, Driver, Secret};
use spelunk_driver_postgres::PostgresDriver;

async fn session() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELUNK_TEST_PG") else {
        eprintln!("SPELUNK_TEST_PG not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let params = ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: "spelunk".into(),
        password: Some(Secret::new("spelunk")),
        database: "spelunk".into(),
        ..Default::default()
    };
    let driver = PostgresDriver;
    let conn = driver.connect(&params).await.expect("connecting");
    Some(Session::new(
        conn,
        driver.dialect(),
        Arc::new(ResultStore::new()),
    ))
}

#[tokio::test]
async fn runs_a_script_statement_by_statement() {
    let Some(session) = session().await else {
        return;
    };
    let script = "create temp table t (id int, name text);
                  insert into t values (1, 'a'), (2, 'b'), (3, 'c');
                  -- stray comment;
                  select * from t order by id;
                  select count(*) from t";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;

    let finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::StatementFinished {
                index,
                rows_affected,
                ..
            } => Some((*index, *rows_affected)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        [(0, None), (1, Some(3)), (2, Some(3)), (3, Some(1))]
    );
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished {
            executed: 4,
            failed: false
        })
    ));

    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    let first = session.results().get(results[0]).unwrap();
    let first = first.read().unwrap();
    assert_eq!(first.rows(), 3);
    assert!(first.is_complete());
    assert_eq!(
        first
            .schema()
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "name"]
    );
}

#[tokio::test]
async fn stops_at_the_first_error_and_points_to_it() {
    let Some(session) = session().await else {
        return;
    };
    let script = "select 1;\nselect * from\n  missing_table;\nselect 3";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;

    let error = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::StatementFailed { index: 1, error } => Some(error.clone()),
            _ => None,
        })
        .expect("the second statement should fail");
    assert_eq!(error.code.as_deref(), Some("42P01"));
    let offset = error.script_offset.expect("error position");
    assert!(
        script[offset..].starts_with("missing_table"),
        "points at {:?}",
        &script[offset..]
    );
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished {
            executed: 2,
            failed: true
        })
    ));
}

#[tokio::test]
async fn large_results_arrive_in_batches() {
    let Some(session) = session().await else {
        return;
    };
    let events: Vec<CoreEvent> = session
        .run_script("select g, md5(g::text) from generate_series(1, 100000) g")
        .collect()
        .await;
    let progress: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::Rows { total_rows, .. } => Some(*total_rows),
            _ => None,
        })
        .collect();
    assert_eq!(progress.len(), 10);
    assert_eq!(progress.last(), Some(&100_000));
}
