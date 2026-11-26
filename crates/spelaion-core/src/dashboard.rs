//! The process monitor's live dashboard (SDB-192): a server's sessions, transactions, reads and
//! log writes over time, sampled every few seconds while the dashboard is shown, kept only in
//! memory.
//!
//! The sampler is a job the core owns (ADR-060): [`start_dashboard`] spawns it and hands back a
//! [`DashboardSampler`]; its one state, [`DashboardState`] with its [`DashboardPhase`], is
//! published on a watch and changed only by the sampler's own task. The page says whether the
//! dashboard is shown ([`DashboardSampler::set_visible`]); the sampler decides when to read, and
//! reads nothing while it is hidden. Dropping the handle stops it. Every statement is a read
//! checked by [`crate::observe::read`] before it runs, on the session it is given, which it
//! holds only weakly: once the connection is closed the sampler stops.

use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::observe::{Rows, read, whole_number};
use crate::{OpenConnection, Session};

/// How often the dashboard samples unless told otherwise.
pub const DASHBOARD_INTERVAL: Duration = Duration::from_secs(5);
/// The shortest and the longest interval a sampler accepts.
const MIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// The samples kept: half an hour at the default interval. Older ones are dropped.
pub const DASHBOARD_SAMPLES: usize = 360;

/// One sample: the gauges read at that moment and the counters' rates since the sample before.
/// A value the server does not give (a standby's WAL position, a counter it lacks) is `None`;
/// so is a rate with no sample before it to compare with, or whose counter went backwards (a
/// statistics reset or a restart).
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardSample {
    /// Milliseconds since the sampler started.
    pub at_ms: u64,
    /// The first sample after a start or a pause: the line breaks before it, and it has no
    /// rates.
    pub gap: bool,
    /// Client sessions connected, the dashboard's own left out.
    pub sessions: Option<i64>,
    /// Of those, the ones running a statement now.
    pub active: Option<i64>,
    /// Of those, the ones waiting: PostgreSQL's active sessions waiting on a lock, I/O or
    /// another server event; MySQL's and MariaDB's InnoDB row-lock waits; SQL Server's user
    /// requests with a wait type.
    pub waiting: Option<i64>,
    /// Transactions ended per second (committed or rolled back).
    pub transactions_per_second: Option<f64>,
    /// Logical reads per second: blocks or pages read, from memory or from disk.
    pub reads_per_second: Option<f64>,
    /// Bytes written to the server's log per second (PostgreSQL's WAL, InnoDB's redo log, SQL
    /// Server's transaction log): what every write costs.
    pub log_bytes_per_second: Option<f64>,
}

/// Where a sampler stands. Only the sampler's task moves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DashboardPhase {
    /// Shown: it reads every interval.
    Sampling,
    /// Hidden: it reads nothing until it is shown again.
    Paused,
    /// A read failed (a permission, a lost connection): it stopped, with the server's words.
    Failed { message: String },
    /// Its handle was dropped or its connection closed: it stopped.
    Stopped,
}

/// What the page draws: the phase and the samples kept, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardState {
    pub phase: DashboardPhase,
    pub interval_ms: u64,
    pub samples: Vec<DashboardSample>,
}

/// A running sampler. Dropping it stops the sampler.
pub struct DashboardSampler {
    visible: watch::Sender<bool>,
    state: watch::Receiver<DashboardState>,
}

impl DashboardSampler {
    /// Whether the dashboard is shown. Hidden, the sampler reads nothing; shown again, it reads
    /// at once and then every interval.
    pub fn set_visible(&self, visible: bool) {
        self.visible.send_if_modified(|shown| {
            let changed = *shown != visible;
            *shown = visible;
            changed
        });
    }

    /// The sampler's state, as it changes.
    pub fn state(&self) -> watch::Receiver<DashboardState> {
        self.state.clone()
    }

    /// Whether it stopped (failed, or its connection closed): it reads nothing more.
    pub fn has_ended(&self) -> bool {
        self.state.borrow().phase.has_ended()
    }
}

impl DashboardPhase {
    /// Failed or stopped: the sampler reads nothing more.
    pub fn has_ended(&self) -> bool {
        match self {
            Self::Sampling | Self::Paused => false,
            Self::Failed { .. } | Self::Stopped => true,
        }
    }
}

impl AsRef<Session> for Session {
    fn as_ref(&self) -> &Session {
        self
    }
}

impl AsRef<Session> for OpenConnection {
    fn as_ref(&self) -> &Session {
        &self.session
    }
}

