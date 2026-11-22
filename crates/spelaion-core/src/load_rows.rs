//! "Load every row" and reaching a page or a row past the ones loaded (SDB-386).
//!
//! A paged query (ADR-054) reaches a later row only by keyset pages, in order: each one asks
//! for the rows after the last row loaded, so every row that existed throughout is loaded once,
//! whatever is written meanwhile. Its last page, or page N past the ones loaded, is therefore
//! reached by loading every page before it, never by guessing an offset. The rows stay in the
//! core (ADR-010) and each page goes through the result store, which masks it (ADR-051).
//!
//! Before loading everything the app says how many rows that is and roughly how much memory
//! they take: the rows counted on the server, times what a loaded row takes now.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::query_page::{append_query_page, count_query_rows};
use crate::sort::SortKey;
use crate::table_data::count_table_rows;
use crate::{MAX_PAGE_ROWS, ResultId, Session, StoredResult};

/// Most rows a result may hold: the grid's windows and the core's sorted views number rows
/// with 32 bits.
pub const MAX_LOADED_ROWS: u64 = u32::MAX as u64;

/// Rows each keyset page of a load asks for.
pub const LOAD_PAGE_ROWS: u64 = MAX_PAGE_ROWS;

/// What loading every row would hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadEstimate {
    /// Rows loaded now.
    pub loaded: u64,
    /// Rows the server counted.
    pub total: u64,
    /// Roughly the memory every row would take, from what a loaded row takes now; `None` with
    /// no row loaded to measure.
    pub bytes: Option<u64>,
    /// More rows than a result may hold ([`MAX_LOADED_ROWS`]): loading them is refused.
    pub too_many: bool,
}

/// The estimate for `total` rows of `stored`'s kind.
pub fn estimate(stored: &StoredResult, total: u64) -> LoadEstimate {
    let loaded = stored.total_rows() as u64;
    let bytes = (loaded > 0).then(|| {
        let per_row = stored.memory_bytes() as u128 / u128::from(loaded);
        u64::try_from(per_row * u128::from(total.max(loaded))).unwrap_or(u64::MAX)
    });
    LoadEstimate {
        loaded,
        total,
        bytes,
        too_many: total > MAX_LOADED_ROWS,
    }
}

/// The estimate for every row of a paged query whose loaded rows are `target`: its rows
/// counted on `session`.
pub async fn estimate_query_load(
    session: &Session,
    target: ResultId,
    sql: &str,
) -> Result<LoadEstimate, String> {
    let total = count_query_rows(session, sql).await?;
    stored_estimate(session, target, total)
}

/// The estimate for every row of a table data tab whose page on screen is `target`: the rows
/// its filter keeps, counted on `session`.
pub async fn estimate_table_load(
    session: &Session,
    target: ResultId,
    schema: &str,
    name: &str,
    filter: &str,
) -> Result<LoadEstimate, String> {
    let total = count_table_rows(session, schema, name, filter).await?;
    stored_estimate(session, target, total)
}

fn stored_estimate(
    session: &Session,
    target: ResultId,
    total: i64,
) -> Result<LoadEstimate, String> {
    let stored = session
        .results()
        .stored(target)
        .map_err(|e| e.to_string())?;
    let stored = stored.read().map_err(|_| "result lock poisoned")?;
    Ok(estimate(&stored, u64::try_from(total).unwrap_or(0)))
}

/// Stops a load: the page being read and every page after it. Created before the load starts,
/// so a cancel that comes before its first page is not lost.
#[derive(Debug, Default)]
pub struct LoadCancel {
    cancelled: AtomicBool,
}

impl LoadCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stops the load on `session`: no page starts after this, and the one being read is
    /// cancelled through [`Session::cancel`], whose gate keeps an engine's cancel that is
    /// still on its way from stopping a later statement of the tab (SDB-266).
    pub async fn cancel(&self, session: &Session) {
        self.stop();
        let _ = session.cancel().await;
    }

    /// Stops the load before its next page, letting the page being read finish.
    pub fn stop(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// How far a load went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// The query has no more rows: every row is loaded.
    Complete,
    /// The rows asked for are loaded; more may follow.
    Reached,
    /// Cancelled: the pages loaded before stay, whole.
    Cancelled,
    /// A page failed: the pages loaded before stay, whole.
    Failed(String),
}

/// Where a load ended: the rows the result holds and why it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRows {
    pub rows: u64,
    pub outcome: LoadOutcome,
}

