//! Rolling back an editor tab's idle transaction, as its connection type asks (ADR-081).
//!
//! The clock runs only while the driver says a transaction is open, from the last time the
//! session's connection was used; keep-alives and reads of the transaction state do not count.
//! A session in use is never touched: a run going, paused after an error or cancelling, or
//! anything holding the connection. Some time before the end the transaction's phase is marked
//! warned, with the moment of the rollback; any use ends the warning. At the end the core holds
//! the session as Commit does, asks the driver once more and runs the engine's `ROLLBACK`: the
//! session stays open. A rollback that cannot run marks the session lost, so its server rolls
//! the transaction back when the session ends. Nothing is ever committed, reconnected or re-run.

use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};

use spelaion_driver_api::{ExecOptions, TransactionState};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::health::{Health, KEEPALIVE_ANSWER_TIMEOUT, SessionHealth, Tracked};
use crate::run_state::Runs;
use crate::sql_mode::SessionDialect;
use crate::transaction::{self, TransactionLifecycle};

/// The longest a warning comes before the rollback; half the time when that is shorter.
pub const IDLE_ROLLBACK_LEAD: Duration = Duration::from_secs(60);

/// How often the clock looks again while the warning is shown or something holds the
/// connection: a use that changes nothing else (an explorer read) still ends the warning soon.
const RECHECK: Duration = Duration::from_secs(1);

/// A transaction rolled back for being idle, as the tab says it once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleRolledBack {
    /// The connection type's time.
    pub after: Duration,
    /// Statements run in the transaction that were undone.
    pub statements: u32,
}

/// How long before the rollback the tab is warned.
pub fn idle_rollback_lead(after: Duration) -> Duration {
    (after / 2).min(IDLE_ROLLBACK_LEAD)
}

/// What the clock shares with its session. The connection is weak: the clock never keeps a
/// closed session's connection open.
pub(crate) struct Parts {
    pub(crate) tracked: Weak<Tracked>,
    pub(crate) health: Health,
    pub(crate) control: Arc<tokio::sync::Mutex<()>>,
    pub(crate) runs: Arc<Runs>,
    pub(crate) transaction: Arc<watch::Sender<TransactionLifecycle>>,
    pub(crate) dialect: Arc<SessionDialect>,
    pub(crate) options: ExecOptions,
}

/// What the attempt at the end found.
enum Attempt {
    /// Rolled back, with this many statements.
    RolledBack(u32),
    /// Used meanwhile, or no transaction any more: the clock starts again.
    NotNow,
    /// The session is gone or lost: the clock stops.
    Gone,
}

/// Runs until the session is dropped or lost.
pub(crate) async fn run(
    parts: Parts,
    after: Duration,
    on_rollback: Box<dyn Fn(IdleRolledBack) + Send + Sync>,
) {
    let lead = idle_rollback_lead(after);
    let mut transaction = parts.transaction.subscribe();
    let mut runs = parts.runs.subscribe();
    let mut health = parts.health.subscribe();
    loop {
        match *health.borrow_and_update() {
            SessionHealth::Alive => {}
            SessionHealth::Lost { .. } | SessionHealth::IdleClosed { .. } => return,
        }
        let lifecycle = *transaction.borrow_and_update();
        let _ = runs.borrow_and_update();
        let since = match parts.tracked.upgrade() {
            Some(tracked) => tracked.idle_since(),
            None => return,
        };
        let warned = lifecycle.idle_rollback_at().is_some();
        let now = Instant::now();
        let wake = if !lifecycle.in_transaction() {
            None
        } else {
            match since.filter(|_| !parts.runs.state().is_busy()) {
                // In use: no warning, and the clock starts again from the end of that use.
                None => {
                    if warned {
                        parts.transaction.send_modify(|l| l.clear_idle_warning());
                    }
                    Some(now + RECHECK)
                }
                Some(since) => {
                    let due = since + after;
                    let warn_at = due - lead;
                    if now < warn_at {
                        // Used since the warning: it ends.
                        if warned {
                            parts.transaction.send_modify(|l| l.clear_idle_warning());
                        }
                        Some(warn_at)
                    } else if now < due {
                        if !warned {
                            let at = SystemTime::now() + (due - now);
                            parts.transaction.send_modify(|l| l.warn_idle_rollback(at));
                        }
                        Some(due.min(now + RECHECK))
                    } else {
                        match roll_back(&parts, since).await {
                            Attempt::RolledBack(statements) => {
                                on_rollback(IdleRolledBack { after, statements })
                            }
                            Attempt::NotNow => {}
                            Attempt::Gone => return,
                        }
                        continue;
                    }
                }
            }
        };
        let sleep = async {
            match wake {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            changed = transaction.changed() => if changed.is_err() { return },
            changed = runs.changed() => if changed.is_err() { return },
            changed = health.changed() => if changed.is_err() { return },
            () = sleep => {}
        }
    }
}

/// Holds the session as Commit does, checks that nothing used it since `since`, asks the driver
/// and rolls back what is still open.
async fn roll_back(parts: &Parts, since: Instant) -> Attempt {
    // A run holds `control`; one waiting for it shows in the run state.
    let Ok(_running) = parts.control.try_lock() else {
        return Attempt::NotNow;
    };
    if parts.runs.state().is_busy() {
        return Attempt::NotNow;
    }
    let Some(tracked) = parts.tracked.upgrade() else {
        return Attempt::Gone;
    };
    if tracked.idle_since() != Some(since) {
        return Attempt::NotNow;
    }
    let Some(held) = tracked.try_hold() else {
        return Attempt::NotNow;
    };
    let statements = parts.transaction.borrow().statements();
    let lost = |reason: String| {
        parts.health.lose(reason);
        Attempt::Gone
    };
    let wait = KEEPALIVE_ANSWER_TIMEOUT.as_secs();
    let state = match tokio::time::timeout(
        KEEPALIVE_ANSWER_TIMEOUT,
        held.connection.transaction_state(),
    )
    .await
    {
        Ok(Ok(state)) => state,
        Ok(Err(e)) => {
            return lost(format!(
                "the idle transaction could not be checked before its rollback: {e}"
            ));
        }
        Err(_) => {
            return lost(format!(
                "the server did not say in {wait} s whether the idle transaction was still open"
            ));
        }
    };
    if state == TransactionState::Idle {
        parts.transaction.send_modify(|l| l.observe(state));
        return Attempt::NotNow;
    }
    let sql = transaction::rollback_statement(parts.dialect.current().name());
    match tokio::time::timeout(
        KEEPALIVE_ANSWER_TIMEOUT,
        crate::session::run_control(&*held.connection, sql, &parts.options),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return lost(format!(
                "the idle transaction could not be rolled back ({e}): when the session ends, its server rolls it back"
            ));
        }
        Err(_) => {
            return lost(format!(
                "the server did not answer the idle transaction's rollback in {wait} s: when the session ends, its server rolls it back"
            ));
        }
    }
    match tokio::time::timeout(
        KEEPALIVE_ANSWER_TIMEOUT,
        held.connection.transaction_state(),
    )
    .await
    {
        Ok(Ok(after)) => parts.transaction.send_modify(|l| l.observe(after)),
        Ok(Err(e)) => return lost(format!("after the idle transaction's rollback: {e}")),
        Err(_) => {
            return lost(format!(
                "the server did not answer in {wait} s after the idle transaction's rollback"
            ));
        }
    }
    Attempt::RolledBack(statements)
}
