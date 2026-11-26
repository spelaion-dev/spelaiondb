//! How a script run behaves like a script runner (ADR-035): what it does when a statement
//! fails, how long a statement may run, and the summary it ends with.
//!
//! The [`OnError::Ask`] mode is a state machine in the core: the run pauses with a ticket and
//! waits for [`crate::Session::answer_error`] with that same ticket. The app only shows the
//! question and sends the answer; it never decides which statements run.

use std::time::Duration;

use tokio::sync::oneshot;

/// What a script run does when a statement fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnError {
    /// The run stops at the failed statement; the rest is not run.
    #[default]
    Stop,
    /// The run goes on with the next statement.
    Continue,
    /// The run pauses and asks: stop, skip this statement, or skip every later error.
    Ask,
}

/// How a script runs. The default is the editor's behaviour before ADR-035: stop at the first
/// error, no timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScriptOptions {
    pub on_error: OnError,
    /// A statement that runs longer is cancelled, and fails as timed out. It counts from the
    /// moment the statement is sent until its last row.
    pub statement_timeout: Option<Duration>,
    /// In a transaction the core opened or the user began, each statement runs after a
    /// savepoint of the core's, rolled back to when the statement fails and released when it
    /// succeeds (ADR-086): a failed statement then leaves the transaction open without it, on
    /// the engines whose errors abort the whole transaction ([`crate::statement_savepoints`]).
    /// Off by default: one more round trip per statement.
    pub statement_savepoints: bool,
}

/// The user's answer to a paused run ([`OnError::Ask`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAnswer {
    /// Stop here: the rest is not run.
    Stop,
    /// Go on with the next statement; a later error asks again.
    Skip,
    /// Go on, and do not ask again in this run.
    SkipAll,
}

/// Identifies one pause: an answer must name the pause it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorTicket(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    #[error("this script is not waiting for an answer to that error any more")]
    NotWaiting,
}

/// Why a script run stopped before going through every statement its mode let it run. A
/// failure or a cancel names the statement it stopped at, also when it was the last one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptStop {
    /// The statement at `index` failed and the run stopped there ([`OnError::Stop`], or Stop
    /// was answered).
    Failed { index: usize },
    /// The user cancelled the run at the statement at `index`.
    Cancelled { index: usize },
    /// The failed statement left its transaction aborted: the engine only rolls it back, so
    /// every later statement would fail too (PostgreSQL, DuckDB). The run stops instead.
    TransactionAborted { index: usize },
    /// The failed statement rolled back the transaction it ran in (SQL Server with
    /// `XACT_ABORT`, a MySQL deadlock): going on would run the rest without what came before.
    TransactionRolledBack { index: usize },
    /// The run was waiting for an answer and its session was closed: its tab closed, it was
    /// dropped, or it was closed after being idle too long (ADR-029).
    Closed { index: usize },
    /// The run was waiting for an answer and its session's connection was lost (ADR-028).
    SessionLost { index: usize },
}

impl ScriptStop {
    /// The statement the run stopped at.
    pub fn index(&self) -> usize {
        match *self {
            ScriptStop::Failed { index }
            | ScriptStop::Cancelled { index }
            | ScriptStop::TransactionAborted { index }
            | ScriptStop::TransactionRolledBack { index }
            | ScriptStop::Closed { index }
            | ScriptStop::SessionLost { index } => index,
        }
    }
}

impl std::fmt::Display for ScriptStop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = self.index() + 1;
        match self {
            ScriptStop::Failed { .. } => write!(f, "stopped at statement {n}, which failed"),
            ScriptStop::Cancelled { .. } => write!(f, "cancelled at statement {n}"),
            ScriptStop::TransactionAborted { .. } => write!(
                f,
                "statement {n} aborted the transaction: the server only rolls it back, so the rest was not run"
            ),
            ScriptStop::TransactionRolledBack { .. } => write!(
                f,
                "statement {n} rolled back the transaction, so the rest was not run"
            ),
            ScriptStop::Closed { .. } => write!(
                f,
                "the session closed while statement {n}'s error waited for an answer"
            ),
            ScriptStop::SessionLost { .. } => write!(
                f,
                "the connection was lost while statement {n}'s error waited for an answer"
            ),
        }
    }
}

/// What a script run did: "N statements: X ok, Y failed", and where its time went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScriptSummary {
    /// Statements in the script.
    pub statements: usize,
    /// Statements sent to the engine.
    pub executed: usize,
    pub succeeded: usize,
    pub failed: usize,
    /// How the run ended: `None` when it went through every statement its mode let it run
    /// (with [`crate::OnError::Continue`], or after Skip, some of them may have failed);
    /// otherwise why it stopped, at which statement, even the last.
    pub stop: Option<ScriptStop>,
    /// Summed over the statements: until the engine's first answer.
    pub execution: Duration,
    /// Summed over the statements: reading the rows after that first answer.
    pub fetch: Duration,
}

impl ScriptSummary {
    /// Statements that were never sent.
    pub fn not_run(&self) -> usize {
        self.statements.saturating_sub(self.executed)
    }
}

