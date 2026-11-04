//! Result store (ADR-010): results live in the core as Arrow batches and the UI requests
//! windows of rows. The frontend never holds the full result.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use arrow::array::{Array, RecordBatch};
use arrow::compute::{concat, interleave_record_batch};
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use spelunk_driver_api::{ResultSchema, Value};

use crate::filter::{ColumnFilter, matches, validate};
use crate::sort::{SortKey, ViewError, sorted_rows};

/// Identifier of a result within a [`ResultStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResultId(pub u64);

/// A result that may keep growing while batches arrive.
#[derive(Debug)]
pub struct StoredResult {
    schema: Arc<ResultSchema>,
    batches: Vec<RecordBatch>,
    /// Index of the first row of each batch.
    starts: Vec<usize>,
    rows: usize,
    complete: bool,
    sort: Option<(SortKey, Vec<u32>)>,
    /// The filter text and, per original row, whether it matches.
    filter: Option<(String, Vec<bool>)>,
    /// Column filters (all must pass) and, per original row, whether it passes them.
    column_filters: Option<(Vec<ColumnFilter>, Vec<bool>)>,
    /// With a sort or a filter, the original row shown at each position.
    view: Option<Vec<u32>>,
}

impl StoredResult {
    fn new(schema: Arc<ResultSchema>) -> Self {
        Self {
            schema,
            batches: Vec::new(),
            starts: Vec::new(),
            rows: 0,
            complete: false,
            sort: None,
            filter: None,
            column_filters: None,
            view: None,
        }
    }

    pub fn schema(&self) -> &Arc<ResultSchema> {
        &self.schema
    }

    /// Rows in the current view (after the filter, if any).
    pub fn rows(&self) -> usize {
        self.view.as_ref().map_or(self.rows, Vec::len)
    }

    /// Rows received, whatever the filter.
    pub fn total_rows(&self) -> usize {
        self.rows
    }

    /// No more rows will arrive.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The column the rows are sorted by, if any.
    pub fn sort_key(&self) -> Option<SortKey> {
        self.sort.as_ref().map(|(key, _)| *key)
    }

    /// The text rows are filtered by, if any.
    pub fn filter_text(&self) -> Option<&str> {
        self.filter.as_ref().map(|(text, _)| text.as_str())
    }

    /// Sorts the rows by one column (`None` goes back to the order the server sent). Every
    /// later window, copy and export sees the new order.
    pub fn sort(&mut self, key: Option<SortKey>) -> Result<(), ViewError> {
        let Some(key) = key else {
            self.sort = None;
            self.rebuild_view();
            return Ok(());
        };
        self.check_viewable()?;
        let meta = self
            .schema
            .columns
            .get(key.column)
            .ok_or(ViewError::NoSuchColumn(key.column))?;
        let parts: Vec<&dyn Array> = self
            .batches
            .iter()
            .map(|b| b.column(key.column).as_ref())
            .collect();
        let rows = if parts.is_empty() {
            Vec::new()
        } else {
            let values = concat(&parts).map_err(|_| ViewError::UnexpectedArray(key.column))?;
            sorted_rows(values.as_ref(), meta.logical, key)?
        };
        self.sort = Some((key, rows));
        self.rebuild_view();
        Ok(())
    }

    /// Keeps the rows where some cell's text (as the grid shows it) contains `text`,
    /// ignoring case; `None` or blank shows every row again. Returns the rows left.
    pub fn filter(&mut self, text: Option<&str>) -> Result<usize, ViewError> {
        let text = text.map(str::trim).filter(|t| !t.is_empty());
        let Some(text) = text else {
            self.filter = None;
            self.rebuild_view();
            return Ok(self.rows());
        };
        self.check_viewable()?;
        let needle = text.to_lowercase();
        let columns = &self.schema.columns;
        let mut matches = Vec::with_capacity(self.rows);
        for batch in &self.batches {
            for row in 0..batch.num_rows() {
                matches.push(columns.iter().enumerate().any(|(i, meta)| {
                    match Value::from_array(batch.column(i).as_ref(), row, meta.logical) {
                        Some(Value::Null) | None => false,
                        Some(value) => value.to_string().to_lowercase().contains(&needle),
                    }
                }));
            }
        }
        self.filter = Some((text.to_string(), matches));
        self.rebuild_view();
        Ok(self.rows())
    }

    /// The column filters in force.
    pub fn column_filters(&self) -> &[ColumnFilter] {
        self.column_filters
            .as_ref()
            .map_or(&[], |(filters, _)| filters.as_slice())
    }

