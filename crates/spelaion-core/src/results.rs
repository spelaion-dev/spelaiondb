//! Result store (ADR-010): results live in the core as Arrow batches and the UI requests
//! windows of rows. The frontend never holds the full result.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use arrow::array::{Array, ArrayRef, RecordBatch};
use arrow::compute::{concat, interleave_record_batch};
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use spelaion_driver_api::{ResultSchema, Value};

use crate::chart::{self, ChartData, ChartError, ChartRequest};
use crate::edit::EditError;
use crate::filter::{ColumnFilter, matches, validate};
use crate::find::{FindMatches, FindQuery, FindTicket, search};
use crate::masking::{MaskPolicy, mask_batch, masked_schema};
use crate::pending::{EditOutcome, PendingEdit, PendingEdits};
use crate::selection_stats::{self, CellBlock, SelectionStats, StatsTicket};
use crate::sort::{SortKey, ViewError, sorted_instants, sorted_rows};

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
    /// Changes whenever the row shown at some position may change (rows appended, a sort,
    /// a filter): find matches of an older generation are stale.
    generation: u64,
    /// The matches of the last search (Ctrl+F) in the current view.
    find: Option<FindMatches>,
    /// The number of the latest search started, so a newer one stops an older one.
    find_latest: Arc<AtomicU64>,
    /// The number of the latest selection calculation started (or cancelled).
    stats_latest: Arc<AtomicU64>,
    /// The number of the latest chart started (or cancelled).
    chart_latest: Arc<AtomicU64>,
    /// The edits of a table data page not yet applied (ADR-044), by position in the view.
    pending: PendingEdits,
    /// Column masking (ADR-051): the masked columns, their original metadata, and per batch
    /// the original arrays that were replaced by the mask. Read only by [`Self::reveal`].
    masking: Option<Masked>,
    /// Where the rows were read (SDB-137); `None` when it is not known, and then nothing is
    /// written from these rows.
    source: Option<ResultSource>,
}

/// Where a result's rows were read (SDB-137): the session that ran the statement and that
/// session's database while it ran (`None` where the engine has no database to change).
/// Edit statements are written only through the same session, still in the same database
/// ([`crate::EditTarget`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSource {
    pub(crate) session: u64,
    pub(crate) database: Option<String>,
}

/// What a masked result keeps aside (ADR-051).
#[derive(Debug)]
struct Masked {
    /// Positions of the masked columns, in order.
    columns: Vec<usize>,
    /// The schema the session reported, before masking.
    original: Arc<ResultSchema>,
    /// Per batch (as `batches`), the original arrays of `columns`, in their order.
    values: Vec<Vec<ArrayRef>>,
}

/// One cell's original value, revealed on the user's request (ADR-051).
#[derive(Debug, Clone, PartialEq)]
pub struct RevealedCell {
    pub column: String,
    pub value: Value,
    /// The column's type before masking.
    pub logical: spelaion_driver_api::LogicalType,
    pub native_type: String,
}

/// Why a cell cannot be revealed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RevealError {
    #[error("there is no such cell")]
    NoSuchCell,
    #[error("the column is not masked")]
    NotMasked,
}

impl StoredResult {
    fn new(schema: Arc<ResultSchema>) -> Self {
        Self::with_masking(schema, None)
    }

    fn with_masking(schema: Arc<ResultSchema>, masking: Option<Masked>) -> Self {
        Self {
            masking,
            schema,
            batches: Vec::new(),
            starts: Vec::new(),
            rows: 0,
            complete: false,
            sort: None,
            filter: None,
            column_filters: None,
            view: None,
            generation: 0,
            find: None,
            find_latest: Arc::new(AtomicU64::new(0)),
            stats_latest: Arc::new(AtomicU64::new(0)),
            chart_latest: Arc::new(AtomicU64::new(0)),
            pending: PendingEdits::default(),
            source: None,
        }
    }

    /// Where the rows were read (SDB-137), if known.
    pub fn source(&self) -> Option<&ResultSource> {
        self.source.as_ref()
    }

    /// The edits of this page not yet applied (ADR-044).
    pub fn pending(&self) -> &PendingEdits {
        &self.pending
    }

    /// Does one edit to this page's pending edits (or undoes or redoes one).
    pub fn edit_pending(&mut self, edit: PendingEdit) -> Result<EditOutcome, EditError> {
        let mut pending = std::mem::take(&mut self.pending);
        let outcome = pending.edit(self, edit);
        self.pending = pending;
        outcome
    }

