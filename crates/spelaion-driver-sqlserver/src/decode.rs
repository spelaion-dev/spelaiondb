//! TDS values (`ColumnData`) to SpelaionDB's Arrow columns (ADR-005).
//!
//! The column's type comes from the result metadata, so each column gets one logical type
//! before the first row. A value that does not match it (`sql_variant` holds anything) is
//! kept as text rather than lost.

use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Float64Builder, Int64Builder,
    StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use spelaion_driver_api::{ColumnMeta, LogicalType};
use tiberius::numeric::Numeric;
use tiberius::time::{Date, DateTime2, Time};
use tiberius::{ColumnData, ColumnType};

/// Days from 0001-01-01 (SQL Server's `date`) to 1970-01-01.
const DAYS_0001_TO_1970: i64 = 719_162;
/// Days from 1900-01-01 (`datetime`, `smalldatetime`) to 1970-01-01.
const DAYS_1900_TO_1970: i64 = 25_567;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The logical type and SQL Server's name for a column type.
pub fn column_meta(name: &str, column_type: ColumnType) -> ColumnMeta {
    let (logical, native) = match column_type {
        ColumnType::Bit | ColumnType::Bitn => (LogicalType::Bool, "bit"),
        ColumnType::Int1 => (LogicalType::Int, "tinyint"),
        ColumnType::Int2 => (LogicalType::Int, "smallint"),
        ColumnType::Int4 | ColumnType::Intn => (LogicalType::Int, "int"),
        ColumnType::Int8 => (LogicalType::Int, "bigint"),
        ColumnType::Float4 => (LogicalType::Float, "real"),
        ColumnType::Float8 | ColumnType::Floatn => (LogicalType::Float, "float"),
        ColumnType::Money => (LogicalType::Decimal, "money"),
        ColumnType::Money4 => (LogicalType::Decimal, "smallmoney"),
        ColumnType::Decimaln => (LogicalType::Decimal, "decimal"),
        ColumnType::Numericn => (LogicalType::Decimal, "numeric"),
        ColumnType::Guid => (LogicalType::Uuid, "uniqueidentifier"),
        ColumnType::BigVarChar => (LogicalType::Text, "varchar"),
        ColumnType::BigChar => (LogicalType::Text, "char"),
        ColumnType::NVarchar => (LogicalType::Text, "nvarchar"),
        ColumnType::NChar => (LogicalType::Text, "nchar"),
        ColumnType::Text => (LogicalType::Text, "text"),
        ColumnType::NText => (LogicalType::Text, "ntext"),
        ColumnType::Xml => (LogicalType::Text, "xml"),
        ColumnType::BigVarBin => (LogicalType::Bytes, "varbinary"),
        ColumnType::BigBinary => (LogicalType::Bytes, "binary"),
        ColumnType::Image => (LogicalType::Bytes, "image"),
        ColumnType::Udt => (LogicalType::Bytes, "udt"),
        ColumnType::Datetime | ColumnType::Datetimen => (LogicalType::Timestamp, "datetime"),
        ColumnType::Datetime4 => (LogicalType::Timestamp, "smalldatetime"),
        ColumnType::Datetime2 => (LogicalType::Timestamp, "datetime2"),
        ColumnType::Daten => (LogicalType::Date, "date"),
        ColumnType::Timen => (LogicalType::Time, "time"),
        ColumnType::DatetimeOffsetn => (LogicalType::TimestampTz, "datetimeoffset"),
        ColumnType::SSVariant => (LogicalType::Native, "sql_variant"),
        ColumnType::Null => (LogicalType::Text, "null"),
    };
    ColumnMeta {
        name: name.to_string(),
        logical,
        native_type: native.to_string(),
        native_id: None,
        origin: None,
    }
}

/// A decimal as SQL Server writes it: `12.50`, `-0.5`, `12` (scale 0).
pub fn numeric(n: Numeric) -> String {
    let value = n.value();
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(n.scale());
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{sign}{int}.{frac}")
}

fn date_days(date: Date) -> i64 {
    i64::from(date.days()) - DAYS_0001_TO_1970
}

/// Microseconds since midnight; `time(7)` has 100 ns steps, cut to microseconds.
fn time_micros(time: Time) -> i64 {
    let increments = i128::from(time.increments());
    let per_second = 10i128.pow(u32::from(time.scale()));
    i64::try_from(increments * 1_000_000 / per_second).unwrap_or(0)
}