/// Starts sampling `session`'s server every `interval` (between 1 and 60 seconds), shown.
/// The engine is checked first: one without a dashboard is refused before anything runs.
pub fn start_dashboard<S>(session: &Arc<S>, interval: Duration) -> Result<DashboardSampler, String>
where
    S: AsRef<Session> + Send + Sync + 'static,
{
    let engine = Engine::of(session.as_ref().as_ref())?;
    let interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
    let (visible, shown) = watch::channel(true);
    let (publish, state) = watch::channel(DashboardState {
        phase: DashboardPhase::Sampling,
        interval_ms: u64::try_from(interval.as_millis()).unwrap_or(u64::MAX),
        samples: Vec::new(),
    });
    tokio::spawn(sample(
        Arc::downgrade(session),
        engine,
        interval,
        shown,
        publish,
    ));
    Ok(DashboardSampler { visible, state })
}

/// The engines with a dashboard, and how each one is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Engine {
    Postgres,
    /// MySQL and MariaDB: `SHOW GLOBAL STATUS`.
    MySql,
    SqlServer,
}

impl Engine {
    fn of(session: &Session) -> Result<Self, String> {
        match session.dialect().name() {
            "postgres" => Ok(Self::Postgres),
            "mysql" | "mariadb" => Ok(Self::MySql),
            "sqlserver" => Ok(Self::SqlServer),
            _ => Err("the dashboard is not supported for this engine".into()),
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Self::Postgres => POSTGRES_COUNTERS,
            Self::MySql => MYSQL_COUNTERS,
            Self::SqlServer => SQLSERVER_COUNTERS,
        }
    }
}

/// One reading: the gauges, and the counters since the server started (or its statistics were
/// reset).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Reading {
    sessions: Option<i64>,
    active: Option<i64>,
    waiting: Option<i64>,
    transactions: Option<i64>,
    reads: Option<i64>,
    log_bytes: Option<i64>,
}

async fn read_counters(session: &Session, engine: Engine) -> Result<Reading, String> {
    let rows = read(session, engine.sql()).await?;
    match engine {
        Engine::Postgres | Engine::SqlServer => one_row(&rows),
        Engine::MySql => mysql_status(&rows),
    }
}

/// PostgreSQL's and SQL Server's answer: one row, a column per value.
fn one_row(rows: &Rows) -> Result<Reading, String> {
    let row = rows.rows.first().ok_or("the server returned no row")?;
    let value = |name: &str| -> Result<Option<i64>, String> {
        Ok(whole_number(row[rows.column(name)?].as_deref()))
    };
    Ok(Reading {
        sessions: value("sessions")?,
        active: value("active")?,
        waiting: value("waiting")?,
        transactions: value("transactions")?,
        reads: value("reads")?,
        log_bytes: value("log_bytes")?,
    })
}

/// `SHOW GLOBAL STATUS`: a row per counter. The dashboard's own connection is one of the
/// threads connected and running, and is left out.
fn mysql_status(rows: &Rows) -> Result<Reading, String> {
    let name = rows.column("Variable_name")?;
    let value = rows.column("Value")?;
    let status = |wanted: &str| {
        rows.rows
            .iter()
            .find(|row| {
                row[name]
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
            })
            .and_then(|row| whole_number(row[value].as_deref()))
    };
    let sum = |a: Option<i64>, b: Option<i64>| match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    let own = |count: Option<i64>| count.map(|c| (c - 1).max(0));
    Ok(Reading {
        sessions: own(status("Threads_connected")),
        active: own(status("Threads_running")),
        waiting: status("Innodb_row_lock_current_waits"),
        // Handler_commit counts every commit, the implicit one of an autocommit statement
        // too (Com_commit counts only COMMIT statements).
        transactions: sum(status("Handler_commit"), status("Handler_rollback")),
        reads: status("Innodb_buffer_pool_read_requests"),
        log_bytes: status("Innodb_os_log_written"),
    })
}

/// A counter's rate between two readings `seconds` apart; none when it went backwards.
fn rate(before: Option<i64>, now: Option<i64>, seconds: f64) -> Option<f64> {
    let (before, now) = (before?, now?);
    if now < before || seconds <= 0.0 {
        return None;
    }
    // A counter's difference over a few seconds fits an f64 exactly.
    #[allow(clippy::cast_precision_loss)]
    let difference = (now - before) as f64;
    Some(difference / seconds)
}

