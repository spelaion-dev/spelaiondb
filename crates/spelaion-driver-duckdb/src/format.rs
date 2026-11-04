//! Text of DuckDB values exactly as `CAST(… AS VARCHAR)` writes them, for the types that
//! travel as text (nested types, intervals, unsigned 64-bit and 128-bit integers…).
//!
//! DuckDB's Arrow export gives typed arrays; these functions write them back the way DuckDB's
//! own console does. The type zoo (`tests/duckdb.rs`) compares them with DuckDB's casts.

use std::fmt::Write;

use arrow::array::{
    Array, AsArray, BinaryArray, BinaryViewArray, BooleanArray, Decimal128Array, Decimal256Array,
    FixedSizeBinaryArray, LargeBinaryArray, LargeStringArray, StringArray, StringViewArray,
};
use arrow::datatypes::{
    DataType, Date32Type, Date64Type, Float16Type, Float32Type, Float64Type, Int8Type, Int16Type,
    Int32Type, Int64Type, IntervalMonthDayNanoType, IntervalUnit, Time32MillisecondType,
    Time32SecondType, Time64MicrosecondType, Time64NanosecondType, TimeUnit,
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow::util::display::{ArrayFormatter, FormatOptions};

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The text of a non-null value at the top level (strings without quotes).
pub fn text(array: &dyn Array, row: usize) -> String {
    let mut out = String::new();
    write_value(&mut out, array, row, false);
    out
}

/// Writes one value; `nested` is true inside a list, struct or map, where DuckDB quotes the
/// strings that need it.
fn write_value(out: &mut String, array: &dyn Array, row: usize, nested: bool) {
    if array.is_null(row) {
        out.push_str("NULL");
        return;
    }
    // A string inside a nested value, quoted if DuckDB would quote it.
    let string = |out: &mut String, s: &str| {
        if nested {
            push_nested_string(out, s);
        } else {
            out.push_str(s);
        }
    };
    match array.data_type() {
        DataType::Boolean => {
            let b = array
                .as_any()
                .downcast_ref::<BooleanArray>()
                .map(|a| a.value(row));
            out.push_str(if b == Some(true) { "true" } else { "false" });
        }
        DataType::Int8 => push(out, array.as_primitive::<Int8Type>().value(row)),
        DataType::Int16 => push(out, array.as_primitive::<Int16Type>().value(row)),
        DataType::Int32 => push(out, array.as_primitive::<Int32Type>().value(row)),
        DataType::Int64 => push(out, array.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => push(out, array.as_primitive::<UInt8Type>().value(row)),
        DataType::UInt16 => push(out, array.as_primitive::<UInt16Type>().value(row)),
        DataType::UInt32 => push(out, array.as_primitive::<UInt32Type>().value(row)),
        DataType::UInt64 => push(out, array.as_primitive::<UInt64Type>().value(row)),
        DataType::Float16 => {
            out.push_str(&float(f64::from(
                array.as_primitive::<Float16Type>().value(row),
            )));
        }
        // Through the shortest text of the f32: 0.1 stays 0.1.
        DataType::Float32 => {
            let x = array.as_primitive::<Float32Type>().value(row);
            out.push_str(&float(x.to_string().parse().unwrap_or(f64::from(x))));
        }
        DataType::Float64 => out.push_str(&float(array.as_primitive::<Float64Type>().value(row))),
        DataType::Decimal128(..) => {
            if let Some(a) = array.as_any().downcast_ref::<Decimal128Array>() {
                out.push_str(&a.value_as_string(row));
            }
        }
        DataType::Decimal256(..) => {
            if let Some(a) = array.as_any().downcast_ref::<Decimal256Array>() {
                out.push_str(&a.value_as_string(row));
            }
        }
        DataType::Utf8 => {
            if let Some(a) = array.as_any().downcast_ref::<StringArray>() {
                string(out, a.value(row));
            }
        }
        DataType::LargeUtf8 => {
            if let Some(a) = array.as_any().downcast_ref::<LargeStringArray>() {
                string(out, a.value(row));
            }
        }
        DataType::Utf8View => {
            if let Some(a) = array.as_any().downcast_ref::<StringViewArray>() {
                string(out, a.value(row));
            }
        }
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => {
            let bytes = binary(array, row).unwrap_or_default();
            string(out, &blob(bytes));
        }
        DataType::Date32 => string(out, &date(array.as_primitive::<Date32Type>().value(row))),
        DataType::Date64 => {
            let ms = array.as_primitive::<Date64Type>().value(row);
            string(
                out,
                &date(i32::try_from(ms.div_euclid(86_400_000)).unwrap_or(i32::MAX)),
            );
        }
        DataType::Time32(unit) | DataType::Time64(unit) => {
            let micros = match unit {
                TimeUnit::Second => {
                    i64::from(array.as_primitive::<Time32SecondType>().value(row)) * 1_000_000
                }
                TimeUnit::Millisecond => {
                    i64::from(array.as_primitive::<Time32MillisecondType>().value(row)) * 1_000
                }
                TimeUnit::Microsecond => array.as_primitive::<Time64MicrosecondType>().value(row),
                TimeUnit::Nanosecond => {
                    array.as_primitive::<Time64NanosecondType>().value(row) / 1_000
                }
            };
            string(out, &time(micros, 0));
        }
        DataType::Timestamp(unit, tz) => {
            let (value, nanos) = match unit {
                TimeUnit::Second => (
                    array.as_primitive::<TimestampSecondType>().value(row),
                    false,
                ),
                TimeUnit::Millisecond => (
                    array.as_primitive::<TimestampMillisecondType>().value(row),
                    false,
                ),
                TimeUnit::Microsecond => (
                    array.as_primitive::<TimestampMicrosecondType>().value(row),
                    false,
                ),
                TimeUnit::Nanosecond => (
                    array.as_primitive::<TimestampNanosecondType>().value(row),
                    true,
                ),
            };
            let mut s = if nanos {
                timestamp_nanos(value)
            } else {
                timestamp(to_micros(value, *unit))
            };
            if tz.is_some() && !s.ends_with("infinity") {
                s.push_str("+00");
            }
            string(out, &s);
        }
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            let v = array.as_primitive::<IntervalMonthDayNanoType>().value(row);
            string(out, &interval(v.months, v.days, v.nanoseconds / 1_000));
        }
        DataType::List(_) => {
            let a = array.as_list::<i32>();
            push_list(out, a.value(row).as_ref());
        }
        DataType::LargeList(_) => {
            let a = array.as_list::<i64>();
            push_list(out, a.value(row).as_ref());
        }
        DataType::FixedSizeList(..) => {
            let a = array.as_fixed_size_list();
            push_list(out, a.value(row).as_ref());
        }
        DataType::Struct(fields) => {
            let a = array.as_struct();
            out.push('{');
            for (i, field) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                // Keys are always quoted.
                out.push('\'');
                out.push_str(&escape(field.name()));
                out.push_str("': ");
                write_value(out, a.column(i).as_ref(), row, true);
            }
            out.push('}');
        }
        DataType::Map(..) => {
            let a = array.as_map();
            let entries = a.value(row);
            let (keys, values) = (entries.column(0), entries.column(1));
            out.push('{');
            for i in 0..entries.len() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_value(out, keys.as_ref(), i, true);
                out.push('=');
                write_value(out, values.as_ref(), i, true);
            }
            out.push('}');
        }
        DataType::Union(..) => {
            let a = array.as_union();
            let child = a.child(a.type_id(row));
            write_value(out, child.as_ref(), a.value_offset(row), nested);
        }
        DataType::Dictionary(..) => {
            let a = array.as_any_dictionary();
            let key = a.normalized_keys()[row];
            write_value(out, a.values().as_ref(), key, nested);
        }
        // Anything else: Arrow's own text, so nothing is lost.
        _ => {
            let options = FormatOptions::default().with_null("NULL");
            let text = ArrayFormatter::try_new(array, &options)
                .map(|f| f.value(row).to_string())
                .unwrap_or_default();
            string(out, &text);
        }
    }
}

