//! Result store (ADR-010): results live in the core as Arrow batches and the UI requests
//! windows of rows. The frontend never holds the full result.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use arrow::array::{Array, RecordBatch};
use arrow::compute::{concat, interleave_record_batch};
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use spelunk_driver_api::ResultSchema;

use crate::sort::{SortError, SortKey, sorted_rows};

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
    /// When sorted, the original row number shown at each position.
    order: Option<(SortKey, Vec<u32>)>,
}

impl StoredResult {
    fn new(schema: Arc<ResultSchema>) -> Self {
        Self {
            schema,
            batches: Vec::new(),
            starts: Vec::new(),
            rows: 0,
            complete: false,
            order: None,
        }
    }

    pub fn schema(&self) -> &Arc<ResultSchema> {
        &self.schema
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// No more rows will arrive.
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// The column the rows are sorted by, if any.
    pub fn sort_key(&self) -> Option<SortKey> {
        self.order.as_ref().map(|(key, _)| *key)
    }

    /// Sorts the rows by one column (`None` goes back to the order the server sent). Every
    /// later window, copy and export sees the new order.
    pub fn sort(&mut self, key: Option<SortKey>) -> Result<(), SortError> {
        let Some(key) = key else {
            self.order = None;
            return Ok(());
        };
        if !self.complete {
            return Err(SortError::Incomplete);
        }
        let meta = self
            .schema
            .columns
            .get(key.column)
            .ok_or(SortError::NoSuchColumn(key.column))?;
        if u32::try_from(self.rows).is_err() {
            return Err(SortError::TooManyRows);
        }
        let parts: Vec<&dyn Array> = self
            .batches
            .iter()
            .map(|b| b.column(key.column).as_ref())
            .collect();
        let rows = if parts.is_empty() {
            Vec::new()
        } else {
            let values = concat(&parts).map_err(|_| SortError::UnexpectedArray(key.column))?;
            sorted_rows(values.as_ref(), meta.logical, key)?
        };
        self.order = Some((key, rows));
        Ok(())
    }

    /// Rows `[start, start + len)` (clipped at the end), in the current order. Unsorted, the
    /// batches are slices without copying data; sorted, the window's rows are gathered.
    pub fn window(&self, start: usize, len: usize) -> Vec<RecordBatch> {
        let end = start.saturating_add(len).min(self.rows);
        if start >= end {
            return Vec::new();
        }
        if let Some((_, order)) = &self.order {
            let picks: Vec<(usize, usize)> = order[start..end]
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
        assert_eq!(r.sort(Some(down)), Err(SortError::Incomplete));
        drop(r);
        store.complete(id);
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        assert_eq!(
            r.sort(Some(SortKey {
                column: 1,
                descending: false
            })),
            Err(SortError::NoSuchColumn(1))
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
