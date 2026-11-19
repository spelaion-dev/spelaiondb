//! A session's run as one state the core publishes (ADR-060, SDB-370): idle, running a
//! statement, paused after an error with its ticket (ADR-035's "ask"), or cancelling.
//!
//! The runs the user sees (their scripts, and the core's generated statements such as a
//! table's page) each hold a [`RunGuard`] from when they ask for the session until they let go
//! of it. [`Runs`] keeps them and the session's cancels, works out the one state they mean in
//! one place ([`Runs::publish`]), and sends it on a watch only when it changes (SDB-110). The
//! core's own short reads (the editor's schema, a catalog) hold the session without a guard:
//! they are not a run the user sees, so they never make the session look busy (SDB-277).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::watch;

use crate::script::ErrorTicket;

/// What a session runs, as the user sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// No run the user sees is going or waiting.
    Idle,
    /// The run's statement at `statement` (0-based) runs, or is the next to. A run that still
    /// waits for the session (for one of the core's reads, or a cancel on its way to the
    /// engine, SDB-266) is about to run its first.
    Running { statement: usize },
    /// The statement at `statement` failed and the run waits for the answer to `ticket`
    /// ([`crate::Session::answer_error`]), with `remaining` statements after it not run.
    Paused {
        statement: usize,
        ticket: ErrorTicket,
        remaining: usize,
    },
    /// A cancel was asked for and the run has not ended yet: it stops at `statement` once the
    /// engine answers. A cancel that the engine never received looks like this until the run
    /// ends, never like a slow statement (SDB-221, SDB-222).
    Cancelling { statement: usize },
}

impl RunState {
    /// A run the user sees is going or waiting: Commit, Rollback and the auto-commit switch
    /// are refused (ADR-031), and closing asks with the last transaction state read.
    pub fn is_busy(&self) -> bool {
        match self {
            RunState::Idle => false,
            RunState::Running { .. } | RunState::Paused { .. } | RunState::Cancelling { .. } => {
                true
            }
        }
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunState::Idle => write!(f, "nothing is running"),
            RunState::Running { statement } => write!(f, "statement {} is running", statement + 1),
            RunState::Paused { statement, .. } => write!(
                f,
                "statement {} failed and the run waits for an answer",
                statement + 1
            ),
            RunState::Cancelling { statement } => write!(
                f,
                "the run is being cancelled at statement {}",
                statement + 1
            ),
        }
    }
}

/// Where one run is, as it tells [`Runs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// It asked for the session and waits for it.
    Waiting,
    Running {
        statement: usize,
    },
    Paused {
        statement: usize,
        ticket: ErrorTicket,
        remaining: usize,
    },
}

impl Phase {
    /// The statement the run is at: a waiting run is at its first.
    fn statement(&self) -> usize {
        match *self {
            Phase::Waiting => 0,
            Phase::Running { statement } | Phase::Paused { statement, .. } => statement,
        }
    }
}

struct Run {
    id: u64,
    /// The session's cancels when the run was asked for: a later cancel stops it.
    cancels_at_start: u64,
    phase: Phase,
}

/// A session's runs and cancels, and the state they mean.
pub(crate) struct Runs {
    /// How many times the user asked to cancel: a run that sees it change stops.
    cancels: AtomicU64,
    runs: Mutex<Vec<Run>>,
    next_id: AtomicU64,
    state: watch::Sender<RunState>,
}

impl Default for Runs {
    fn default() -> Self {
        Self {
            cancels: AtomicU64::new(0),
            runs: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
            state: watch::Sender::new(RunState::Idle),
        }
    }
}

impl Runs {
    pub(crate) fn subscribe(&self) -> watch::Receiver<RunState> {
        self.state.subscribe()
    }

    pub(crate) fn state(&self) -> RunState {
        *self.state.borrow()
    }

    /// The cancels so far: a run asked for now compares against it ([`Runs::cancelled_since`]).
    pub(crate) fn cancels(&self) -> u64 {
        self.cancels.load(Ordering::SeqCst)
    }

    /// Whether a cancel came after `cancels_at_start` ([`Runs::cancels`]).
    pub(crate) fn cancelled_since(&self, cancels_at_start: u64) -> bool {
        self.cancels() != cancels_at_start
    }

    /// Counts a cancel: every run asked for before it stops, and shows as cancelling until it
    /// has.
    pub(crate) fn cancel(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        self.publish();
    }

    /// A run the user sees asks for the session. It counts as running from now until the
    /// guard is dropped, also while it waits for the session (SDB-277).
    pub(crate) fn begin(self: &std::sync::Arc<Self>, cancels_at_start: u64) -> RunGuard {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.runs.lock().expect("lock poisoned").push(Run {
            id,
            cancels_at_start,
            phase: Phase::Waiting,
        });
        self.publish();
        RunGuard {
            runs: self.clone(),
            id,
        }
    }

    fn set(&self, id: u64, phase: Phase) {
        if let Some(run) = self
            .runs
            .lock()
            .expect("lock poisoned")
            .iter_mut()
            .find(|run| run.id == id)
        {
            run.phase = phase;
        }
        self.publish();
    }

    fn end(&self, id: u64) {
        self.runs
            .lock()
            .expect("lock poisoned")
            .retain(|run| run.id != id);
        self.publish();
    }

