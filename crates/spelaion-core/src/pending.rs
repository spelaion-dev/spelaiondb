//! The pending edits of a table data page (ADR-044): cells typed or changed in bulk, rows marked
//! for deletion and rows to insert, kept in the core with the stored result they refer to until
//! they are applied (`edit.rs`, ADR-014) or discarded. The UI only shows them and sends what
//! the user does; every change, even a single cell typed, is a step that undo and redo restore
//! exactly.
//!
//! Rows and columns are positions in the result shown, as `edit_statements` reads them; the
//! stored result refuses to sort or filter while edits are pending, so a position keeps naming
//! the same row until they are applied or discarded.

use std::collections::{BTreeMap, BTreeSet};

use spelaion_driver_api::{LogicalType, Value};

use crate::edit::{CellInput, EditError, RowChange, check_input};
use crate::results::StoredResult;

/// New values planned for cells, by row and column (`None` takes a cell back).
type Planned = Vec<(usize, usize, Option<CellInput>)>;

/// Undo steps kept; older ones are forgotten.
pub const MAX_UNDO_STEPS: usize = 500;
/// The most text a paste reads (a block copied from a spreadsheet, as tab-separated text).
pub const MAX_PASTE_BYTES: usize = 16 * 1024 * 1024;

/// A rectangle of the page: rows `first_row..=last_row` of `columns` (the shown ones, in the
/// order shown; hidden columns are never touched).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub first_row: usize,
    pub last_row: usize,
    pub columns: Vec<usize>,
}

/// The value of a cell given by the user (`None` takes it back to its value in the database).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellEdit {
    pub row: usize,
    pub column: usize,
    pub value: Option<CellInput>,
}

/// What a bulk edit does to each cell of a block. Rows marked for deletion are left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BulkAction {
    /// The same value (NULL and DEFAULT included) in every cell.
    Set(CellInput),
    /// Each column's value in the block's first row copied to the rows below it; a block of one
    /// row copies the row above it.
    FillDown,
    /// Spaces removed from both ends (text columns only).
    Trim,
    UpperCase,
    LowerCase,
    /// A new random UUID in each cell.
    NewUuid,
    /// The current date and time, written for each column's type (one instant for all).
    Now,
}

/// One thing the user did to the pending edits of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingEdit {
    /// Values for some cells, as one step.
    SetCells(Vec<CellEdit>),
    /// A keystroke in a cell's text box: typing on in the same cell joins the previous step,
    /// so undo takes back the whole value typed, not one character.
    Type(CellEdit),
    /// Marks rows for deletion.
    Delete(Vec<usize>),
    /// Takes rows back to the database: their cells and their deletion.
    Revert(Vec<usize>),
    /// A row to insert, with only the columns given (the others take their default).
    Insert(Vec<(usize, CellInput)>),
    UpdateInsert {
        index: usize,
        values: Vec<(usize, CellInput)>,
    },
    RemoveInsert(usize),
    /// Every pending edit dropped, as one step that undo brings back.
    Discard,
    Bulk {
        block: Block,
        action: BulkAction,
    },
    /// Tab-separated text (a block copied from the grid or a spreadsheet). One value fills
    /// `selection`; a larger block is pasted from the selection's first cell down the page and
    /// through `columns` (the shown columns from the selection's first one, in order), and
    /// whatever falls past the page or the last column is left out.
    Paste {
        selection: Block,
        columns: Vec<usize>,
        text: String,
    },
    Undo,
    Redo,
}

/// What one edit did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EditOutcome {
    /// Cells, deletions and new rows that changed (for undo and redo: restored).
    pub changed: usize,
    /// Pasted values that fell past the last row or column and were left out.
    pub clipped: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    Cell {
        row: usize,
        column: usize,
        before: Option<CellInput>,
        after: Option<CellInput>,
    },
    Deleted {
        row: usize,
        after: bool,
    },
    Inserts {
        before: Vec<Vec<(usize, CellInput)>>,
        after: Vec<Vec<(usize, CellInput)>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Step {
    changes: Vec<Change>,
    /// The cell this step typed into, while typing there may still join it.
    typing: Option<(usize, usize)>,
}

/// The pending edits of one page, and their undo and redo history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingEdits {
    cells: BTreeMap<(usize, usize), CellInput>,
    deleted: BTreeSet<usize>,
    inserts: Vec<Vec<(usize, CellInput)>>,
    undo: Vec<Step>,
    redo: Vec<Step>,
}

