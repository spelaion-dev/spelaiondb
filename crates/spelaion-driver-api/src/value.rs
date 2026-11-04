use std::fmt;

use arrow::array::{
    Array, BinaryArray, BooleanArray, Date32Array, Float64Array, Int64Array, StringArray,
    Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, TimeUnit};

/// Common logical type. Decides how the column is stored in Arrow and how it is rendered.
/// Anything that fits no common type is `Native`: its text is kept and never lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalType {
    Bool,
    Int,
    Float,
    /// Exact decimal, stored as text to avoid losing precision.
    Decimal,
    Text,
    Bytes,
    /// Days since 1970-01-01.
    Date,
    /// Microseconds since midnight.
    Time,
    /// Microseconds since 1970-01-01, without time zone.
    Timestamp,
    /// Microseconds since 1970-01-01 UTC.
    TimestampTz,
    Uuid,
    Json,
    Interval,
    /// Array, stored in the engine's text format (`{1,2,3}`).
    Array,
    Native,
}

/// Value representing ±infinity in `Date`, `Timestamp` and `TimestampTz`.
pub const INFINITY_DATE: i32 = i32::MAX;
pub const NEG_INFINITY_DATE: i32 = i32::MIN;
pub const INFINITY_MICROS: i64 = i64::MAX;
pub const NEG_INFINITY_MICROS: i64 = i64::MIN;

impl LogicalType {
    pub const ALL: [LogicalType; 15] = [
        Self::Bool,
        Self::Int,
        Self::Float,
        Self::Decimal,
        Self::Text,
        Self::Bytes,
        Self::Date,
        Self::Time,
        Self::Timestamp,
        Self::TimestampTz,
        Self::Uuid,
        Self::Json,
        Self::Interval,
        Self::Array,
        Self::Native,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::Decimal => "decimal",
            Self::Text => "text",
            Self::Bytes => "bytes",
            Self::Date => "date",
            Self::Time => "time",
            Self::Timestamp => "timestamp",
            Self::TimestampTz => "timestamptz",
            Self::Uuid => "uuid",
            Self::Json => "json",
            Self::Interval => "interval",
            Self::Array => "array",
            Self::Native => "native",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    pub fn arrow_type(self) -> DataType {
        match self {
            Self::Bool => DataType::Boolean,
            Self::Int => DataType::Int64,
            Self::Float => DataType::Float64,
            Self::Bytes => DataType::Binary,
            Self::Date => DataType::Date32,
            Self::Time => DataType::Time64(TimeUnit::Microsecond),
            Self::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Self::TimestampTz => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            Self::Decimal
            | Self::Text
            | Self::Uuid
            | Self::Json
            | Self::Interval
            | Self::Array
            | Self::Native => DataType::Utf8,
        }
    }
}

/// A single value. Results travel as Arrow; `Value` is for reading individual cells
/// (MCP, editing, tests) and for displaying them as text.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(i32),
    Time(i64),
    Timestamp(i64),
    TimestampTz(i64),
    Uuid(String),
    Json(String),
    Interval(String),
    Array(String),
    Native(String),
}

impl Value {
    /// Reads row `row` from an Arrow column built with `logical.arrow_type()`.
    /// Returns `None` if the array does not have the expected physical type.
    pub fn from_array(array: &dyn Array, row: usize, logical: LogicalType) -> Option<Value> {
        if array.is_null(row) {
            return Some(Value::Null);
        }
        let any = array.as_any();
        let text = || {
            any.downcast_ref::<StringArray>()
                .map(|a| a.value(row).to_string())
        };
        Some(match logical {
            LogicalType::Bool => Value::Bool(any.downcast_ref::<BooleanArray>()?.value(row)),
            LogicalType::Int => Value::Int(any.downcast_ref::<Int64Array>()?.value(row)),
            LogicalType::Float => Value::Float(any.downcast_ref::<Float64Array>()?.value(row)),
            LogicalType::Bytes => {
                Value::Bytes(any.downcast_ref::<BinaryArray>()?.value(row).to_vec())
            }
            LogicalType::Date => Value::Date(any.downcast_ref::<Date32Array>()?.value(row)),
            LogicalType::Time => {
                Value::Time(any.downcast_ref::<Time64MicrosecondArray>()?.value(row))
            }
            LogicalType::Timestamp => {
                Value::Timestamp(any.downcast_ref::<TimestampMicrosecondArray>()?.value(row))
            }
            LogicalType::TimestampTz => {
                Value::TimestampTz(any.downcast_ref::<TimestampMicrosecondArray>()?.value(row))
            }
            LogicalType::Decimal => Value::Decimal(text()?),
            LogicalType::Text => Value::Text(text()?),
            LogicalType::Uuid => Value::Uuid(text()?),
            LogicalType::Json => Value::Json(text()?),
            LogicalType::Interval => Value::Interval(text()?),
            LogicalType::Array => Value::Array(text()?),
            LogicalType::Native => Value::Native(text()?),
        })
    }
}

