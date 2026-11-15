//! SQLite's dynamic typing as Arrow columns (ADR-006).
//!
//! SQLite has dynamic typing: a column can mix integers, text and blobs. Arrow needs one
//! type per column, so it is decided from the declared type and the first batch of rows.
//! After that, a value that does not fit is converted only if no information is lost;
//! otherwise the query fails with a clear message. Data is never lost silently.
//!
//! A transport reads a first batch of rows as [`SqlValue`]s, builds a [`ResultBuilder`] from
//! them and its columns, and then appends every further row, owned (`push_row`) or one cell at
//! a time from its own buffers (`append_*`), taking a batch whenever it wants one.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, Float64Builder, Int64Builder, RecordBatch, StringBuilder,
};
use spelaion_driver_api::{
    ColumnMeta, ColumnOrigin, DriverError, LogicalType, Result, ResultSchema, Value,
};

use crate::SqlValue;

/// SQLite type affinity from the declared type (rules from the documentation,
/// section 3.1 of "Datatypes In SQLite").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// The affinity of a column declared as `declared` (none: an expression, or a column declared
/// without a type).
pub fn affinity(declared: Option<&str>) -> Affinity {
    let Some(d) = declared else {
        return Affinity::Blob;
    };
    let d = d.to_ascii_uppercase();
    if d.contains("INT") {
        Affinity::Integer
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        Affinity::Text
    } else if d.contains("BLOB") || d.is_empty() {
        Affinity::Blob
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// The storage classes seen in a column.
#[derive(Debug, Default, Clone, Copy)]
pub struct Seen {
    int: bool,
    real: bool,
    text: bool,
    blob: bool,
}

impl Seen {
    pub fn add(&mut self, v: &SqlValue) {
        match v {
            SqlValue::Null => {}
            SqlValue::Integer(_) => self.int = true,
            SqlValue::Real(_) => self.real = true,
            SqlValue::Text(_) => self.text = true,
            SqlValue::Blob(_) => self.blob = true,
        }
    }

    /// The storage class seen, as a column's native type when it declares none.
    pub fn storage_class(self) -> &'static str {
        match (self.int, self.real, self.text, self.blob) {
            (true, false, false, false) => "INTEGER",
            (false, true, false, false) | (true, true, false, false) => "REAL",
            (false, false, true, false) => "TEXT",
            (false, false, false, true) => "BLOB",
            (false, false, false, false) => "NULL",
            _ => "MIXED",
        }
    }
}

/// Logical type of a column from its affinity and what was seen in the first batch.
pub fn decide(affinity: Affinity, seen: Seen) -> LogicalType {
    match (seen.int, seen.real, seen.text, seen.blob) {
        (false, false, false, false) => match affinity {
            Affinity::Integer => LogicalType::Int,
            Affinity::Real => LogicalType::Float,
            Affinity::Blob => LogicalType::Bytes,
            // NUMERIC (DATE, BOOLEAN, DECIMAL…) can hold anything: text accepts everything.
            Affinity::Text | Affinity::Numeric => LogicalType::Text,
        },
        (true, false, false, false) if affinity == Affinity::Real => LogicalType::Float,
        (true, false, false, false) => LogicalType::Int,
        (_, true, false, false) => LogicalType::Float,
        (false, false, false, true) => LogicalType::Bytes,
        _ => LogicalType::Text,
    }
}

/// A value borrowed from wherever the transport holds it.
#[derive(Debug, Clone, Copy)]
enum Cell<'a> {
    Null,
    Integer(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob(&'a [u8]),
}

impl SqlValue {
    fn cell(&self) -> Cell<'_> {
        match self {
            SqlValue::Null => Cell::Null,
            SqlValue::Integer(i) => Cell::Integer(*i),
            SqlValue::Real(x) => Cell::Real(*x),
            SqlValue::Text(t) => Cell::Text(t.as_bytes()),
            SqlValue::Blob(b) => Cell::Blob(b),
        }
    }
}

/// One Arrow column being built, of the type decided for it.
pub enum Builder {
    Int(Int64Builder),
    Float(Float64Builder),
    Bytes(BinaryBuilder),
    Text(StringBuilder),
}

/// `Err` with the SQLite type name of a value that does not fit a column without losing
/// information.
pub type Mismatch = std::result::Result<(), &'static str>;

impl Builder {
    pub fn new(logical: LogicalType) -> Self {
        match logical {
            LogicalType::Int => Self::Int(Int64Builder::new()),
            LogicalType::Float => Self::Float(Float64Builder::new()),
            LogicalType::Bytes => Self::Bytes(BinaryBuilder::new()),
            _ => Self::Text(StringBuilder::new()),
        }
    }

    /// Adds a value; `Err` with the SQLite type name if it does not fit without losing information.
    pub fn append(&mut self, v: &SqlValue) -> Mismatch {
        self.push(v.cell())
    }

    pub fn append_null(&mut self) -> Mismatch {
        self.push(Cell::Null)
    }

    pub fn append_integer(&mut self, i: i64) -> Mismatch {
        self.push(Cell::Integer(i))
    }

    pub fn append_real(&mut self, x: f64) -> Mismatch {
        self.push(Cell::Real(x))
    }

    /// A TEXT value's bytes, which SQLite does not guarantee to be UTF-8.
    pub fn append_text(&mut self, t: &[u8]) -> Mismatch {
        self.push(Cell::Text(t))
    }

    pub fn append_blob(&mut self, b: &[u8]) -> Mismatch {
        self.push(Cell::Blob(b))
    }

    fn push(&mut self, v: Cell<'_>) -> Mismatch {
        match (self, v) {
            (Self::Int(b), Cell::Null) => b.append_null(),
            (Self::Float(b), Cell::Null) => b.append_null(),
            (Self::Bytes(b), Cell::Null) => b.append_null(),
            (Self::Text(b), Cell::Null) => b.append_null(),

            (Self::Int(b), Cell::Integer(i)) => b.append_value(i),
            (Self::Int(b), Cell::Real(x)) if x.fract() == 0.0 && x.abs() < 9.0e15 => {
                b.append_value(x as i64)
            }
            (Self::Float(b), Cell::Real(x)) => b.append_value(x),
            // Exact up to 2^53; beyond that, a SQLite REAL would not be exact either.
            (Self::Float(b), Cell::Integer(i)) if i.unsigned_abs() <= 1 << 53 => {
                b.append_value(i as f64)
            }
            (Self::Bytes(b), Cell::Blob(x)) => b.append_value(x),

            (Self::Text(b), Cell::Text(t)) => match std::str::from_utf8(t) {
                Ok(s) => b.append_value(s),
                // Invalid UTF-8: as hexadecimal, without losing any byte.
                Err(_) => b.append_value(Value::Bytes(t.to_vec()).to_string()),
            },
            (Self::Text(b), Cell::Integer(i)) => b.append_value(i.to_string()),
            (Self::Text(b), Cell::Real(x)) => b.append_value(real_text(x)),
            (Self::Text(b), Cell::Blob(x)) => b.append_value(Value::Bytes(x.to_vec()).to_string()),

            (_, Cell::Integer(_)) => return Err("INTEGER"),
            (_, Cell::Real(_)) => return Err("REAL"),
            (_, Cell::Text(_)) => return Err("TEXT"),
            (_, Cell::Blob(_)) => return Err("BLOB"),
        }
        Ok(())
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Int(b) => Arc::new(b.finish()),
            Self::Float(b) => Arc::new(b.finish()),
            Self::Bytes(b) => Arc::new(b.finish()),
            Self::Text(b) => Arc::new(b.finish()),
        }
    }
}