fn push(out: &mut String, value: impl std::fmt::Display) {
    let _ = write!(out, "{value}");
}

fn push_list(out: &mut String, values: &dyn Array) {
    out.push('[');
    for i in 0..values.len() {
        if i > 0 {
            out.push_str(", ");
        }
        write_value(out, values, i, true);
    }
    out.push(']');
}

fn binary(array: &dyn Array, row: usize) -> Option<&[u8]> {
    let any = array.as_any();
    any.downcast_ref::<BinaryArray>()
        .map(|a| a.value(row))
        .or_else(|| any.downcast_ref::<LargeBinaryArray>().map(|a| a.value(row)))
        .or_else(|| any.downcast_ref::<BinaryViewArray>().map(|a| a.value(row)))
        .or_else(|| {
            any.downcast_ref::<FixedSizeBinaryArray>()
                .map(|a| a.value(row))
        })
}

/// A string inside a list, struct or map: quoted (with `'` and `\` escaped) when it is empty,
/// is `NULL`, has spaces at either end or contains a character with a meaning there.
fn push_nested_string(out: &mut String, s: &str) {
    let needs_quotes = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.starts_with(char::is_whitespace)
        || s.ends_with(char::is_whitespace)
        || s.contains([',', '\'', '"', '\\', '[', ']', '{', '}', '(', ')', ':', '=']);
    if needs_quotes {
        out.push('\'');
        out.push_str(&escape(s));
        out.push('\'');
    } else {
        out.push_str(s);
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// A BLOB as DuckDB writes it: printable ASCII as is, every other byte as `\xHH`.
pub fn blob(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if (0x20..0x7f).contains(&b) && b != b'\\' && b != b'\'' && b != b'"' {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "\\x{b:02X}");
        }
    }
    out
}

