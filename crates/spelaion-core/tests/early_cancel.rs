//! A cancel pressed right after Run stops the statement on MySQL and MariaDB (SDB-222): before
//! it reaches the server, at its start and while it runs. Their `KILL QUERY` only stops what
//! the connection runs at that moment, so the plugin remembers a cancel for what was sent
//! before it and kills the statement again until it ends; it never reaches the statement sent
//! after it. Each run is bounded: a lost cancel fails in 10 seconds instead of waiting for a
//! read that takes hours. Needs `SPELAION_TEST_MYSQL` and `SPELAION_TEST_MARIADB` (each
//! skipped without its variable), through the MySQL plugin.

use crate::engines;

use std::sync::Arc;
use std::time::{Duration, Instant};

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, ScriptStop, Session};
use spelaion_driver_api::{
    CancelHandle, Capabilities, ConnectParams, Connection, Driver, DriverError, EventStream,
    ExecEvent, ExecOptions, ObjectInfo, Result, SchemaInfo, TableInfo, Value,
};
use tokio::sync::Notify;

/// An engine that numbers a statement only once it is sent, and whose cancel stops the
/// statement it numbered: a cancel that arrives before the statement does stops nothing (the
/// MySQL and PostgreSQL plugins' rule, SDB-221, SDB-222).
#[derive(Default)]
struct Numbering {
    /// The next `database_context` (which a run asks just before it sends its statement, after
    /// its last check for a cancel) waits for the test.
    armed: std::sync::atomic::AtomicBool,
    /// The run is there.
    entered: Notify,
    /// The test lets it go on.
    release: Notify,
    in_flight: Mutex<Option<Arc<Notify>>>,
    cancels: AtomicUsize,
}

struct NumberingFake(Arc<Numbering>, Capabilities);

struct NumberedCancel(Arc<Numbering>);

#[async_trait]
impl CancelHandle for NumberedCancel {
    async fn cancel(&self) -> Result<()> {
        self.0.cancels.fetch_add(1, Ordering::SeqCst);
        let in_flight = self.0.in_flight.lock().unwrap().clone();
        if let Some(stop) = in_flight {
            stop.notify_one();
        }
        Ok(())
    }
}

#[async_trait]
impl Connection for NumberingFake {
    fn capabilities(&self) -> &Capabilities {
        &self.1
    }

    async fn database_context(&self) -> Result<Option<String>> {
        if self.0.armed.swap(false, Ordering::SeqCst) {
            self.0.entered.notify_one();
            self.0.release.notified().await;
        }
        Ok(None)
    }

    async fn execute(&self, _sql: &str, _opts: &ExecOptions) -> Result<EventStream> {
        // Numbered at once, as a plugin's first poll of `execute` numbers its statement.
        let engine = self.0.clone();
        let stop = Arc::new(Notify::new());
        *engine.in_flight.lock().unwrap() = Some(stop.clone());
        Ok(Box::pin(async_stream::stream! {
            let cancelled = tokio::select! {
                _ = stop.notified() => true,
                _ = tokio::time::sleep(Duration::from_secs(5)) => false,
            };
            *engine.in_flight.lock().unwrap() = None;
            if cancelled {
                yield Err(DriverError::Cancelled);
            } else {
                yield Ok(ExecEvent::Done { rows_affected: None });
            }
        }))
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(NumberedCancel(self.0.clone()))
    }

    async fn server_version(&self) -> Result<String> {
        Ok("fake".into())
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        Ok(Vec::new())
    }

    async fn list_objects(&self, _schema: &str) -> Result<Vec<ObjectInfo>> {
        Ok(Vec::new())
    }

    async fn describe_table(&self, _schema: &str, _name: &str) -> Result<TableInfo> {
        Err(DriverError::Other("none".into()))
    }
}

/// SDB-428, with no server: a cancel that comes after the run's last check reaches the engine
/// before the statement it was meant for, so it stops nothing there. The run sends it again,
/// once, as soon as the statement has started: it then stops it, and is not sent a third time.
#[tokio::test]
async fn a_cancel_that_reached_the_engine_before_its_statement_is_sent_again_once() {
    let _turn = crate::serial::file_turn(module_path!());
    let engine = Arc::new(Numbering::default());
    let fake = NumberingFake(
        engine.clone(),
        Capabilities {
            transactions: false,
            cancel: true,
            explain: false,
            editable_results: false,
            schemas: false,
            insert_rows: false,
            bound_params: false,
        },
    );
    let session = Arc::new(Session::new(
        Box::new(fake),
        Arc::new(spelaion_dialect::SqliteDialect),
        Arc::new(ResultStore::new()),
    ));
    engine.armed.store(true, Ordering::SeqCst);
    let run = tokio::spawn(session.run_script("long").collect::<Vec<_>>());
    // The run is past its last check, its statement not sent yet: the cancel arrives now, and
    // finds nothing to stop.
    tokio::time::timeout(Duration::from_secs(5), engine.entered.notified())
        .await
        .expect("the run is about to send its statement");
    session.cancel().await.unwrap();
    assert_eq!(engine.cancels.load(Ordering::SeqCst), 1);
    engine.release.notify_one();
    let events = tokio::time::timeout(Duration::from_secs(4), run)
        .await
        .expect("the statement was stopped, not left to run")
        .unwrap();
    assert!(cancelled(&events), "{events:?}");
    assert_eq!(
        engine.cancels.load(Ordering::SeqCst),
        2,
        "sent again once, and only once"
    );

    // A run with no cancel sends none.
    let quiet = tokio::spawn(session.run_script("long").collect::<Vec<_>>());
    let events = tokio::time::timeout(Duration::from_secs(10), quiet)
        .await
        .expect("the statement ended")
        .unwrap();
    assert!(!cancelled(&events), "{events:?}");
    assert_eq!(engine.cancels.load(Ordering::SeqCst), 2);
}

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
async fn a_cancel_at_and_during_the_start_of_a_long_read_stops_it() {
    let _turn = crate::serial::file_turn(module_path!());
    cancel_after(&[1, 5, 20, 100, 1_000]).await;
}

/// 0 ms: the cancel follows Run at once, before the statement can reach the server. It can
/// reach the driver before the statement is numbered and be lost two ways (SDB-428): written
/// before the statement (the run sends it again once the statement is sent), or delivered
/// first by a plugin that serves each request in a task of its own (the plugin SDK now hands
/// a session's execute and cancel to the driver in the order it read them, since plugin SDK
/// 0.3.3; the MySQL plugin is built on it from 0.1.6). Both fixes are needed: either alone
/// passed 84 and 86 runs of 100.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_right_after_run_stops_a_long_read() {
    let _turn = crate::serial::file_turn(module_path!());
    cancel_after(&[0]).await;
}

/// Cancels a long read `delay` milliseconds after Run, for each delay, on each server: it must
/// end as cancelled within [`LIMIT`], and the session then runs the next statement.
async fn cancel_after(delays: &[u64]) {
    for (label, var, driver) in servers() {
        let Some(session) = session(var, driver).await else {
            continue;
        };
        for &delay in delays {
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
    let _turn = crate::serial::file_turn(module_path!());
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
