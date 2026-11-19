//! How a run leaves its stored results (SDB-367): whole when the statement finished, and cut
//! short with the reason when it was cancelled, failed, timed out or lost its connection. The
//! rows that arrived stay kept either way (SDB-275).

use std::sync::Arc;
use std::time::Duration;

use arrow::array::{Int64Array, RecordBatch};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use spelaion_driver_api::{
    CancelHandle, Capabilities, ColumnMeta, Connection, DriverError, EventStream, ExecEvent,
    ExecOptions, LogicalType, ObjectInfo, Result, ResultSchema, SchemaInfo, TableInfo,
};
use tokio::sync::Notify;

use crate::{
    CoreEvent, PartialReason, ResultEnd, ResultFreed, ResultId, ResultStore, ScriptOptions, Session,
};

/// How the fake statement's stream ends after its rows.
#[derive(Clone)]
enum Ending {
    /// The engine finished it.
    Done,
    /// The engine reports an error halfway.
    Fails(fn() -> DriverError),
    /// The stream stops with neither `Done` nor an error (the driver's actor stopped).
    Stops,
    /// Code the run polls panics after the rows (SDB-163).
    Panics,
    /// The engine keeps it running until it is cancelled, then answers the cancel with an
    /// error, as PostgreSQL does.
    UntilCancelled,
}

/// One statement: a column, `rows` rows in two batches, then `ending`.
struct Scripted {
    capabilities: Capabilities,
    rows: i64,
    ending: Ending,
    cancelled: Arc<Notify>,
}

struct Cancel(Arc<Notify>);

#[async_trait]
impl CancelHandle for Cancel {
    async fn cancel(&self) -> Result<()> {
        self.0.notify_one();
        Ok(())
    }
}

fn schema() -> Arc<ResultSchema> {
    Arc::new(ResultSchema::new(vec![ColumnMeta {
        name: "n".into(),
        logical: LogicalType::Int,
        native_type: "int8".into(),
        native_id: None,
        origin: None,
    }]))
}

fn batch(schema: &ResultSchema, from: i64, to: i64) -> RecordBatch {
    RecordBatch::try_new(
        schema.arrow.clone(),
        vec![Arc::new(Int64Array::from_iter_values(from..to))],
    )
    .unwrap()
}

#[async_trait]
impl Connection for Scripted {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }
    async fn execute(&self, _sql: &str, _opts: &ExecOptions) -> Result<EventStream> {
        let schema = schema();
        let half = self.rows / 2;
        let rows: Vec<Result<ExecEvent>> = vec![
            Ok(ExecEvent::Columns(schema.clone())),
            Ok(ExecEvent::Batch(batch(&schema, 0, half))),
            Ok(ExecEvent::Batch(batch(&schema, half, self.rows))),
        ];
        let rows = futures_util::stream::iter(rows);
        let stream: EventStream = match self.ending.clone() {
            Ending::Done => {
                Box::pin(rows.chain(futures_util::stream::iter([Ok(ExecEvent::Done {
                    rows_affected: None,
                })])))
            }
            Ending::Fails(error) => {
                Box::pin(rows.chain(futures_util::stream::iter([Err(error())])))
            }
            Ending::Stops => Box::pin(rows),
            Ending::Panics => Box::pin(rows.chain(futures_util::stream::poll_fn(
                |_| -> std::task::Poll<Option<Result<ExecEvent>>> {
                    panic!("the scripted driver panicked")
                },
            ))),
            Ending::UntilCancelled => {
                let cancelled = self.cancelled.clone();
                Box::pin(rows.chain(futures_util::stream::once(async move {
                    cancelled.notified().await;
                    Err(DriverError::Cancelled)
                })))
            }
        };
        Ok(stream)
    }
    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(Cancel(self.cancelled.clone()))
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