/// A REAL as the SQLite shell writes it: `1.0`, not `1`.
pub fn real_text(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{x:.1}")
    } else {
        Value::Float(x).to_string()
    }
}

/// A result column as the transport knows it.
#[derive(Debug, Clone)]
pub struct ResultColumn {
    pub name: String,
    /// The column's declared type; none for an expression, or when the transport cannot tell
    /// (a server's `sqlite3` prints none).
    pub declared: Option<String>,
    /// The table column it reads (ADR-052), when the transport can tell.
    pub origin: Option<ColumnOrigin>,
}

/// A result being typed and built: the schema decided from the first batch, then rows.
pub struct ResultBuilder {
    schema: Arc<ResultSchema>,
    builders: Vec<Builder>,
    /// Rows started so far, the first batch included (the number in a mismatch message).
    rows: u64,
    /// Rows added since the last batch was taken.
    pending: usize,
}

impl ResultBuilder {
    /// Decides each column's type from its declared type and the `first` rows, and adds them.
    pub fn new(columns: Vec<ResultColumn>, first: &[Vec<SqlValue>]) -> Result<Self> {
        let mut seen = vec![Seen::default(); columns.len()];
        for row in first {
            for (s, v) in seen.iter_mut().zip(row) {
                s.add(v);
            }
        }
        let columns: Vec<ColumnMeta> = columns
            .into_iter()
            .zip(&seen)
            .map(|(column, seen)| ColumnMeta {
                logical: decide(affinity(column.declared.as_deref()), *seen),
                native_type: column
                    .declared
                    .unwrap_or_else(|| seen.storage_class().to_string()),
                name: column.name,
                native_id: None,
                origin: column.origin,
            })
            .collect();
        let schema = Arc::new(ResultSchema::new(columns));
        let builders = schema
            .columns
            .iter()
            .map(|c| Builder::new(c.logical))
            .collect();
        let mut result = Self {
            schema,
            builders,
            rows: 0,
            pending: 0,
        };
        for row in first {
            result.push_row(row)?;
        }
        Ok(result)
    }