/// A DOUBLE as DuckDB writes it (like Python's `repr`): the shortest digits that read back
/// the same number, in scientific notation below 1e-4 and from 1e16 on.
pub fn float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    // Rust's `{:e}` gives the shortest round-trip digits: `1.2345e3`.
    let sci = format!("{x:e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    if !(-4..16).contains(&exponent) {
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let plain = format!("{x}");
    if plain.contains('.') {
        plain
    } else {
        format!("{plain}.0")
    }
}

pub fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        // DuckDB's -infinity is -i32::MAX.
        i32::MIN | -2_147_483_647 => "-infinity".into(),
        _ => {
            let (y, m, d) = civil_from_days(days.into());
            if y <= 0 {
                format!("{:04}-{m:02}-{d:02} (BC)", 1 - y)
            } else {
                format!("{y:04}-{m:02}-{d:02}")
            }
        }
    }
}

/// `HH:MM:SS` plus the fraction without trailing zeros; `extra_nanos` adds digits 7 to 9.
fn time(micros: i64, extra_nanos: i64) -> String {
    let secs = micros / 1_000_000;
    let mut s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    let fraction = (micros % 1_000_000) * 1_000 + extra_nanos;
    if fraction != 0 {
        let digits = format!("{fraction:09}");
        s.push('.');
        s.push_str(digits.trim_end_matches('0'));
    }
    s
}

fn to_micros(value: i64, unit: TimeUnit) -> i64 {
    if value == i64::MAX || value <= -i64::MAX {
        return value;
    }
    match unit {
        TimeUnit::Second => value.checked_mul(1_000_000).unwrap_or(i64::MAX),
        TimeUnit::Millisecond => value.checked_mul(1_000).unwrap_or(i64::MAX),
        TimeUnit::Microsecond => value,
        TimeUnit::Nanosecond => value.div_euclid(1_000),
    }
}

