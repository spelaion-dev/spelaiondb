//! A cancel still on its way to the engine never stops the next run on the same session
//! (SDB-266). MySQL's `KILL QUERY` and PostgreSQL's cancel request travel over a connection of
//! their own and stop whatever the session runs when they arrive; a script that saw the cancel
//! between statements has already ended by then, and the editor lets the user run again at
//! once. A fake connection makes the race certain; MySQL, MariaDB and PostgreSQL (with their
//! `SPELAION_TEST_*` variables) run it for real.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session};
use spelaion_driver_api::{
    CancelHandle, Capabilities, ConnectParams, Connection, Driver, DriverError, EventStream,
    ExecEvent, ExecOptions, ObjectInfo, Result, SchemaInfo, TableInfo,
};

/// What the fake engine shares with its cancel handle.
#[derive(Default)]
struct Engine {
    /// The statements it was sent.
    sent: Mutex<Vec<String>>,
    /// A statement is running now.
    running: AtomicBool,
    /// The first script's first statement may finish.
    release_first: tokio::sync::Notify,
    first_released: AtomicBool,
    /// The cancel may arrive (the test lets it go).
    cancel_may_arrive: tokio::sync::watch::Sender<bool>,
    /// The cancel arrived, and whether it stopped a running statement.
    arrived: tokio::sync::watch::Sender<bool>,
    killed: AtomicBool,
}

struct Fake {
    engine: Arc<Engine>,
    capabilities: Capabilities,
}

/// Like `KILL QUERY`: on its way until the test lets it arrive, then stops whatever runs.
struct LateCancel(Arc<Engine>);

#[async_trait]
impl CancelHandle for LateCancel {
    async fn cancel(&self) -> Result<()> {
        let mut may = self.0.cancel_may_arrive.subscribe();
        let _ = may.wait_for(|go| *go).await;
        if self.0.running.load(Ordering::SeqCst) {
            self.0.killed.store(true, Ordering::SeqCst);
        }
        self.0.arrived.send_replace(true);
        Ok(())
    }
}

fn done() -> EventStream {
    let done: Result<ExecEvent> = Ok(ExecEvent::Done {
        rows_affected: None,
    });
    Box::pin(futures_util::stream::iter([done]))
}

#[async_trait]
impl Connection for Fake {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, _opts: &ExecOptions) -> Result<EventStream> {
        let engine = &self.engine;
        engine.sent.lock().unwrap().push(sql.to_string());
        engine.running.store(true, Ordering::SeqCst);
        let result = match sql {
            // Runs until the test lets it finish; the cancel has not arrived by then.
            "first" => {
                if !engine.first_released.load(Ordering::SeqCst) {
                    engine.release_first.notified().await;
                }
                Ok(done())
            }
            // Runs until the cancel has arrived: stopped if it was running when it did.
            "next" => {
                let mut arrived = engine.arrived.subscribe();
                let _ = arrived.wait_for(|a| *a).await;
                if engine.killed.load(Ordering::SeqCst) {
                    Err(DriverError::Cancelled)
                } else {
                    Ok(done())
                }
            }
            _ => Ok(done()),
        };
        engine.running.store(false, Ordering::SeqCst);
        result
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(LateCancel(self.engine.clone()))
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

/// Waits (briefly) until `condition` holds; whether it did.
async fn eventually(condition: impl Fn() -> bool, within: Duration) -> bool {
    let until = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < until {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    condition()
}

#[tokio::test]
async fn a_cancel_still_on_its_way_never_stops_the_next_run() {
    let engine = Arc::new(Engine::default());
    let fake = Fake {
        engine: engine.clone(),
        capabilities: Capabilities {
            transactions: false,
            cancel: true,
            explain: false,
            editable_results: false,
            schemas: false,
            insert_rows: false,
            bound_params: false,
        },
    };
    let session = Arc::new(Session::new(
        Box::new(fake),
        Arc::new(spelaion_driver_sqlite::SqliteDialect),
        Arc::new(ResultStore::new()),
    ));

    // A script runs its first statement; the user cancels it.
    let first: Vec<CoreEvent> = {
        let run = session.run_script("first; second");
        let run = tokio::spawn(run.collect::<Vec<_>>());
        assert!(
            eventually(
                || engine.running.load(Ordering::SeqCst),
                Duration::from_secs(5)
            )
            .await,
            "the first statement started"
        );
        let cancelling = session.clone();
        let cancel = tokio::spawn(async move { cancelling.cancel().await });
        // The statement ends before the cancel reaches the engine; the script sees the cancel
        // and stops before its second statement, with the cancel still on its way.
        tokio::task::yield_now().await;
        engine.first_released.store(true, Ordering::SeqCst);
        engine.release_first.notify_waiters();
        let events = tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .expect("the cancelled script ended")
            .unwrap();

        // The user runs again at once. Let the new run go as far as it can, then the cancel
        // arrives: it must find nothing running.
        let next = tokio::spawn(session.run_script("next").collect::<Vec<_>>());
        eventually(
            || engine.running.load(Ordering::SeqCst),
            Duration::from_millis(300),
        )
        .await;
        engine.cancel_may_arrive.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), cancel)
            .await
            .expect("the cancel returned")
            .unwrap()
            .unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .expect("the next run ended")
            .unwrap();
        assert!(
            !next
                .iter()
                .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
            "the late cancel stopped the next run: {next:?}"
        );
        assert!(
            next.iter()
                .any(|e| matches!(e, CoreEvent::StatementFinished { index: 0, .. })),
            "{next:?}"
        );
        events
    };
    // The cancelled script stopped after its first statement; nothing was run again.
    assert!(
        first
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFinished { index: 0, .. })),
        "{first:?}"
    );
    assert_eq!(*engine.sent.lock().unwrap(), ["first", "next"]);
    assert!(!engine.killed.load(Ordering::SeqCst));
}

