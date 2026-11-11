//! From MySQL/MariaDB text-protocol values to Arrow columns.
//!
//! Statements run with the text protocol, so every value arrives exactly as the server
//! writes it (what the `mysql` client shows). Numbers, dates and timestamps are parsed into
//! typed Arrow columns; the rest stays as text or bytes. Temporal columns are decided with
//! the first batch: legacy values such as `0000-00-00` do not fit a date, so a column that
//! has one is kept as text. A later value that does not fit fails with a clear message:
//! data is never lost silently.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, Date32Builder, Float64Builder, Int64Builder, StringBuilder,
    TimestampMicrosecondBuilder,
};
use mysql_async::Column;
use mysql_async::consts::{ColumnFlags, ColumnType};
use spelaion_driver_api::{ColumnMeta, ColumnOrigin, DriverError, LogicalType};

/// Character set number of binary strings (`BINARY`, `VARBINARY`, `BLOB`).
const BINARY_CHARSET: u16 = 63;

/// The engine's name for a column type, as close to `SHOW COLUMNS` as the protocol allows.
pub(crate) fn native_type(column: &Column) -> String {
    let flags = column.flags();
    let binary = column.character_set() == BINARY_CHARSET;
    let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);
    let base = match column.column_type() {
        ColumnType::MYSQL_TYPE_TINY => "tinyint",
        ColumnType::MYSQL_TYPE_SHORT => "smallint",
        ColumnType::MYSQL_TYPE_INT24 => "mediumint",
        ColumnType::MYSQL_TYPE_LONG => "int",
        ColumnType::MYSQL_TYPE_LONGLONG => "bigint",
        ColumnType::MYSQL_TYPE_FLOAT => "float",
        ColumnType::MYSQL_TYPE_DOUBLE => "double",
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => "decimal",
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => "date",
        ColumnType::MYSQL_TYPE_DATETIME | ColumnType::MYSQL_TYPE_DATETIME2 => "datetime",
        ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => "time",
        ColumnType::MYSQL_TYPE_YEAR => "year",
        ColumnType::MYSQL_TYPE_BIT => "bit",
        ColumnType::MYSQL_TYPE_JSON => "json",
        ColumnType::MYSQL_TYPE_GEOMETRY => "geometry",
        ColumnType::MYSQL_TYPE_NULL => "null",
        ColumnType::MYSQL_TYPE_ENUM => "enum",
        ColumnType::MYSQL_TYPE_SET => "set",
        ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING if binary => "varbinary",
        ColumnType::MYSQL_TYPE_VARCHAR | ColumnType::MYSQL_TYPE_VAR_STRING => "varchar",
        ColumnType::MYSQL_TYPE_STRING if flags.contains(ColumnFlags::ENUM_FLAG) => "enum",
        ColumnType::MYSQL_TYPE_STRING if flags.contains(ColumnFlags::SET_FLAG) => "set",
        ColumnType::MYSQL_TYPE_STRING if binary => "binary",
        ColumnType::MYSQL_TYPE_STRING => "char",
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB
            if binary =>
        {
            "blob"
        }
        ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => "text",
        other => return format!("{other:?}").to_ascii_lowercase(),
    };
    let numeric = matches!(
        column.column_type(),
        ColumnType::MYSQL_TYPE_TINY
            | ColumnType::MYSQL_TYPE_SHORT
            | ColumnType::MYSQL_TYPE_INT24
            | ColumnType::MYSQL_TYPE_LONG
            | ColumnType::MYSQL_TYPE_LONGLONG
            | ColumnType::MYSQL_TYPE_FLOAT
            | ColumnType::MYSQL_TYPE_DOUBLE
            | ColumnType::MYSQL_TYPE_DECIMAL
            | ColumnType::MYSQL_TYPE_NEWDECIMAL
    );
    if numeric && unsigned {
        format!("{base} unsigned")
    } else {
        base.to_string()
    }
}