/// Text exactly as the engine's console would show it (PostgreSQL style).
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Bool(b) => f.write_str(if *b { "true" } else { "false" }),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write_float(f, *x),
            Value::Bytes(b) => {
                f.write_str("\\x")?;
                b.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
            }
            Value::Date(d) => write_date(f, *d),
            Value::Time(us) => write_time(f, *us),
            Value::Timestamp(us) => write_timestamp(f, *us),
            Value::TimestampTz(us) => {
                write_timestamp(f, *us)?;
                if *us != INFINITY_MICROS && *us != NEG_INFINITY_MICROS {
                    f.write_str("+00")?;
                }
                Ok(())
            }
            Value::Decimal(s)
            | Value::Text(s)
            | Value::Uuid(s)
            | Value::Json(s)
            | Value::Interval(s)
            | Value::Array(s)
            | Value::Native(s) => f.write_str(s),
        }
    }
}

fn write_float(f: &mut fmt::Formatter<'_>, x: f64) -> fmt::Result {
    if x.is_nan() {
        f.write_str("NaN")
    } else if x.is_infinite() {
        f.write_str(if x > 0.0 { "Infinity" } else { "-Infinity" })
    } else {
        write!(f, "{x}")
    }
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

fn write_date(f: &mut fmt::Formatter<'_>, days: i32) -> fmt::Result {
    match days {
        INFINITY_DATE => f.write_str("infinity"),
        NEG_INFINITY_DATE => f.write_str("-infinity"),
        _ => {
            let (y, m, d) = civil_from_days(days.into());
            if y <= 0 {
                // There is no year 0: year 0 is 1 BC.
                write!(f, "{:04}-{m:02}-{d:02} BC", 1 - y)
            } else {
                write!(f, "{y:04}-{m:02}-{d:02}")
            }
        }
    }
}

fn write_time(f: &mut fmt::Formatter<'_>, micros: i64) -> fmt::Result {
    let secs = micros / 1_000_000;
    write!(
        f,
        "{:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )?;
    write_fraction(f, micros % 1_000_000)
}

fn write_timestamp(f: &mut fmt::Formatter<'_>, micros: i64) -> fmt::Result {
    match micros {
        INFINITY_MICROS => f.write_str("infinity"),
        NEG_INFINITY_MICROS => f.write_str("-infinity"),
        _ => {
            let days = micros.div_euclid(MICROS_PER_DAY);
            let time = micros.rem_euclid(MICROS_PER_DAY);
            let (y, m, d) = civil_from_days(days);
            let bc = y <= 0;
            write!(f, "{:04}-{m:02}-{d:02} ", if bc { 1 - y } else { y })?;
            write_time(f, time)?;
            if bc {
                f.write_str(" BC")?;
            }
            Ok(())
        }
    }
}

/// Fractional seconds without trailing zeros (`.5`, `.123456`), like PostgreSQL.
fn write_fraction(f: &mut fmt::Formatter<'_>, micros: i64) -> fmt::Result {
    if micros == 0 {
        return Ok(());
    }
    let digits = format!("{micros:06}");
    write!(f, ".{}", digits.trim_end_matches('0'))
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian calendar.
/// Howard Hinnant's algorithm, valid over the whole relevant i64 range.
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_type_names_round_trip() {
        for t in LogicalType::ALL {
            assert_eq!(LogicalType::parse(t.as_str()), Some(t));
        }
    }

    #[test]
    fn dates_and_timestamps_display_like_postgres() {
        assert_eq!(Value::Date(0).to_string(), "1970-01-01");
        assert_eq!(Value::Date(20_454).to_string(), "2026-01-01");
        assert_eq!(Value::Date(-719_163).to_string(), "0001-12-31 BC");
        assert_eq!(Value::Date(-719_162).to_string(), "0001-01-01");
        assert_eq!(Value::Date(INFINITY_DATE).to_string(), "infinity");
        assert_eq!(
            Value::Timestamp(1_500_000).to_string(),
            "1970-01-01 00:00:01.5"
        );
        assert_eq!(
            Value::Timestamp(-1).to_string(),
            "1969-12-31 23:59:59.999999"
        );
        assert_eq!(Value::TimestampTz(0).to_string(), "1970-01-01 00:00:00+00");
        assert_eq!(
            Value::TimestampTz(NEG_INFINITY_MICROS).to_string(),
            "-infinity"
        );
        assert_eq!(Value::Time(45_296_000_100).to_string(), "12:34:56.0001");
    }

    #[test]
    fn other_values_display() {
        assert_eq!(Value::Bytes(vec![0xde, 0xad]).to_string(), "\\xdead");
        assert_eq!(Value::Float(f64::NAN).to_string(), "NaN");
        assert_eq!(Value::Float(1.5).to_string(), "1.5");
        assert_eq!(Value::Null.to_string(), "NULL");
    }
}