/// A session of a real engine, if its variable is set.
async fn real(var: &str, driver: Box<dyn Driver>) -> Option<Session> {
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
    Some(Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    ))
}

/// An engine to run against: its label, variable, driver, and a short and a long statement.
type RealEngine = (
    &'static str,
    &'static str,
    Box<dyn Driver>,
    &'static str,
    &'static str,
);

/// Cancel a script of short statements, then run a longer one at once, many times: the
/// engine's late cancel must never stop the new run.
#[tokio::test]
async fn cancel_then_run_at_once_never_cancels_the_new_run_on_mysql_mariadb_and_postgres() {
    let engines: Vec<RealEngine> = vec![
        (
            "mysql",
            "SPELAION_TEST_MYSQL",
            Box::new(spelaion_driver_mysql::MysqlDriver::mysql()),
            "select sleep(0.02)",
            "select sleep(0.3)",
        ),
        (
            "mariadb",
            "SPELAION_TEST_MARIADB",
            Box::new(spelaion_driver_mysql::MysqlDriver::mariadb()),
            "select sleep(0.02)",
            "select sleep(0.3)",
        ),
        (
            "postgres",
            "SPELAION_TEST_PG",
            Box::new(spelaion_driver_postgres::PostgresDriver),
            "select pg_sleep(0.02)",
            "select pg_sleep(0.3)",
        ),
    ];
    for (label, var, driver, short, long) in engines {
        let Some(session) = real(var, driver).await else {
            continue;
        };
        let session = Arc::new(session);
        let script = vec![short; 50].join(";\n");
        for round in 0..15 {
            let run = tokio::spawn(session.run_script(&script).collect::<Vec<_>>());
            tokio::time::sleep(Duration::from_millis(30 + (round % 5) * 7)).await;
            let cancelling = session.clone();
            let cancel = tokio::spawn(async move { cancelling.cancel().await });
            tokio::time::timeout(Duration::from_secs(30), run)
                .await
                .expect("the cancelled script ended")
                .unwrap();
            // Run again the moment the cancelled script has ended, as the editor allows.
            let next: Vec<CoreEvent> =
                tokio::time::timeout(Duration::from_secs(30), session.run_script(long).collect())
                    .await
                    .expect("the next run ended");
            let failed = next.iter().find_map(|e| match e {
                CoreEvent::StatementFailed { error, .. } => Some(error.message.clone()),
                _ => None,
            });
            assert_eq!(
                failed, None,
                "{label}, round {round}: the late cancel stopped it"
            );
            tokio::time::timeout(Duration::from_secs(30), cancel)
                .await
                .expect("the cancel returned")
                .unwrap()
                .unwrap_or_else(|e| panic!("{label}: cancel failed: {e}"));
        }
    }
}