fn datetime2_micros(value: DateTime2) -> i64 {
    date_days(value.date()) * MICROS_PER_DAY + time_micros(value.time())
}

/// The value as a timestamp in microseconds, for the date and time types.
fn timestamp(data: &ColumnData<'static>) -> Option<i64> {
    match data {
        // 1/300 s steps, shown by SQL Server rounded to the millisecond (.003, .007).
        ColumnData::DateTime(Some(dt)) => {
            let days = i64::from(dt.days()) - DAYS_1900_TO_1970;
            let millis = (i64::from(dt.seconds_fragments()) * 10 + 1) / 3;
            Some(days * MICROS_PER_DAY + millis * 1_000)
        }
        ColumnData::SmallDateTime(Some(dt)) => {
            let days = i64::from(dt.days()) - DAYS_1900_TO_1970;
            Some(days * MICROS_PER_DAY + i64::from(dt.seconds_fragments()) * 60_000_000)
        }
        ColumnData::DateTime2(Some(dt)) => Some(datetime2_micros(*dt)),
        // The date and time of a `datetimeoffset` travel in UTC.
        ColumnData::DateTimeOffset(Some(dto)) => Some(datetime2_micros(dto.datetime2())),
        _ => None,
    }
}

/// Any value as text, for columns without a typed Arrow column and for values that do not
/// fit their column's type.
pub fn text(data: &ColumnData<'static>) -> Option<String> {
    Some(match data {
        ColumnData::U8(v) => v.as_ref()?.to_string(),
        ColumnData::I16(v) => v.as_ref()?.to_string(),
        ColumnData::I32(v) => v.as_ref()?.to_string(),
        ColumnData::I64(v) => v.as_ref()?.to_string(),
        ColumnData::F32(v) => v.as_ref()?.to_string(),
        ColumnData::F64(v) => v.as_ref()?.to_string(),
        ColumnData::Bit(v) => if (*v)? { "1" } else { "0" }.to_string(),
        ColumnData::String(v) => v.as_ref()?.to_string(),
        // SQL Server writes GUIDs in upper case.
        ColumnData::Guid(v) => v.as_ref()?.to_string().to_uppercase(),
        ColumnData::Binary(v) => {
            let bytes = v.as_ref()?;
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            format!("0x{hex}")
        }
        ColumnData::Numeric(v) => numeric((*v)?),
        ColumnData::Xml(v) => v.as_ref()?.to_string(),
        ColumnData::Date(v) => {
            let days = date_days((*v)?);
            spelaion_driver_api::Value::Date(i32::try_from(days).ok()?).to_string()
        }
        ColumnData::Time(v) => spelaion_driver_api::Value::Time(time_micros((*v)?)).to_string(),
        ColumnData::DateTimeOffset(Some(dto)) => {
            let micros = datetime2_micros(dto.datetime2());
            spelaion_driver_api::Value::TimestampTz(micros).to_string()
        }
        other => spelaion_driver_api::Value::Timestamp(timestamp(other)?).to_string(),
    })
}