/// Logical type from the column metadata (before seeing any value).
pub(crate) fn logical_type(column: &Column) -> LogicalType {
    let unsigned = column.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    match column.column_type() {
        // BIGINT UNSIGNED does not fit in an i64: kept exact as decimal text.
        ColumnType::MYSQL_TYPE_LONGLONG if unsigned => LogicalType::Decimal,
        ColumnType::MYSQL_TYPE_TINY
        | ColumnType::MYSQL_TYPE_SHORT
        | ColumnType::MYSQL_TYPE_INT24
        | ColumnType::MYSQL_TYPE_LONG
        | ColumnType::MYSQL_TYPE_LONGLONG
        | ColumnType::MYSQL_TYPE_YEAR => LogicalType::Int,
        ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE => LogicalType::Float,
        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => LogicalType::Decimal,
        ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE => LogicalType::Date,
        ColumnType::MYSQL_TYPE_DATETIME
        | ColumnType::MYSQL_TYPE_DATETIME2
        | ColumnType::MYSQL_TYPE_TIMESTAMP
        | ColumnType::MYSQL_TYPE_TIMESTAMP2 => LogicalType::Timestamp,
        // TIME is a duration (-838:59:59 to 838:59:59), not a time of day.
        ColumnType::MYSQL_TYPE_TIME | ColumnType::MYSQL_TYPE_TIME2 => LogicalType::Interval,
        ColumnType::MYSQL_TYPE_JSON => LogicalType::Json,
        ColumnType::MYSQL_TYPE_BIT | ColumnType::MYSQL_TYPE_GEOMETRY => LogicalType::Bytes,
        _ if column.character_set() == BINARY_CHARSET => LogicalType::Bytes,
        _ => LogicalType::Text,
    }
}

pub(crate) fn column_meta(column: &Column, logical: LogicalType) -> ColumnMeta {
    ColumnMeta {
        name: column.name_str().into_owned(),
        logical,
        native_type: native_type(column),
        native_id: Some(column.column_type() as u32),
        origin: origin(column),
    }
}