/// Appends keyset pages of the paged query `sql` (ADR-054) to `target`, which holds `offset`
/// rows read with the grid's `sort`, until it holds `until` rows (every row when `None`), the
/// query has no more, or `cancel` stops it. Each page holds at most `page_rows` rows (at most
/// [`MAX_PAGE_ROWS`]), and `progress` hears the rows held after each one. A page is appended
/// whole or not at all, so a cancel or a failure leaves exactly the rows of the pages before.
#[allow(clippy::too_many_arguments)]
pub async fn load_query_rows(
    session: &Session,
    target: ResultId,
    sql: &str,
    offset: u64,
    sort: &[SortKey],
    until: Option<u64>,
    page_rows: u64,
    cancel: &LoadCancel,
    progress: &(dyn Fn(u64) + Send + Sync),
) -> Result<LoadedRows, String> {
    if page_rows == 0 || page_rows > MAX_PAGE_ROWS {
        return Err(format!("a page holds from 1 to {MAX_PAGE_ROWS} rows"));
    }
    let until = until.unwrap_or(MAX_LOADED_ROWS);
    if until > MAX_LOADED_ROWS {
        return Err(format!("a result holds at most {MAX_LOADED_ROWS} rows"));
    }
    let mut rows = offset;
    loop {
        if cancel.is_cancelled() {
            return Ok(LoadedRows {
                rows,
                outcome: LoadOutcome::Cancelled,
            });
        }
        if rows >= until {
            return Ok(LoadedRows {
                rows,
                outcome: LoadOutcome::Reached,
            });
        }
        let limit = page_rows.min(until - rows);
        match append_query_page(session, target, sql, limit, rows, sort).await {
            Ok(added) => {
                rows += added as u64;
                progress(rows);
                if (added as u64) < limit {
                    return Ok(LoadedRows {
                        rows,
                        outcome: LoadOutcome::Complete,
                    });
                }
            }
            Err(_) if cancel.is_cancelled() => {
                return Ok(LoadedRows {
                    rows,
                    outcome: LoadOutcome::Cancelled,
                });
            }
            Err(error) => {
                return Ok(LoadedRows {
                    rows,
                    outcome: LoadOutcome::Failed(error),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ResultStore;
    use arrow::array::{Int64Array, RecordBatch, StringArray};
    use spelaion_driver_api::{ColumnMeta, LogicalType, ResultSchema};
    use std::sync::Arc;

    fn stored(rows: usize) -> (ResultStore, ResultId) {
        let schema = Arc::new(ResultSchema::new(vec![
            ColumnMeta {
                name: "id".into(),
                logical: LogicalType::Int,
                native_type: "bigint".into(),
                native_id: None,
                origin: None,
            },
            ColumnMeta {
                name: "name".into(),
                logical: LogicalType::Text,
                native_type: "text".into(),
                native_id: None,
                origin: None,
            },
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        if rows > 0 {
            let batch = RecordBatch::try_new(
                schema.arrow.clone(),
                vec![
                    Arc::new(Int64Array::from_iter_values(0..rows as i64)),
                    Arc::new(StringArray::from_iter_values(
                        (0..rows).map(|i| format!("name {i}")),
                    )),
                ],
            )
            .unwrap();
            store.append(id, batch);
        }
        store.complete(id);
        (store, id)
    }

    #[test]
    fn the_estimate_scales_what_a_loaded_row_takes_to_every_row() {
        let (store, id) = stored(1000);
        let result = store.get(id).unwrap();
        let result = result.read().unwrap();
        let per_row = result.memory_bytes() as u64 / 1000;
        assert!(per_row > 8, "a row holds an i64 and a text: {per_row}");
        let estimate = estimate(&result, 1_000_000);
        assert_eq!(estimate.loaded, 1000);
        assert_eq!(estimate.total, 1_000_000);
        assert_eq!(estimate.bytes, Some(per_row * 1_000_000));
        assert!(!estimate.too_many);
    }

    #[test]
    fn an_empty_result_has_nothing_to_measure_and_too_many_rows_are_refused() {
        let (store, id) = stored(0);
        let result = store.get(id).unwrap();
        let result = result.read().unwrap();
        let estimate = estimate(&result, MAX_LOADED_ROWS + 1);
        assert_eq!(estimate.bytes, None);
        assert!(estimate.too_many);
    }

    #[test]
    fn a_count_below_the_rows_loaded_is_estimated_from_the_rows_loaded() {
        // Rows deleted since they were loaded: the result still holds them.
        let (store, id) = stored(100);
        let result = store.get(id).unwrap();
        let result = result.read().unwrap();
        let estimate = estimate(&result, 10);
        assert_eq!(
            estimate.bytes,
            Some(result.memory_bytes() as u64 / 100 * 100)
        );
    }
}
