//! The SQL Server plan protocol is one actor request: no other request may observe its SETs.
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arrow::array::{RecordBatch, StringArray};
use futures_util::TryStreamExt;
use spelaion_driver_api::{DriverError, ExecEvent, PlanMode, Result, ResultSchema};
use tiberius::{ColumnType, QueryItem};
use tokio::sync::{mpsc, watch};

use super::{SqlClient, decode, info_notices, map_error, rows, run_batch};

const COLUMN: &str = "Microsoft SQL Server 2005 XML Showplan";
// The core enforces the same limit for plans opened from a file or another driver.
const MAX_XML_BYTES: usize = 8 * 1024 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

enum Wait<T> {
    Done(T),
    Cancelled(bool),
    Abandoned(bool),
}

async fn cancelled(cancels: &mut watch::Receiver<u64>, epoch: u64) {
    loop {
        if *cancels.borrow() != epoch {
            return;
        }
        if cancels.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn wait<T>(
    future: impl Future<Output = T>,
    cancels: &mut watch::Receiver<u64>,
    epoch: u64,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> Wait<T> {
    tokio::pin!(future);
    let polled = AtomicBool::new(false);
    let observed = poll_fn(|cx| {
        polled.store(true, Ordering::Relaxed);
        future.as_mut().poll(cx)
    });
    tokio::pin!(observed);
    tokio::select! {
        biased;
        _ = cancelled(cancels, epoch) => Wait::Cancelled(polled.load(Ordering::Relaxed)),
        _ = events.closed() => Wait::Abandoned(polled.load(Ordering::Relaxed)),
        result = &mut observed => Wait::Done(result),
    }
}

/// Attention drains the interrupted batch before another batch can be sent.
async fn interrupt(client: &mut SqlClient) -> bool {
    matches!(
        tokio::time::timeout(CLEANUP_TIMEOUT, client.cancel_query()).await,
        Ok(Ok(()))
    )
}

/// Refuse pre-existing plan modes rather than turn off an option the user set. With any
/// SHOWPLAN mode on DBCC is itself compiled; its result is not the two-column options table.
async fn check_options(client: &mut SqlClient) -> Result<()> {
    let options = rows(client, "dbcc useroptions with no_infomsgs", &[]).await?;
    if options.is_empty()
        || options.iter().any(|row| row.len() != 2)
        || !options.iter().any(|row| {
            row[0]
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("textsize"))
        })
    {
        return Err(DriverError::Other(
            "a SHOWPLAN mode appears to be active; turn it off before opening a visual plan".into(),
        ));
    }
    if options.iter().any(|row| {
        row[0]
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("statistics xml"))
            && row[1]
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("set"))
    }) {
        return Err(DriverError::Other(
            "STATISTICS XML is already active; turn it off before opening a visual plan".into(),
        ));
    }
    Ok(())
}

enum CollectError {
    Driver(DriverError),
    TooLarge,
}

/// Discard ordinary result rows as they arrive. A SELECT must have a result set before its
/// appended plan, so a user column with the same name cannot impersonate a missing plan.
async fn collect(
    client: &mut SqlClient,
    sql: &str,
    mode: PlanMode,
) -> std::result::Result<String, CollectError> {
    let mut stream = client
        .simple_query(sql)
        .await
        .map_err(|e| CollectError::Driver(map_error(e, Some(sql))))?;
    let mut sets = 0usize;
    let mut candidate = false;
    let mut plan = None;
    let mut candidate_rows = 0usize;
    let mut too_large = false;
    while let Some(item) = stream
        .try_next()
        .await
        .map_err(|e| CollectError::Driver(map_error(e, Some(sql))))?
    {
        match item {
            QueryItem::Metadata(meta) => {
                sets += 1;
                candidate = meta.columns().len() == 1 && meta.columns()[0].name() == COLUMN;
                // A prior user result is never a substitute for the final server result.
                plan = None;
                candidate_rows = 0;
                too_large = false;
            }
            QueryItem::Row(row) if candidate => {
                candidate_rows += 1;
                if let Some(value) = row
                    .into_iter()
                    .next()
                    .and_then(|value| decode::text(&value))
                {
                    let trimmed = value.trim_start_matches('\u{feff}').trim_start();
                    if trimmed.starts_with("<ShowPlanXML") || trimmed.starts_with("<?xml") {
                        if value.len() > MAX_XML_BYTES {
                            // This may still be an ordinary user result set. Continue to the
                            // last result without retaining it, and enforce the limit there.
                            too_large = true;
                            continue;
                        }
                        plan = Some(value);
                    }
                }
            }
            QueryItem::Row(_) => {}
        }
    }
    let expected_sets = match mode {
        PlanMode::Estimated
        | PlanMode::Analyze {
            query_result: false,
        } => 1,
        PlanMode::Analyze { query_result: true } => 2,
    };
    if sets != expected_sets {
        return Err(CollectError::Driver(DriverError::Other(
            "SQL Server returned an unexpected number of result sets; the statement may have run, so check its transaction before retrying".into(),
        )));
    }
    if too_large {
        return Err(CollectError::TooLarge);
    }
    if !candidate || candidate_rows != 1 {
        return Err(CollectError::Driver(DriverError::Other(
            "the final SQL Server plan result must contain exactly one XML row".into(),
        )));
    }
    plan.ok_or_else(|| CollectError::Driver(DriverError::Other(match mode {
        PlanMode::Estimated => "SQL Server returned no XML plan".into(),
        PlanMode::Analyze { .. } => "ANALYZE ran, but SQL Server returned no XML plan; check the transaction before retrying".into(),
    })))
}

