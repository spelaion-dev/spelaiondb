//! The transposed view of a result (SDB-386): the current row, or the selected rows, shown as
//! columns, one row per column of the result. It is a result of its own in the store, made
//! from the stored rows in the core (ADR-010): the grid asks for its windows, and copies, the
//! cell viewer, Ctrl+F and the calc panel read it as they read any result.
//!
//! Each cell holds the database's text of the value (what the grid copies), so a column of the
//! transposed view mixes the types of a row. A masked cell holds the mask (ADR-051): the view
//! is made from the stored, masked rows, and a masked value is never read for it.

use std::sync::Arc;

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use spelaion_driver_api::{ColumnMeta, LogicalType, ResultSchema, Value};

use crate::{ResultId, ResultStore};

/// Most rows shown as columns at once.
pub const MAX_TRANSPOSED_ROWS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransposeError {
    #[error("select at least one row to transpose")]
    NoRows,
    #[error("at most {MAX_TRANSPOSED_ROWS} rows can be transposed at once: select fewer")]
    TooManyRows,
    #[error("row {0} is not in the result")]
    NoSuchRow(u64),
    #[error("the result was freed")]
    Freed,
}

/// What the transposed view shows, in the store.
#[derive(Debug, Clone, PartialEq)]
pub struct Transposed {
    pub result: ResultId,
    pub columns: Vec<ColumnMeta>,
    /// One per column of the source result.
    pub rows: usize,
}

fn text_column(name: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.to_string(),
        logical: LogicalType::Text,
        native_type: "text".into(),
        native_id: None,
        origin: None,
    }
}

