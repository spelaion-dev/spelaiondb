//! A cancel pressed right after Run stops the statement on MySQL and MariaDB (SDB-222): before
//! it reaches the server, at its start and while it runs. Their `KILL QUERY` only stops what
//! the connection runs at that moment, so the plugin remembers a cancel for what was sent
//! before it and kills the statement again until it ends; it never reaches the statement sent
//! after it. Each run is bounded: a lost cancel fails in 10 seconds instead of waiting for a
//! read that takes hours. Needs `SPELAION_TEST_MYSQL` and `SPELAION_TEST_MARIADB` (each
//! skipped without its variable), through the MySQL plugin.

mod engines;

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, ScriptStop, Session};
use spelaion_driver_api::{ConnectParams, Driver, Value};

/// A read that runs for hours unless it is cancelled, and fails when it is.
const SLOW: &str = "select count(*) from information_schema.columns a, \
                    information_schema.columns b, information_schema.columns c";
/// The longest a cancelled run may take to end.
const LIMIT: Duration = Duration::from_secs(10);

async fn session(var: &str, driver: Arc<dyn Driver>) -> Option<Arc<Session>> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let params = ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: "spelaion".into(),
        password: Some(spelaion_driver_api::Secret::new("spelaion")),
        database: "spelaion".into(),
        ..Default::default()
    };
    let conn = driver.connect(&params).await.expect("connecting");
    Some(Arc::new(Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )))
}

/// Whether the run's statement ended as cancelled: stopped by the engine (a one-statement
/// script has nothing left after it, so its summary names no stop), or never sent because the
/// cancel arrived first (its summary stops at it, with nothing executed).
fn cancelled(events: &[CoreEvent]) -> bool {
    events.iter().any(|event| match event {
        CoreEvent::StatementFailed { index: 0, error } => error.cancelled,
        CoreEvent::ScriptFinished { summary, .. } => {
            summary.stop == Some(ScriptStop::Cancelled { index: 0 }) && summary.executed == 0
        }
        _ => false,
    })
}

/// The first value of `sql`, and how long it took.
async fn timed_value(session: &Session, sql: &str) -> (String, Duration) {
    let started = Instant::now();
    let mut events = session.run_script(sql);
    let mut last = None;
    while let Some(event) = tokio::time::timeout(LIMIT, events.next())
        .await
        .unwrap_or_else(|_| panic!("{sql}: did not end in time"))
    {
        match event {
            CoreEvent::ResultStarted { result, .. } => last = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {}", error.message),
            _ => {}
        }
    }
    let elapsed = started.elapsed();
    let stored = session
        .results()
        .get(last.expect("a result"))
        .expect("stored");
    let cell = stored.read().unwrap().value_at(0, 0).expect("a cell");
    let text = match cell {
        Value::Text(text) => text,
        other => other.to_string(),
    };
    (text, elapsed)
}

fn servers() -> Vec<(&'static str, &'static str, Arc<dyn Driver>)> {
    vec![
        ("mysql", "SPELAION_TEST_MYSQL", engines::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", engines::mariadb()),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_before_at_and_during_the_start_of_a_long_read_stops_it() {
    for (label, var, driver) in servers() {
        let Some(session) = session(var, driver).await else {
            continue;
        };
        // 0 ms: the cancel follows Run at once, before the statement can reach the server.
        for delay in [0, 1, 5, 20, 100, 1_000] {
            let run = tokio::spawn(session.run_script(SLOW).collect::<Vec<_>>());
            tokio::time::sleep(Duration::from_millis(delay)).await;
            session
                .cancel()
                .await
                .unwrap_or_else(|e| panic!("{label}: cancel failed: {e}"));
            let events = tokio::time::timeout(LIMIT, run)
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "{label}: still running {} s after a cancel at {delay} ms",
                        LIMIT.as_secs()
                    )
                })
                .unwrap();
            assert!(
                cancelled(&events),
                "{label}: cancelled at {delay} ms: {events:?}"
            );
            // The session goes on: the next statement runs to its end.
            let (value, _) = timed_value(&session, "select 42").await;
            assert_eq!(value, "42", "{label}: after a cancel at {delay} ms");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_with_nothing_running_does_not_touch_the_next_statement() {
    for (label, var, driver) in servers() {
        let Some(session) = session(var, driver).await else {
            continue;
        };
        // A `KILL QUERY` that reached the next statement would end its sleep at once.
        session.cancel().await.unwrap();
        let (value, took) = timed_value(&session, "select sleep(1)").await;
        assert_eq!(value, "0", "{label}");
        assert!(
            took >= Duration::from_millis(900),
            "{label}: the next statement was cut short ({took:?})"
        );
        // A cancel after a statement ended does not reach the one after it either.
        let (value, _) = timed_value(&session, "select 1").await;
        assert_eq!(value, "1");
        session.cancel().await.unwrap();
        let (value, took) = timed_value(&session, "select sleep(1)").await;
        assert_eq!(value, "0", "{label}");
        assert!(
            took >= Duration::from_millis(900),
            "{label}: a late cancel cut the next statement short ({took:?})"
        );
    }
}