    /// Forgets the pending edits and their history (they were applied).
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Pending edits name rows by position: the view cannot change under them.
    fn check_unedited(&self) -> Result<(), ViewError> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err(ViewError::PendingEdits)
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
        if self.sort_key() != key {
            self.check_unedited()?;
        }
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
        if self.is_masked(key.column) {
            return Err(ViewError::Masked(meta.name.clone()));
        }
        let parts: Vec<&dyn Array> = self
            .batches
            .iter()
            .map(|b| b.column(key.column).as_ref())
            .collect();
        let rows = if parts.is_empty() {
            Vec::new()
        } else {
            let values = concat(&parts).map_err(|_| ViewError::UnexpectedArray(key.column))?;
            match crate::temporal::text_temporal(meta) {
                Some(spelaion_driver_api::LogicalType::TimestampTz) => {
                    sorted_instants(values.as_ref(), key)?
                }
                _ => sorted_rows(values.as_ref(), meta.logical, key)?,
            }
        };
        self.sort = Some((key, rows));
        self.rebuild_view();
        Ok(())
    }

    /// Keeps the rows where some cell's text (as the grid shows it) contains `text`,
    /// ignoring case; `None` or blank shows every row again. Returns the rows left.
    pub fn filter(&mut self, text: Option<&str>) -> Result<usize, ViewError> {
        let text = text.map(str::trim).filter(|t| !t.is_empty());
        if self.filter_text() != text {
            self.check_unedited()?;
        }
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
        if self.column_filters() != filters.as_slice() {
            self.check_unedited()?;
        }
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

    /// The batch holding an original row, and the row's index inside it.
    pub(crate) fn locate(&self, row: usize) -> (&RecordBatch, usize) {
        let batch = self.starts.partition_point(|&s| s <= row) - 1;
        (&self.batches[batch], row - self.starts[batch])
    }

    /// The original row shown at a position of the current view.
    pub(crate) fn original_row(&self, position: usize) -> usize {
        self.view
            .as_ref()
            .map_or(position, |view| view[position] as usize)
    }

    /// The typed value of a cell of the current view (`None` past the end).
    pub fn value_at(&self, position: usize, column: usize) -> Option<Value> {
        if position >= self.rows() {
            return None;
        }
        let meta = self.schema.columns.get(column)?;
        let (batch, row) = self.locate(self.original_row(position));
        Some(
            Value::from_array(batch.column(column).as_ref(), row, meta.logical)
                .unwrap_or(Value::Null),
        )
    }

    /// Whether a column of this result is masked (ADR-051).
    pub fn is_masked(&self, column: usize) -> bool {
        self.masking
            .as_ref()
            .is_some_and(|m| m.columns.contains(&column))
    }

    /// The names of the masked columns, in order.
    pub fn masked_columns(&self) -> Vec<String> {
        self.masking.as_ref().map_or_else(Vec::new, |m| {
            m.columns
                .iter()
                .map(|&c| self.schema.columns[c].name.clone())
                .collect()
        })
    }

    /// The original value of one masked cell of the current view (ADR-051): the user's
    /// explicit, local request to see it. Nothing runs, and the value is never put back into
    /// the result, so every window, copy and export stays masked.
    pub fn reveal(&self, position: usize, column: usize) -> Result<RevealedCell, RevealError> {
        if position >= self.rows() || column >= self.schema.columns.len() {
            return Err(RevealError::NoSuchCell);
        }
        let masking = self.masking.as_ref().ok_or(RevealError::NotMasked)?;
        let slot = masking
            .columns
            .iter()
            .position(|&c| c == column)
            .ok_or(RevealError::NotMasked)?;
        let row = self.original_row(position);
        let batch = self.starts.partition_point(|&s| s <= row) - 1;
        let inside = row - self.starts[batch];
        let meta = &masking.original.columns[column];
        let array = &masking.values[batch][slot];
        Ok(RevealedCell {
            column: meta.name.clone(),
            value: Value::from_array(array.as_ref(), inside, meta.logical).unwrap_or(Value::Null),
            logical: meta.logical,
            native_type: meta.native_type.clone(),
        })
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Rows changed place or were added: the search matches no longer point at them.
    fn moved(&mut self) {
        self.generation += 1;
        self.find = None;
    }

    /// Starts a search: one still running on this result stops (its [`Self::find_matches`]
    /// returns `None`).
    pub fn find_ticket(&self) -> FindTicket {
        FindTicket::new(&self.find_latest)
    }

    /// Searches the loaded rows for the query's text, in the current view's order, without
    /// changing anything (a read lock is enough, so windows keep being served meanwhile).
    /// Keep the result with [`Self::keep_find`]. `None`: a newer search replaced this one.
    pub fn find_matches(
        &self,
        query: &FindQuery,
        ticket: &FindTicket,
    ) -> Result<Option<FindMatches>, ViewError> {
        self.check_viewable()?;
        search(self, query, ticket)
    }

    /// Keeps the matches of a search as the current ones, unless the rows moved since it ran
    /// or a newer search started (then `false`, and nothing changes).
    pub fn keep_find(&mut self, matches: FindMatches, ticket: &FindTicket) -> bool {
        if matches.generation != self.generation || ticket.is_stale() {
            return false;
        }
        self.find = Some(matches);
        true
    }

    /// The matches of the last search, if the rows have not moved since.
    pub fn find(&self) -> Option<&FindMatches> {
        self.find.as_ref()
    }

    /// Forgets the search (the find bar was closed or emptied).
    pub fn clear_find(&mut self) {
        self.find = None;
    }

    /// Starts a selection calculation: one still running on this result stops (its
    /// [`Self::selection_stats`] returns `None`).
    pub fn stats_ticket(&self) -> StatsTicket {
        StatsTicket::new(&self.stats_latest)
    }

    /// Stops the selection calculation running on this result, if any.
    pub fn cancel_stats(&self) {
        selection_stats::cancel(&self.stats_latest);
    }

    /// The numbers about a block of cells of the current view (the calc panel), with a read
    /// lock only. `None`: a newer calculation, or a cancel, stopped this one.
    pub fn selection_stats(
        &self,
        block: &CellBlock,
        ticket: &StatsTicket,
    ) -> Result<Option<SelectionStats>, ViewError> {
        self.check_viewable()?;
        selection_stats::compute(self, block, ticket)
    }

    /// Starts preparing a chart: one still being prepared on this result stops (its
    /// [`Self::chart`] returns `None`).
    pub fn chart_ticket(&self) -> StatsTicket {
        StatsTicket::new(&self.chart_latest)
    }

    /// Stops the chart being prepared on this result, if any.
    pub fn cancel_chart(&self) {
        selection_stats::cancel(&self.chart_latest);
    }

    /// The points of a chart over the current view (ADR-040), with a read lock only. `None`:
    /// a newer chart, or a cancel, stopped this one.
    pub fn chart(
        &self,
        request: &ChartRequest,
        ticket: &StatsTicket,
    ) -> Result<Option<ChartData>, ChartError> {
        self.check_viewable()?;
        chart::prepare(self, request, ticket)
    }

    /// The order of the sort (or the server's) keeping only the rows both filters let
    /// through.
    fn rebuild_view(&mut self) {
        self.moved();
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

    /// Creates a result whose columns `policy` masks (ADR-051): every batch appended to it is
    /// masked before it is kept. Returns its id and the schema it shows (masked columns as
    /// text, marked `spelaion.masked`).
    pub(crate) fn create_masked(
        &self,
        schema: Arc<ResultSchema>,
        policy: &MaskPolicy,
    ) -> (ResultId, Arc<ResultSchema>) {
        let (shown, columns) = masked_schema(policy, &schema);
        if columns.is_empty() {
            return (self.create(schema), shown);
        }
        let id = ResultId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let masking = Masked {
            columns,
            original: schema,
            values: Vec::new(),
        };
        self.lock().insert(
            id,
            Arc::new(RwLock::new(StoredResult::with_masking(
                shown.clone(),
                Some(masking),
            ))),
        );
        (id, shown)
    }

    /// Adds a batch and returns the total number of rows. In a masked result the batch is
    /// masked first, and its original columns are kept aside.
    pub(crate) fn append(&self, id: ResultId, batch: RecordBatch) -> Option<usize> {
        let result = self.get(id)?;
        let mut guard = result.write().expect("result lock poisoned");
        let r = &mut *guard;
        let batch = match &mut r.masking {
            None => batch,
            Some(masking) => {
                let (masked, originals) = mask_batch(&batch, &masking.columns, &r.schema);
                masking.values.push(originals);
                masked
            }
        };
        let start = r.rows;
        r.starts.push(start);
        r.rows += batch.num_rows();
        r.batches.push(batch);
        r.moved();
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
        if page.masking.as_ref().map(|m| &m.columns) != target.masking.as_ref().map(|m| &m.columns)
        {
            return Err("the masked columns changed while fetching more rows".into());
        }
        if let (Some(from), Some(to)) = (&page.masking, &mut target.masking) {
            to.values.extend(from.values.iter().cloned());
        }
        let added = page.rows;
        for batch in &page.batches {
            let start = target.rows;
            target.starts.push(start);
            target.rows += batch.num_rows();
            target.batches.push(batch.clone());
        }
        target.moved();
        Ok(added)
    }

    pub(crate) fn complete(&self, id: ResultId) {
        if let Some(result) = self.get(id) {
            result.write().expect("result lock poisoned").complete = true;
        }
    }

    /// Records where a result's rows were read, or that it is not known (SDB-137).
    pub(crate) fn set_source(&self, id: ResultId, source: Option<ResultSource>) {
        if let Some(result) = self.get(id) {
            result.write().expect("result lock poisoned").source = source;
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
    use spelaion_driver_api::{ColumnMeta, LogicalType};

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
                .get("spelaion.native_type")
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