    pub fn schema(&self) -> Arc<ResultSchema> {
        self.schema.clone()
    }

    /// Rows added so far, the first batch included.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Rows added since the last batch was taken.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Adds a row of owned values; a row shorter than the result has NULL in the rest.
    pub fn push_row(&mut self, row: &[SqlValue]) -> Result<()> {
        self.begin_row();
        for i in 0..self.builders.len() {
            let value = row.get(i).unwrap_or(&SqlValue::Null);
            let found = self.builders[i].append(value);
            self.check(i, found)?;
        }
        self.end_row();
        Ok(())
    }

    /// Starts a row added one cell at a time with the `append_*` methods, every column in
    /// order, then [`end_row`](Self::end_row).
    pub fn begin_row(&mut self) {
        self.rows += 1;
    }

    pub fn end_row(&mut self) {
        self.pending += 1;
    }

    pub fn append_null(&mut self, column: usize) -> Result<()> {
        let found = self.builders[column].append_null();
        self.check(column, found)
    }

    pub fn append_integer(&mut self, column: usize, i: i64) -> Result<()> {
        let found = self.builders[column].append_integer(i);
        self.check(column, found)
    }

    pub fn append_real(&mut self, column: usize, x: f64) -> Result<()> {
        let found = self.builders[column].append_real(x);
        self.check(column, found)
    }

    pub fn append_text(&mut self, column: usize, t: &[u8]) -> Result<()> {
        let found = self.builders[column].append_text(t);
        self.check(column, found)
    }

    pub fn append_blob(&mut self, column: usize, b: &[u8]) -> Result<()> {
        let found = self.builders[column].append_blob(b);
        self.check(column, found)
    }