/// The table column a result column reads (ADR-052): the column definition's original table
/// and column names, empty for an expression. A view's column names the view.
fn origin(column: &Column) -> Option<ColumnOrigin> {
    let table = column.org_table_str();
    let name = column.org_name_str();
    (!table.is_empty() && !name.is_empty()).then(|| ColumnOrigin {
        table: table.into_owned(),
        column: name.into_owned(),
    })
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

/// `YYYY-MM-DD` → days since 1970-01-01. `None` for zero or impossible dates.
pub(crate) fn parse_date(text: &str) -> Option<i32> {
    let mut parts = text.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    i32::try_from(days_from_civil(y, m, d)).ok()
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` → microseconds since 1970-01-01.
pub(crate) fn parse_timestamp(text: &str) -> Option<i64> {
    let (date, time) = text.split_once(' ')?;
    let days = i64::from(parse_date(date)?);
    let (hms, fraction) = match time.split_once('.') {
        Some((hms, fraction)) => (hms, fraction),
        None => (time, ""),
    };
    let mut parts = hms.splitn(3, ':');
    let h: i64 = parts.next()?.parse().ok()?;
    let mi: i64 = parts.next()?.parse().ok()?;
    let s: i64 = parts.next()?.parse().ok()?;
    if h > 23 || mi > 59 || s > 59 || fraction.len() > 6 {
        return None;
    }
    let micros = if fraction.is_empty() {
        0
    } else {
        let padded = format!("{fraction:0<6}");
        padded.parse::<i64>().ok()?
    };
    Some(((days * 24 + h) * 60 + mi) * 60_000_000 + s * 1_000_000 + micros)
}

/// Whether `raw` fits the column's logical type (to decide temporal columns).
pub(crate) fn fits(logical: LogicalType, raw: &[u8]) -> bool {
    let text = || std::str::from_utf8(raw).ok();
    match logical {
        LogicalType::Date => text().and_then(parse_date).is_some(),
        LogicalType::Timestamp => text().and_then(parse_timestamp).is_some(),
        // Only temporal columns are decided by their values; binary data is never text.
        _ => true,
    }
}

pub(crate) enum ColumnBuilder {
    Int(Int64Builder),
    Float(Float64Builder),
    Text(StringBuilder),
    Bytes(BinaryBuilder),
    Date(Date32Builder),
    Timestamp(TimestampMicrosecondBuilder),
}

/// Why a value could not be appended: the caller adds the column name.
pub(crate) struct Mismatch(pub String);

impl ColumnBuilder {
    pub(crate) fn new(logical: LogicalType, capacity: usize) -> Self {
        match logical {
            LogicalType::Int => Self::Int(Int64Builder::with_capacity(capacity)),
            LogicalType::Float => Self::Float(Float64Builder::with_capacity(capacity)),
            LogicalType::Bytes => Self::Bytes(BinaryBuilder::with_capacity(capacity, 0)),
            LogicalType::Date => Self::Date(Date32Builder::with_capacity(capacity)),
            LogicalType::Timestamp => {
                Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            LogicalType::Bool
            | LogicalType::Decimal
            | LogicalType::Text
            | LogicalType::Time
            | LogicalType::TimestampTz
            | LogicalType::Uuid
            | LogicalType::Json
            | LogicalType::Interval
            | LogicalType::Array
            | LogicalType::Native => Self::Text(StringBuilder::with_capacity(capacity, 0)),
        }
    }

    pub(crate) fn append(&mut self, raw: Option<&[u8]>) -> Result<(), Mismatch> {
        let Some(raw) = raw else {
            match self {
                Self::Int(b) => b.append_null(),
                Self::Float(b) => b.append_null(),
                Self::Text(b) => b.append_null(),
                Self::Bytes(b) => b.append_null(),
                Self::Date(b) => b.append_null(),
                Self::Timestamp(b) => b.append_null(),
            }
            return Ok(());
        };
        if let Self::Bytes(b) = self {
            b.append_value(raw);
            return Ok(());
        }
        let text = std::str::from_utf8(raw)
            .map_err(|_| Mismatch("the server sent text that is not valid UTF-8".into()))?;
        let bad = || Mismatch(format!("value {text:?}"));
        match self {
            Self::Int(b) => b.append_value(text.parse().map_err(|_| bad())?),
            Self::Float(b) => b.append_value(text.parse().map_err(|_| bad())?),
            Self::Text(b) => b.append_value(text),
            Self::Date(b) => b.append_value(parse_date(text).ok_or_else(bad)?),
            Self::Timestamp(b) => b.append_value(parse_timestamp(text).ok_or_else(bad)?),
            Self::Bytes(_) => unreachable!("handled above"),
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Int(b) => Arc::new(b.finish()),
            Self::Float(b) => Arc::new(b.finish()),
            Self::Text(b) => Arc::new(b.finish()),
            Self::Bytes(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Timestamp(b) => Arc::new(b.finish()),
        }
    }
}

/// Error for a value that does not fit its column after the first batch.
pub(crate) fn mismatch_error(meta: &ColumnMeta, row: u64, mismatch: Mismatch) -> DriverError {
    DriverError::Decode {
        type_name: meta.native_type.clone(),
        message: format!(
            "column \"{}\": {} at row {row} does not fit a {} column. Use CAST(`{}` AS CHAR) to see it as text",
            meta.name,
            mismatch.0,
            meta.logical.as_str(),
            meta.name
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_api::Value;

    #[test]
    fn dates_and_timestamps_round_trip_to_the_same_text() {
        for text in [
            "1970-01-01",
            "2026-02-28",
            "2024-02-29",
            "0001-01-01",
            "9999-12-31",
        ] {
            assert_eq!(Value::Date(parse_date(text).unwrap()).to_string(), text);
        }
        for text in [
            "2026-01-31 10:20:30",
            "1999-12-31 23:59:59.5",
            "2026-01-31 00:00:00.123456",
        ] {
            assert_eq!(
                Value::Timestamp(parse_timestamp(text).unwrap()).to_string(),
                text
            );
        }
    }

    #[test]
    fn zero_and_impossible_dates_do_not_fit() {
        for text in ["0000-00-00", "2026-00-10", "2026-02-30", "2025-02-29", "x"] {
            assert_eq!(parse_date(text), None, "{text}");
        }
        assert_eq!(parse_timestamp("0000-00-00 00:00:00"), None);
        assert_eq!(parse_timestamp("2026-01-31 24:00:00"), None);
        assert!(!fits(LogicalType::Date, b"0000-00-00"));
        assert!(fits(LogicalType::Text, b"0000-00-00"));
        assert!(fits(LogicalType::Bytes, &[0xde, 0xad, 0xbe, 0xef]));
    }
}
