//! The rows of an import (ADR-038) or a copy (ADR-069), as they travel: one nullable column per
//! target column, in Arrow. A column is text, or, from plugin protocol 1.3, bytes (SDB-464). A
//! driver reads its values from here as `&str` or `&[u8]` and binds them; nothing is copied into
//! a `String` per cell on the way, in process or through a plugin (ADR-041).

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, LargeBinaryArray, LargeBinaryBuilder, LargeStringArray, LargeStringBuilder,
    RecordBatch, RecordBatchOptions,
};
use arrow::datatypes::{DataType, Field, Schema};

use crate::{DriverError, Result};

/// How a column's values travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// Text the server converts to the column's type.
    Text,
    /// Bytes, bound as the engine's binary type (plugin protocol 1.3, SDB-464). Only sent to a
    /// connection whose [`Capabilities::insert_bytes`] is true.
    ///
    /// [`Capabilities::insert_bytes`]: crate::Capabilities::insert_bytes
    Bytes,
}

/// One value of a row, as it travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cell<'a> {
    Text(&'a str),
    Bytes(&'a [u8]),
}

impl Cell<'_> {
    /// Its size in bytes: what a statement's size is estimated from.
    pub fn len(&self) -> usize {
        match self {
            Cell::Text(text) => text.len(),
            Cell::Bytes(bytes) => bytes.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A column of rows.
#[derive(Debug, Clone)]
enum Column {
    Text(LargeStringArray),
    Bytes(LargeBinaryArray),
}

impl Column {
    fn kind(&self) -> ColumnKind {
        match self {
            Column::Text(_) => ColumnKind::Text,
            Column::Bytes(_) => ColumnKind::Bytes,
        }
    }

    fn cell(&self, row: usize) -> Option<Cell<'_>> {
        match self {
            Column::Text(array) => (!array.is_null(row)).then(|| Cell::Text(array.value(row))),
            Column::Bytes(array) => (!array.is_null(row)).then(|| Cell::Bytes(array.value(row))),
        }
    }

    fn slice(&self, offset: usize, length: usize) -> Self {
        match self {
            Column::Text(array) => Column::Text(array.slice(offset, length)),
            Column::Bytes(array) => Column::Bytes(array.slice(offset, length)),
        }
    }

    fn array(&self) -> ArrayRef {
        match self {
            Column::Text(array) => Arc::new(array.clone()),
            Column::Bytes(array) => Arc::new(array.clone()),
        }
    }
}

/// Rows of values, `None` for NULL: what [`Connection::insert_rows`] loads. Each column is text
/// or bytes ([`ColumnKind`]). Cheap to clone and to slice (the values are shared).
///
/// [`Connection::insert_rows`]: crate::Connection::insert_rows
#[derive(Debug, Clone)]
pub struct TextRows {
    columns: Vec<Column>,
    /// Kept apart: rows with no columns still count.
    rows: usize,
}

impl TextRows {
    /// No rows of `width` text columns.
    pub fn empty(width: usize) -> Self {
        TextRowsBuilder::new(width).finish()
    }

    /// Rows of `width` text columns, to be filled a row at a time.
    pub fn builder(width: usize) -> TextRowsBuilder {
        TextRowsBuilder::new(width)
    }

    /// Rows of columns of these kinds, to be filled a row at a time.
    pub fn builder_with(kinds: &[ColumnKind]) -> TextRowsBuilder {
        TextRowsBuilder::with_kinds(kinds)
    }

    /// The rows of `rows`, each with `width` text values.
    pub fn from_rows(width: usize, rows: &[Vec<Option<String>>]) -> Result<Self> {
        let mut builder = TextRowsBuilder::new(width);
        for row in rows {
            builder.push(row.iter().map(Option::as_deref))?;
        }
        Ok(builder.finish())
    }

    /// The rows of a batch made by [`TextRows::to_batch`]: every column nullable text or
    /// nullable bytes. Anything else is refused.
    pub fn from_batch(batch: &RecordBatch) -> Result<Self> {
        let columns = batch
            .columns()
            .iter()
            .map(|column| {
                let any = column.as_any();
                if let Some(text) = any.downcast_ref::<LargeStringArray>() {
                    Ok(Column::Text(text.clone()))
                } else if let Some(bytes) = any.downcast_ref::<LargeBinaryArray>() {
                    Ok(Column::Bytes(bytes.clone()))
                } else {
                    Err(DriverError::Other(
                        "the rows to load are neither text nor bytes".into(),
                    ))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            columns,
            rows: batch.num_rows(),
        })
    }

    /// These rows as one Arrow batch (columns `c0`, `c1`…), as they are sent to a plugin.
    pub fn to_batch(&self) -> RecordBatch {
        let schema = Arc::new(Schema::new(
            self.columns
                .iter()
                .enumerate()
                .map(|(i, column)| {
                    let data_type = match column.kind() {
                        ColumnKind::Text => DataType::LargeUtf8,
                        ColumnKind::Bytes => DataType::LargeBinary,
                    };
                    Field::new(format!("c{i}"), data_type, true)
                })
                .collect::<Vec<_>>(),
        ));
        let arrays: Vec<ArrayRef> = self.columns.iter().map(Column::array).collect();
        RecordBatch::try_new_with_options(
            schema,
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(self.rows)),
        )
        .expect("columns of one length are a batch")
    }

    /// Several sets of rows of the same columns, one after another.
    pub fn concat(width: usize, parts: &[TextRows]) -> Result<Self> {
        match parts {
            [] => return Ok(Self::empty(width)),
            [one] if one.width() == width => return Ok(one.clone()),
            _ => {}
        }
        let kinds = parts[0].kinds();
        if kinds.len() != width {
            return Err(DriverError::Other(format!(
                "a row has {} values for {width} columns",
                kinds.len()
            )));
        }
        let mut builder = TextRowsBuilder::with_kinds(&kinds);
        for part in parts {
            if part.kinds() != kinds {
                return Err(DriverError::Other(
                    "the rows to load do not all have the same columns".into(),
                ));
            }
            for row in 0..part.len() {
                builder.push_cells(part.cells(row))?;
            }
        }
        Ok(builder.finish())
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Values per row.
    pub fn width(&self) -> usize {
        self.columns.len()
    }

    /// How each column's values travel.
    pub fn kinds(&self) -> Vec<ColumnKind> {
        self.columns.iter().map(Column::kind).collect()
    }

    /// Whether any column carries bytes: only a connection with
    /// [`Capabilities::insert_bytes`](crate::Capabilities::insert_bytes) loads these rows.
    pub fn has_bytes(&self) -> bool {
        self.columns
            .iter()
            .any(|column| column.kind() == ColumnKind::Bytes)
    }

    /// One value, text or bytes. Panics outside the rows, as a slice does.
    pub fn cell(&self, row: usize, column: usize) -> Option<Cell<'_>> {
        assert!(row < self.rows, "row {row} of {}", self.rows);
        self.columns[column].cell(row)
    }

    /// The values of one row, in column order, text or bytes.
    pub fn cells(&self, row: usize) -> impl ExactSizeIterator<Item = Option<Cell<'_>>> + '_ {
        assert!(row < self.rows, "row {row} of {}", self.rows);
        self.columns.iter().map(move |column| column.cell(row))
    }

    /// One value of a text column. Panics outside the rows, as a slice does, and on a column of
    /// bytes, which only [`TextRows::cell`] reads: a driver that never asked for bytes
    /// ([`Capabilities::insert_bytes`](crate::Capabilities::insert_bytes)) is never sent any.
    pub fn get(&self, row: usize, column: usize) -> Option<&str> {
        self.cell(row, column).map(|cell| text_of(cell, column))
    }

    /// The values of one row of text columns, in column order. Panics on a column of bytes, as
    /// [`TextRows::get`] does.
    pub fn row(&self, row: usize) -> impl ExactSizeIterator<Item = Option<&str>> + '_ {
        self.cells(row)
            .enumerate()
            .map(|(column, cell)| cell.map(|cell| text_of(cell, column)))
    }

    /// The bytes of one row's values (a NULL counts one): what a statement's size is estimated
    /// from.
    pub fn row_bytes(&self, row: usize) -> usize {
        self.cells(row)
            .map(|cell| cell.map_or(1, |c| c.len()))
            .sum()
    }

    /// `length` rows from `offset`: the same values, not a copy.
    pub fn slice(&self, offset: usize, length: usize) -> Self {
        assert!(
            offset + length <= self.rows,
            "rows {offset}+{length} of {}",
            self.rows
        );
        Self {
            columns: self
                .columns
                .iter()
                .map(|column| column.slice(offset, length))
                .collect(),
            rows: length,
        }
    }

    /// The rows of text columns as owned values (tests and small sets). Panics on a column of
    /// bytes, as [`TextRows::get`] does.
    pub fn to_vecs(&self) -> Vec<Vec<Option<String>>> {
        (0..self.rows)
            .map(|row| {
                self.row(row)
                    .map(|value| value.map(str::to_string))
                    .collect()
            })
            .collect()
    }
}

/// The text of a cell of a text column.
fn text_of(cell: Cell<'_>, column: usize) -> &str {
    match cell {
        Cell::Text(text) => text,
        Cell::Bytes(_) => {
            panic!("column {column} carries bytes: read it with `TextRows::cell`")
        }
    }
}

/// A column being filled.
enum ColumnBuilder {
    Text(LargeStringBuilder),
    Bytes(LargeBinaryBuilder),
}

/// Fills a [`TextRows`] a row at a time.
pub struct TextRowsBuilder {
    columns: Vec<ColumnBuilder>,
    rows: usize,
}

impl TextRowsBuilder {
    /// Rows of `width` text columns.
    pub fn new(width: usize) -> Self {
        Self::with_kinds(&vec![ColumnKind::Text; width])
    }

    /// Rows of columns of these kinds.
    pub fn with_kinds(kinds: &[ColumnKind]) -> Self {
        Self {
            columns: kinds
                .iter()
                .map(|kind| match kind {
                    ColumnKind::Text => ColumnBuilder::Text(LargeStringBuilder::new()),
                    ColumnKind::Bytes => ColumnBuilder::Bytes(LargeBinaryBuilder::new()),
                })
                .collect(),
            rows: 0,
        }
    }

    /// Adds a row of text values. It must have one value per column, and every column must be
    /// text.
    pub fn push<'a>(&mut self, row: impl IntoIterator<Item = Option<&'a str>>) -> Result<()> {
        self.push_cells(row.into_iter().map(|value| value.map(Cell::Text)))
    }

    /// Adds a row. It must have one value per column, each of its column's kind. A row that
    /// does not fit adds nothing.
    pub fn push_cells<'a>(
        &mut self,
        row: impl IntoIterator<Item = Option<Cell<'a>>>,
    ) -> Result<()> {
        let row: Vec<Option<Cell<'a>>> = row.into_iter().collect();
        if row.len() != self.columns.len() {
            return Err(DriverError::Other(format!(
                "a row has {} values for {} columns",
                row.len(),
                self.columns.len()
            )));
        }
        for (column, (builder, value)) in self.columns.iter().zip(&row).enumerate() {
            match (builder, value) {
                (_, None)
                | (ColumnBuilder::Text(_), Some(Cell::Text(_)))
                | (ColumnBuilder::Bytes(_), Some(Cell::Bytes(_))) => {}
                (ColumnBuilder::Text(_), Some(Cell::Bytes(_))) => {
                    return Err(DriverError::Other(format!(
                        "column {column} takes text, not bytes"
                    )));
                }
                (ColumnBuilder::Bytes(_), Some(Cell::Text(_))) => {
                    return Err(DriverError::Other(format!(
                        "column {column} takes bytes, not text"
                    )));
                }
            }
        }
        for (builder, value) in self.columns.iter_mut().zip(row) {
            match (builder, value) {
                (ColumnBuilder::Text(builder), value) => {
                    builder.append_option(value.map(|cell| match cell {
                        Cell::Text(text) => text,
                        // Checked above.
                        Cell::Bytes(_) => "",
                    }));
                }
                (ColumnBuilder::Bytes(builder), value) => {
                    builder.append_option(value.map(|cell| match cell {
                        Cell::Bytes(bytes) => bytes,
                        // Checked above.
                        Cell::Text(_) => &[][..],
                    }));
                }
            }
        }
        self.rows += 1;
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    pub fn finish(mut self) -> TextRows {
        TextRows {
            columns: self
                .columns
                .iter_mut()
                .map(|column| match column {
                    ColumnBuilder::Text(builder) => Column::Text(builder.finish()),
                    ColumnBuilder::Bytes(builder) => Column::Bytes(builder.finish()),
                })
                .collect(),
            rows: self.rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(text: &str) -> Option<String> {
        Some(text.to_string())
    }

    #[test]
    fn rows_come_back_as_they_went_in() {
        let rows = vec![
            vec![some("1"), some("a\tb\nc"), None],
            vec![some(""), some("ñ €"), some("x")],
        ];
        let text = TextRows::from_rows(3, &rows).unwrap();
        assert_eq!((text.len(), text.width()), (2, 3));
        assert_eq!(text.get(0, 1), Some("a\tb\nc"));
        // NULL and the empty text are two values.
        assert_eq!(text.get(0, 2), None);
        assert_eq!(text.get(1, 0), Some(""));
        assert_eq!(
            text.row(1).collect::<Vec<_>>(),
            [Some(""), Some("ñ €"), Some("x")]
        );
        assert_eq!(text.to_vecs(), rows);
        assert_eq!(text.row_bytes(0), 1 + 5 + 1);
        assert!(!text.has_bytes());

        // Through a batch, as they travel to a plugin.
        let back = TextRows::from_batch(&text.to_batch()).unwrap();
        assert_eq!(back.to_vecs(), rows);
    }

    #[test]
    fn a_column_of_bytes_carries_every_byte() {
        let kinds = [ColumnKind::Text, ColumnKind::Bytes];
        let mut builder = TextRows::builder_with(&kinds);
        let every: Vec<u8> = (0..=255).collect();
        builder
            .push_cells([Some(Cell::Text("1")), Some(Cell::Bytes(&every))])
            .unwrap();
        builder
            .push_cells([Some(Cell::Text("2")), Some(Cell::Bytes(&[]))])
            .unwrap();
        builder.push_cells([None, None]).unwrap();
        let rows = builder.finish();
        assert!(rows.has_bytes());
        assert_eq!(rows.kinds(), kinds);
        assert_eq!(rows.cell(0, 1), Some(Cell::Bytes(&every[..])));
        // The empty bytes and NULL are two values.
        assert_eq!(rows.cell(1, 1), Some(Cell::Bytes(&[][..])));
        assert_eq!(rows.cell(2, 1), None);
        assert_eq!(rows.get(0, 0), Some("1"));
        assert_eq!(rows.row_bytes(0), 1 + 256);
        assert_eq!(rows.row_bytes(2), 2);

        // Through a batch, as they travel to a plugin, and sliced and joined again.
        let back = TextRows::from_batch(&rows.to_batch()).unwrap();
        assert_eq!(back.kinds(), kinds);
        assert_eq!(back.cell(0, 1), Some(Cell::Bytes(&every[..])));
        let joined = TextRows::concat(2, &[back.slice(0, 1), back.slice(1, 2)]).unwrap();
        assert_eq!(joined.len(), 3);
        assert_eq!(joined.cell(0, 1), Some(Cell::Bytes(&every[..])));
        assert_eq!(joined.cell(2, 1), None);
    }

    #[test]
    fn a_value_of_the_other_kind_is_refused() {
        let mut builder = TextRows::builder_with(&[ColumnKind::Bytes]);
        let error = builder.push([Some("00ff")]).unwrap_err();
        assert_eq!(error.to_string(), "column 0 takes bytes, not text");
        let mut builder = TextRows::builder(1);
        let error = builder.push_cells([Some(Cell::Bytes(b"x"))]).unwrap_err();
        assert_eq!(error.to_string(), "column 0 takes text, not bytes");
        // Nothing was added.
        assert!(builder.is_empty());
        // Rows of other columns are not joined.
        let text = TextRows::from_rows(1, &[vec![some("a")]]).unwrap();
        let mut bytes = TextRows::builder_with(&[ColumnKind::Bytes]);
        bytes.push_cells([Some(Cell::Bytes(b"a"))]).unwrap();
        assert!(TextRows::concat(1, &[text, bytes.finish()]).is_err());
    }

    #[test]
    #[should_panic(expected = "column 0 carries bytes")]
    fn bytes_are_never_read_as_text() {
        let mut builder = TextRows::builder_with(&[ColumnKind::Bytes]);
        builder.push_cells([Some(Cell::Bytes(b"a"))]).unwrap();
        let _ = builder.finish().get(0, 0);
    }

    #[test]
    fn a_slice_is_those_rows() {
        let rows: Vec<_> = (0..10).map(|i| vec![some(&i.to_string()), None]).collect();
        let text = TextRows::from_rows(2, &rows).unwrap();
        let part = text.slice(3, 4);
        assert_eq!(part.len(), 4);
        assert_eq!(part.to_vecs(), rows[3..7]);
        assert_eq!(part.slice(1, 2).to_vecs(), rows[4..6]);
        let joined = TextRows::concat(2, &[text.slice(0, 3), text.slice(3, 7)]).unwrap();
        assert_eq!(joined.to_vecs(), rows);
        assert!(TextRows::concat(2, &[]).unwrap().is_empty());
    }

    #[test]
    fn a_row_of_another_width_is_refused() {
        let wide = TextRows::from_rows(1, &[vec![some("a"), some("b")]]).unwrap_err();
        assert_eq!(wide.to_string(), "a row has 2 values for 1 columns");
        let short = TextRows::from_rows(2, &[vec![some("a")]]).unwrap_err();
        assert_eq!(short.to_string(), "a row has 1 values for 2 columns");
        let mixed = TextRows::concat(
            2,
            &[
                TextRows::empty(2),
                TextRows::from_rows(1, &[vec![None]]).unwrap(),
            ],
        );
        assert!(mixed.is_err());
    }

    #[test]
    fn rows_with_no_columns_still_count() {
        let text = TextRows::from_rows(0, &[vec![], vec![]]).unwrap();
        assert_eq!((text.len(), text.width()), (2, 0));
        assert_eq!(TextRows::from_batch(&text.to_batch()).unwrap().len(), 2);
    }

    #[test]
    fn a_batch_that_is_neither_text_nor_bytes_is_refused() {
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]));
        let numbers: ArrayRef = Arc::new(arrow::array::Int64Array::from(vec![1, 2]));
        let batch = RecordBatch::try_new(schema, vec![numbers]).unwrap();
        assert!(TextRows::from_batch(&batch).is_err());
    }
}
