//! A session's run state through real runs (SDB-370): one test per transition, a cancel and a
//! loss while paused included, and the summary that always says how the run ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_driver_api::{
    CancelHandle, Capabilities, Connection, DriverError, EventStream, ExecEvent, ExecOptions,
    ObjectInfo, Result, SchemaInfo, TableInfo,
};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::{
    CoreEvent, ErrorAnswer, OnError, ResultStore, RunState, ScriptOptions, ScriptStop,
    ScriptSummary, Session, StatementEnd, TransactionError,
};

/// The engine: `ok` finishes, `fail` fails, `wait` runs until the test releases it or a cancel
/// stops it.
#[derive(Default)]
struct Engine {
    release: Notify,
    stop: Notify,
    /// Its cancel is lost on the way: it reaches nothing (SDB-221).
    deaf: std::sync::atomic::AtomicBool,
    cancels: AtomicUsize,
}

struct Fake(Arc<Engine>, Capabilities);

struct Cancel(Arc<Engine>);

#[async_trait]
impl CancelHandle for Cancel {
    async fn cancel(&self) -> Result<()> {
        self.0.cancels.fetch_add(1, Ordering::SeqCst);
        if !self.0.deaf.load(Ordering::SeqCst) {
            self.0.stop.notify_one();
        }
        Ok(())
    }
}

