//! The process monitor's live dashboard (SDB-192) against every engine that has it:
//! PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without
//! them they are skipped, which does not count as tested).
//!
//! The sampler samples while a second session does a known amount of work (committed
//! transactions that insert rows, and reads): the transactions the samples add up to are at
//! least that work's, reads and log writes move, and the working session is counted. Hidden,
//! it reads nothing; shown again, its next sample starts a new line; dropped, it stops.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, DashboardPhase, DashboardSample, DashboardState, ResultStore, Session,
    start_dashboard,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};
use tokio::sync::watch;
use tokio::time::{Instant, timeout};

use crate::engines;

/// The committed transactions of the known work.
const TRANSACTIONS: usize = 25;

async fn session(driver: &dyn Driver, params: &ConnectParams) -> Session {
    let conn = driver
        .connect(params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn login(host: &str, port: u16, user: &str, password: &str, database: &str) -> ConnectParams {
    ConnectParams {
        host: host.into(),
        port,
        user: user.into(),
        password: Some(Secret::new(password)),
        database: database.into(),
        ..Default::default()
    }
}

async fn run(label: &str, session: &Session, statement: &str) {
    let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
    for event in &events {
        if let CoreEvent::StatementFailed { error, .. } = event {
            panic!("{label}: {statement}\n→ {}", error.message)
        }
    }
}

async fn try_run(session: &Session, statement: &str) {
    let _: Vec<CoreEvent> = session.run_script(statement).collect().await;
}

/// Waits until the state passes `check`, or panics after `limit` with what it last was.
async fn wait_for(
    label: &str,
    what: &str,
    state: &mut watch::Receiver<DashboardState>,
    limit: Duration,
    check: impl Fn(&DashboardState) -> bool,
) -> DashboardState {
    let deadline = Instant::now() + limit;
    loop {
        {
            let now = state.borrow_and_update();
            if check(&now) {
                return now.clone();
            }
            if let DashboardPhase::Failed { message } = &now.phase {
                panic!("{label}: the sampler failed while waiting for {what}: {message}");
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if timeout(left, state.changed()).await.is_err() {
            panic!("{label}: {what} never came: {:#?}", *state.borrow());
        }
    }
}

/// What a counter added up to over `samples`, from each sample's rate and the time since the
/// sample before it.
fn added(samples: &[DashboardSample], rate: fn(&DashboardSample) -> Option<f64>) -> f64 {
    samples
        .windows(2)
        .filter_map(|pair| {
            let seconds = (pair[1].at_ms - pair[0].at_ms) as f64 / 1_000.0;
            rate(&pair[1]).map(|r| r * seconds)
        })
        .sum()
}

struct Engine {
    label: &'static str,
    driver: Arc<dyn Driver>,
    monitor: ConnectParams,
    worker: ConnectParams,
    /// Run by the worker first: SQL Server's work goes into a user database, since `tempdb`'s
    /// commits do not flush its log.
    prepare: &'static [&'static str],
    table: &'static str,
    begin: &'static str,
    commit: &'static str,
}

async fn samples_around_work(engine: Engine) {
    let Engine {
        label,
        driver,
        monitor,
        worker,
        prepare,
        table,
        begin,
        commit,
    } = engine;
    let monitor = Arc::new(session(driver.as_ref(), &monitor).await);
    let worker = session(driver.as_ref(), &worker).await;
    for statement in prepare {
        run(label, &worker, statement).await;
    }
    try_run(&worker, &format!("DROP TABLE {table}")).await;
    run(
        label,
        &worker,
        &format!("CREATE TABLE {table} (id INT PRIMARY KEY, v VARCHAR(200))"),
    )
    .await;

    let sampler = start_dashboard(&monitor, Duration::from_secs(1))
        .unwrap_or_else(|e| panic!("{label}: starting: {e}"));
    let mut state = sampler.state();
    let first = wait_for(
        label,
        "a first sample",
        &mut state,
        Duration::from_secs(20),
        |s| !s.samples.is_empty(),
    )
    .await;
    assert_eq!(first.interval_ms, 1_000, "{label}");
    assert!(
        first.samples[0].gap,
        "{label}: the first sample starts a line"
    );
    assert_eq!(
        first.samples[0].transactions_per_second, None,
        "{label}: nothing to compare the first sample with"
    );
    let before = first.samples.len();

    // The known work: committed transactions that insert rows, then reads of them.
    for id in 0..TRANSACTIONS {
        run(label, &worker, begin).await;
        run(
            label,
            &worker,
            &format!(
                "INSERT INTO {table} (id, v) VALUES ({id}, '{}')",
                "x".repeat(150)
            ),
        )
        .await;
        run(label, &worker, commit).await;
    }
    for _ in 0..5 {
        run(label, &worker, &format!("SELECT count(*) FROM {table}")).await;
    }

    // PostgreSQL's backends report their counters when they go idle, at most once a second,
    // so the work can take a few samples to show.
    let after = wait_for(
        label,
        "the work in the samples",
        &mut state,
        Duration::from_secs(40),
        |s| {
            let since = &s.samples[before - 1..];
            added(since, |x| x.transactions_per_second) >= TRANSACTIONS as f64 * 0.99
                && added(since, |x| x.reads_per_second) > 0.0
                && added(since, |x| x.log_bytes_per_second) > 0.0
        },
    )
    .await;
    let latest = after.samples.last().unwrap();
    assert!(
        latest.sessions.is_some_and(|n| n >= 1),
        "{label}: the working session is counted: {latest:?}"
    );
    assert!(
        latest.active.is_some() && latest.waiting.is_some(),
        "{label}: {latest:?}"
    );
    assert!(
        after.samples.windows(2).all(|p| p[0].at_ms < p[1].at_ms),
        "{label}: samples in order"
    );

    // Hidden: nothing is read.
    sampler.set_visible(false);
    let paused = wait_for(
        label,
        "the pause",
        &mut state,
        Duration::from_secs(10),
        |s| s.phase == DashboardPhase::Paused,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert_eq!(
        state.borrow().samples.len(),
        paused.samples.len(),
        "{label}: no sample while hidden"
    );
    // Shown again: the next sample starts a new line, without rates.
    sampler.set_visible(true);
    let count = paused.samples.len();
    let resumed = wait_for(
        label,
        "a sample after the pause",
        &mut state,
        Duration::from_secs(20),
        |s| s.phase == DashboardPhase::Sampling && s.samples.len() > count,
    )
    .await;
    let next = &resumed.samples[count];
    assert!(next.gap, "{label}: a new line after the pause: {next:?}");
    assert_eq!(next.reads_per_second, None, "{label}");

    // Dropped: it stops.
    drop(sampler);
    wait_for(
        label,
        "the stop",
        &mut state,
        Duration::from_secs(10),
        |s| s.phase == DashboardPhase::Stopped,
    )
    .await;

    run(label, &worker, &format!("DROP TABLE {table}")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_dashboard_postgres_counters_move_with_work() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_PG") else {
        return;
    };
    let user = login(&host, port, "spelaion", "spelaion", "spelaion");
    samples_around_work(Engine {
        label: "postgres",
        driver: engines::postgres(),
        monitor: user.clone(),
        worker: user,
        prepare: &[],
        table: "dashboard_work",
        begin: "BEGIN",
        commit: "COMMIT",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_dashboard_mysql_counters_move_with_work() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_MYSQL") else {
        return;
    };
    // SHOW GLOBAL STATUS needs no privilege: the plain test user samples.
    let user = login(&host, port, "spelaion", "spelaion", "spelaion");
    samples_around_work(Engine {
        label: "mysql",
        driver: engines::mysql(),
        monitor: user.clone(),
        worker: user,
        prepare: &[],
        table: "dashboard_work",
        begin: "START TRANSACTION",
        commit: "COMMIT",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_dashboard_mariadb_counters_move_with_work() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_MARIADB") else {
        return;
    };
    let user = login(&host, port, "spelaion", "spelaion", "spelaion");
    samples_around_work(Engine {
        label: "mariadb",
        driver: engines::mariadb(),
        monitor: user.clone(),
        worker: user,
        prepare: &[],
        table: "dashboard_work",
        begin: "START TRANSACTION",
        commit: "COMMIT",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_dashboard_sqlserver_counters_move_with_work() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_MSSQL") else {
        return;
    };
    let user = ConnectParams {
        tls: TlsMode::Require,
        ..login(&host, port, "sa", "Spelaion-test-1", "tempdb")
    };
    samples_around_work(Engine {
        label: "sqlserver",
        driver: engines::sqlserver(),
        monitor: user.clone(),
        worker: user,
        prepare: &[
            "IF DB_ID('spelaion') IS NULL CREATE DATABASE spelaion",
            "USE spelaion",
        ],
        table: "dashboard_work",
        begin: "BEGIN TRANSACTION",
        commit: "COMMIT",
    })
    .await;
}

/// An engine without a dashboard is refused before anything runs.
#[tokio::test(flavor = "multi_thread")]
async fn server_dashboard_refuses_an_engine_without_one() {
    let _turn = crate::serial::file_turn(module_path!());
    let sqlite = session(
        engines::sqlite().as_ref(),
        &ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        },
    )
    .await;
    let refused = start_dashboard(&Arc::new(sqlite), Duration::from_secs(5))
        .err()
        .unwrap_or_default();
    assert!(refused.contains("not supported"), "{refused}");
}
