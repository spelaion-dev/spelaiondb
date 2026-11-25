//! A run whose connection panics ends as an error (SDB-163), never as a tab that waits for the
//! end of the run or as a statement that "finished" with half its rows. A fake connection makes
//! the two ways it happens certain: the driver's actor dies in the middle of a statement (its
//! thread panicked: the stream ends without `Done`), and code polled by the run panics.
//!
//! Before: the first ended as a successful statement (the rows it had were the result); the
//! second unwound into whoever was reading the run, and the app's window never heard the end of
//! the run.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session, SessionHealth};
use spelaion_driver_api::{
    CancelHandle, Capabilities, Connection, DriverError, EventStream, ExecEvent, ExecOptions,
    Notice, ObjectInfo, Result, SchemaInfo, TableInfo,
};

struct Fake {
    capabilities: Capabilities,
}

struct NoCancel;

#[async_trait]
impl CancelHandle for NoCancel {
    async fn cancel(&self) -> Result<()> {
        Ok(())
    }
}

fn notice() -> Result<ExecEvent> {
    Ok(ExecEvent::Notice(Notice {
        severity: "NOTICE".into(),
        message: "working".into(),
    }))
}

#[async_trait]
impl Connection for Fake {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, _opts: &ExecOptions) -> Result<EventStream> {
        let stream: EventStream = match sql {
            // The actor's thread panicked: its channel closes before `Done`.
            "cut" => Box::pin(futures_util::stream::iter([notice()])),
            // The stream itself panics while it is read.
            "boom" => Box::pin(futures_util::stream::iter([notice()]).chain(
                futures_util::stream::poll_fn(|_| -> std::task::Poll<Option<Result<ExecEvent>>> {
                    panic!("the fake driver panicked")
                }),
            )),
            _ => Box::pin(futures_util::stream::iter([Ok(ExecEvent::Done {
                rows_affected: None,
            })])),
        };
        Ok(stream)
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(NoCancel)
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

fn session() -> Session {
    Session::new(
        Box::new(Fake {
            capabilities: Capabilities {
                transactions: false,
                cancel: false,
                explain: false,
                editable_results: false,
                schemas: false,
                insert_rows: false,
                bound_params: false,
                insert_bytes: false,
            },
        }),
        Arc::new(spelaion_dialect::SqliteDialect),
        Arc::new(ResultStore::new()),
    )
}

/// Runs `script` to its end, which must come within seconds.
async fn run(session: &Session, script: &str) -> Vec<CoreEvent> {
    tokio::time::timeout(
        Duration::from_secs(10),
        session.run_script(script).collect::<Vec<_>>(),
    )
    .await
    .expect("the run must end, not wait")
}

fn assert_ends_as_an_internal_error(events: &[CoreEvent]) {
    let failure = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::StatementFailed { index: 1, error } => Some(error.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the second statement failed: {events:?}"));
    assert!(failure.message.contains("internal error"), "{failure:?}");
    assert!(
        !failure.message.contains("fake driver panicked"),
        "{failure:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            CoreEvent::StatementFinished { index: 1, .. }
                | CoreEvent::StatementStarted { index: 2, .. }
        )),
        "nothing after it: {events:?}"
    );
    match events.last() {
        Some(CoreEvent::ScriptFinished {
            failed: true,
            summary,
            ..
        }) => assert_eq!(summary.executed, 2, "{summary:?}"),
        other => panic!("the run finished: {other:?}"),
    }
}

#[tokio::test]
async fn a_statement_whose_driver_stops_halfway_fails_and_the_session_is_lost() {
    let _turn = crate::serial::file_turn(module_path!());
    let session = session();
    let events = run(&session, "first; cut; third").await;
    assert_ends_as_an_internal_error(&events);
    assert!(matches!(
        &*session.health().borrow(),
        SessionHealth::Lost { reason } if reason.contains("internal error")
    ));
    // The app goes on: another session works.
    let other = session_runs_fine().await;
    assert!(other);
}

#[tokio::test]
async fn a_run_that_panics_ends_with_an_error_and_the_session_is_lost() {
    let _turn = crate::serial::file_turn(module_path!());
    let session = session();
    let events = run(&session, "first; boom; third").await;
    assert_ends_as_an_internal_error(&events);
    assert!(matches!(
        &*session.health().borrow(),
        SessionHealth::Lost { reason } if reason.contains("internal error")
    ));
    assert!(session_runs_fine().await);
}

async fn session_runs_fine() -> bool {
    let events = run(&session(), "first; second").await;
    matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished { failed: false, .. })
    )
}
