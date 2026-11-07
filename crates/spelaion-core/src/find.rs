//! Find in a stored result (the grid's Ctrl+F, ADR-010): every cell whose text, as the grid
//! shows it, contains the search. It runs in the core over the loaded rows, so a million
//! rows never travel to the UI: the grid asks for the matches of the rows it shows, steps to
//! the next or previous one, and reads "N of M".
//!
//! Matches are positions in the current view (after the sort and the filters). Anything that
//! moves rows (a sort, a filter, more rows appended) drops them, and the grid searches again.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spelaion_driver_api::Value;

use crate::results::StoredResult;
use crate::sort::ViewError;

/// Most matches kept; beyond them the count reads "1,000,000+" (and stepping wraps among them).
pub const MAX_MATCHES: usize = 1_000_000;

/// Rows read between two checks of whether a newer search replaced this one.
const CHECK_EVERY: usize = 4_096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindQuery {
    /// The text looked for, as typed (blank finds nothing).
    pub text: String,
    /// Upper and lower case must match (otherwise they are the same letter).
    pub match_case: bool,
    /// Columns searched, by position (the grid leaves out the ones it hides).
    pub columns: Vec<usize>,
}

/// A cell that matches: its row in the current view and its column in the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FindMatch {
    pub row: usize,
    pub column: usize,
}

/// The matches of one search, in reading order (row by row, columns left to right).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindMatches {
    /// The view they were found in; another view makes them stale.
    pub(crate) generation: u64,
    cells: Vec<(u32, u32)>,
    truncated: bool,
}

impl FindMatches {
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// There were more than [`MAX_MATCHES`]: only the first ones are kept.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub fn get(&self, index: usize) -> Option<FindMatch> {
        self.cells.get(index).map(|&(row, column)| FindMatch {
            row: row as usize,
            column: column as usize,
        })
    }

    /// The matches in rows `[first, first + rows)` of the view (what the grid shows).
    pub fn window(&self, first: usize, rows: usize) -> Vec<FindMatch> {
        let start = self
            .cells
            .partition_point(|&(row, _)| (row as usize) < first);
        let end = first.saturating_add(rows);
        self.cells[start..]
            .iter()
            .take_while(|&&(row, _)| (row as usize) < end)
            .map(|&(row, column)| FindMatch {
                row: row as usize,
                column: column as usize,
            })
            .collect()
    }

    /// The index of the next match after the cell (`row`, `column`), or the previous one
    /// before it with `backwards`, going round at the ends; `None` with no matches.
    pub fn step(&self, row: usize, column: usize, backwards: bool) -> Option<usize> {
        if self.cells.is_empty() {
            return None;
        }
        let at = (
            row.min(u32::MAX as usize) as u32,
            column.min(u32::MAX as usize) as u32,
        );
        let index = if backwards {
            match self.cells.partition_point(|&cell| cell < at) {
                0 => self.cells.len() - 1,
                i => i - 1,
            }
        } else {
            match self.cells.partition_point(|&cell| cell <= at) {
                i if i == self.cells.len() => 0,
                i => i,
            }
        };
        Some(index)
    }

    /// The index of the match at exactly this cell, if it is one.
    pub fn index_of(&self, row: usize, column: usize) -> Option<usize> {
        let at = (u32::try_from(row).ok()?, u32::try_from(column).ok()?);
        self.cells.binary_search(&at).ok()
    }
}

/// Lets a newer search on the same result stop an older one that is still running.
#[derive(Debug, Clone)]
pub struct FindTicket {
    latest: Arc<AtomicU64>,
    number: u64,
}