/// The sample of `now`, compared with the reading before it (none after a start or a pause).
fn sample_of(
    at_ms: u64,
    now: Reading,
    before: Option<(Instant, Reading)>,
    taken: Instant,
) -> DashboardSample {
    let rates = before.map(|(when, before)| (taken.duration_since(when).as_secs_f64(), before));
    let rate_of = |pick: fn(&Reading) -> Option<i64>| {
        rates.and_then(|(seconds, before)| rate(pick(&before), pick(&now), seconds))
    };
    DashboardSample {
        at_ms,
        gap: before.is_none(),
        sessions: now.sessions,
        active: now.active,
        waiting: now.waiting,
        transactions_per_second: rate_of(|r| r.transactions),
        reads_per_second: rate_of(|r| r.reads),
        log_bytes_per_second: rate_of(|r| r.log_bytes),
    }
}

/// The sampler's task: the only place its phase changes.
async fn sample<S>(
    session: Weak<S>,
    engine: Engine,
    interval: Duration,
    mut shown: watch::Receiver<bool>,
    publish: watch::Sender<DashboardState>,
) where
    S: AsRef<Session> + Send + Sync + 'static,
{
    let started = Instant::now();
    let mut before: Option<(Instant, Reading)> = None;
    let set_phase = |phase: DashboardPhase| {
        publish.send_if_modified(|state| {
            let changed = state.phase != phase;
            state.phase = phase;
            changed
        });
    };
    let end = loop {
        // The handle was dropped.
        if shown.has_changed().is_err() {
            break DashboardPhase::Stopped;
        }
        // Hidden: nothing is read, and the next sample starts a new line.
        if !*shown.borrow_and_update() {
            set_phase(DashboardPhase::Paused);
            before = None;
            if shown.changed().await.is_err() {
                break DashboardPhase::Stopped;
            }
            continue;
        }
        set_phase(DashboardPhase::Sampling);
        let Some(open) = session.upgrade() else {
            break DashboardPhase::Stopped;
        };
        let reading = read_counters(open.as_ref().as_ref(), engine).await;
        drop(open);
        let taken = Instant::now();
        match reading {
            Err(message) => break DashboardPhase::Failed { message },
            Ok(now) => {
                let at_ms =
                    u64::try_from(taken.duration_since(started).as_millis()).unwrap_or(u64::MAX);
                let sample = sample_of(at_ms, now, before, taken);
                before = Some((taken, now));
                publish.send_modify(|state| {
                    state.samples.push(sample);
                    let excess = state.samples.len().saturating_sub(DASHBOARD_SAMPLES);
                    state.samples.drain(..excess);
                });
            }
        }
        // Wait for the next sample; hiding the dashboard or dropping its handle wakes it.
        let next = tokio::time::sleep(interval);
        tokio::pin!(next);
        loop {
            tokio::select! {
                () = &mut next => break,
                changed = shown.changed() => {
                    if changed.is_err() || !*shown.borrow() {
                        break;
                    }
                }
            }
        }
    };
    set_phase(end);
}

// One row. The dashboard's own backend is left out; `wait_event_type` 'Client' is a session
// waiting for its client, not for the server. pg_stat_database sums every database's
// counters (a backend reports them when it goes idle, at most once a second). A standby has
// no WAL position of its own: NULL there.
pub(crate) const POSTGRES_COUNTERS: &str = r#"
SELECT
  (SELECT count(*) FROM pg_catalog.pg_stat_activity
    WHERE backend_type = 'client backend' AND pid <> pg_catalog.pg_backend_pid()) AS sessions,
  (SELECT count(*) FROM pg_catalog.pg_stat_activity
    WHERE backend_type = 'client backend' AND pid <> pg_catalog.pg_backend_pid()
      AND state = 'active') AS active,
  (SELECT count(*) FROM pg_catalog.pg_stat_activity
    WHERE backend_type = 'client backend' AND pid <> pg_catalog.pg_backend_pid()
      AND state = 'active' AND wait_event_type IS NOT NULL AND wait_event_type <> 'Client') AS waiting,
  (SELECT sum(xact_commit + xact_rollback) FROM pg_catalog.pg_stat_database) AS transactions,
  (SELECT sum(blks_read + blks_hit) FROM pg_catalog.pg_stat_database) AS reads,
  CASE WHEN pg_catalog.pg_is_in_recovery() THEN NULL
       ELSE pg_catalog.pg_wal_lsn_diff(pg_catalog.pg_current_wal_lsn(), '0/0') END AS log_bytes"#;

pub(crate) const MYSQL_COUNTERS: &str = "SHOW GLOBAL STATUS";

