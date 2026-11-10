//! Cancelling the work of an embedded engine that runs on its own thread (DuckDB, SQLite).
//!
//! Those engines are interrupted through a handle that only reaches the statement running at
//! that moment: DuckDB clears its interrupt flag whenever a statement starts (on prepare, and
//! again when the prepared statement begins executing), and SQLite ignores an interrupt when
//! no statement is running. So an interrupt sent while a statement is still queued on the
//! connection's thread, or is being prepared, is lost and the statement runs to its end,
//! which for some queries is never.
//!
//! A cancel here is remembered instead: it applies to every piece of work submitted before it,
//! queued or running. Work that has not started when its cancel arrives never starts (the
//! thread checks [`Work::check`] first), and work that is running is interrupted again every
//! few milliseconds until it ends, so an interrupt the engine dropped while starting is
//! followed by one it sees.
//!
//! A cancel never reaches work submitted after it: interrupts are sent, under the state lock,
//! only while a cancelled piece of work is running, and the thread takes that lock to mark the
//! work finished before it starts the next one. Both engines clear what is left of an
//! interrupt when the next statement starts.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::{DriverError, Result};

/// How often a running cancelled piece of work is interrupted again.
const REINTERRUPT_EVERY: Duration = Duration::from_millis(10);

/// Interrupts whatever the engine is running at that moment (DuckDB's or SQLite's handle).
pub type Interrupt = Box<dyn Fn() + Send + Sync>;

/// The cancel state of one connection, shared by its cancel handle, its thread and the
/// interrupter thread a cancel starts.
pub struct Canceller {
    state: Mutex<State>,
    changed: Condvar,
    interrupt: Interrupt,
}

#[derive(Default)]
struct State {
    /// Id of the last piece of work submitted (ids start at 1).
    submitted: u64,
    /// Id of the last piece of work the thread finished (work runs in id order).
    finished: u64,
    /// The piece of work the thread is running, if any.
    running: Option<u64>,
    /// Work with an id up to this one is cancelled.
    cancelled_up_to: u64,
    /// Whether an interrupter thread is running.
    interrupter: bool,
    /// The connection's thread has ended: nothing will finish any more.
    closed: bool,
}