impl FindTicket {
    pub(crate) fn new(latest: &Arc<AtomicU64>) -> Self {
        Self {
            latest: latest.clone(),
            number: latest.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }

    /// A newer search started on the same result.
    pub fn is_stale(&self) -> bool {
        self.latest.load(Ordering::Relaxed) != self.number
    }
}

/// Searches the rows of `result` in the order of its current view and returns the matches as
/// positions of the current view; `None` if a newer search replaced this one meanwhile.
pub(crate) fn search(
    result: &StoredResult,
    query: &FindQuery,
    ticket: &FindTicket,
) -> Result<Option<FindMatches>, ViewError> {
    let generation = result.generation();
    let needle = if query.match_case {
        query.text.clone()
    } else {
        query.text.to_lowercase()
    };
    if needle.is_empty() {
        return Ok(Some(FindMatches {
            generation,
            cells: Vec::new(),
            truncated: false,
        }));
    }
    let schema = result.schema();
    let mut columns: Vec<usize> = Vec::with_capacity(query.columns.len());
    let mut seen = HashSet::new();
    for &column in &query.columns {
        if column >= schema.columns.len() {
            return Err(ViewError::NoSuchColumn(column));
        }
        if seen.insert(column) {
            columns.push(column);
        }
    }
    columns.sort_unstable();

    // Rows in the view's order, so the matches come out in reading order and the first
    // MAX_MATCHES are the ones kept.
    let mut cells: Vec<(u32, u32)> = Vec::new();
    let mut truncated = false;
    'rows: for position in 0..result.rows() {
        if position % CHECK_EVERY == 0 && ticket.is_stale() {
            return Ok(None);
        }
        let (batch, row) = result.locate(result.original_row(position));
        for &column in &columns {
            let logical = schema.columns[column].logical;
            let text = match Value::from_array(batch.column(column).as_ref(), row, logical) {
                Some(Value::Null) | None => continue,
                Some(value) => value.to_string(),
            };
            let found = if query.match_case {
                text.contains(&needle)
            } else {
                text.to_lowercase().contains(&needle)
            };
            if found {
                if cells.len() == MAX_MATCHES {
                    truncated = true;
                    break 'rows;
                }
                cells.push((position as u32, column as u32));
            }
        }
    }
    Ok(Some(FindMatches {
        generation,
        cells,
        truncated,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use spelaion_driver_api::{ColumnMeta, LogicalType, ResultSchema};

    use super::*;
    use crate::ResultStore;
    use crate::filter::{ColumnFilter, FilterOp};
    use crate::results::ResultId;
    use crate::sort::SortKey;

    fn store() -> (ResultStore, ResultId) {
        let meta = |name: &str, logical| ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
        };
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int),
            meta("name", LogicalType::Text),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        // Two batches, so matches cross a batch boundary.
        for (ids, names) in [
            (vec![1, 2, 3], vec![Some("Ana"), Some("bob"), None]),
            (
                vec![4, 5, 12],
                vec![Some("ANA MARIA"), Some("Iñaki"), Some("x1")],
            ),
        ] {
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(names)),
            ];
            store
                .append(
                    id,
                    RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
                )
                .unwrap();
        }
        store.complete(id);
        (store, id)
    }

    fn query(text: &str, match_case: bool) -> FindQuery {
        FindQuery {
            text: text.into(),
            match_case,
            columns: vec![0, 1],
        }
    }

    fn cells(matches: &FindMatches) -> Vec<(usize, usize)> {
        (0..matches.len())
            .map(|i| matches.get(i).map(|m| (m.row, m.column)).unwrap())
            .collect()
    }

    fn run(r: &StoredResult, q: &FindQuery) -> FindMatches {
        r.find_matches(q, &r.find_ticket()).unwrap().unwrap()
    }

    #[test]
    fn finds_cells_by_their_text_in_reading_order() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        // "ana" in rows 0 and 3 (any case); NULL never matches.
        assert_eq!(cells(&run(&r, &query("ana", false))), [(0, 1), (3, 1)]);
        assert_eq!(cells(&run(&r, &query("Ana", true))), [(0, 1)]);
        // Numbers by the text the grid shows: "1" is in 1, 12 and "x1".
        assert_eq!(
            cells(&run(&r, &query("1", false))),
            [(0, 0), (5, 0), (5, 1)]
        );
        // Letters beyond ASCII ignore case too.
        assert_eq!(cells(&run(&r, &query("IÑAKI", false))), [(4, 1)]);
        // Only the columns asked for (the grid's hidden ones are left out).
        let names_only = FindQuery {
            columns: vec![1],
            ..query("1", false)
        };
        assert_eq!(cells(&run(&r, &names_only)), [(5, 1)]);
        assert!(run(&r, &query("", false)).is_empty());
        assert_eq!(
            r.find_matches(
                &FindQuery {
                    columns: vec![7],
                    ..query("a", false)
                },
                &r.find_ticket()
            ),
            Err(ViewError::NoSuchColumn(7))
        );
    }

    #[test]
    fn steps_go_round_and_windows_show_the_visible_rows() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let m = run(&r, &query("1", false));
        // From the cell (0, 0) itself: the next match is after it.
        assert_eq!(m.step(0, 0, false), Some(1));
        assert_eq!(m.step(5, 0, false), Some(2));
        // After the last one, back to the first.
        assert_eq!(m.step(5, 1, false), Some(0));
        assert_eq!(m.step(0, 0, true), Some(2));
        assert_eq!(m.step(5, 1, true), Some(1));
        assert_eq!(m.step(3, 0, true), Some(0));
        assert_eq!(m.index_of(5, 1), Some(2));
        assert_eq!(m.index_of(4, 1), None);
        assert_eq!(
            m.window(1, 10),
            [
                FindMatch { row: 5, column: 0 },
                FindMatch { row: 5, column: 1 }
            ]
        );
        assert!(m.window(1, 4).is_empty());
        let none = run(&r, &query("zzz", false));
        assert_eq!(none.step(0, 0, false), None);
    }

    #[test]
    fn matches_follow_the_view_and_go_stale_when_rows_move() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        r.sort(Some(SortKey {
            column: 0,
            descending: true,
        }))
        .unwrap();
        // Sorted 12, 5, 4, 3, 2, 1: "ana" is now in rows 2 and 5.
        let m = run(&r, &query("ana", false));
        assert_eq!(cells(&m), [(2, 1), (5, 1)]);
        let ticket = r.find_ticket();
        let fresh = r
            .find_matches(&query("ana", false), &ticket)
            .unwrap()
            .unwrap();
        assert!(r.keep_find(fresh, &ticket));
        assert_eq!(r.find().map(FindMatches::len), Some(2));
        // A filter moves rows: the kept matches are dropped, and older ones are refused.
        let ticket = r.find_ticket();
        let old = r
            .find_matches(&query("ana", false), &ticket)
            .unwrap()
            .unwrap();
        r.filter_columns(vec![ColumnFilter {
            column: 0,
            op: FilterOp::Gt,
            value: "3".into(),
        }])
        .unwrap();
        assert!(r.find().is_none());
        assert!(!r.keep_find(old, &ticket));
        // 12, 5, 4 left: "ana" only in row 2.
        assert_eq!(cells(&run(&r, &query("ana", false))), [(2, 1)]);
        r.clear_find();
        assert!(r.find().is_none());
    }

    #[test]
    fn a_newer_search_stops_an_older_one() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let first = r.find_ticket();
        let second = r.find_ticket();
        assert!(first.is_stale() && !second.is_stale());
        assert_eq!(r.find_matches(&query("a", false), &first), Ok(None));
        assert!(
            r.find_matches(&query("a", false), &second)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn an_incomplete_result_is_not_searched() {
        let store = ResultStore::new();
        let id = store.create(Arc::new(ResultSchema::new(Vec::new())));
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        assert_eq!(
            r.find_matches(&query("a", false), &r.find_ticket()),
            Err(ViewError::Incomplete)
        );
    }
}