/// How a paused run resumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resume {
    Answer(ErrorAnswer),
    Cancelled,
    /// Its session was closed (its tab, dropped, idle).
    Closed,
    /// Its session's connection was lost.
    Lost,
}

struct Waiting {
    ticket: u64,
    reply: oneshot::Sender<Resume>,
}

/// The one run of a session that may wait for an answer, shared by the session and its run.
#[derive(Default)]
pub(crate) struct Pause {
    waiting: std::sync::Mutex<Option<Waiting>>,
    tickets: std::sync::atomic::AtomicU64,
}

impl Pause {
    /// Starts waiting: returns the ticket to show and where the answer arrives.
    ///
    /// Only the run that holds the session pauses, so none waits already. If one did, it is
    /// stopped as closed, never forgotten.
    pub(crate) fn wait(&self) -> (ErrorTicket, oneshot::Receiver<Resume>) {
        let ticket = self
            .tickets
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let (reply, answer) = oneshot::channel();
        let earlier = self
            .waiting
            .lock()
            .expect("lock poisoned")
            .replace(Waiting { ticket, reply });
        if let Some(earlier) = earlier {
            let _ = earlier.reply.send(Resume::Closed);
        }
        (ErrorTicket(ticket), answer)
    }

    pub(crate) fn answer(
        &self,
        ticket: ErrorTicket,
        answer: ErrorAnswer,
    ) -> Result<(), AnswerError> {
        let mut waiting = self.waiting.lock().expect("lock poisoned");
        match waiting.take() {
            Some(w) if w.ticket == ticket.0 => w
                .reply
                .send(Resume::Answer(answer))
                .map_err(|_| AnswerError::NotWaiting),
            other => {
                *waiting = other;
                Err(AnswerError::NotWaiting)
            }
        }
    }

    /// Resumes the waiting run, if any, without an answer. Returns whether one was waiting.
    pub(crate) fn resume(&self, how: Resume) -> bool {
        let waiting = self.waiting.lock().expect("lock poisoned").take();
        waiting.is_some_and(|w| w.reply.send(how).is_ok())
    }

    /// Forgets the pause of `ticket` once its run went on (also when nothing answered it).
    pub(crate) fn clear(&self, ticket: ErrorTicket) {
        let mut waiting = self.waiting.lock().expect("lock poisoned");
        if waiting.as_ref().is_some_and(|w| w.ticket == ticket.0) {
            *waiting = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_the_current_ticket_is_answered() {
        let pause = Pause::default();
        let (first, _old) = pause.wait();
        pause.clear(first);
        let (second, answer) = pause.wait();
        assert_ne!(first, second);
        assert_eq!(
            pause.answer(first, ErrorAnswer::Skip),
            Err(AnswerError::NotWaiting)
        );
        pause.answer(second, ErrorAnswer::SkipAll).unwrap();
        assert_eq!(answer.await.unwrap(), Resume::Answer(ErrorAnswer::SkipAll));
        assert_eq!(
            pause.answer(second, ErrorAnswer::Stop),
            Err(AnswerError::NotWaiting),
            "a ticket answers once"
        );
    }

    #[tokio::test]
    async fn cancelling_or_closing_resumes_without_an_answer() {
        let pause = Pause::default();
        assert!(!pause.resume(Resume::Cancelled), "nothing waits");
        let (ticket, answer) = pause.wait();
        assert!(pause.resume(Resume::Cancelled));
        assert_eq!(answer.await.unwrap(), Resume::Cancelled);
        assert_eq!(
            pause.answer(ticket, ErrorAnswer::Skip),
            Err(AnswerError::NotWaiting)
        );
        let (_, answer) = pause.wait();
        drop(pause);
        assert!(answer.await.is_err(), "a dropped pause stops the run");
    }

    #[tokio::test]
    async fn a_second_pause_stops_the_first_as_closed() {
        let pause = Pause::default();
        let (first, earlier) = pause.wait();
        let (second, _answer) = pause.wait();
        assert_eq!(earlier.await.unwrap(), Resume::Closed);
        assert_eq!(
            pause.answer(first, ErrorAnswer::Skip),
            Err(AnswerError::NotWaiting)
        );
        assert!(pause.answer(second, ErrorAnswer::Skip).is_ok());
    }

    #[test]
    fn a_lost_session_is_told_apart_from_a_closed_one() {
        assert_eq!(
            ScriptStop::SessionLost { index: 1 }.to_string(),
            "the connection was lost while statement 2's error waited for an answer"
        );
        assert_eq!(
            ScriptStop::Closed { index: 1 }.to_string(),
            "the session closed while statement 2's error waited for an answer"
        );
    }

    #[test]
    fn summary_counts_what_was_not_run() {
        let summary = ScriptSummary {
            statements: 5,
            executed: 3,
            succeeded: 2,
            failed: 1,
            stop: Some(ScriptStop::Failed { index: 2 }),
            ..Default::default()
        };
        assert_eq!(summary.not_run(), 2);
        assert_eq!(
            summary.stop.unwrap().to_string(),
            "stopped at statement 3, which failed"
        );
    }
}