    /// Keeps the rows that pass every column filter (an empty list removes them), on top of
    /// the text filter. Returns the rows left.
    pub fn filter_columns(&mut self, filters: Vec<ColumnFilter>) -> Result<usize, ViewError> {
        if filters.is_empty() {
            self.column_filters = None;
            self.rebuild_view();
            return Ok(self.rows());
        }
        self.check_viewable()?;
        let columns = &self.schema.columns;
        for filter in &filters {
            let meta = columns
                .get(filter.column)
                .ok_or(ViewError::NoSuchColumn(filter.column))?;
            validate(filter, meta.logical)?;
        }
        let mut passes = Vec::with_capacity(self.rows);
        for batch in &self.batches {
            for row in 0..batch.num_rows() {
                passes.push(filters.iter().all(|filter| {
                    let logical = columns[filter.column].logical;
                    let cell =
                        Value::from_array(batch.column(filter.column).as_ref(), row, logical)
                            .unwrap_or(Value::Null);
                    matches(&cell, logical, filter)
                }));
            }
        }
        self.column_filters = Some((filters, passes));
        self.rebuild_view();
        Ok(self.rows())
    }

    fn check_viewable(&self) -> Result<(), ViewError> {
        if !self.complete {
            return Err(ViewError::Incomplete);
        }
        if u32::try_from(self.rows).is_err() {
            return Err(ViewError::TooManyRows);
        }
        Ok(())
    }

    /// The order of the sort (or the server's) keeping only the rows both filters let
    /// through.
    fn rebuild_view(&mut self) {
        let text = self.filter.as_ref().map(|(_, m)| m);
        let columns = self.column_filters.as_ref().map(|(_, m)| m);
        let keep = |row: u32| {
            text.is_none_or(|m| m[row as usize]) && columns.is_none_or(|m| m[row as usize])
        };
        let filtered = text.is_some() || columns.is_some();
        self.view = match &self.sort {
            None if !filtered => None,
            None => Some((0..self.rows as u32).filter(|&row| keep(row)).collect()),
            Some((_, order)) => Some(order.iter().copied().filter(|&row| keep(row)).collect()),
        };
    }

    /// Rows `[start, start + len)` of the current view (clipped at the end). Without a sort
    /// or filter, the batches are slices without copying data; otherwise the window's rows
    /// are gathered.
    pub fn window(&self, start: usize, len: usize) -> Vec<RecordBatch> {
        let end = start.saturating_add(len).min(self.rows());
        if start >= end {
            return Vec::new();
        }
        if let Some(view) = &self.view {
            let picks: Vec<(usize, usize)> = view[start..end]
                .iter()
                .map(|&row| {
                    let row = row as usize;
                    let batch = self.starts.partition_point(|&s| s <= row) - 1;
                    (batch, row - self.starts[batch])
                })
                .collect();
            let batches: Vec<&RecordBatch> = self.batches.iter().collect();
            let gathered = interleave_record_batch(&batches, &picks)
                .expect("the batches of one result share its schema");
            return vec![gathered];
        }
        let first = self.starts.partition_point(|&s| s <= start) - 1;
        let mut out = Vec::new();
        for (batch, &batch_start) in self.batches[first..].iter().zip(&self.starts[first..]) {
            if batch_start >= end {
                break;
            }
            let from = start.saturating_sub(batch_start);
            let to = (end - batch_start).min(batch.num_rows());
            out.push(batch.slice(from, to - from));
        }
        out
    }

    /// The same window serialized as an Arrow IPC stream, ready to send to the UI.
    pub fn window_ipc(&self, start: usize, len: usize) -> Result<Vec<u8>, ArrowError> {
        let mut out = Vec::new();
        let mut writer = StreamWriter::try_new(&mut out, &self.schema.arrow)?;
        for batch in self.window(start, len) {
            writer.write(&batch)?;
        }
        writer.finish()?;
        drop(writer);
        Ok(out)
    }
}

/// Live results of a session or of the whole app.
#[derive(Debug, Default)]
pub struct ResultStore {
    next_id: AtomicU64,
    results: Mutex<HashMap<ResultId, Arc<RwLock<StoredResult>>>>,
}