impl PendingEdits {
    /// Nothing is pending (the history may still hold steps to redo).
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty() && self.deleted.is_empty() && self.inserts.is_empty()
    }

    /// Pending cell values by row and column, in order.
    pub fn cells(&self) -> impl Iterator<Item = (usize, usize, &CellInput)> {
        self.cells
            .iter()
            .map(|(&(row, column), input)| (row, column, input))
    }

    pub fn deleted(&self) -> impl Iterator<Item = usize> + '_ {
        self.deleted.iter().copied()
    }

    pub fn inserts(&self) -> &[Vec<(usize, CellInput)>] {
        &self.inserts
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Rows with a pending value that are not marked for deletion.
    pub fn updated_rows(&self) -> usize {
        self.cells
            .keys()
            .map(|(row, _)| *row)
            .filter(|row| !self.deleted.contains(row))
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// The changes `edit_statements` writes: one UPDATE per edited row (in row order, a row
    /// marked for deletion only deleted), then the deletions, then the new rows.
    pub fn changes(&self) -> Vec<RowChange> {
        let mut updates: BTreeMap<usize, Vec<(usize, CellInput)>> = BTreeMap::new();
        for (&(row, column), input) in &self.cells {
            if !self.deleted.contains(&row) {
                updates
                    .entry(row)
                    .or_default()
                    .push((column, input.clone()));
            }
        }
        updates
            .into_iter()
            .map(|(row, values)| RowChange::Update { row, values })
            .chain(self.deleted.iter().map(|&row| RowChange::Delete { row }))
            .chain(self.inserts.iter().map(|values| RowChange::Insert {
                values: values.clone(),
            }))
            .collect()
    }

    /// Forgets everything, history included (after the edits were applied).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Does `edit` to the edits of `result`'s page. A refused edit (a bulk value or a pasted
    /// one the column cannot take, a row past the page) changes nothing.
    pub fn edit(
        &mut self,
        result: &StoredResult,
        edit: PendingEdit,
    ) -> Result<EditOutcome, EditError> {
        let page = Page { result };
        let mut typing = None;
        let mut clipped = 0;
        let changes = match edit {
            PendingEdit::Undo => return Ok(self.undo()),
            PendingEdit::Redo => return Ok(self.redo()),
            // What the user types is checked when it is applied, so typing is never
            // interrupted half way (`-` before `-5`, `tr` before `true`).
            PendingEdit::SetCells(cells) => {
                let mut planned = Vec::with_capacity(cells.len());
                for cell in cells {
                    page.check_cell(cell.row, cell.column)?;
                    planned.push((cell.row, cell.column, cell.value));
                }
                self.cell_changes(&page, planned)
            }
            PendingEdit::Type(cell) => {
                page.check_cell(cell.row, cell.column)?;
                typing = Some((cell.row, cell.column));
                self.cell_changes(&page, vec![(cell.row, cell.column, cell.value)])
            }
            PendingEdit::Delete(rows) => {
                for &row in &rows {
                    page.check_row(row)?;
                }
                let rows: BTreeSet<usize> = rows.into_iter().collect();
                rows.into_iter()
                    .filter(|row| !self.deleted.contains(row))
                    .map(|row| Change::Deleted { row, after: true })
                    .collect()
            }
            PendingEdit::Revert(rows) => {
                let rows: BTreeSet<usize> = rows.into_iter().collect();
                let mut changes: Vec<Change> = self
                    .cells
                    .iter()
                    .filter(|((row, _), _)| rows.contains(row))
                    .map(|(&(row, column), input)| Change::Cell {
                        row,
                        column,
                        before: Some(input.clone()),
                        after: None,
                    })
                    .collect();
                changes.extend(
                    rows.iter()
                        .filter(|row| self.deleted.contains(row))
                        .map(|&row| Change::Deleted { row, after: false }),
                );
                changes
            }
            PendingEdit::Insert(values) => {
                page.check_insert(&values)?;
                let mut after = self.inserts.clone();
                after.push(values);
                self.insert_change(after)
            }
            PendingEdit::UpdateInsert { index, values } => {
                page.check_insert(&values)?;
                let mut after = self.inserts.clone();
                *after.get_mut(index).ok_or(EditError::NoSuchNewRow(index))? = values;
                self.insert_change(after)
            }
            PendingEdit::RemoveInsert(index) => {
                if index >= self.inserts.len() {
                    return Err(EditError::NoSuchNewRow(index));
                }
                let mut after = self.inserts.clone();
                after.remove(index);
                self.insert_change(after)
            }
            PendingEdit::Discard => {
                let mut changes: Vec<Change> = self
                    .cells
                    .iter()
                    .map(|(&(row, column), input)| Change::Cell {
                        row,
                        column,
                        before: Some(input.clone()),
                        after: None,
                    })
                    .collect();
                changes.extend(
                    self.deleted
                        .iter()
                        .map(|&row| Change::Deleted { row, after: false }),
                );
                changes.extend(self.insert_change(Vec::new()));
                changes
            }
            PendingEdit::Bulk { block, action } => {
                page.check_block(&block)?;
                // Even a bulk edit that would leave the mask as it is (trim) acts on it.
                for &column in &block.columns {
                    page.check_unmasked(column)?;
                }
                let planned = self.bulk(&page, &block, &action)?;
                self.cell_changes(&page, planned)
            }
            PendingEdit::Paste {
                selection,
                columns,
                text,
            } => {
                page.check_block(&selection)?;
                for &column in &columns {
                    page.check_cell(selection.first_row, column)?;
                }
                let (planned, left_out) = self.paste(&page, &selection, &columns, &text)?;
                clipped = left_out;
                self.cell_changes(&page, planned)
            }
        };
        // A masked cell holds the mask, not its value (ADR-051): editing it, or computing a
        // value from it (fill down, trim, case), would write the mask over the data. Taking a
        // pending value back (Revert, Discard, undo) is not an edit of the cell.
        for change in &changes {
            if let Change::Cell {
                column,
                after: Some(_),
                ..
            } = change
            {
                page.check_unmasked(*column)?;
            }
        }
        let changed = changes.len();
        if changed > 0 {
            self.record(changes, typing);
        }
        Ok(EditOutcome { changed, clipped })
    }

    /// The changes that give these cells these values, skipping rows marked for deletion and
    /// values that are already there. A value equal to the database's takes the cell back.
    fn cell_changes(&self, page: &Page, planned: Planned) -> Vec<Change> {
        let mut seen = BTreeMap::new();
        for (row, column, value) in planned {
            if self.deleted.contains(&row) {
                continue;
            }
            let after = value.filter(|input| !page.is_original(row, column, input));
            seen.insert((row, column), after);
        }
        seen.into_iter()
            .filter_map(|((row, column), after)| {
                let before = self.cells.get(&(row, column)).cloned();
                (before != after).then_some(Change::Cell {
                    row,
                    column,
                    before,
                    after,
                })
            })
            .collect()
    }

    fn insert_change(&self, after: Vec<Vec<(usize, CellInput)>>) -> Vec<Change> {
        if after == self.inserts {
            Vec::new()
        } else {
            vec![Change::Inserts {
                before: self.inserts.clone(),
                after,
            }]
        }
    }

    /// The value a cell shows now: its pending value, or the database's.
    fn current(&self, page: &Page, row: usize, column: usize) -> CellInput {
        self.cells
            .get(&(row, column))
            .cloned()
            .unwrap_or_else(|| page.original(row, column))
    }

    fn bulk(&self, page: &Page, block: &Block, action: &BulkAction) -> Result<Planned, EditError> {
        let rows = block.first_row..=block.last_row;
        let cells = || {
            rows.clone()
                .flat_map(|row| block.columns.iter().map(move |&column| (row, column)))
        };
        let mut planned = Vec::new();
        match action {
            BulkAction::Set(input) => {
                for &column in &block.columns {
                    page.check_value(column, input)?;
                }
                planned.extend(cells().map(|(row, column)| (row, column, Some(input.clone()))));
            }
            BulkAction::FillDown => {
                let (source, targets) = if block.first_row == block.last_row {
                    match block.first_row.checked_sub(1) {
                        Some(above) => (above, block.first_row..=block.last_row),
                        None => return Ok(Vec::new()),
                    }
                } else {
                    (block.first_row, block.first_row + 1..=block.last_row)
                };
                for &column in &block.columns {
                    let value = self.current(page, source, column);
                    planned.extend(
                        targets
                            .clone()
                            .map(|row| (row, column, Some(value.clone()))),
                    );
                }
            }
            BulkAction::Trim | BulkAction::UpperCase | BulkAction::LowerCase => {
                for (row, column) in cells() {
                    if !page.is_text(column) {
                        continue;
                    }
                    let CellInput::Text(text) = self.current(page, row, column) else {
                        continue;
                    };
                    let changed = match action {
                        BulkAction::Trim => text.trim().to_string(),
                        BulkAction::UpperCase => text.to_uppercase(),
                        _ => text.to_lowercase(),
                    };
                    planned.push((row, column, Some(CellInput::Text(changed))));
                }
            }
            BulkAction::NewUuid => {
                for (row, column) in cells() {
                    let input = CellInput::Text(uuid::Uuid::new_v4().to_string());
                    page.check_value(column, &input)?;
                    planned.push((row, column, Some(input)));
                }
            }
            BulkAction::Now => {
                let local = chrono::Local::now();
                let utc = local.with_timezone(&chrono::Utc);
                for &column in &block.columns {
                    let text = match page.date_logical(column) {
                        LogicalType::Date => local.format("%Y-%m-%d").to_string(),
                        LogicalType::Time => local.format("%H:%M:%S").to_string(),
                        // SQL Server's `datetimeoffset` is its text (SDB-265): where it is, with
                        // its offset, as the server writes it (it does not read `+00`).
                        LogicalType::TimestampTz if page.is_text_date(column) => {
                            local.format("%Y-%m-%d %H:%M:%S %:z").to_string()
                        }
                        // As the grid shows a time zone: UTC, `+00`.
                        LogicalType::TimestampTz => utc.format("%Y-%m-%d %H:%M:%S+00").to_string(),
                        LogicalType::Timestamp | LogicalType::Text | LogicalType::Native => {
                            local.format("%Y-%m-%d %H:%M:%S").to_string()
                        }
                        LogicalType::Int
                        | LogicalType::Float
                        | LogicalType::Decimal
                        | LogicalType::Bool
                        | LogicalType::Bytes
                        | LogicalType::Uuid
                        | LogicalType::Json
                        | LogicalType::Interval
                        | LogicalType::Array => {
                            return Err(EditError::BadValue {
                                column: page.name(column).to_string(),
                                value: local.format("%Y-%m-%d %H:%M:%S").to_string(),
                                expected: "a date or time column",
                            });
                        }
                    };
                    planned.extend(
                        rows.clone()
                            .map(|row| (row, column, Some(CellInput::Text(text.clone())))),
                    );
                }
            }
        }
        Ok(planned)
    }

    fn paste(
        &self,
        page: &Page,
        selection: &Block,
        columns: &[usize],
        text: &str,
    ) -> Result<(Planned, usize), EditError> {
        if text.len() > MAX_PASTE_BYTES {
            return Err(EditError::TooMuchToPaste(MAX_PASTE_BYTES));
        }
        let records = parse_block(text);
        let mut planned = Vec::new();
        let mut clipped = 0;
        let value_for = |column: usize, field: &str| {
            // The grid copies NULL as an empty field; a text column takes it as empty text.
            if field.is_empty() && !page.is_text(column) {
                CellInput::Null
            } else {
                CellInput::Text(field.to_string())
            }
        };
        if let [record] = records.as_slice()
            && let [field] = record.as_slice()
        {
            for row in selection.first_row..=selection.last_row {
                for &column in &selection.columns {
                    let input = value_for(column, field);
                    page.check_value(column, &input)?;
                    planned.push((row, column, Some(input)));
                }
            }
            return Ok((planned, 0));
        }
        for (i, record) in records.iter().enumerate() {
            let row = selection.first_row + i;
            for (j, field) in record.iter().enumerate() {
                match columns.get(j) {
                    Some(&column) if row < page.rows() => {
                        let input = value_for(column, field);
                        page.check_value(column, &input)?;
                        planned.push((row, column, Some(input)));
                    }
                    _ => clipped += 1,
                }
            }
        }
        Ok((planned, clipped))
    }

    fn record(&mut self, changes: Vec<Change>, typing: Option<(usize, usize)>) {
        self.apply(&changes, true);
        self.redo.clear();
        // Typing on in the same cell joins the step that started typing there.
        if let (Some(cell), Some(last)) = (typing, self.undo.last_mut())
            && last.typing == Some(cell)
            && let ([Change::Cell { after, .. }], [Change::Cell { after: next, .. }]) =
                (last.changes.as_mut_slice(), changes.as_slice())
        {
            after.clone_from(next);
            return;
        }
        self.undo.push(Step { changes, typing });
        if self.undo.len() > MAX_UNDO_STEPS {
            self.undo.remove(0);
        }
    }

    fn undo(&mut self) -> EditOutcome {
        let Some(mut step) = self.undo.pop() else {
            return EditOutcome::default();
        };
        self.apply(&step.changes, false);
        step.typing = None;
        let changed = step.changes.len();
        self.redo.push(step);
        EditOutcome {
            changed,
            clipped: 0,
        }
    }

    fn redo(&mut self) -> EditOutcome {
        let Some(step) = self.redo.pop() else {
            return EditOutcome::default();
        };
        self.apply(&step.changes, true);
        let changed = step.changes.len();
        self.undo.push(step);
        EditOutcome {
            changed,
            clipped: 0,
        }
    }

    /// Applies each change forward (its `after`) or backward (its `before`, in reverse order).
    fn apply(&mut self, changes: &[Change], forward: bool) {
        let mut each = |change: &Change| match change {
            Change::Cell {
                row,
                column,
                before,
                after,
            } => {
                let value = if forward { after } else { before };
                match value {
                    Some(input) => self.cells.insert((*row, *column), input.clone()),
                    None => self.cells.remove(&(*row, *column)),
                };
            }
            Change::Deleted { row, after } => {
                if *after == forward {
                    self.deleted.insert(*row);
                } else {
                    self.deleted.remove(row);
                }
            }
            Change::Inserts { before, after } => {
                self.inserts = if forward { after } else { before }.clone();
            }
        };
        if forward {
            changes.iter().for_each(&mut each);
        } else {
            changes.iter().rev().for_each(&mut each);
        }
    }
}

/// The stored page the edits refer to.
struct Page<'a> {
    result: &'a StoredResult,
}