/// The first result says the connection is safe; the second says a user batch was attempted.
pub(super) async fn execute(
    client: &mut SqlClient,
    sql: &str,
    mode: PlanMode,
    events: &mpsc::Sender<Result<ExecEvent>>,
    cancels: &mut watch::Receiver<u64>,
    epoch: u64,
) -> (bool, bool) {
    if events.is_closed() {
        return (true, false);
    }
    if *cancels.borrow() != epoch {
        let _ = events.send(Err(DriverError::Cancelled)).await;
        return (true, false);
    }
    match wait(check_options(client), cancels, epoch, events).await {
        Wait::Done(Ok(())) => {}
        Wait::Done(Err(error)) => {
            let healthy = !matches!(error, DriverError::Connection(_));
            let _ = events.send(Err(error)).await;
            return (healthy, false);
        }
        Wait::Cancelled(polled) | Wait::Abandoned(polled) => {
            let healthy = !polled || interrupt(client).await;
            let _ = events.send(Err(DriverError::Cancelled)).await;
            return (healthy, false);
        }
    }
    if *cancels.borrow() != epoch || events.is_closed() {
        let _ = events.send(Err(DriverError::Cancelled)).await;
        return (true, false);
    }

    let (on, off) = match mode {
        PlanMode::Estimated => ("set showplan_xml on", "set showplan_xml off"),
        PlanMode::Analyze { .. } => ("set statistics xml on", "set statistics xml off"),
    };
    // Even an interrupted ON can have reached the server, so always attempt OFF below.
    let mut interrupted = false;
    let mut attempted = false;
    let mut notices = Vec::new();
    let outcome = match wait(run_batch(client, on), cancels, epoch, events).await {
        Wait::Done(Ok(())) if *cancels.borrow() != epoch || events.is_closed() => {
            Err(DriverError::Cancelled)
        }
        Wait::Done(Ok(())) => {
            attempted = true;
            let _ = client.take_info_messages();
            let result = wait(collect(client, sql, mode), cancels, epoch, events).await;
            notices = info_notices(client);
            match result {
                Wait::Done(Ok(xml)) => Ok(xml),
                Wait::Done(Err(CollectError::Driver(error))) => Err(match mode {
                    PlanMode::Analyze { .. } => DriverError::Other(format!(
                        "ANALYZE may have executed the statement; check the transaction before retrying: {error}"
                    )),
                    PlanMode::Estimated => error,
                }),
                Wait::Done(Err(CollectError::TooLarge)) => {
                    Err(DriverError::Other("the XML plan exceeds 8 MiB; the query may have run, so check its transaction before retrying".into()))
                }
                Wait::Cancelled(polled) | Wait::Abandoned(polled) => {
                    interrupted = polled;
                    Err(DriverError::Cancelled)
                }
            }
        }
        Wait::Done(Err(error)) => Err(error),
        Wait::Cancelled(polled) | Wait::Abandoned(polled) => {
            interrupted = polled;
            Err(DriverError::Cancelled)
        }
    };
    if interrupted && !interrupt(client).await {
        let _ = events
            .send(Err(DriverError::Connection(
                "could not drain the cancelled plan; the session was closed".into(),
            )))
            .await;
        return (false, attempted);
    }
    if !matches!(
        tokio::time::timeout(CLEANUP_TIMEOUT, run_batch(client, off)).await,
        Ok(Ok(()))
    ) {
        let _ = events.send(Err(DriverError::Connection("could not restore the SQL Server plan setting; the session was closed and ANALYZE may have run".into()))).await;
        return (false, attempted);
    }
    let _ = client.take_info_messages();
    for notice in notices {
        if events.send(Ok(ExecEvent::Notice(notice))).await.is_err() {
            return (true, attempted);
        }
    }
    match outcome {
        Err(error) => {
            let _ = events.send(Err(error)).await;
        }
        Ok(xml) => {
            let schema = Arc::new(ResultSchema::new(vec![decode::column_meta(
                COLUMN,
                ColumnType::Xml,
            )]));
            let batch = RecordBatch::try_new(
                schema.arrow.clone(),
                vec![Arc::new(StringArray::from(vec![xml]))],
            );
            match batch {
                Ok(batch) => {
                    let _ = events.send(Ok(ExecEvent::Columns(schema))).await;
                    let _ = events.send(Ok(ExecEvent::Batch(batch))).await;
                    let _ = events
                        .send(Ok(ExecEvent::Done {
                            rows_affected: None,
                        }))
                        .await;
                }
                Err(error) => {
                    let _ = events
                        .send(Err(DriverError::Other(error.to_string())))
                        .await;
                }
            }
        }
    }
    (true, attempted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_before_first_poll_does_not_start_the_request() {
        let (cancel, mut receiver) = watch::channel(0);
        cancel.send(1).unwrap();
        let (events, _rx) = mpsc::channel(1);
        let called = AtomicBool::new(false);
        let pending = poll_fn(|_| {
            called.store(true, Ordering::Relaxed);
            std::task::Poll::<()>::Pending
        });
        assert!(matches!(
            wait(pending, &mut receiver, 0, &events).await,
            Wait::Cancelled(false)
        ));
        assert!(!called.load(Ordering::Relaxed));
    }
}