pub fn timestamp(micros: i64) -> String {
    match micros {
        i64::MAX => "infinity".into(),
        i64::MIN | -9_223_372_036_854_775_807 => "-infinity".into(),
        _ => timestamp_parts(micros, 0),
    }
}

fn timestamp_nanos(nanos: i64) -> String {
    match nanos {
        i64::MAX => "infinity".into(),
        i64::MIN | -9_223_372_036_854_775_807 => "-infinity".into(),
        _ => timestamp_parts(nanos.div_euclid(1_000), nanos.rem_euclid(1_000)),
    }
}

fn timestamp_parts(micros: i64, extra_nanos: i64) -> String {
    let days = micros.div_euclid(MICROS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let clock = time(micros.rem_euclid(MICROS_PER_DAY), extra_nanos);
    if y <= 0 {
        format!("{:04}-{m:02}-{d:02} {clock} (BC)", 1 - y)
    } else {
        format!("{y:04}-{m:02}-{d:02} {clock}")
    }
}

/// `1 year 2 months 3 days 04:05:06.5`: each part only if it is not zero, and the clock
/// alone (`00:00:00`) when everything is.
pub fn interval(months: i32, days: i32, micros: i64) -> String {
    let mut parts = Vec::new();
    let unit =
        |n: i64, word: &str| format!("{n} {word}{}", if n == 1 || n == -1 { "" } else { "s" });
    let (years, months) = (months / 12, months % 12);
    if years != 0 {
        parts.push(unit(years.into(), "year"));
    }
    if months != 0 {
        parts.push(unit(months.into(), "month"));
    }
    if days != 0 {
        parts.push(unit(days.into(), "day"));
    }
    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 { "-" } else { "" };
        let abs = micros.unsigned_abs();
        let secs = abs / 1_000_000;
        let mut clock = format!(
            "{sign}{:02}:{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        );
        let fraction = abs % 1_000_000;
        if fraction != 0 {
            clock.push('.');
            clock.push_str(format!("{fraction:06}").trim_end_matches('0'));
        }
        parts.push(clock);
    }
    parts.join(" ")
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (Howard Hinnant).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
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
    fn doubles_read_like_duckdb() {
        assert_eq!(float(1e20), "1e+20");
        assert_eq!(float(0.1), "0.1");
        assert_eq!(float(1e-5), "1e-05");
        assert_eq!(float(1e15), "1000000000000000.0");
        assert_eq!(float(1e16), "1e+16");
        assert_eq!(float(123_456.789), "123456.789");
        assert_eq!(float(2.0), "2.0");
    }

    #[test]
    fn intervals_read_like_duckdb() {
        assert_eq!(interval(12, 3, 4 * 3_600_000_000), "1 year 3 days 04:00:00");
        assert_eq!(interval(0, -1, -1_500_000), "-1 day -00:00:01.5");
        assert_eq!(interval(0, 0, 0), "00:00:00");
        assert_eq!(interval(0, 0, 25 * 3_600_000_000), "25:00:00");
        assert_eq!(interval(1, 0, 1), "1 month 00:00:00.000001");
        assert_eq!(interval(29, 0, 0), "2 years 5 months");
    }

    #[test]
    fn nested_strings_are_quoted_when_needed() {
        let quoted = |s: &str| {
            let mut out = String::new();
            push_nested_string(&mut out, s);
            out
        };
        assert_eq!(quoted("a b"), "a b");
        assert_eq!(quoted(""), "''");
        assert_eq!(quoted("it's"), "'it\\'s'");
        assert_eq!(quoted("NULL"), "'NULL'");
        assert_eq!(quoted(" lead"), "' lead'");
        assert_eq!(quoted("a\nb"), "a\nb");
        assert_eq!(quoted("x:y"), "'x:y'");
    }
}
