//! Statement execution and conversion of SQLite values to Arrow columns.
//!
//! SQLite has dynamic typing: a column can mix integers, text and blobs. Arrow needs one
//! type per column, so it is decided from the declared type and the first batch of rows.
//! After that, a value that does not fit is converted only if no information is lost;
//! otherwise the query fails with a clear message. Data is never lost silently.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, Float64Builder, Int64Builder, RecordBatch, StringBuilder,
};
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{ErrorCode, Statement};
use spelunk_driver_api::{
    ColumnMeta, DriverError, ExecEvent, LogicalType, Result, ResultSchema, Value,
};
use tokio::sync::mpsc;

pub fn map_error(e: rusqlite::Error) -> DriverError {
    match e {
        rusqlite::Error::SqliteFailure(err, message) => {
            if err.code == ErrorCode::OperationInterrupted {
                return DriverError::Cancelled;
            }
            DriverError::Database {
                code: Some(err.extended_code.to_string()),
                message: message.unwrap_or_else(|| err.to_string()),
                detail: None,
                hint: None,
                position: None,
            }
        }
        // Syntax errors: they carry the position (in bytes) within the statement.
        rusqlite::Error::SqlInputError {
            error,
            msg,
            sql,
            offset,
        } => DriverError::Database {
            code: Some(error.extended_code.to_string()),
            message: msg,
            detail: None,
            hint: None,
            position: usize::try_from(offset)
                .ok()
                .and_then(|o| sql.get(..o))
                .map(|prefix| prefix.chars().count() as u32 + 1),
        },
        rusqlite::Error::MultipleStatement => {
            DriverError::Other("expected a single statement (the core splits scripts)".into())
        }
        other => DriverError::Other(other.to_string()),
    }
}