#[async_trait]
impl Connection for Fake {
    fn capabilities(&self) -> &Capabilities {
        &self.1
    }
    async fn execute(&self, sql: &str, _opts: &ExecOptions) -> Result<EventStream> {
        let done = || {
            Ok(ExecEvent::Done {
                rows_affected: None,
            })
        };
        match sql.trim() {
            "ok" => Ok(Box::pin(futures_util::stream::iter([done()]))),
            "fail" => Err(DriverError::Database {
                code: None,
                message: "it failed".into(),
                detail: None,
                hint: None,
                position: None,
            }),
            "wait" => {
                let engine = self.0.clone();
                Ok(Box::pin(futures_util::stream::once(async move {
                    tokio::select! {
                        () = engine.release.notified() => done(),
                        () = engine.stop.notified() => Err(DriverError::Cancelled),
                    }
                })))
            }
            other => panic!("the fake engine does not know {other:?}"),
        }
    }
    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(Cancel(self.0.clone()))
    }
    async fn server_version(&self) -> Result<String> {
        Ok("test".into())
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

fn session() -> (Session, Arc<Engine>) {
    let engine = Arc::new(Engine::default());
    let fake = Fake(
        engine.clone(),
        Capabilities {
            transactions: false,
            cancel: true,
            explain: false,
            editable_results: false,
            schemas: false,
            insert_rows: false,
            bound_params: false,
            insert_bytes: false,
        },
    );
    let session = Session::new(
        Box::new(fake),
        spelaion_dialect::for_engine("sqlite").expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    (session, engine)
}

const LIMIT: Duration = Duration::from_secs(5);

fn start(session: &Session, script: &str, on_error: OnError) -> JoinHandle<Vec<CoreEvent>> {
    let run = session.run_script_with(
        script,
        ScriptOptions {
            on_error,
            statement_timeout: None,
            statement_savepoints: false,
        },
    );
    tokio::spawn(run.collect::<Vec<_>>())
}

/// Waits until the session's run state is `want`, and returns it.
async fn reaches(session: &Session, want: impl Fn(&RunState) -> bool) -> RunState {
    let mut state = session.run_state();
    *tokio::time::timeout(LIMIT, state.wait_for(want))
        .await
        .expect("the run state came")
        .expect("the session is there")
}

async fn ended(run: JoinHandle<Vec<CoreEvent>>) -> (Vec<CoreEvent>, ScriptSummary) {
    let events = tokio::time::timeout(LIMIT, run)
        .await
        .expect("the run ended")
        .expect("the run did not panic");
    let summary = match events.last() {
        Some(CoreEvent::ScriptFinished {
            executed,
            failed,
            summary,
        }) => {
            assert_eq!(*executed, summary.executed, "said once, by the summary");
            assert_eq!(*failed, summary.failed > 0, "said once, by the summary");
            *summary
        }
        other => panic!("the run finished: {other:?}"),
    };
    (events, summary)
}

fn failure(events: &[CoreEvent], at: usize) -> crate::StatementError {
    events
        .iter()
        .find_map(|event| match event {
            CoreEvent::StatementFailed { index, error } if *index == at => Some(error.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("statement {at} failed: {events:?}"))
}

#[tokio::test]
async fn a_run_is_running_its_statement_and_idle_once_it_has_finished() {
    let (session, engine) = session();
    assert_eq!(session.current_run_state(), RunState::Idle);
    let run = start(&session, "ok; wait; ok", OnError::Stop);
    reaches(&session, |s| *s == RunState::Running { statement: 1 }).await;
    engine.release.notify_one();
    let (events, summary) = ended(run).await;
    assert_eq!(summary.stop, None, "it went through every statement");
    assert_eq!(summary.succeeded, 3);
    // Idle before anyone heard the run ended (SDB-98).
    assert_eq!(session.current_run_state(), RunState::Idle);
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished { .. })
    ));
}

#[tokio::test]
async fn a_paused_run_says_its_statement_and_ticket_and_goes_on_when_answered() {
    let (session, engine) = session();
    let run = start(&session, "ok; fail; wait; ok", OnError::Ask);
    let paused = reaches(&session, |s| matches!(s, RunState::Paused { .. })).await;
    let RunState::Paused {
        statement,
        ticket,
        remaining,
    } = paused
    else {
        unreachable!()
    };
    assert_eq!((statement, remaining), (1, 2));
    // Answered with the ticket the state shows, as the page does (ADR-035).
    session.answer_error(ticket, ErrorAnswer::Skip).unwrap();
    reaches(&session, |s| *s == RunState::Running { statement: 2 }).await;
    engine.release.notify_one();
    let (events, summary) = ended(run).await;
    let shown = events.iter().find_map(|event| match event {
        CoreEvent::ErrorPaused { ticket, .. } => Some(*ticket),
        _ => None,
    });
    assert_eq!(
        shown,
        Some(ticket),
        "the state and the event name one pause"
    );
    assert_eq!(summary.stop, None);
    assert_eq!((summary.succeeded, summary.failed), (3, 1));
    assert_eq!(session.current_run_state(), RunState::Idle);
}

#[tokio::test]
async fn a_cancel_shows_as_cancelling_until_the_run_ends_even_when_the_engine_never_gets_it() {
    let (session, engine) = session();
    engine.deaf.store(true, Ordering::SeqCst);
    let run = start(&session, "wait; ok", OnError::Continue);
    reaches(&session, |s| *s == RunState::Running { statement: 0 }).await;
    session.cancel().await.unwrap();
    assert_eq!(engine.cancels.load(Ordering::SeqCst), 1);
    assert_eq!(
        session.current_run_state(),
        RunState::Cancelling { statement: 0 },
        "not a slow statement: a cancel on its way"
    );
    // The statement ends on its own; the run stops before the next one.
    engine.release.notify_one();
    let (_, summary) = ended(run).await;
    assert_eq!(summary.stop, Some(ScriptStop::Cancelled { index: 1 }));
    assert_eq!(session.current_run_state(), RunState::Idle);
}

#[tokio::test]
async fn a_cancel_while_paused_stops_the_run_at_that_statement() {
    let (session, _engine) = session();
    let run = start(&session, "fail; ok", OnError::Ask);
    reaches(&session, |s| matches!(s, RunState::Paused { .. })).await;
    session.cancel().await.unwrap();
    let (_, summary) = ended(run).await;
    assert_eq!(summary.stop, Some(ScriptStop::Cancelled { index: 0 }));
    assert_eq!(summary.not_run(), 1);
    assert_eq!(session.current_run_state(), RunState::Idle);
}

#[tokio::test]
async fn a_loss_while_paused_is_told_apart_from_a_closed_tab() {
    let (session, _engine) = session();
    let run = start(&session, "ok; fail; ok", OnError::Ask);
    reaches(&session, |s| matches!(s, RunState::Paused { .. })).await;
    session.mark_lost("the server went away");
    let (_, summary) = ended(run).await;
    assert_eq!(summary.stop, Some(ScriptStop::SessionLost { index: 1 }));
    assert_eq!(session.current_run_state(), RunState::Idle);

    let (session, _engine) = self::session();
    let run = start(&session, "ok; fail; ok", OnError::Ask);
    reaches(&session, |s| matches!(s, RunState::Paused { .. })).await;
    assert!(session.stop_paused_run());
    let (_, summary) = ended(run).await;
    assert_eq!(summary.stop, Some(ScriptStop::Closed { index: 1 }));
    assert_eq!(session.current_run_state(), RunState::Idle);
}

#[tokio::test]
async fn a_refused_commit_says_what_runs() {
    let (session, engine) = session();
    let run = start(&session, "ok; wait", OnError::Stop);
    reaches(&session, |s| *s == RunState::Running { statement: 1 }).await;
    let refused = session.commit().await.unwrap_err();
    assert_eq!(refused, TransactionError::Busy);
    assert_eq!(
        session.transaction_refusal(&refused),
        "statement 2 is running on this tab: wait for it to finish or cancel it"
    );
    engine.release.notify_one();
    ended(run).await;

    let run = start(&session, "fail; ok", OnError::Ask);
    reaches(&session, |s| matches!(s, RunState::Paused { .. })).await;
    let refused = session.rollback().await.unwrap_err();
    assert!(
        session
            .transaction_refusal(&refused)
            .starts_with("statement 1 failed and the run on this tab waits for your answer"),
        "{}",
        session.transaction_refusal(&refused)
    );
    session.stop_paused_run();
    ended(run).await;
}

#[tokio::test]
async fn the_cores_own_reads_leave_the_session_idle() {
    let (session, engine) = session();
    let mut read = session.run_metadata("wait");
    let reading = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = read.next().await {
            events.push(event);
        }
        events
    });
    // The read holds the session (a run of the user's would wait for it) …
    tokio::time::sleep(Duration::from_millis(50)).await;
    // … but it is not a run the user sees.
    assert_eq!(session.current_run_state(), RunState::Idle);
    engine.release.notify_one();
    tokio::time::timeout(LIMIT, reading).await.unwrap().unwrap();
}

