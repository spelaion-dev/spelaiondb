//! Idle sessions against real servers (ADR-028): keep-alives keep them, never inside an open
//! transaction, and a session the server ended shows as lost. Skipped without
//! `SPELAION_TEST_PG` / `SPELAION_TEST_MYSQL` / `SPELAION_TEST_MARIADB`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session, SessionHealth};
use spelaion_driver_api::{ConnectParams, Driver, Secret};
use spelaion_driver_mysql::MysqlDriver;
use spelaion_driver_postgres::PostgresDriver;

fn params(var: &str) -> Option<ConnectParams> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: "spelaion".into(),
        ..Default::default()
    })
}

async fn session(driver: &dyn Driver, params: &ConnectParams) -> Session {
    Session::new(
        driver.connect(params).await.expect("connecting"),
        driver.dialect(),
        Arc::new(ResultStore::new()),
    )
}

/// Runs a script; `Err` with the first error's message.
async fn run(session: &Session, sql: &str) -> Result<(), String> {
    let mut events = session.run_script(sql);
    while let Some(event) = events.next().await {
        if let CoreEvent::StatementFailed { error, .. } = event {
            return Err(error.message);
        }
    }
    Ok(())
}

/// The first value of a query, through the session's connection.
async fn value(session: &Session, sql: &str) -> String {
    use spelaion_driver_api::{ColumnMeta, ExecEvent, ExecOptions, Value};
    let mut events = session
        .connection()
        .execute(sql, &ExecOptions::default())
        .await
        .expect(sql);
    while let Some(event) = events.next().await {
        if let ExecEvent::Batch(batch) = event.expect(sql)
            && batch.num_rows() > 0
        {
            let logical = ColumnMeta::from_field(batch.schema().field(0))
                .unwrap()
                .logical;
            return Value::from_array(batch.column(0).as_ref(), 0, logical)
                .unwrap()
                .to_string();
        }
    }
    panic!("{sql}: no rows");
}

async fn lost_within(session: &Session, limit: Duration) -> Option<String> {
    let mut health = session.health();
    let lost = tokio::time::timeout(
        limit,
        health.wait_for(|h| matches!(h, SessionHealth::Lost { .. })),
    )
    .await;
    match lost {
        Ok(Ok(health)) => match &*health {
            SessionHealth::Lost { reason } => Some(reason.clone()),
            SessionHealth::Alive => None,
        },
        Ok(Err(_)) | Err(_) => None,
    }
}

/// What happens overnight with MySQL: `wait_timeout` ends an idle session. Keep-alives keep
/// it; without them it is lost, and the session says so.
#[tokio::test]
async fn keep_alives_outlast_the_servers_idle_timeout() {
    for var in ["SPELAION_TEST_MYSQL", "SPELAION_TEST_MARIADB"] {
        let Some(params) = params(var) else { continue };
        let driver = if var.ends_with("MARIADB") {
            MysqlDriver::mariadb()
        } else {
            MysqlDriver::mysql()
        };
        let kept = session(&driver, &params).await;
        run(&kept, "set session wait_timeout = 3").await.unwrap();
        kept.start_keep_alive(Duration::from_secs(1));
        let dropped = session(&driver, &params).await;
        run(&dropped, "set session wait_timeout = 3").await.unwrap();

        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(*kept.health().borrow(), SessionHealth::Alive, "{var}");
        assert_eq!(value(&kept, "select 1").await, "1", "{var}");

        // The other one is gone: its next use fails and marks it lost.
        assert!(run(&dropped, "select 1").await.is_err(), "{var}");
        assert!(
            matches!(*dropped.health().borrow(), SessionHealth::Lost { .. }),
            "{var}"
        );
    }
}

/// A forgotten transaction is not kept alive: no keep-alive is sent while one is open, so its
/// locks are not held all night by them.
#[tokio::test]
async fn no_keep_alive_is_sent_inside_a_transaction() {
    let Some(params) = params("SPELAION_TEST_PG") else {
        return;
    };
    let watcher = session(&PostgresDriver, &params).await;
    let idle = session(&PostgresDriver, &params).await;
    let name = format!("spelaion-idle-{}", std::process::id());
    run(&idle, &format!("set application_name = '{name}'"))
        .await
        .unwrap();
    idle.start_keep_alive(Duration::from_secs(1));
    let query = format!(
        "select extract(epoch from query_start)::text from pg_stat_activity where application_name = '{name}'"
    );
    let (watcher, query) = (&watcher, query.as_str());
    let last_query = || value(watcher, query);

    // Idle and outside a transaction: keep-alives run.
    let before = last_query().await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let after = last_query().await;
    assert_ne!(before, after, "a keep-alive should have run");

    // Inside one: nothing more is sent.
    run(&idle, "begin; select 1").await.unwrap();
    let after_begin = last_query().await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let in_transaction = last_query().await;
    assert_eq!(
        in_transaction, after_begin,
        "even the first keep-alive must not enter the transaction"
    );
    tokio::time::sleep(Duration::from_millis(3000)).await;
    assert_eq!(
        last_query().await,
        in_transaction,
        "no keep-alive in a transaction"
    );
    assert_eq!(*idle.health().borrow(), SessionHealth::Alive);

    // Once it ends they start again.
    run(&idle, "commit").await.unwrap();
    let committed = last_query().await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_ne!(last_query().await, committed);
}

/// A session the server ended is noticed by the next keep-alive, without any query.
#[tokio::test]
async fn keep_alives_notice_a_session_the_server_ended() {
    let Some(params) = params("SPELAION_TEST_PG") else {
        return;
    };
    let admin = session(&PostgresDriver, &params).await;
    let victim = session(&PostgresDriver, &params).await;
    let name = format!("spelaion-ended-core-{}", std::process::id());
    run(&victim, &format!("set application_name = '{name}'"))
        .await
        .unwrap();
    victim.start_keep_alive(Duration::from_secs(1));
    run(
        &admin,
        &format!(
            "select pg_terminate_backend(pid) from pg_stat_activity where application_name = '{name}'"
        ),
    )
    .await
    .unwrap();
    let reason = lost_within(&victim, Duration::from_secs(10))
        .await
        .expect("the keep-alive should notice");
    assert!(!reason.is_empty());
    // Lost stays lost: nothing reconnects on its own.
    assert!(run(&victim, "select 1").await.is_err());
    assert!(matches!(
        *victim.health().borrow(),
        SessionHealth::Lost { .. }
    ));
}

/// A statement that ends its own session (the server's FATAL error comes before the close)
/// leaves the session lost at once, not at the next statement.
#[tokio::test]
async fn a_statement_that_ends_the_session_marks_it_lost() {
    let Some(params) = params("SPELAION_TEST_PG") else {
        return;
    };
    let session = session(&PostgresDriver, &params).await;
    assert!(
        run(&session, "select pg_terminate_backend(pg_backend_pid())")
            .await
            .is_err()
    );
    assert!(matches!(
        *session.health().borrow(),
        SessionHealth::Lost { .. }
    ));
}