impl Page<'_> {
    fn rows(&self) -> usize {
        self.result.rows()
    }

    fn name(&self, column: usize) -> &str {
        self.result
            .schema()
            .columns
            .get(column)
            .map_or("", |c| c.name.as_str())
    }

    fn logical(&self, column: usize) -> LogicalType {
        self.result
            .schema()
            .columns
            .get(column)
            .map_or(LogicalType::Native, |c| c.logical)
    }

    /// The column's logical type, with a text column that holds dates (MySQL's `DATE`,
    /// `DATETIME`, `TIMESTAMP`, SDB-122; SQL Server's `datetime2` and `datetimeoffset`,
    /// SDB-265) read as its date type: what "now" is written as, and
    /// which columns are free text. Inputs are still checked against the column's own type.
    fn date_logical(&self, column: usize) -> LogicalType {
        self.result
            .schema()
            .columns
            .get(column)
            .map_or(LogicalType::Native, crate::temporal::effective_logical)
    }

    /// A text column that holds dates (`temporal.rs`).
    fn is_text_date(&self, column: usize) -> bool {
        self.result
            .schema()
            .columns
            .get(column)
            .and_then(crate::temporal::text_temporal)
            .is_some()
    }

    /// Columns whose values are free text: the ones trimming and case changes touch.
    fn is_text(&self, column: usize) -> bool {
        matches!(
            self.date_logical(column),
            LogicalType::Text | LogicalType::Native
        )
    }

    fn check_row(&self, row: usize) -> Result<(), EditError> {
        if row < self.rows() {
            Ok(())
        } else {
            Err(EditError::NoSuchRow(row))
        }
    }

    fn check_column(&self, column: usize) -> Result<(), EditError> {
        if column < self.result.schema().columns.len() {
            Ok(())
        } else {
            Err(EditError::NotATableColumn(format!("#{column}")))
        }
    }

    fn check_cell(&self, row: usize, column: usize) -> Result<(), EditError> {
        self.check_row(row)?;
        self.check_column(column)
    }

    fn check_block(&self, block: &Block) -> Result<(), EditError> {
        if block.first_row > block.last_row {
            return Err(EditError::NoSuchRow(block.first_row));
        }
        self.check_row(block.last_row)?;
        block
            .columns
            .iter()
            .try_for_each(|&column| self.check_column(column))
    }

    fn check_insert(&self, values: &[(usize, CellInput)]) -> Result<(), EditError> {
        values.iter().try_for_each(|(column, input)| {
            self.check_column(*column)?;
            // A new row's masked column is typed by the user; the mask itself, copied from a
            // row of the page (duplicating it), would be written as data (ADR-051).
            if self.result.is_masked(*column)
                && matches!(input, CellInput::Text(text) if text == crate::masking::MASK)
            {
                return Err(EditError::Masked(self.name(*column).to_string()));
            }
            Ok(())
        })
    }

    /// Masked cells cannot be edited (ADR-051).
    fn check_unmasked(&self, column: usize) -> Result<(), EditError> {
        if self.result.is_masked(column) {
            Err(EditError::Masked(self.name(column).to_string()))
        } else {
            Ok(())
        }
    }

    /// Refuses a value of a bulk edit or a paste that the column's type cannot take, before
    /// any of them becomes pending.
    fn check_value(&self, column: usize, input: &CellInput) -> Result<(), EditError> {
        check_input(self.name(column), self.logical(column), input)
    }

    /// The database's value of a cell, as an input.
    fn original(&self, row: usize, column: usize) -> CellInput {
        match self.result.value_at(row, column) {
            None | Some(Value::Null) => CellInput::Null,
            Some(Value::Bytes(bytes)) => CellInput::Bytes(bytes),
            Some(value) => CellInput::Text(value.to_string()),
        }
    }

    fn is_original(&self, row: usize, column: usize, input: &CellInput) -> bool {
        match input {
            CellInput::Default => false,
            other => *other == self.original(row, column),
        }
    }
}