impl Canceller {
    pub fn new(interrupt: Interrupt) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            interrupt,
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A few counters, valid whatever a panicking holder left.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Gives the next piece of work its id and hands it to `send` (which puts it in the
    /// thread's queue without waiting), under the lock, so ids reach the thread in order.
    /// Nothing is counted if `send` fails.
    pub fn submit(&self, send: impl FnOnce(u64) -> Result<()>) -> Result<()> {
        let mut state = self.lock();
        let id = state.submitted + 1;
        send(id)?;
        state.submitted = id;
        Ok(())
    }

    /// The thread starts the piece of work `id`. Dropping the returned [`Work`] marks it
    /// finished, so it must outlive every engine call the work makes.
    pub fn begin(self: &Arc<Self>, id: u64) -> Work {
        self.lock().running = Some(id);
        Work {
            canceller: Arc::clone(self),
            id,
        }
    }

    /// Cancels every piece of work submitted so far.
    pub fn cancel(self: &Arc<Self>) -> Result<()> {
        let mut state = self.lock();
        if state.submitted == state.finished {
            drop(state);
            // No submitted work in flight: what may be running is the thread's own catalog
            // work, interrupted once. If it lands on nothing, the next statement's start
            // clears it.
            (self.interrupt)();
            return Ok(());
        }
        state.cancelled_up_to = state.submitted;
        if state.interrupter || state.closed {
            return Ok(());
        }
        state.interrupter = true;
        drop(state);
        let canceller = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("cancel".into())
            .spawn(move || canceller.interrupt_until_done());
        if let Err(e) = spawned {
            self.lock().interrupter = false;
            // Work that has not started still never starts; only a running one may go on.
            (self.interrupt)();
            return Err(DriverError::Other(format!(
                "could not start cancelling the statement: {e}"
            )));
        }
        Ok(())
    }

    /// Interrupts the running cancelled work until every cancelled piece has finished.
    fn interrupt_until_done(&self) {
        let mut state = self.lock();
        loop {
            if state.closed || state.finished >= state.cancelled_up_to {
                state.interrupter = false;
                return;
            }
            if state.running.is_some_and(|id| id <= state.cancelled_up_to) {
                // Under the lock: the thread cannot finish this work and start the next one
                // while the interrupt is on its way.
                (self.interrupt)();
            }
            state = self
                .changed
                .wait_timeout(state, REINTERRUPT_EVERY)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The connection's thread has ended: see [`Closer`].
    fn close(&self) {
        self.lock().closed = true;
        self.changed.notify_all();
    }
}

/// A piece of work the connection's thread is running.
pub struct Work {
    canceller: Arc<Canceller>,
    id: u64,
}

impl Work {
    /// Whether a cancel reached this work.
    pub fn is_cancelled(&self) -> bool {
        self.id <= self.canceller.lock().cancelled_up_to
    }

    /// `Err(Cancelled)` if a cancel reached this work: checked before it starts and between
    /// the steps where the engine itself would not notice an interrupt.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(DriverError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let mut state = self.canceller.lock();
        state.running = None;
        state.finished = self.id;
        drop(state);
        self.canceller.changed.notify_all();
    }
}

/// Held by the connection's thread: marks it ended when dropped, also when it panics, so an
/// interrupter never waits for work that will not finish.
pub struct Closer(pub Arc<Canceller>);

impl Drop for Closer {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    fn counting() -> (Arc<Canceller>, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&count);
        let canceller = Canceller::new(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        (canceller, count)
    }

    fn submit(canceller: &Canceller) -> u64 {
        let mut given = 0;
        canceller
            .submit(|id| {
                given = id;
                Ok(())
            })
            .unwrap();
        given
    }

    fn wait_for(what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn work_cancelled_while_queued_never_starts() {
        let (canceller, interrupts) = counting();
        let id = submit(&canceller);
        canceller.cancel().unwrap();
        let work = canceller.begin(id);
        assert!(matches!(work.check(), Err(DriverError::Cancelled)));
        drop(work);
        wait_for("the interrupter to stop", || !canceller.lock().interrupter);
        // Nothing of ours was running: the engine was never interrupted.
        assert_eq!(interrupts.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn running_cancelled_work_is_interrupted_until_it_ends() {
        let (canceller, interrupts) = counting();
        let id = submit(&canceller);
        let work = canceller.begin(id);
        assert!(work.check().is_ok());
        canceller.cancel().unwrap();
        // The engine dropped the first interrupts (it was starting): more keep coming.
        wait_for("several interrupts", || {
            interrupts.load(Ordering::SeqCst) >= 3
        });
        assert!(work.is_cancelled());
        drop(work);
        wait_for("the interrupter to stop", || !canceller.lock().interrupter);
        let after = interrupts.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            interrupts.load(Ordering::SeqCst),
            after,
            "no interrupt after the end"
        );
    }

    #[test]
    fn a_cancel_never_reaches_later_work() {
        let (canceller, interrupts) = counting();
        // Nothing submitted: one interrupt, as before, and nothing remembered.
        canceller.cancel().unwrap();
        assert_eq!(interrupts.load(Ordering::SeqCst), 1);
        let id = submit(&canceller);
        let work = canceller.begin(id);
        assert!(work.check().is_ok());
        drop(work);
        // A cancel while one runs, then one submitted after it.
        let first = submit(&canceller);
        let running = canceller.begin(first);
        canceller.cancel().unwrap();
        let second = submit(&canceller);
        drop(running);
        let next = canceller.begin(second);
        assert!(next.check().is_ok(), "the later work was cancelled");
        let during = interrupts.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            interrupts.load(Ordering::SeqCst),
            during,
            "the later work was interrupted"
        );
        drop(next);
    }

    #[test]
    fn a_cancel_reaches_every_piece_submitted_before_it() {
        let (canceller, _) = counting();
        let ids: Vec<u64> = (0..3).map(|_| submit(&canceller)).collect();
        canceller.cancel().unwrap();
        for id in ids {
            assert!(canceller.begin(id).is_cancelled(), "work {id}");
        }
        wait_for("the interrupter to stop", || !canceller.lock().interrupter);
    }

    #[test]
    fn the_interrupter_stops_when_the_thread_ends() {
        let (canceller, _) = counting();
        let closer = Closer(Arc::clone(&canceller));
        submit(&canceller);
        submit(&canceller);
        canceller.cancel().unwrap();
        // The thread ends without finishing them (it panicked, say).
        drop(closer);
        wait_for("the interrupter to stop", || !canceller.lock().interrupter);
        // Later cancels do not start another.
        canceller.cancel().unwrap();
        assert!(!canceller.lock().interrupter);
    }

    #[test]
    fn a_failed_submit_counts_nothing() {
        let (canceller, interrupts) = counting();
        let failed = canceller.submit(|_| Err(DriverError::Connection("closed".into())));
        assert!(failed.is_err());
        // Nothing in flight: a plain interrupt, no interrupter.
        canceller.cancel().unwrap();
        assert_eq!(interrupts.load(Ordering::SeqCst), 1);
        assert!(!canceller.lock().interrupter);
    }
}