    /// The rows added since the last batch, as a batch.
    pub fn take_batch(&mut self) -> Result<RecordBatch> {
        self.pending = 0;
        RecordBatch::try_new(
            self.schema.arrow.clone(),
            self.builders.iter_mut().map(Builder::finish).collect(),
        )
        .map_err(|e| DriverError::Other(e.to_string()))
    }

    fn check(&self, column: usize, found: Mismatch) -> Result<()> {
        found.map_err(|found| {
            let meta = &self.schema.columns[column];
            DriverError::Decode {
                type_name: meta.native_type.clone(),
                message: format!(
                    "column \"{}\" mixes types: value {found} at row {}, in a column read as {}. \
                     SQLite allows it; use CAST(\"{}\" AS TEXT) to see it in full",
                    meta.name,
                    self.rows,
                    meta.logical.as_str(),
                    meta.name
                ),
            }
        })
    }
}

/// Statements whose result is rows, even when there are none.
pub fn is_query(sql: &str) -> bool {
    first_word_is(sql, &["select", "with", "values", "pragma", "explain"])
}

/// Statements whose rows affected SQLite counts (`changes()` repeats the last count for the
/// rest).
pub fn is_dml(sql: &str) -> bool {
    first_word_is(sql, &["insert", "update", "delete", "replace"])
}

fn first_word_is(sql: &str, words: &[&str]) -> bool {
    let first = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    words.iter().any(|k| first.eq_ignore_ascii_case(k))
}

/// SQLite gives no position for "no such table: x" or "no such column: x". The name is searched
/// for in the statement (whole word, case-insensitive) so it can be underlined.
pub fn locate_name(error: DriverError, sql: &str) -> DriverError {
    let DriverError::Database {
        code,
        message,
        detail,
        hint,
        position: None,
    } = error
    else {
        return error;
    };
    let name = ["no such table: ", "no such column: "]
        .iter()
        .find_map(|prefix| message.strip_prefix(prefix))
        .map(str::trim);
    let position = name.and_then(|name| {
        // `main.t` may appear as `t` in the statement.
        find_word(sql, name).or_else(|| {
            name.rsplit('.')
                .next()
                .and_then(|last| find_word(sql, last))
        })
    });
    DriverError::Database {
        code,
        message,
        detail,
        hint,
        position,
    }
}

