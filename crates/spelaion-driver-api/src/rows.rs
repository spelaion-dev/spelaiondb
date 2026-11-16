//! The rows of an import (ADR-038), as they travel: one nullable text column per target
//! column, in Arrow. A driver reads its values from here as `&str` and binds them; nothing is
//! copied into a `String` per cell on the way, in process or through a plugin (ADR-041).

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, LargeStringArray, LargeStringBuilder, RecordBatch, RecordBatchOptions,
};
use arrow::datatypes::{DataType, Field, Schema};

use crate::{DriverError, Result};

/// Rows of text values, `None` for NULL: what [`Connection::insert_rows`] loads. Cheap to
/// clone and to slice (the text is shared).
///
/// [`Connection::insert_rows`]: crate::Connection::insert_rows
#[derive(Debug, Clone)]
pub struct TextRows {
    columns: Vec<LargeStringArray>,
    /// Kept apart: rows with no columns still count.
    rows: usize,
}

impl TextRows {
    /// No rows of `width` columns.
    pub fn empty(width: usize) -> Self {
        TextRowsBuilder::new(width).finish()
    }

    /// Rows of `width` columns, to be filled a row at a time.
    pub fn builder(width: usize) -> TextRowsBuilder {
        TextRowsBuilder::new(width)
    }

    /// The rows of `rows`, each with `width` values.
    pub fn from_rows(width: usize, rows: &[Vec<Option<String>>]) -> Result<Self> {
        let mut builder = TextRowsBuilder::new(width);
        for row in rows {
            builder.push(row.iter().map(Option::as_deref))?;
        }
        Ok(builder.finish())
    }

    /// The rows of a batch made by [`TextRows::to_batch`]: every column nullable text.
    /// Anything else is refused.
    pub fn from_batch(batch: &RecordBatch) -> Result<Self> {
        let columns = batch
            .columns()
            .iter()
            .map(|column| {
                column
                    .as_any()
                    .downcast_ref::<LargeStringArray>()
                    .cloned()
                    .ok_or_else(|| DriverError::Other("the rows to load are not text".into()))
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
            (0..self.columns.len())
                .map(|i| Field::new(format!("c{i}"), DataType::LargeUtf8, true))
                .collect::<Vec<_>>(),
        ));
        let arrays: Vec<ArrayRef> = self
            .columns
            .iter()
            .map(|column| Arc::new(column.clone()) as ArrayRef)
            .collect();
        RecordBatch::try_new_with_options(
            schema,
            arrays,
            &RecordBatchOptions::new().with_row_count(Some(self.rows)),
        )
        .expect("text columns of one length are a batch")
    }

    /// Several sets of rows of the same width, one after another.
    pub fn concat(width: usize, parts: &[TextRows]) -> Result<Self> {
        match parts {
            [] => return Ok(Self::empty(width)),
            [one] if one.width() == width => return Ok(one.clone()),
            _ => {}
        }
        let mut builder = TextRowsBuilder::new(width);
        for part in parts {
            for row in 0..part.len() {
                builder.push(part.row(row))?;
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

    /// One value. Panics outside the rows, as a slice does.
    pub fn get(&self, row: usize, column: usize) -> Option<&str> {
        let column = &self.columns[column];
        (!column.is_null(row)).then(|| column.value(row))
    }

    /// The values of one row, in column order.
    pub fn row(&self, row: usize) -> impl ExactSizeIterator<Item = Option<&str>> + '_ {
        assert!(row < self.rows, "row {row} of {}", self.rows);
        self.columns
            .iter()
            .map(move |column| (!column.is_null(row)).then(|| column.value(row)))
    }

    /// The bytes of one row's text (a NULL counts one): what a statement's size is estimated
    /// from.
    pub fn row_bytes(&self, row: usize) -> usize {
        self.row(row).map(|value| value.map_or(1, str::len)).sum()
    }

    /// `length` rows from `offset`: the same text, not a copy.
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

    /// The rows as owned values (tests and small sets).
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

/// Fills a [`TextRows`] a row at a time.
pub struct TextRowsBuilder {
    columns: Vec<LargeStringBuilder>,
    rows: usize,
}

impl TextRowsBuilder {
    pub fn new(width: usize) -> Self {
        Self {
            columns: (0..width).map(|_| LargeStringBuilder::new()).collect(),
            rows: 0,
        }
    }

    /// Adds a row. It must have one value per column.
    pub fn push<'a>(&mut self, row: impl IntoIterator<Item = Option<&'a str>>) -> Result<()> {
        let mut values = 0;
        for value in row {
            let Some(column) = self.columns.get_mut(values) else {
                // Counted to the end so the error says how many there were.
                values += 1;
                continue;
            };
            column.append_option(value);
            values += 1;
        }
        if values != self.columns.len() {
            return Err(DriverError::Other(format!(
                "a row has {values} values for {} columns",
                self.columns.len()
            )));
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
                .map(|column| column.finish())
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

        // Through a batch, as they travel to a plugin.
        let back = TextRows::from_batch(&text.to_batch()).unwrap();
        assert_eq!(back.to_vecs(), rows);
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
    fn a_batch_that_is_not_text_is_refused() {
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]));
        let numbers: ArrayRef = Arc::new(arrow::array::Int64Array::from(vec![1, 2]));
        let batch = RecordBatch::try_new(schema, vec![numbers]).unwrap();
        assert!(TextRows::from_batch(&batch).is_err());
    }
}
