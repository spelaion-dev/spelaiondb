//! A result's row count at a pace a window can draw.
//!
//! Every batch a driver reads becomes a [`CoreEvent::Rows`]: a fast engine streaming a big
//! result sends hundreds of them a second. A client that redraws on each one (the desktop app
//! re-renders the tab and its grid) spends its whole main thread there, and a click on Cancel
//! has to wait behind them. [`coalesce_rows`] lets the first count through at once, then at
//! most one per interval, always the latest; every other event goes through as it comes, after
//! the count held back, so the order clients rely on (rows before the statement finishes) and
//! the final total stay exact.

use std::time::Duration;

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use tokio::time::{Instant, sleep_until};

use crate::{CoreEvent, ResultId};

/// How often a client that draws progress gets a streaming result's row count.
pub const ROWS_PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// The same events, with the [`CoreEvent::Rows`] of a fast stream thinned to at most one per
/// `interval` (the latest count). A count held back is sent when the interval ends, before
/// any other event, or when the stream ends: nothing but intermediate counts is ever dropped.
pub fn coalesce_rows(
    mut events: BoxStream<'static, CoreEvent>,
    interval: Duration,
) -> BoxStream<'static, CoreEvent> {
    Box::pin(async_stream::stream! {
        // The latest count not sent yet: (statement index, result, total rows).
        let mut held: Option<(usize, ResultId, usize)> = None;
        // When a count was last sent.
        let mut sent: Option<Instant> = None;
        loop {
            // `None`: the interval ended with a count held back.
            let next = match (held, sent) {
                (Some(_), Some(at)) => tokio::select! {
                    biased;
                    event = events.next() => Some(event),
                    () = sleep_until(at + interval) => None,
                },
                _ => Some(events.next().await),
            };
            match next {
                None => {
                    if let Some((index, result, total_rows)) = held.take() {
                        sent = Some(Instant::now());
                        yield CoreEvent::Rows { index, result, total_rows };
                    }
                }
                Some(None) => {
                    if let Some((index, result, total_rows)) = held.take() {
                        yield CoreEvent::Rows { index, result, total_rows };
                    }
                    break;
                }
                Some(Some(CoreEvent::Rows { index, result, total_rows })) => {
                    // Another result's count is that result's last one: it goes first.
                    if let Some((held_index, held_result, held_rows)) = held
                        && held_result != result
                    {
                        yield CoreEvent::Rows { index: held_index, result: held_result, total_rows: held_rows };
                    }
                    // This count replaces any held one.
                    if sent.is_none_or(|at| at.elapsed() >= interval) {
                        held = None;
                        sent = Some(Instant::now());
                        yield CoreEvent::Rows { index, result, total_rows };
                    } else {
                        held = Some((index, result, total_rows));
                    }
                }
                Some(Some(event)) => {
                    if let Some((index, result, total_rows)) = held.take() {
                        yield CoreEvent::Rows { index, result, total_rows };
                    }
                    yield event;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    const INTERVAL: Duration = Duration::from_millis(200);

    fn rows(result: u64, total_rows: usize) -> CoreEvent {
        CoreEvent::Rows {
            index: 0,
            result: ResultId(result),
            total_rows,
        }
    }

    fn other(text: &str) -> CoreEvent {
        CoreEvent::HistoryWarning {
            message: text.to_string(),
        }
    }

    /// What the coalesced stream yields, each with the milliseconds since the start.
    fn describe(event: &CoreEvent) -> String {
        match event {
            CoreEvent::Rows {
                result, total_rows, ..
            } => format!("rows {}:{total_rows}", result.0),
            CoreEvent::HistoryWarning { message } => message.clone(),
            other => format!("{other:?}"),
        }
    }

    /// Feeds `script` (a pause in ms before each event) through the coalescer, in paused time;
    /// the source stays open `quiet` ms after its last event.
    async fn run_quiet(script: Vec<(u64, CoreEvent)>, quiet: u64) -> Vec<(u128, String)> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let source: BoxStream<'static, CoreEvent> = Box::pin(async_stream::stream! {
            while let Some(event) = rx.recv().await {
                yield event;
            }
        });
        let start = Instant::now();
        tokio::spawn(async move {
            for (pause, event) in script {
                tokio::time::sleep(Duration::from_millis(pause)).await;
                if tx.send(event).is_err() {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(quiet)).await;
        });
        let mut out = Vec::new();
        let mut events = coalesce_rows(source, INTERVAL);
        while let Some(event) = events.next().await {
            out.push((start.elapsed().as_millis(), describe(&event)));
        }
        out
    }

    async fn run(script: Vec<(u64, CoreEvent)>) -> Vec<(u128, String)> {
        run_quiet(script, 0).await
    }

    #[tokio::test(start_paused = true)]
    async fn first_count_goes_at_once_and_a_burst_becomes_its_latest() {
        let mut script = vec![(0, rows(1, 10))];
        script.extend((2..=50).map(|n| (1, rows(1, n * 10))));
        let out = run_quiet(script, 500).await;
        // The first at once; the burst (49 ms) held back, its latest sent when the interval ends
        // even though the stream is quiet, then the end of the stream.
        assert_eq!(
            out,
            vec![
                (0, "rows 1:10".to_string()),
                (200, "rows 1:500".to_string())
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn another_event_sends_the_held_count_first() {
        let out = run(vec![
            (0, rows(1, 1)),
            (10, rows(1, 2)),
            (10, rows(1, 3)),
            (10, other("finished")),
        ])
        .await;
        assert_eq!(
            out,
            vec![
                (0, "rows 1:1".to_string()),
                (30, "rows 1:3".to_string()),
                (30, "finished".to_string()),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_stream_sends_the_held_count() {
        // The sender is dropped right after the last count: nothing waits for the interval.
        let out = run(vec![(0, rows(1, 1)), (5, rows(1, 7))]).await;
        assert_eq!(
            out,
            vec![(0, "rows 1:1".to_string()), (5, "rows 1:7".to_string())]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_result_never_hides_the_last_count_of_the_previous_one() {
        let out = run(vec![
            (0, rows(1, 1)),
            (10, rows(1, 2)),
            (10, rows(2, 5)),
            (10, rows(2, 6)),
        ])
        .await;
        assert_eq!(
            out,
            vec![
                (0, "rows 1:1".to_string()),
                (20, "rows 1:2".to_string()),
                (30, "rows 2:6".to_string()),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn slow_counts_are_not_delayed() {
        let out = run(vec![(0, rows(1, 1)), (250, rows(1, 2)), (300, rows(1, 3))]).await;
        assert_eq!(
            out,
            vec![
                (0, "rows 1:1".to_string()),
                (250, "rows 1:2".to_string()),
                (550, "rows 1:3".to_string()),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_fast_stream_is_bounded_and_ends_exact() {
        // 10 seconds of a count every millisecond, as a fast engine streams batches.
        let mut script: Vec<(u64, CoreEvent)> = (1..=10_000).map(|n| (1, rows(1, n))).collect();
        script.push((0, other("finished")));
        let out = run(script).await;
        let counts: Vec<_> = out.iter().filter(|(_, e)| e.starts_with("rows")).collect();
        // One per interval (plus the first), never more.
        assert!(counts.len() <= 10_000 / 200 + 2, "{} counts", counts.len());
        for pair in counts.windows(2) {
            assert!(pair[1].0 - pair[0].0 >= 200 || pair[1] == counts[counts.len() - 1]);
        }
        // The exact total, right before the event that follows it.
        assert_eq!(out[out.len() - 2].1, "rows 1:10000");
        assert_eq!(out[out.len() - 1].1, "finished");
    }
}