/// Position (in characters, 1-based) of `word` as a whole word within `sql`.
fn find_word(sql: &str, word: &str) -> Option<u32> {
    if word.is_empty() {
        return None;
    }
    let lower = sql.to_lowercase();
    let target = word.to_lowercase();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(found) = lower[from..].find(&target) {
        let start = from + found;
        let end = start + target.len();
        let before_ok = lower[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident(c));
        let after_ok = lower[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            // Same number of characters before in the original text and in the lowercased one,
            // except for exotic Unicode cases; counted on the original to be safe.
            return u32::try_from(sql.get(..start).map_or(0, |p| p.chars().count()) + 1).ok();
        }
        from = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_names_for_errors_without_position() {
        assert_eq!(
            find_word(
                "select id
from ordrs
where x",
                "ordrs"
            ),
            Some(16)
        );
        assert_eq!(
            find_word("select ordrs_id from ordrs", "ordrs"),
            Some(22),
            "whole word"
        );
        assert_eq!(find_word("select nothing", "ordrs"), None);
        let located = locate_name(
            DriverError::Database {
                code: None,
                message: "no such column: totl".into(),
                detail: None,
                hint: None,
                position: None,
            },
            "select ñ, totl from t",
        );
        assert!(
            matches!(
                located,
                DriverError::Database {
                    position: Some(11),
                    ..
                }
            ),
            "{located:?}"
        );
    }

    #[test]
    fn affinity_follows_sqlite_rules() {
        assert_eq!(affinity(Some("INTEGER")), Affinity::Integer);
        assert_eq!(affinity(Some("BIGINT")), Affinity::Integer);
        assert_eq!(affinity(Some("VARCHAR(80)")), Affinity::Text);
        assert_eq!(affinity(Some("DOUBLE PRECISION")), Affinity::Real);
        assert_eq!(affinity(Some("DECIMAL(10,2)")), Affinity::Numeric);
        assert_eq!(affinity(Some("BLOB")), Affinity::Blob);
        assert_eq!(affinity(None), Affinity::Blob);
        // Rule 1 before rule 2: "POINT" contains "INT".
        assert_eq!(affinity(Some("POINT")), Affinity::Integer);
    }

    #[test]
    fn decides_types_from_affinity_and_values() {
        let ints = Seen {
            int: true,
            ..Default::default()
        };
        let mixed_num = Seen {
            int: true,
            real: true,
            ..Default::default()
        };
        let text = Seen {
            text: true,
            ..Default::default()
        };
        assert_eq!(decide(Affinity::Integer, ints), LogicalType::Int);
        assert_eq!(decide(Affinity::Real, ints), LogicalType::Float);
        assert_eq!(decide(Affinity::Numeric, mixed_num), LogicalType::Float);
        assert_eq!(
            decide(
                Affinity::Integer,
                Seen {
                    int: true,
                    text: true,
                    ..Default::default()
                }
            ),
            LogicalType::Text
        );
        assert_eq!(
            decide(Affinity::Numeric, Seen::default()),
            LogicalType::Text
        );
        assert_eq!(decide(Affinity::Blob, text), LogicalType::Text);
    }

    #[test]
    fn lossless_coercions_only() {
        let mut int = Builder::new(LogicalType::Int);
        assert!(int.append_real(3.0).is_ok());
        assert_eq!(int.append_real(3.5), Err("REAL"));
        assert_eq!(int.append_text(b"x"), Err("TEXT"));
        let mut float = Builder::new(LogicalType::Float);
        assert!(float.append_integer(7).is_ok());
        let mut text = Builder::new(LogicalType::Text);
        assert!(text.append_blob(&[1, 2]).is_ok());
        assert_eq!(real_text(1.0), "1.0");
        assert_eq!(real_text(0.25), "0.25");
    }

    fn column(name: &str, declared: Option<&str>) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            declared: declared.map(str::to_string),
            origin: None,
        }
    }

    #[test]
    fn a_result_is_typed_from_its_first_rows_and_refuses_what_would_be_lost() {
        let first = vec![
            vec![SqlValue::Integer(1), SqlValue::Text("a".into())],
            vec![SqlValue::Integer(2)],
        ];
        let mut result = ResultBuilder::new(
            vec![column("id", Some("INTEGER")), column("x", None)],
            &first,
        )
        .unwrap();
        let schema = result.schema();
        assert_eq!(schema.columns[0].logical, LogicalType::Int);
        assert_eq!(schema.columns[0].native_type, "INTEGER");
        // No declared type: the storage class seen is the native type.
        assert_eq!(schema.columns[1].logical, LogicalType::Text);
        assert_eq!(schema.columns[1].native_type, "TEXT");
        assert_eq!((result.rows(), result.pending()), (2, 2));

        result.begin_row();
        result.append_real(0, 3.0).unwrap();
        result.append_integer(1, 7).unwrap();
        result.end_row();
        let batch = result.take_batch().unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(result.pending(), 0);

        result.begin_row();
        let error = result.append_text(0, b"three").unwrap_err();
        assert!(error.to_string().contains("value TEXT at row 4"), "{error}");
    }

    #[test]
    fn text_that_is_not_utf8_is_kept_whole() {
        assert_eq!(
            SqlValue::text_from_bytes(&[0xff, 0x00]),
            SqlValue::Blob(vec![0xff, 0x00])
        );
        assert_eq!(
            SqlValue::text_from_bytes(b"ok"),
            SqlValue::Text("ok".into())
        );
        let mut text = Builder::new(LogicalType::Text);
        text.append_text(&[0xff]).unwrap();
    }
}