fn session(rows: i64, ending: Ending) -> (Session, Arc<ResultStore>) {
    let store = Arc::new(ResultStore::new());
    let connection = Scripted {
        capabilities: Capabilities {
            transactions: false,
            cancel: true,
            explain: false,
            editable_results: false,
            schemas: false,
            insert_rows: false,
            bound_params: false,
        },
        rows,
        ending,
        cancelled: Arc::new(Notify::new()),
    };
    let session = Session::new(
        Box::new(connection),
        spelaion_dialect::for_engine("sqlite").expect("a dialect"),
        store.clone(),
    );
    (session, store)
}

/// What a run said about its one result, and the rows it kept.
struct Ran {
    result: ResultId,
    /// Every `ResultEnded` the run sent, in order.
    ended: Vec<ResultEnd>,
    /// The `ResultEnded` came before the statement's own end.
    ended_first: bool,
    end: ResultEnd,
    rows: usize,
}

async fn drain(store: &ResultStore, events: Vec<CoreEvent>) -> Ran {
    let mut result = None;
    let mut ended = Vec::new();
    let mut ended_first = false;
    for event in events {
        match event {
            CoreEvent::ResultStarted { result: id, .. } => result = Some(id),
            CoreEvent::ResultEnded {
                result: id, end, ..
            } => {
                assert_eq!(Some(id), result);
                ended.push(end);
            }
            CoreEvent::StatementFinished { .. } | CoreEvent::StatementFailed { .. } => {
                ended_first = !ended.is_empty();
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    let result = result.expect("the statement started a result");
    let stored = store.stored(result).expect("the result is kept");
    let stored = stored.read().unwrap();
    Ran {
        result,
        ended,
        ended_first,
        end: stored.end(),
        rows: stored.rows(),
    }
}

async fn run(rows: i64, ending: Ending, options: ScriptOptions) -> Ran {
    let (session, store) = session(rows, ending);
    let events = session
        .run_script_with("select n from t", options)
        .collect()
        .await;
    drain(&store, events).await
}

fn check(ran: &Ran, end: ResultEnd, rows: usize) {
    assert_eq!(ran.end, end);
    assert_eq!(ran.ended, [end], "one ResultEnded, with the stored end");
    assert!(
        ran.ended_first,
        "ResultEnded comes before the statement's end"
    );
    assert_eq!(ran.rows, rows, "the rows that arrived are kept");
}

#[tokio::test]
async fn a_finished_statement_leaves_a_whole_result() {
    let ran = run(10, Ending::Done, ScriptOptions::default()).await;
    check(&ran, ResultEnd::Whole, 10);
}

#[tokio::test]
async fn a_failed_statement_leaves_a_partial_result_that_says_it_failed() {
    let ran = run(
        10,
        Ending::Fails(|| DriverError::Other("division by zero".into())),
        ScriptOptions::default(),
    )
    .await;
    check(&ran, ResultEnd::Partial(PartialReason::Failed), 10);
}

#[tokio::test]
async fn a_lost_connection_leaves_a_partial_result_that_says_so() {
    let ran = run(
        10,
        Ending::Fails(|| DriverError::Connection("reset by peer".into())),
        ScriptOptions::default(),
    )
    .await;
    check(&ran, ResultEnd::Partial(PartialReason::ConnectionLost), 10);
    // A stream that stops with neither `Done` nor an error is a connection that stopped too.
    let ran = run(10, Ending::Stops, ScriptOptions::default()).await;
    check(&ran, ResultEnd::Partial(PartialReason::ConnectionLost), 10);
    // A run that panics loses its session, and its result does not stay loading.
    let ran = run(10, Ending::Panics, ScriptOptions::default()).await;
    check(&ran, ResultEnd::Partial(PartialReason::ConnectionLost), 10);
}

#[tokio::test]
async fn a_cancelled_statement_leaves_a_partial_result_that_says_it_was_cancelled() {
    let (session, store) = session(120, Ending::UntilCancelled);
    let mut run: BoxStream<'static, CoreEvent> = session.run_script("select n from t");
    let mut events = Vec::new();
    // Every row it will send has arrived; the statement still runs.
    while let Some(event) = run.next().await {
        let all_rows = matches!(
            event,
            CoreEvent::Rows {
                total_rows: 120,
                ..
            }
        );
        events.push(event);
        if all_rows {
            break;
        }
    }
    session.cancel().await.unwrap();
    events.extend(run.collect::<Vec<_>>().await);
    let ran = drain(&store, events).await;
    check(&ran, ResultEnd::Partial(PartialReason::Cancelled), 120);
}

#[tokio::test]
async fn a_timed_out_statement_leaves_a_partial_result_that_says_it_timed_out() {
    let ran = run(
        10,
        Ending::UntilCancelled,
        ScriptOptions {
            statement_timeout: Some(Duration::from_millis(50)),
            ..ScriptOptions::default()
        },
    )
    .await;
    check(&ran, ResultEnd::Partial(PartialReason::TimedOut), 10);
}

#[tokio::test]
async fn a_partial_result_is_still_sorted_and_filtered_as_the_rows_it_has() {
    let (session, store) = session(10, Ending::Fails(|| DriverError::Other("boom".into())));
    let events = session.run_script("select n from t").collect().await;
    let ran = drain(&store, events).await;
    assert_eq!(ran.end.partial(), Some(PartialReason::Failed));
    let stored = store.stored(ran.result).unwrap();
    let mut stored = stored.write().unwrap();
    let down = crate::SortKey {
        column: 0,
        descending: true,
    };
    assert_eq!(stored.sort(&[down]), Ok(()));
    assert_eq!(stored.filter(Some("1")), Ok(1));
    // The rows themselves and how the result ended do not change.
    assert_eq!(stored.total_rows(), 10);
    assert_eq!(stored.end(), ResultEnd::Partial(PartialReason::Failed));
}

#[test]
fn a_freed_result_is_one_typed_error() {
    let store = ResultStore::new();
    assert_eq!(store.stored(ResultId(42)).err(), Some(ResultFreed));
    assert_eq!(
        ResultFreed.to_string(),
        "the result was freed: run its query again"
    );
}

/// Only finished, failed and cut-short results end: a loading one has not.
#[test]
fn only_a_loading_result_has_not_ended() {
    assert!(!ResultEnd::Loading.has_ended());
    assert!(ResultEnd::Whole.has_ended());
    for reason in [
        PartialReason::Cancelled,
        PartialReason::Failed,
        PartialReason::TimedOut,
        PartialReason::ConnectionLost,
    ] {
        assert!(ResultEnd::Partial(reason).has_ended());
        assert_eq!(ResultEnd::Partial(reason).partial(), Some(reason));
    }
    assert_eq!(PartialReason::TimedOut.code(), "timed_out");
}

/// The same through a real driver: an endless query in SQLite, cancelled once rows arrive.
#[tokio::test]
async fn a_real_cancelled_query_keeps_its_rows_and_says_it_was_cancelled() {
    let driver = crate::open::test_driver(spelaion_config::Engine::Sqlite);
    let params = spelaion_driver_api::ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let store = Arc::new(ResultStore::new());
    let session = Session::new(
        driver.connect(&params).await.expect("connecting"),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        store.clone(),
    );
    let mut run = session.run_script(
        "with recursive c(n) as (select 1 union all select n + 1 from c) select n from c",
    );
    let mut events = Vec::new();
    while let Some(event) = run.next().await {
        let rows = matches!(event, CoreEvent::Rows { .. });
        events.push(event);
        if rows {
            break;
        }
    }
    session.cancel().await.unwrap();
    events.extend(run.collect::<Vec<_>>().await);
    let ran = drain(&store, events).await;
    assert_eq!(ran.end, ResultEnd::Partial(PartialReason::Cancelled));
    assert_eq!(ran.ended, [ran.end]);
    assert!(ran.rows > 0, "the rows read before the cancel are kept");
}
