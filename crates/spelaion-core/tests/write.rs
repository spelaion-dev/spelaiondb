//! Writes from agents (ADR-019) against SQLite (always) and a real PostgreSQL
//! (`SPELAION_TEST_PG`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session, WriteError, WriteLimits};
use spelaion_driver_api::{ConnectParams, Driver, Secret};
use spelaion_driver_postgres::PostgresDriver;
use spelaion_driver_sqlite::SqliteDriver;

async fn sqlite() -> Session {
    let driver = SqliteDriver::default();
    let conn = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

async fn pg() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let conn = PostgresDriver
        .connect(&ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "spelaion".into(),
            password: Some(Secret::new("spelaion")),
            database: "spelaion".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    Some(Session::new(
        conn,
        PostgresDriver.dialect(),
        Arc::new(ResultStore::new()),
    ))
}

async fn run(session: &Session, script: &str) {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
        "{events:?}"
    );
}

/// The first value of a query, through a normal statement: the read-only path would leave a
/// SQLite connection in `query_only`, and the write session is not one for reads.
async fn count(session: &Session, sql: &str) -> String {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result");
    let stored = session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    spelaion_driver_api::Value::from_array(
        batch.column(0).as_ref(),
        0,
        stored.schema().columns[0].logical,
    )
    .unwrap()
    .to_string()
}

#[tokio::test]
async fn destructive_ddl_is_rejected_before_reaching_sqlite_or_postgres() {
    let mut sessions = vec![sqlite().await];
    if let Some(session) = pg().await {
        sessions.push(session);
    }
    for session in sessions {
        let table = format!("agent_ddl_{}", std::process::id());
        run(
            &session,
            &format!(
                "create table {table} (id int, body text); insert into {table} values (1, 'keep')"
            ),
        )
        .await;
        let result = session
            .run_write(
                &format!("alter table {table} drop column body"),
                &WriteLimits::default(),
            )
            .await;
        assert!(matches!(result, Err(WriteError::Rejected(_))), "{result:?}");
        assert_eq!(
            count(
                &session,
                &format!("select count(*) from {table} where body = 'keep'")
            )
            .await,
            "1"
        );
        session
            .run_write(
                &format!("alter table {table} add column extra int"),
                &WriteLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            count(
                &session,
                &format!("select count(*) from {table} where extra is null")
            )
            .await,
            "1"
        );
        run(&session, &format!("drop table {table}")).await;
    }
}

#[tokio::test]
async fn writes_commit_with_their_row_count() {
    let session = sqlite().await;
    run(
        &session,
        "create table notes (id integer primary key, body text)",
    )
    .await;
    let limits = WriteLimits::default();
    let done = session
        .run_write("insert into notes (body) values ('a'), ('b');", &limits)
        .await
        .unwrap();
    assert_eq!(done.rows_affected, Some(2));
    let done = session
        .run_write("update notes set body = 'c' where id = 1", &limits)
        .await
        .unwrap();
    assert_eq!(done.rows_affected, Some(1));
    // DDL has no count, and is allowed.
    let done = session
        .run_write("create index notes_body on notes (body)", &limits)
        .await
        .unwrap();
    assert_eq!(done.rows_affected, None);
    assert_eq!(
        count(&session, "select count(*) from notes where body = 'c'").await,
        "1"
    );
}

#[tokio::test]
async fn refused_writes_never_reach_the_database() {
    let session = sqlite().await;
    run(&session, "create table notes (id integer primary key, body text); insert into notes values (1, 'keep')").await;
    for sql in [
        "drop table notes",
        "delete from notes",
        "update notes set body = 'x'",
        "insert into notes values (2, 'a'); delete from notes",
        "begin",
        "attach 'other.db' as other",
        "select * from notes",
    ] {
        match session.run_write(sql, &WriteLimits::default()).await {
            Err(WriteError::Rejected(_)) => {}
            other => panic!("{sql}: {other:?}"),
        }
    }
    assert_eq!(count(&session, "select count(*) from notes").await, "1");
}

#[tokio::test]
async fn a_failed_write_leaves_nothing_behind() {
    let session = sqlite().await;
    run(
        &session,
        "create table notes (id integer primary key, body text not null)",
    )
    .await;
    let err = session
        .run_write(
            "insert into notes (body) values ('a'), (null)",
            &WriteLimits::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, WriteError::Database(_)), "{err:?}");
    // The first row was undone with the transaction, and the session is usable.
    assert_eq!(count(&session, "select count(*) from notes").await, "0");
    session
        .run_write(
            "insert into notes (body) values ('ok')",
            &WriteLimits::default(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn postgres_writes_commit_roll_back_and_time_out() {
    let Some(session) = pg().await else { return };
    run(
        &session,
        "drop table if exists agent_notes; create table agent_notes (id int primary key, body text not null)",
    )
    .await;
    let limits = WriteLimits {
        timeout: Duration::from_secs(2),
    };
    let done = session
        .run_write(
            "insert into agent_notes values (1, 'a'), (2, 'b') returning id",
            &limits,
        )
        .await
        .unwrap();
    assert_eq!(done.rows_affected, Some(2));
    let err = session
        .run_write(
            "insert into agent_notes values (3, 'c'), (1, 'duplicate')",
            &limits,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, WriteError::Database(_)), "{err:?}");
    assert_eq!(
        count(&session, "select count(*) from agent_notes").await,
        "2"
    );
    // A write that runs too long is stopped by statement_timeout and undone.
    let started = Instant::now();
    let err = session
        .run_write(
            "update agent_notes set body = body || pg_sleep(10)::text where id = 1",
            &limits,
        )
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert!(
        err.to_string().contains("timeout") || matches!(err, WriteError::Timeout(_)),
        "{err}"
    );
    assert_eq!(
        count(&session, "select body from agent_notes where id = 1").await,
        "a"
    );
    run(&session, "drop table agent_notes").await;
}