/// Tab-separated text as spreadsheets and the grid copy it: records end at a line break
/// (`\n` or `\r\n`, a final one ignored), fields at a tab, and a field that starts with `"`
/// is quoted (it may hold tabs and line breaks; `""` is a quote).
pub fn parse_block(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut at_field_start = true;
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_field_start => {
                quoted = true;
                at_field_start = false;
            }
            '\t' => {
                record.push(std::mem::take(&mut field));
                at_field_start = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                record.push(std::mem::take(&mut field));
                records.push(std::mem::take(&mut record));
                at_field_start = true;
            }
            other => {
                field.push(other);
                at_field_start = false;
            }
        }
    }
    if !at_field_start || !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use spelaion_driver_api::{ColumnMeta, ResultSchema};

    use super::*;
    use crate::ResultStore;
    use crate::filter::{ColumnFilter, FilterOp};
    use crate::results::ResultId;
    use crate::sort::{SortKey, ViewError};

    /// A page of `id` (int), `name` and `born` (text), rows 1 to 6.
    fn page() -> (ResultStore, ResultId) {
        let meta = |name: &str, logical| ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
            origin: None,
        };
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int),
            meta("name", LogicalType::Text),
            meta("born", LogicalType::Text),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![1, 2, 3, 4, 5, 6])),
            Arc::new(StringArray::from(vec![
                Some("  Ana "),
                Some("bob"),
                None,
                Some("Iñaki"),
                Some("eve"),
                Some("x"),
            ])),
            Arc::new(StringArray::from(vec![Some("2001-01-01"); 6])),
        ];
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
            )
            .unwrap();
        store.complete(id);
        (store, id)
    }

    type Shown = (
        Vec<(usize, usize, CellInput)>,
        Vec<usize>,
        Vec<Vec<(usize, CellInput)>>,
    );

    fn shown(pending: &PendingEdits) -> Shown {
        (
            pending
                .cells()
                .map(|(r, c, input)| (r, c, input.clone()))
                .collect(),
            pending.deleted().collect(),
            pending.inserts().to_vec(),
        )
    }

    fn text(value: &str) -> CellInput {
        CellInput::Text(value.into())
    }

    fn block(first_row: usize, last_row: usize, columns: &[usize]) -> Block {
        Block {
            first_row,
            last_row,
            columns: columns.to_vec(),
        }
    }

    #[test]
    fn every_edit_is_a_step_that_undo_and_redo_restore_exactly() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        let mut stored = stored.write().unwrap();
        let edits = vec![
            PendingEdit::SetCells(vec![CellEdit {
                row: 0,
                column: 1,
                value: Some(text("Ana")),
            }]),
            PendingEdit::Bulk {
                block: block(0, 5, &[1]),
                action: BulkAction::UpperCase,
            },
            PendingEdit::Delete(vec![3, 4]),
            PendingEdit::Insert(vec![(1, text("new"))]),
            PendingEdit::Bulk {
                block: block(1, 2, &[1, 2]),
                action: BulkAction::Set(CellInput::Default),
            },
            PendingEdit::Revert(vec![4]),
            PendingEdit::Paste {
                selection: block(4, 4, &[1]),
                columns: vec![1, 2],
                text: "p\t2002-02-02\nq\t2003-03-03\nr\tlost\n".into(),
            },
            PendingEdit::UpdateInsert {
                index: 0,
                values: vec![(1, CellInput::Null)],
            },
            PendingEdit::Discard,
        ];
        let mut states = vec![shown(stored.pending())];
        for edit in edits {
            let outcome = stored.edit_pending(edit.clone()).unwrap();
            assert!(outcome.changed > 0, "{edit:?} changed nothing");
            states.push(shown(stored.pending()));
        }
        assert!(stored.pending().is_empty());
        for expected in states.iter().rev().skip(1) {
            stored.edit_pending(PendingEdit::Undo).unwrap();
            assert_eq!(&shown(stored.pending()), expected);
        }
        assert!(!stored.pending().can_undo());
        assert_eq!(
            stored.edit_pending(PendingEdit::Undo).unwrap(),
            EditOutcome::default()
        );
        for expected in states.iter().skip(1) {
            stored.edit_pending(PendingEdit::Redo).unwrap();
            assert_eq!(&shown(stored.pending()), expected);
        }
        assert!(!stored.pending().can_redo());
        // A new edit after an undo forgets what could be redone.
        stored.edit_pending(PendingEdit::Undo).unwrap();
        stored.edit_pending(PendingEdit::Delete(vec![0])).unwrap();
        assert!(!stored.pending().can_redo());
    }

    #[test]
    fn bulk_actions_compute_from_the_values_shown() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        let mut stored = stored.write().unwrap();
        stored
            .edit_pending(PendingEdit::SetCells(vec![CellEdit {
                row: 1,
                column: 1,
                value: Some(text("  Bob  ")),
            }]))
            .unwrap();
        // Trim reads the pending value of row 1, and NULL stays NULL.
        let trimmed = stored
            .edit_pending(PendingEdit::Bulk {
                block: block(0, 2, &[1]),
                action: BulkAction::Trim,
            })
            .unwrap();
        assert_eq!(trimmed.changed, 2);
        assert_eq!(
            shown(stored.pending()).0,
            vec![(0, 1, text("Ana")), (1, 1, text("Bob"))]
        );
        // Fill down copies the block's first row; one row copies the row above.
        stored
            .edit_pending(PendingEdit::Bulk {
                block: block(1, 3, &[1]),
                action: BulkAction::FillDown,
            })
            .unwrap();
        stored
            .edit_pending(PendingEdit::Bulk {
                block: block(4, 4, &[1]),
                action: BulkAction::FillDown,
            })
            .unwrap();
        assert_eq!(
            shown(stored.pending()).0,
            vec![
                (0, 1, text("Ana")),
                (1, 1, text("Bob")),
                (2, 1, text("Bob")),
                (3, 1, text("Bob")),
                (4, 1, text("Bob")),
            ]
        );
        // A value equal to the database's takes the cell back.
        stored
            .edit_pending(PendingEdit::SetCells(vec![CellEdit {
                row: 4,
                column: 1,
                value: Some(text("EVE")),
            }]))
            .unwrap();
        stored
            .edit_pending(PendingEdit::Bulk {
                block: block(4, 4, &[1]),
                action: BulkAction::LowerCase,
            })
            .unwrap();
        assert!(!stored.pending().cells().any(|(r, c, _)| (r, c) == (4, 1)));
        // A new UUID per cell, all different; a number column refuses one, changing nothing.
        stored
            .edit_pending(PendingEdit::Bulk {
                block: block(0, 5, &[2]),
                action: BulkAction::NewUuid,
            })
            .unwrap();
        let uuids: BTreeSet<_> = stored
            .pending()
            .cells()
            .filter(|(_, c, _)| *c == 2)
            .map(|(_, _, v)| format!("{v:?}"))
            .collect();
        assert_eq!(uuids.len(), 6);
        let before = shown(stored.pending());
        for action in [BulkAction::NewUuid, BulkAction::Now] {
            assert!(matches!(
                stored.edit_pending(PendingEdit::Bulk {
                    block: block(0, 5, &[0]),
                    action,
                }),
                Err(EditError::BadValue { .. })
            ));
        }
        assert_eq!(shown(stored.pending()), before);
        // Rows marked for deletion are left out of bulk edits.
        stored.edit_pending(PendingEdit::Delete(vec![5])).unwrap();
        stored
            .edit_pending(PendingEdit::Bulk {
                block: block(5, 5, &[1]),
                action: BulkAction::Set(CellInput::Null),
            })
            .unwrap();
        assert!(!stored.pending().cells().any(|(r, c, _)| (r, c) == (5, 1)));
    }

    #[test]
    fn a_paste_fills_the_selection_or_lays_out_its_block() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        let mut stored = stored.write().unwrap();
        // One value fills the selection; an empty field is NULL in a number column.
        stored
            .edit_pending(PendingEdit::Paste {
                selection: block(0, 1, &[0, 1]),
                columns: vec![0, 1, 2],
                text: "\r\n".into(),
            })
            .unwrap();
        assert_eq!(
            shown(stored.pending()).0,
            vec![
                (0, 0, CellInput::Null),
                (0, 1, text("")),
                (1, 0, CellInput::Null),
                (1, 1, text("")),
            ]
        );
        stored.edit_pending(PendingEdit::Discard).unwrap();
        // A block goes down and right from the first cell; past the page it is left out.
        let outcome = stored
            .edit_pending(PendingEdit::Paste {
                selection: block(4, 4, &[1]),
                columns: vec![1, 2],
                text: "a\tb\tc\nd\te\tf\ng\th\ti\n".into(),
            })
            .unwrap();
        assert_eq!(outcome.clipped, 5);
        assert_eq!(
            shown(stored.pending()).0,
            vec![
                (4, 1, text("a")),
                (4, 2, text("b")),
                (5, 1, text("d")),
                (5, 2, text("e")),
            ]
        );
        // A value a column cannot take refuses the whole paste.
        let before = shown(stored.pending());
        assert!(matches!(
            stored.edit_pending(PendingEdit::Paste {
                selection: block(0, 0, &[0]),
                columns: vec![0, 1],
                text: "7\tok\nseven\tok".into(),
            }),
            Err(EditError::BadValue { .. })
        ));
        assert_eq!(shown(stored.pending()), before);
        assert!(matches!(
            stored.edit_pending(PendingEdit::Paste {
                selection: block(0, 0, &[1]),
                columns: vec![1],
                text: "x".repeat(MAX_PASTE_BYTES + 1),
            }),
            Err(EditError::TooMuchToPaste(_))
        ));
    }

    #[test]
    fn typing_in_one_cell_is_one_step() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        let mut stored = stored.write().unwrap();
        for typed in ["b", "bo", "bob!"] {
            stored
                .edit_pending(PendingEdit::Type(CellEdit {
                    row: 1,
                    column: 1,
                    value: Some(text(typed)),
                }))
                .unwrap();
        }
        stored
            .edit_pending(PendingEdit::Type(CellEdit {
                row: 4,
                column: 1,
                value: Some(text("E")),
            }))
            .unwrap();
        stored.edit_pending(PendingEdit::Undo).unwrap();
        assert_eq!(shown(stored.pending()).0, vec![(1, 1, text("bob!"))]);
        stored.edit_pending(PendingEdit::Undo).unwrap();
        assert!(stored.pending().is_empty());
        assert!(!stored.pending().can_undo());
    }

    #[test]
    fn edits_outside_the_page_change_nothing() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        let mut stored = stored.write().unwrap();
        assert_eq!(
            stored.edit_pending(PendingEdit::Delete(vec![6])),
            Err(EditError::NoSuchRow(6))
        );
        assert!(
            stored
                .edit_pending(PendingEdit::Bulk {
                    block: block(0, 1, &[3]),
                    action: BulkAction::Trim,
                })
                .is_err()
        );
        assert!(
            stored
                .edit_pending(PendingEdit::Bulk {
                    block: block(3, 1, &[1]),
                    action: BulkAction::Trim,
                })
                .is_err()
        );
        assert_eq!(
            stored.edit_pending(PendingEdit::RemoveInsert(0)),
            Err(EditError::NoSuchNewRow(0))
        );
        assert!(stored.pending().is_empty() && !stored.pending().can_undo());
    }

    #[test]
    fn the_view_cannot_change_under_pending_edits() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        let mut stored = stored.write().unwrap();
        stored.edit_pending(PendingEdit::Delete(vec![0])).unwrap();
        let key = SortKey {
            column: 1,
            descending: false,
        };
        assert_eq!(stored.sort(Some(key)), Err(ViewError::PendingEdits));
        assert_eq!(stored.filter(Some("bob")), Err(ViewError::PendingEdits));
        let names_with_a = ColumnFilter {
            column: 1,
            op: FilterOp::Contains,
            value: "a".into(),
        };
        assert_eq!(
            stored.filter_columns(vec![names_with_a.clone()]),
            Err(ViewError::PendingEdits)
        );
        // Asking for the view it already has is no change.
        assert_eq!(stored.sort(None), Ok(()));
        assert_eq!(stored.filter(Some("  ")), Ok(6));
        assert_eq!(stored.filter_columns(vec![]), Ok(6));
        stored.edit_pending(PendingEdit::Discard).unwrap();
        assert_eq!(stored.sort(Some(key)), Ok(()));
        assert_eq!(stored.sort(None), Ok(()));

        // A column filter on before the edits: neither removing it nor adding another moves
        // the rows the edits name (SDB-138).
        assert_eq!(stored.filter_columns(vec![names_with_a.clone()]), Ok(2));
        stored
            .edit_pending(PendingEdit::SetCells(vec![CellEdit {
                row: 1,
                column: 1,
                value: Some(text("changed")),
            }]))
            .unwrap();
        let ids_above_two = ColumnFilter {
            column: 0,
            op: FilterOp::Gt,
            value: "2".into(),
        };
        assert_eq!(stored.filter_columns(vec![]), Err(ViewError::PendingEdits));
        assert_eq!(
            stored.filter_columns(vec![names_with_a.clone(), ids_above_two]),
            Err(ViewError::PendingEdits)
        );
        assert_eq!(stored.filter_columns(vec![names_with_a]), Ok(2));
        // Position 1 of the filtered view is still id 4, `Iñaki`.
        assert_eq!(stored.value_at(1, 0), Some(Value::Int(4)));
        assert_eq!(
            stored.pending().changes(),
            vec![RowChange::Update {
                row: 1,
                values: vec![(1, text("changed"))],
            }]
        );
    }

    #[test]
    fn a_page_appended_under_pending_edits_leaves_them_on_their_rows() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        stored
            .write()
            .unwrap()
            .edit_pending(PendingEdit::SetCells(vec![CellEdit {
                row: 4,
                column: 1,
                value: Some(text("changed")),
            }]))
            .unwrap();
        stored
            .write()
            .unwrap()
            .edit_pending(PendingEdit::Delete(vec![5]))
            .unwrap();
        // The next page of the query, rows 7 and 8, comes after the rows on screen.
        let schema = stored.read().unwrap().schema().clone();
        let next = store.create(schema.clone());
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![7, 8])),
            Arc::new(StringArray::from(vec![Some("g"), Some("h")])),
            Arc::new(StringArray::from(vec![Some("2001-01-01"); 2])),
        ];
        store
            .append(
                next,
                RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
            )
            .unwrap();
        store.complete(next);
        assert_eq!(store.append_page(id, next, 6), Ok(2));
        let stored = stored.read().unwrap();
        assert_eq!(stored.rows(), 8);
        // The edits still name ids 5 and 6, the rows they were made on.
        assert_eq!(
            stored.pending().changes(),
            vec![
                RowChange::Update {
                    row: 4,
                    values: vec![(1, text("changed"))],
                },
                RowChange::Delete { row: 5 },
            ]
        );
        assert_eq!(stored.value_at(4, 0), Some(Value::Int(5)));
        assert_eq!(stored.value_at(5, 0), Some(Value::Int(6)));
    }

    #[test]
    fn statements_come_from_the_edits_held() {
        let (store, id) = page();
        let stored = store.get(id).unwrap();
        let mut stored = stored.write().unwrap();
        stored
            .edit_pending(PendingEdit::Bulk {
                block: block(0, 2, &[1, 2]),
                action: BulkAction::Set(text("z")),
            })
            .unwrap();
        stored
            .edit_pending(PendingEdit::Delete(vec![2, 5]))
            .unwrap();
        stored
            .edit_pending(PendingEdit::Insert(vec![(1, CellInput::Default)]))
            .unwrap();
        assert_eq!(stored.pending().updated_rows(), 2);
        assert_eq!(
            stored.pending().changes(),
            vec![
                RowChange::Update {
                    row: 0,
                    values: vec![(1, text("z")), (2, text("z"))],
                },
                RowChange::Update {
                    row: 1,
                    values: vec![(1, text("z")), (2, text("z"))],
                },
                RowChange::Delete { row: 2 },
                RowChange::Delete { row: 5 },
                RowChange::Insert {
                    values: vec![(1, CellInput::Default)],
                },
            ]
        );
    }

    #[test]
    fn blocks_are_read_like_spreadsheets_copy_them() {
        assert_eq!(
            parse_block("a\tb\nc\td\n"),
            vec![vec!["a", "b"], vec!["c", "d"]]
        );
        assert_eq!(
            parse_block("a\tb\r\nc\td"),
            vec![vec!["a", "b"], vec!["c", "d"]]
        );
        assert_eq!(parse_block("one"), vec![vec!["one"]]);
        assert_eq!(parse_block("\t\n"), vec![vec!["", ""]]);
        assert_eq!(
            parse_block("\"a\tb\"\t\"say \"\"hi\"\"\nthere\"\nx"),
            vec![vec!["a\tb", "say \"hi\"\nthere"], vec!["x"]]
        );
        assert_eq!(parse_block("in \"middle\""), vec![vec!["in \"middle\""]]);
        assert!(parse_block("").is_empty());
    }
}