/// Builds one Arrow column of a given logical type, row by row.
pub enum ColumnBuilder {
    Bool(BooleanBuilder),
    Int(Int64Builder),
    Float(Float64Builder),
    Bytes(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Timestamp(TimestampMicrosecondBuilder, bool),
    /// Text, decimals, GUIDs, money and anything without its own column.
    Text(StringBuilder, ColumnType),
}

impl ColumnBuilder {
    pub fn new(meta: &ColumnMeta, column_type: ColumnType, capacity: usize) -> Self {
        match meta.logical {
            LogicalType::Bool => Self::Bool(BooleanBuilder::with_capacity(capacity)),
            LogicalType::Int => Self::Int(Int64Builder::with_capacity(capacity)),
            LogicalType::Float => Self::Float(Float64Builder::with_capacity(capacity)),
            LogicalType::Bytes => Self::Bytes(BinaryBuilder::with_capacity(capacity, 0)),
            LogicalType::Date => Self::Date(Date32Builder::with_capacity(capacity)),
            LogicalType::Time => Self::Time(Time64MicrosecondBuilder::with_capacity(capacity)),
            LogicalType::Timestamp => {
                Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity), false)
            }
            LogicalType::TimestampTz => {
                Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity), true)
            }
            LogicalType::Decimal
            | LogicalType::Text
            | LogicalType::Uuid
            | LogicalType::Json
            | LogicalType::Interval
            | LogicalType::Array
            | LogicalType::Native => Self::Text(
                StringBuilder::with_capacity(capacity, capacity * 8),
                column_type,
            ),
        }
    }

    /// Appends a value; `Err` with its text when it does not fit this column's type.
    pub fn append(&mut self, data: &ColumnData<'static>) -> Result<(), String> {
        let mismatch = || text(data).unwrap_or_default();
        match self {
            Self::Bool(b) => match data {
                ColumnData::Bit(v) => b.append_option(*v),
                other if text(other).is_none() => b.append_null(),
                _ => return Err(mismatch()),
            },
            Self::Int(b) => match data {
                ColumnData::U8(v) => b.append_option(v.map(i64::from)),
                ColumnData::I16(v) => b.append_option(v.map(i64::from)),
                ColumnData::I32(v) => b.append_option(v.map(i64::from)),
                ColumnData::I64(v) => b.append_option(*v),
                other if text(other).is_none() => b.append_null(),
                _ => return Err(mismatch()),
            },
            Self::Float(b) => match data {
                // Through the shortest text of the f32: 0.1 stays 0.1.
                ColumnData::F32(v) => {
                    b.append_option(v.map(|x| x.to_string().parse().unwrap_or(f64::from(x))))
                }
                ColumnData::F64(v) => b.append_option(*v),
                other if text(other).is_none() => b.append_null(),
                _ => return Err(mismatch()),
            },
            Self::Bytes(b) => match data {
                ColumnData::Binary(v) => b.append_option(v.as_deref()),
                other if text(other).is_none() => b.append_null(),
                _ => return Err(mismatch()),
            },
            Self::Date(b) => match data {
                ColumnData::Date(v) => {
                    b.append_option(v.and_then(|d| i32::try_from(date_days(d)).ok()))
                }
                other if text(other).is_none() => b.append_null(),
                _ => return Err(mismatch()),
            },
            Self::Time(b) => match data {
                ColumnData::Time(v) => b.append_option(v.map(time_micros)),
                other if text(other).is_none() => b.append_null(),
                _ => return Err(mismatch()),
            },
            Self::Timestamp(b, _) => match timestamp(data) {
                Some(micros) => b.append_value(micros),
                None if text(data).is_none() => b.append_null(),
                None => return Err(mismatch()),
            },
            Self::Text(b, column_type) => match (data, *column_type) {
                // Money arrives as a float with 4 decimals; SQL Server shows all four.
                (ColumnData::F64(Some(v)), ColumnType::Money | ColumnType::Money4) => {
                    b.append_value(format!("{v:.4}"))
                }
                (other, _) => b.append_option(text(other)),
            },
        }
        Ok(())
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Int(b) => Arc::new(b.finish()),
            Self::Float(b) => Arc::new(b.finish()),
            Self::Bytes(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Time(b) => Arc::new(b.finish()),
            Self::Timestamp(b, true) => Arc::new(b.finish().with_timezone("UTC")),
            Self::Timestamp(b, false) => Arc::new(b.finish()),
            Self::Text(b, _) => Arc::new(b.finish()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_read_like_sql_server() {
        assert_eq!(numeric(Numeric::new_with_scale(1250, 2)), "12.50");
        assert_eq!(numeric(Numeric::new_with_scale(-5, 1)), "-0.5");
        assert_eq!(numeric(Numeric::new_with_scale(12, 0)), "12");
        assert_eq!(numeric(Numeric::new_with_scale(7, 3)), "0.007");
    }

    #[test]
    fn datetime_fractions_round_to_milliseconds() {
        let at = |fragments| {
            timestamp(&ColumnData::DateTime(Some(tiberius::time::DateTime::new(
                0, fragments,
            ))))
        };
        let base = -DAYS_1900_TO_1970 * MICROS_PER_DAY;
        assert_eq!(at(1), Some(base + 3_000));
        assert_eq!(at(2), Some(base + 7_000));
        assert_eq!(at(300), Some(base + 1_000_000));
    }
}