/// Makes the transposed view of `rows` (positions in `source`'s current view, as the grid
/// numbers them from 0; any order, repeats ignored) and keeps it in `store` as a complete
/// result. Its first column holds the source's column names (headed `column_label`), its
/// second their types (headed `type_label`: the labels are the app's, in the user's language),
/// then one column per row, headed by the row's number as the grid shows it: `first_number`
/// for the view's first row (1, or a table data page's offset plus 1).
pub fn transpose(
    store: &ResultStore,
    source: ResultId,
    rows: &[usize],
    first_number: u64,
    column_label: &str,
    type_label: &str,
) -> Result<Transposed, TransposeError> {
    let mut rows = rows.to_vec();
    rows.sort_unstable();
    rows.dedup();
    if rows.is_empty() {
        return Err(TransposeError::NoRows);
    }
    if rows.len() > MAX_TRANSPOSED_ROWS {
        return Err(TransposeError::TooManyRows);
    }
    let stored = store.get(source).ok_or(TransposeError::Freed)?;
    let stored = stored.read().map_err(|_| TransposeError::Freed)?;
    if let Some(&row) = rows.iter().find(|&&row| row >= stored.rows()) {
        return Err(TransposeError::NoSuchRow(first_number + row as u64));
    }
    let source_columns = &stored.schema().columns;
    let mut columns = vec![text_column(column_label), text_column(type_label)];
    columns.extend(
        rows.iter()
            .map(|&row| text_column(&(first_number + row as u64).to_string())),
    );
    let mut arrays: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(
            source_columns.iter().map(|c| c.name.as_str()),
        )),
        Arc::new(StringArray::from_iter_values(
            source_columns.iter().map(|c| c.native_type.as_str()),
        )),
    ];
    for &row in &rows {
        let cells: StringArray = (0..source_columns.len())
            .map(|column| match stored.value_at(row, column) {
                Some(Value::Null) | None => None,
                Some(value) => Some(value.to_string()),
            })
            .collect();
        arrays.push(Arc::new(cells));
    }
    drop(stored);
    let schema = Arc::new(ResultSchema::new(columns.clone()));
    let batch = RecordBatch::try_new(schema.arrow.clone(), arrays)
        .expect("one text array per column, each as long as the source's columns");
    let shown = batch.num_rows();
    let result = store.create(schema);
    store.append(result, batch);
    store.complete(result);
    Ok(Transposed {
        result,
        columns,
        rows: shown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::masking::MaskPolicy;
    use arrow::array::{BinaryArray, Int64Array};

    fn meta(name: &str, logical: LogicalType, native_type: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            logical,
            native_type: native_type.into(),
            native_id: None,
            origin: None,
        }
    }

    fn source(store: &ResultStore) -> ResultId {
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int, "bigint"),
            meta("name", LogicalType::Text, "text"),
            meta("photo", LogicalType::Bytes, "bytea"),
        ]));
        let id = store.create(schema.clone());
        let batch = RecordBatch::try_new(
            schema.arrow.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 2, 3])),
                Arc::new(StringArray::from(vec![Some("ann"), None, Some("cy")])),
                Arc::new(BinaryArray::from(vec![
                    Some(&b"\x01"[..]),
                    Some(&b""[..]),
                    None,
                ])),
            ],
        )
        .unwrap();
        store.append(id, batch);
        store.complete(id);
        id
    }

    fn cells(store: &ResultStore, id: ResultId) -> Vec<Vec<Option<String>>> {
        let stored = store.get(id).unwrap();
        let stored = stored.read().unwrap();
        (0..stored.rows())
            .map(|row| {
                (0..stored.schema().columns.len())
                    .map(|column| match stored.value_at(row, column).unwrap() {
                        Value::Null => None,
                        value => Some(value.to_string()),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn rows_become_columns_headed_by_their_numbers() {
        let store = ResultStore::new();
        let id = source(&store);
        let transposed = transpose(&store, id, &[2, 0, 2], 1, "Column", "Type").unwrap();
        let names: Vec<&str> = transposed.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Column", "Type", "1", "3"]);
        assert_eq!(transposed.rows, 3);
        let s = |text: &str| Some(text.to_string());
        assert_eq!(
            cells(&store, transposed.result),
            vec![
                vec![s("id"), s("bigint"), s("1"), s("3")],
                vec![s("name"), s("text"), s("ann"), s("cy")],
                vec![s("photo"), s("bytea"), s("\\x01"), None],
            ]
        );
        assert!(
            store
                .get(transposed.result)
                .unwrap()
                .read()
                .unwrap()
                .is_complete()
        );
        // On a table data page that starts at row 501, rows are numbered from there.
        let paged = transpose(&store, id, &[1], 501, "Column", "Type").unwrap();
        assert_eq!(paged.columns[2].name, "502");
    }

    #[test]
    fn the_view_follows_the_sorted_view_of_the_source() {
        let store = ResultStore::new();
        let id = source(&store);
        store
            .get(id)
            .unwrap()
            .write()
            .unwrap()
            .sort(&[crate::SortKey {
                column: 0,
                descending: true,
            }])
            .unwrap();
        let transposed = transpose(&store, id, &[0], 1, "Column", "Type").unwrap();
        // The first row shown is id 3.
        assert_eq!(cells(&store, transposed.result)[0][2].as_deref(), Some("3"));
    }

    #[test]
    fn masked_cells_stay_masked() {
        let store = ResultStore::new();
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int, "bigint"),
            meta("email", LogicalType::Text, "text"),
        ]));
        let policy = MaskPolicy::new(&spelaion_config::MaskingRules {
            required: vec!["email".into()],
            exemptible: Vec::new(),
            never: Vec::new(),
        });
        let (id, _) = store.create_masked(schema.clone(), &policy);
        let batch = RecordBatch::try_new(
            schema.arrow.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(StringArray::from(vec!["secret@example.com"])),
            ],
        )
        .unwrap();
        store.append(id, batch);
        store.complete(id);
        let transposed = transpose(&store, id, &[0], 1, "Column", "Type").unwrap();
        let all = cells(&store, transposed.result);
        assert!(
            all.iter()
                .flatten()
                .flatten()
                .all(|cell| !cell.contains("secret")),
            "{all:?}"
        );
    }

    #[test]
    fn no_rows_too_many_rows_and_rows_past_the_end_are_refused() {
        let store = ResultStore::new();
        let id = source(&store);
        assert_eq!(
            transpose(&store, id, &[], 1, "C", "T").unwrap_err(),
            TransposeError::NoRows
        );
        assert_eq!(
            transpose(&store, id, &[3], 1, "C", "T").unwrap_err(),
            TransposeError::NoSuchRow(4)
        );
        let many: Vec<usize> = (0..=MAX_TRANSPOSED_ROWS).collect();
        assert_eq!(
            transpose(&store, id, &many, 1, "C", "T").unwrap_err(),
            TransposeError::TooManyRows
        );
        assert_eq!(
            transpose(&store, ResultId(999), &[0], 1, "C", "T").unwrap_err(),
            TransposeError::Freed
        );
    }
}