#[tokio::test]
async fn a_run_dropped_halfway_leaves_the_session_idle() {
    let (session, _engine) = session();
    let mut run = session.run_script("ok; wait");
    while let Some(event) = run.next().await {
        if matches!(event, CoreEvent::StatementStarted { index: 1, .. }) {
            break;
        }
    }
    assert_eq!(
        session.current_run_state(),
        RunState::Running { statement: 1 }
    );
    drop(run);
    assert_eq!(session.current_run_state(), RunState::Idle);
}

#[tokio::test]
async fn the_summary_says_how_the_run_ended_also_at_its_last_statement() {
    let (session, engine) = session();
    // Stopped by the failure of its last statement.
    let (events, summary) = ended(start(&session, "ok; fail", OnError::Stop)).await;
    assert_eq!(summary.stop, Some(ScriptStop::Failed { index: 1 }));
    assert_eq!(summary.not_run(), 0);
    assert_eq!(failure(&events, 1).end, StatementEnd::Failed);
    let (_, summary) = ended(start(&session, "ok; fail", OnError::Ask)).await;
    assert_eq!(summary.stop, Some(ScriptStop::Failed { index: 1 }));

    // A run that goes on past errors went through every statement.
    let (_, summary) = ended(start(&session, "fail; fail", OnError::Continue)).await;
    assert_eq!(summary.stop, None);
    assert_eq!(summary.failed, 2);

    // Cancelled at its last statement.
    let run = start(&session, "ok; wait", OnError::Continue);
    reaches(&session, |s| *s == RunState::Running { statement: 1 }).await;
    session.cancel().await.unwrap();
    let (events, summary) = ended(run).await;
    assert_eq!(summary.stop, Some(ScriptStop::Cancelled { index: 1 }));
    let error = failure(&events, 1);
    assert_eq!(error.end, StatementEnd::Cancelled);
    assert!(error.cancelled && !error.timed_out);
    drop(engine);
}

#[tokio::test]
async fn a_statement_that_runs_past_its_timeout_ends_timed_out() {
    let (session, _engine) = session();
    let run = session.run_script_with(
        "wait",
        ScriptOptions {
            on_error: OnError::Stop,
            statement_timeout: Some(Duration::from_millis(50)),
            statement_savepoints: false,
        },
    );
    let events = tokio::time::timeout(LIMIT, run.collect::<Vec<_>>())
        .await
        .expect("the timeout cancelled it");
    let error = failure(&events, 0);
    assert_eq!(error.end, StatementEnd::TimedOut);
    assert!(error.timed_out && !error.cancelled);
}