    /// The one place the state is worked out. The run that holds the session (the one past
    /// waiting: only one at a time can be) is the session's state; otherwise the first that
    /// waits. A run a cancel came for is cancelling, whatever it was doing. Sent only when it
    /// changes.
    fn publish(&self) {
        let runs = self.runs.lock().expect("lock poisoned");
        let shown = runs
            .iter()
            .find(|run| run.phase != Phase::Waiting)
            .or_else(|| runs.first());
        let state = match shown {
            None => RunState::Idle,
            Some(run) if self.cancelled_since(run.cancels_at_start) => RunState::Cancelling {
                statement: run.phase.statement(),
            },
            Some(run) => match run.phase {
                Phase::Waiting => RunState::Running { statement: 0 },
                Phase::Running { statement } => RunState::Running { statement },
                Phase::Paused {
                    statement,
                    ticket,
                    remaining,
                } => RunState::Paused {
                    statement,
                    ticket,
                    remaining,
                },
            },
        };
        // Under the lock, so two changes are never sent in the wrong order.
        self.state.send_if_modified(|current| {
            let changed = *current != state;
            *current = state;
            changed
        });
    }
}

/// One run the user sees, from when it asks for the session until it lets go of it or is
/// dropped (a stream dropped halfway, a run that panicked): the session is idle again then.
pub(crate) struct RunGuard {
    runs: std::sync::Arc<Runs>,
    id: u64,
}

impl RunGuard {
    /// The run starts the statement at `statement`.
    pub(crate) fn running(&self, statement: usize) {
        self.runs.set(self.id, Phase::Running { statement });
    }

    /// The statement at `statement` failed and the run waits for the answer to `ticket`.
    pub(crate) fn paused(&self, statement: usize, ticket: ErrorTicket, remaining: usize) {
        self.runs.set(
            self.id,
            Phase::Paused {
                statement,
                ticket,
                remaining,
            },
        );
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.runs.end(self.id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_run_goes_from_idle_through_its_statements_and_back() {
        let runs = Arc::new(Runs::default());
        let mut seen = runs.subscribe();
        assert_eq!(runs.state(), RunState::Idle);
        assert!(!runs.state().is_busy());

        let run = runs.begin(runs.cancels());
        assert_eq!(runs.state(), RunState::Running { statement: 0 });
        assert!(runs.state().is_busy());
        run.running(0);
        run.running(1);
        assert_eq!(runs.state(), RunState::Running { statement: 1 });
        run.paused(1, ErrorTicket(7), 3);
        assert_eq!(
            runs.state(),
            RunState::Paused {
                statement: 1,
                ticket: ErrorTicket(7),
                remaining: 3
            }
        );
        run.running(2);
        assert_eq!(runs.state(), RunState::Running { statement: 2 });
        drop(run);
        assert_eq!(runs.state(), RunState::Idle);
        assert!(seen.has_changed().unwrap());
        seen.mark_unchanged();

        // Setting what is already there sends nothing (SDB-110).
        let run = runs.begin(runs.cancels());
        seen.mark_unchanged();
        run.running(0);
        assert!(!seen.has_changed().unwrap(), "already running statement 1");
    }

    #[test]
    fn a_cancel_shows_until_the_run_ends_and_only_for_runs_asked_for_before_it() {
        let runs = Arc::new(Runs::default());
        // A cancel with nothing running changes nothing.
        runs.cancel();
        assert_eq!(runs.state(), RunState::Idle);

        let run = runs.begin(runs.cancels());
        run.running(2);
        runs.cancel();
        assert_eq!(runs.state(), RunState::Cancelling { statement: 2 });
        // Whatever the run does until it ends, it is cancelling.
        run.running(3);
        assert_eq!(runs.state(), RunState::Cancelling { statement: 3 });
        drop(run);
        assert_eq!(runs.state(), RunState::Idle);

        // A run asked for after the cancel runs.
        let run = runs.begin(runs.cancels());
        run.running(0);
        assert_eq!(runs.state(), RunState::Running { statement: 0 });
    }

    #[test]
    fn a_cancel_while_paused_or_waiting_is_cancelling_at_that_statement() {
        let runs = Arc::new(Runs::default());
        let run = runs.begin(runs.cancels());
        run.paused(4, ErrorTicket(1), 1);
        runs.cancel();
        assert_eq!(runs.state(), RunState::Cancelling { statement: 4 });
        drop(run);

        let waiting = runs.begin(runs.cancels());
        runs.cancel();
        assert_eq!(runs.state(), RunState::Cancelling { statement: 0 });
        drop(waiting);
        assert_eq!(runs.state(), RunState::Idle);
    }

    #[test]
    fn the_run_holding_the_session_is_shown_before_one_that_waits() {
        let runs = Arc::new(Runs::default());
        let first = runs.begin(runs.cancels());
        first.running(5);
        let second = runs.begin(runs.cancels());
        assert_eq!(runs.state(), RunState::Running { statement: 5 });
        drop(first);
        // The second still waits: the session stays busy.
        assert_eq!(runs.state(), RunState::Running { statement: 0 });
        second.running(0);
        drop(second);
        assert_eq!(runs.state(), RunState::Idle);
    }

    #[test]
    fn the_state_says_what_runs() {
        assert_eq!(
            RunState::Running { statement: 2 }.to_string(),
            "statement 3 is running"
        );
        assert_eq!(
            RunState::Paused {
                statement: 0,
                ticket: ErrorTicket(1),
                remaining: 2
            }
            .to_string(),
            "statement 1 failed and the run waits for an answer"
        );
        assert_eq!(
            RunState::Cancelling { statement: 1 }.to_string(),
            "the run is being cancelled at statement 2"
        );
    }
}