/// SQLite gives no position for "no such table: x" or "no such column: x". The name is searched
/// for in the statement (whole word, case-insensitive) so it can be underlined.
fn locate_name(error: DriverError, sql: &str) -> DriverError {
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

/// SQLite type affinity from the declared type (rules from the documentation,
/// section 3.1 of "Datatypes In SQLite").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

fn affinity(declared: Option<&str>) -> Affinity {
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

#[derive(Debug, Default, Clone, Copy)]
struct Seen {
    int: bool,
    real: bool,
    text: bool,
    blob: bool,
}

impl Seen {
    fn add(&mut self, v: &SqlValue) {
        match v {
            SqlValue::Null => {}
            SqlValue::Integer(_) => self.int = true,
            SqlValue::Real(_) => self.real = true,
            SqlValue::Text(_) => self.text = true,
            SqlValue::Blob(_) => self.blob = true,
        }
    }

    fn storage_class(self) -> &'static str {
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
fn decide(affinity: Affinity, seen: Seen) -> LogicalType {
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

enum Builder {
    Int(Int64Builder),
    Float(Float64Builder),
    Bytes(BinaryBuilder),
    Text(StringBuilder),
}

impl Builder {
    fn new(logical: LogicalType) -> Self {
        match logical {
            LogicalType::Int => Self::Int(Int64Builder::new()),
            LogicalType::Float => Self::Float(Float64Builder::new()),
            LogicalType::Bytes => Self::Bytes(BinaryBuilder::new()),
            _ => Self::Text(StringBuilder::new()),
        }
    }

    /// Adds a value; `Err` with the SQLite type name if it does not fit without losing information.
    fn append(&mut self, v: ValueRef<'_>) -> std::result::Result<(), &'static str> {
        match (self, v) {
            (Self::Int(b), ValueRef::Null) => b.append_null(),
            (Self::Float(b), ValueRef::Null) => b.append_null(),
            (Self::Bytes(b), ValueRef::Null) => b.append_null(),
            (Self::Text(b), ValueRef::Null) => b.append_null(),

            (Self::Int(b), ValueRef::Integer(i)) => b.append_value(i),
            (Self::Int(b), ValueRef::Real(x)) if x.fract() == 0.0 && x.abs() < 9.0e15 => {
                b.append_value(x as i64)
            }
            (Self::Float(b), ValueRef::Real(x)) => b.append_value(x),
            // Exact up to 2^53; beyond that, a SQLite REAL would not be exact either.
            (Self::Float(b), ValueRef::Integer(i)) if i.unsigned_abs() <= 1 << 53 => {
                b.append_value(i as f64)
            }
            (Self::Bytes(b), ValueRef::Blob(x)) => b.append_value(x),

            (Self::Text(b), ValueRef::Text(t)) => match std::str::from_utf8(t) {
                Ok(s) => b.append_value(s),
                // Invalid UTF-8: as hexadecimal, without losing any byte.
                Err(_) => b.append_value(Value::Bytes(t.to_vec()).to_string()),
            },
            (Self::Text(b), ValueRef::Integer(i)) => b.append_value(i.to_string()),
            (Self::Text(b), ValueRef::Real(x)) => b.append_value(real_text(x)),
            (Self::Text(b), ValueRef::Blob(x)) => {
                b.append_value(Value::Bytes(x.to_vec()).to_string())
            }

            (_, ValueRef::Integer(_)) => return Err("INTEGER"),
            (_, ValueRef::Real(_)) => return Err("REAL"),
            (_, ValueRef::Text(_)) => return Err("TEXT"),
            (_, ValueRef::Blob(_)) => return Err("BLOB"),
        }
        Ok(())
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Int(b) => Arc::new(b.finish()),
            Self::Float(b) => Arc::new(b.finish()),
            Self::Bytes(b) => Arc::new(b.finish()),
            Self::Text(b) => Arc::new(b.finish()),
        }
    }
}

/// A REAL as the SQLite shell writes it: `1.0`, not `1`.
fn real_text(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        format!("{x:.1}")
    } else {
        Value::Float(x).to_string()
    }
}

fn to_owned(v: ValueRef<'_>) -> SqlValue {
    match v {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(i) => SqlValue::Integer(i),
        ValueRef::Real(x) => SqlValue::Real(x),
        ValueRef::Text(t) => match std::str::from_utf8(t) {
            Ok(s) => SqlValue::Text(s.to_string()),
            // SQLite does not guarantee valid UTF-8 in TEXT: treated as bytes so nothing is lost.
            Err(_) => SqlValue::Blob(t.to_vec()),
        },
        ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
    }
}

fn as_ref(v: &SqlValue) -> ValueRef<'_> {
    match v {
        SqlValue::Null => ValueRef::Null,
        SqlValue::Integer(i) => ValueRef::Integer(*i),
        SqlValue::Real(x) => ValueRef::Real(*x),
        SqlValue::Text(t) => ValueRef::Text(t.as_bytes()),
        SqlValue::Blob(b) => ValueRef::Blob(b),
    }
}

fn send(events: &mpsc::Sender<Result<ExecEvent>>, event: ExecEvent) -> Result<()> {
    // If the receiver is gone (tab closed, stream dropped), stop reading.
    events
        .blocking_send(Ok(event))
        .map_err(|_| DriverError::Cancelled)
}

pub fn execute(
    conn: &rusqlite::Connection,
    sql: &str,
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> Result<()> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| locate_name(map_error(e), sql))?;
    if stmt.column_count() == 0 {
        let readonly = stmt.readonly();
        stmt.raw_execute().map_err(map_error)?;
        // Only INSERT/UPDATE/DELETE have a count; changes() repeats the last one for the rest.
        let counts = !readonly && is_dml(sql);
        return send(
            events,
            ExecEvent::Done {
                rows_affected: counts.then(|| conn.changes()),
            },
        );
    }
    match stream_rows(&mut stmt, batch_size, events) {
        Err(DriverError::Cancelled) if events.is_closed() => Ok(()),
        other => other,
    }
}

fn is_dml(sql: &str) -> bool {
    let first = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    ["insert", "update", "delete", "replace"]
        .iter()
        .any(|k| first.eq_ignore_ascii_case(k))
}

fn stream_rows(
    stmt: &mut Statement<'_>,
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> Result<()> {
    let names: Vec<String> = stmt.column_names().map(str::to_string).collect();
    let declared: Vec<Option<String>> = stmt
        .columns()
        .map(|c| c.decl_type().map(str::to_string))
        .collect();
    let ncols = names.len();
    let mut rows = stmt.raw_query();

    // First batch in memory to decide the types.
    let mut first: Vec<Vec<SqlValue>> = Vec::with_capacity(batch_size.min(4096));
    while first.len() < batch_size {
        let Some(row) = rows.next().map_err(map_error)? else {
            break;
        };
        first.push(
            (0..ncols)
                .map(|i| row.get_ref(i).map(to_owned))
                .collect::<rusqlite::Result<_>>()
                .map_err(map_error)?,
        );
    }
    let mut seen = vec![Seen::default(); ncols];
    for row in &first {
        for (s, v) in seen.iter_mut().zip(row) {
            s.add(v);
        }
    }
    let columns: Vec<ColumnMeta> = (0..ncols)
        .map(|i| ColumnMeta {
            name: names[i].clone(),
            logical: decide(affinity(declared[i].as_deref()), seen[i]),
            native_type: declared[i]
                .clone()
                .unwrap_or_else(|| seen[i].storage_class().to_string()),
            native_id: None,
        })
        .collect();
    let schema = Arc::new(ResultSchema::new(columns));
    send(events, ExecEvent::Columns(schema.clone()))?;

    let mismatch = |col: usize, found: &str, row: u64| DriverError::Decode {
        type_name: schema.columns[col].native_type.clone(),
        message: format!(
            "column \"{}\" mixes types: value {found} at row {row}, in a column read as {}. \
             SQLite allows it; use CAST(\"{}\" AS TEXT) to see it in full",
            schema.columns[col].name,
            schema.columns[col].logical.as_str(),
            schema.columns[col].name
        ),
    };

    let mut builders: Vec<Builder> = schema
        .columns
        .iter()
        .map(|c| Builder::new(c.logical))
        .collect();
    let mut total: u64 = 0;
    for row in &first {
        total += 1;
        for (i, v) in row.iter().enumerate() {
            builders[i]
                .append(as_ref(v))
                .map_err(|found| mismatch(i, found, total))?;
        }
    }
    let mut pending = first.len();
    drop(first);
    let finish = |builders: &mut Vec<Builder>| {
        RecordBatch::try_new(
            schema.arrow.clone(),
            builders.iter_mut().map(Builder::finish).collect(),
        )
        .map_err(|e| DriverError::Other(e.to_string()))
    };
    if pending == batch_size {
        send(events, ExecEvent::Batch(finish(&mut builders)?))?;
        pending = 0;
    }

    while let Some(row) = rows.next().map_err(map_error)? {
        total += 1;
        for (i, builder) in builders.iter_mut().enumerate() {
            let v = row.get_ref(i).map_err(map_error)?;
            builder
                .append(v)
                .map_err(|found| mismatch(i, found, total))?;
        }
        pending += 1;
        if pending == batch_size {
            send(events, ExecEvent::Batch(finish(&mut builders)?))?;
            pending = 0;
        }
    }
    if pending > 0 {
        send(events, ExecEvent::Batch(finish(&mut builders)?))?;
    }
    send(
        events,
        ExecEvent::Done {
            rows_affected: Some(total),
        },
    )
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
        assert!(int.append(ValueRef::Real(3.0)).is_ok());
        assert_eq!(int.append(ValueRef::Real(3.5)), Err("REAL"));
        assert_eq!(int.append(ValueRef::Text(b"x")), Err("TEXT"));
        let mut float = Builder::new(LogicalType::Float);
        assert!(float.append(ValueRef::Integer(7)).is_ok());
        let mut text = Builder::new(LogicalType::Text);
        assert!(text.append(ValueRef::Blob(&[1, 2])).is_ok());
        assert_eq!(real_text(1.0), "1.0");
        assert_eq!(real_text(0.25), "0.25");
    }
}