// One row. sys.dm_os_performance_counters' cumulative counters (the "/sec" ones are totals the
// reader divides); a named instance's objects are `MSSQL$NAME:…`, a default one's
// `SQLServer:…`, both padded with spaces. It needs VIEW SERVER STATE (VIEW SERVER PERFORMANCE
// STATE on 2022): without it the server's refusal is shown.
pub(crate) const SQLSERVER_COUNTERS: &str = r#"
SELECT
  (SELECT count(*) FROM sys.dm_exec_sessions
    WHERE is_user_process = 1 AND session_id <> @@SPID) AS sessions,
  (SELECT count(*) FROM sys.dm_exec_requests r JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
    WHERE s.is_user_process = 1 AND r.session_id <> @@SPID) AS active,
  (SELECT count(*) FROM sys.dm_exec_requests r JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id
    WHERE s.is_user_process = 1 AND r.session_id <> @@SPID AND r.wait_type IS NOT NULL) AS waiting,
  (SELECT sum(cntr_value) FROM sys.dm_os_performance_counters
    WHERE RTRIM(object_name) LIKE '%:Databases' AND RTRIM(counter_name) = 'Transactions/sec'
      AND RTRIM(instance_name) = '_Total') AS transactions,
  (SELECT sum(cntr_value) FROM sys.dm_os_performance_counters
    WHERE RTRIM(object_name) LIKE '%:Buffer Manager' AND RTRIM(counter_name) = 'Page lookups/sec') AS reads,
  (SELECT sum(cntr_value) FROM sys.dm_os_performance_counters
    WHERE RTRIM(object_name) LIKE '%:Databases' AND RTRIM(counter_name) = 'Log Bytes Flushed/sec'
      AND RTRIM(instance_name) = '_Total') AS log_bytes"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_is_a_read() {
        for (engine, sql) in [
            ("postgres", POSTGRES_COUNTERS),
            ("mysql", MYSQL_COUNTERS),
            ("sqlserver", SQLSERVER_COUNTERS),
        ] {
            crate::classify_read_only(engine, sql)
                .unwrap_or_else(|e| panic!("{engine}: {e}\n{sql}"));
        }
    }

    fn reading(transactions: i64, reads: i64, log_bytes: Option<i64>) -> Reading {
        Reading {
            sessions: Some(3),
            active: Some(1),
            waiting: Some(0),
            transactions: Some(transactions),
            reads: Some(reads),
            log_bytes,
        }
    }

    #[test]
    fn rates_are_differences_over_the_time_between_readings() {
        let first = Instant::now();
        let later = first + Duration::from_secs(2);
        let sample = sample_of(
            2_000,
            reading(110, 1_000, Some(4_096)),
            Some((first, reading(100, 400, Some(0)))),
            later,
        );
        assert!(!sample.gap);
        assert_eq!(sample.sessions, Some(3));
        assert_eq!(sample.transactions_per_second, Some(5.0));
        assert_eq!(sample.reads_per_second, Some(300.0));
        assert_eq!(sample.log_bytes_per_second, Some(2_048.0));
    }

    #[test]
    fn a_first_sample_has_gauges_and_no_rates() {
        let sample = sample_of(0, reading(10, 10, Some(10)), None, Instant::now());
        assert!(sample.gap);
        assert_eq!(sample.active, Some(1));
        assert_eq!(sample.transactions_per_second, None);
        assert_eq!(sample.reads_per_second, None);
    }

    #[test]
    fn a_counter_that_went_back_or_is_missing_has_no_rate() {
        let first = Instant::now();
        let later = first + Duration::from_secs(1);
        let sample = sample_of(
            1_000,
            reading(5, 2_000, None),
            Some((first, reading(100, 1_000, Some(7)))),
            later,
        );
        assert_eq!(sample.transactions_per_second, None, "a reset");
        assert_eq!(sample.reads_per_second, Some(1_000.0));
        assert_eq!(sample.log_bytes_per_second, None, "not given");
    }

    #[test]
    fn mysql_status_leaves_the_dashboards_own_thread_out() {
        let rows = Rows::new(
            vec!["Variable_name".into(), "Value".into()],
            [
                ("Threads_connected", "4"),
                ("Threads_running", "1"),
                ("Innodb_row_lock_current_waits", "2"),
                ("Handler_commit", "40"),
                ("Handler_rollback", "2"),
                ("Innodb_buffer_pool_read_requests", "900"),
                ("Innodb_os_log_written", "12345"),
                ("Uptime", "77"),
            ]
            .into_iter()
            .map(|(n, v)| vec![Some(n.to_string()), Some(v.to_string())])
            .collect(),
        );
        assert_eq!(
            mysql_status(&rows).unwrap(),
            Reading {
                sessions: Some(3),
                active: Some(0),
                waiting: Some(2),
                transactions: Some(42),
                reads: Some(900),
                log_bytes: Some(12_345),
            }
        );
    }
}