impl ResultStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn create(&self, schema: Arc<ResultSchema>) -> ResultId {
        let id = ResultId(self.next_id.fetch_add(1, Ordering::Relaxed));
        self.lock()
            .insert(id, Arc::new(RwLock::new(StoredResult::new(schema))));
        id
    }

    /// Adds a batch and returns the total number of rows.
    pub(crate) fn append(&self, id: ResultId, batch: RecordBatch) -> Option<usize> {
        let result = self.get(id)?;
        let mut r = result.write().expect("result lock poisoned");
        let start = r.rows;
        r.starts.push(start);
        r.rows += batch.num_rows();
        r.batches.push(batch);
        Some(r.rows)
    }

    /// Append an independently fetched page only if the target is still the expected view.
    pub(crate) fn append_page(
        &self,
        target: ResultId,
        source: ResultId,
        offset: usize,
    ) -> Result<usize, String> {
        let page = self.get(source).ok_or("page was freed")?;
        let page = page.read().map_err(|_| "page lock poisoned")?;
        let target = self.get(target).ok_or("result was freed")?;
        let mut target = target.write().map_err(|_| "result lock poisoned")?;
        if !target.complete || target.view.is_some() || target.rows != offset {
            return Err("the result changed while fetching more rows".into());
        }
        if target.schema.columns != page.schema.columns {
            return Err("the result columns changed while fetching more rows".into());
        }
        let added = page.rows;
        for batch in &page.batches {
            let start = target.rows;
            target.starts.push(start);
            target.rows += batch.num_rows();
            target.batches.push(batch.clone());
        }
        Ok(added)
    }

    pub(crate) fn complete(&self, id: ResultId) {
        if let Some(result) = self.get(id) {
            result.write().expect("result lock poisoned").complete = true;
        }
    }

    pub fn get(&self, id: ResultId) -> Option<Arc<RwLock<StoredResult>>> {
        self.lock().get(&id).cloned()
    }

    /// Frees a result (for example, when its tab is closed).
    pub fn remove(&self, id: ResultId) -> bool {
        self.lock().remove(&id).is_some()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ResultId, Arc<RwLock<StoredResult>>>> {
        self.results.lock().expect("store lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{Array, Int64Array};
    use spelunk_driver_api::{ColumnMeta, LogicalType};

    fn schema() -> Arc<ResultSchema> {
        Arc::new(ResultSchema::new(vec![ColumnMeta {
            name: "n".into(),
            logical: LogicalType::Int,
            native_type: "int8".into(),
            native_id: None,
        }]))
    }

    fn batch(schema: &ResultSchema, from: i64, to: i64) -> RecordBatch {
        RecordBatch::try_new(
            schema.arrow.clone(),
            vec![Arc::new(Int64Array::from_iter_values(from..to))],
        )
        .unwrap()
    }

    fn values(batches: &[RecordBatch]) -> Vec<i64> {
        batches
            .iter()
            .flat_map(|b| {
                let col = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
                (0..col.len()).map(|i| col.value(i)).collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn windows_span_batches_and_clip_at_the_end() {
        let store = ResultStore::new();
        let s = schema();
        let id = store.create(s.clone());
        for (from, to) in [(0, 10), (10, 15), (15, 30)] {
            store.append(id, batch(&s, from, to));
        }
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        assert_eq!(r.rows(), 30);
        assert_eq!(values(&r.window(8, 9)), (8..17).collect::<Vec<_>>());
        assert_eq!(values(&r.window(25, 100)), (25..30).collect::<Vec<_>>());
        assert!(r.window(30, 5).is_empty());
        assert!(r.window(0, 0).is_empty());
    }

    #[test]
    fn ipc_window_round_trips() {
        let store = ResultStore::new();
        let s = schema();
        let id = store.create(s.clone());
        store.append(id, batch(&s, 0, 100));
        let bytes = store
            .get(id)
            .unwrap()
            .read()
            .unwrap()
            .window_ipc(40, 3)
            .unwrap();
        let reader =
            arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
        let batches: Vec<RecordBatch> = reader.map(Result::unwrap).collect();
        assert_eq!(values(&batches), [40, 41, 42]);
        // Type metadata travels with the data.
        assert_eq!(
            batches[0]
                .schema()
                .field(0)
                .metadata()
                .get("spelunk.native_type")
                .map(String::as_str),
            Some("int8")
        );
    }

    #[test]
    fn sorting_reorders_windows_across_batches_and_can_be_undone() {
        let store = ResultStore::new();
        let s = schema();
        let id = store.create(s.clone());
        for (from, to) in [(0, 10), (10, 15), (15, 30)] {
            store.append(id, batch(&s, from, to));
        }
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        let down = SortKey {
            column: 0,
            descending: true,
        };
        assert_eq!(r.sort(Some(down)), Err(ViewError::Incomplete));
        drop(r);
        store.complete(id);
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        assert_eq!(
            r.sort(Some(SortKey {
                column: 1,
                descending: false
            })),
            Err(ViewError::NoSuchColumn(1))
        );
        r.sort(Some(down)).unwrap();
        assert_eq!(r.sort_key(), Some(down));
        assert_eq!(values(&r.window(8, 9)), (13..=21).rev().collect::<Vec<_>>());
        assert_eq!(values(&r.window(25, 100)), [4, 3, 2, 1, 0]);
        assert!(r.window(30, 5).is_empty());
        r.sort(None).unwrap();
        assert_eq!(r.sort_key(), None);
        assert_eq!(values(&r.window(8, 3)), [8, 9, 10]);
    }

    #[test]
    fn filters_combine_with_sorting_and_count_what_is_left() {
        let store = ResultStore::new();
        let s = schema();
        let id = store.create(s.clone());
        for (from, to) in [(0, 10), (10, 25)] {
            store.append(id, batch(&s, from, to));
        }
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        assert_eq!(r.filter(Some("1")), Err(ViewError::Incomplete));
        drop(r);
        store.complete(id);
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        // 1, 10..=19, 21: the text as the grid shows it.
        assert_eq!(r.filter(Some(" 1 ")), Ok(12));
        assert_eq!(r.rows(), 12);
        assert_eq!(r.total_rows(), 25);
        assert_eq!(r.filter_text(), Some("1"));
        assert_eq!(values(&r.window(0, 3)), [1, 10, 11]);
        r.sort(Some(SortKey {
            column: 0,
            descending: true,
        }))
        .unwrap();
        assert_eq!(values(&r.window(0, 3)), [21, 19, 18]);
        assert_eq!(values(&r.window(10, 5)), [10, 1]);
        assert_eq!(r.filter(Some("nothing")), Ok(0));
        assert!(r.window(0, 5).is_empty());
        assert_eq!(r.filter(Some("  ")), Ok(25));
        assert_eq!(r.filter_text(), None);
        assert_eq!(values(&r.window(0, 2)), [24, 23]);
    }

    #[test]
    fn column_filters_combine_with_the_text_filter_and_the_sort() {
        use crate::filter::FilterOp;
        let store = ResultStore::new();
        let s = schema();
        let id = store.create(s.clone());
        store.append(id, batch(&s, 0, 10));
        store.append(id, batch(&s, 10, 30));
        store.complete(id);
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        let filter = |op, value: &str| ColumnFilter {
            column: 0,
            op,
            value: value.into(),
        };
        // 10..=19 by value (not as text, where "9" > "19").
        assert_eq!(
            r.filter_columns(vec![filter(FilterOp::Ge, "10"), filter(FilterOp::Lt, "20")]),
            Ok(10)
        );
        assert_eq!(values(&r.window(0, 3)), [10, 11, 12]);
        assert_eq!(r.filter(Some("5")), Ok(1));
        assert_eq!(values(&r.window(0, 5)), [15]);
        r.filter(None).unwrap();
        r.sort(Some(SortKey {
            column: 0,
            descending: true,
        }))
        .unwrap();
        assert_eq!(values(&r.window(0, 2)), [19, 18]);
        assert_eq!(r.column_filters().len(), 2);
        assert_eq!(
            r.filter_columns(vec![filter(FilterOp::Gt, "many")]),
            Err(ViewError::NotANumber {
                column: 0,
                value: "many".into()
            })
        );
        // A refused filter leaves the previous ones in force.
        assert_eq!(r.rows(), 10);
        assert_eq!(
            r.filter_columns(vec![ColumnFilter {
                column: 3,
                op: FilterOp::IsNull,
                value: String::new()
            }]),
            Err(ViewError::NoSuchColumn(3))
        );
        assert_eq!(r.filter_columns(Vec::new()), Ok(30));
        assert_eq!(values(&r.window(0, 1)), [29]);
    }

    #[test]
    fn an_empty_result_sorts() {
        let store = ResultStore::new();
        let id = store.create(schema());
        store.complete(id);
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        r.sort(Some(SortKey {
            column: 0,
            descending: false,
        }))
        .unwrap();
        assert!(r.window(0, 10).is_empty());
    }

    #[test]
    fn results_can_be_removed() {
        let store = ResultStore::new();
        let id = store.create(schema());
        assert_eq!(store.len(), 1);
        assert!(store.remove(id));
        assert!(store.get(id).is_none());
        assert!(store.is_empty());
    }
}
