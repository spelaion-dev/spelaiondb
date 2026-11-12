//! Text columns that hold dates (ADR-005's and ADR-006's 2026-11-12 notes). MySQL and MariaDB
//! `DATE`, `DATETIME` and `TIMESTAMP` columns arrive as text with their native type (SDB-122):
//! their legacy zero dates (`0000-00-00`) are no date, so a typed column would fail on them.
//! SQL Server's `datetime2` and `datetimeoffset` do too (SDB-265): they keep 100 ns, past a
//! timestamp's microseconds, and a `datetimeoffset` keeps the offset it was written with.
//! Their text sorts and compares as the date does (a `datetimeoffset` sorts by its instant,
//! [`instant_key`]), so filters and literals need nothing more; what places values on a time
//! line (a chart's X axis) or writes "now" in the column's own form reads them as dates
//! through here.

use spelaion_driver_api::{ColumnMeta, LogicalType, Value};

/// The date type a text column's values are, from its native type: `Date` for `date`,
/// `Timestamp` for `datetime`, `timestamp` and `datetime2` (with or without a precision),
/// `TimestampTz` for `datetimeoffset`. `None` for any other column, typed ones included.
pub(crate) fn text_temporal(column: &ColumnMeta) -> Option<LogicalType> {
    if column.logical != LogicalType::Text {
        return None;
    }
    let native = column.native_type.trim().to_ascii_lowercase();
    let base = native
        .split(|c: char| c == '(' || c.is_whitespace())
        .next()
        .unwrap_or_default();
    match base {
        "date" => Some(LogicalType::Date),
        "datetime" | "timestamp" | "datetime2" => Some(LogicalType::Timestamp),
        "datetimeoffset" => Some(LogicalType::TimestampTz),
        _ => None,
    }
}

/// The column's logical type, with a text column that holds dates read as its date type.
pub(crate) fn effective_logical(column: &ColumnMeta) -> LogicalType {
    text_temporal(column).unwrap_or(column.logical)
}

/// A text value of a date column as that date: `YYYY-MM-DD` as a [`Value::Date`],
/// `YYYY-MM-DD hh:mm:ss[.fffffff]` as a [`Value::Timestamp`] and the same followed by
/// ` ±hh:mm` as a [`Value::TimestampTz`] in UTC; digits past the microsecond are dropped.
/// `None` for a zero or impossible date (`0000-00-00`, `2024-00-10`, `2024-02-30`), which is
/// no point in time.
pub(crate) fn parse(text: &str, logical: LogicalType) -> Option<Value> {
    match logical {
        LogicalType::Date => parse_date(text).map(Value::Date),
        LogicalType::Timestamp => parse_timestamp(text).map(|n| Value::Timestamp(micros(n))),
        LogicalType::TimestampTz => instant_key(text).map(|n| Value::TimestampTz(micros(n))),
        _ => None,
    }
}

/// A `datetimeoffset`'s text (`2024-01-01 10:00:00.1234567 +02:00`) as the instant it is, in
/// nanoseconds since 1970-01-01 UTC: what it sorts by, as the server orders it. `None` for
/// text that is not one.
pub(crate) fn instant_key(text: &str) -> Option<i128> {
    let (local, offset) = text.rsplit_once(' ')?;
    let sign = match offset.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = offset[1..].split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (i128, i128) = (h.parse().ok()?, m.parse().ok()?);
    if h > 14 || m > 59 {
        return None;
    }
    Some(parse_timestamp(local)? - sign * (h * 60 + m) * 60 * NANOS_PER_SECOND)
}

const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// Nanoseconds to whole microseconds, rounding down as a timestamp keeps them.
fn micros(nanos: i128) -> i64 {
    i64::try_from(nanos.div_euclid(1_000)).unwrap_or(i64::MAX)
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

fn parse_date(text: &str) -> Option<i32> {
    let mut parts = text.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    i32::try_from(days_from_civil(y, m, d)).ok()
}

/// `YYYY-MM-DD hh:mm:ss[.f…]` (up to nine fractional digits) in nanoseconds since 1970.
fn parse_timestamp(text: &str) -> Option<i128> {
    let (date, time) = text.split_once(' ')?;
    let days = i128::from(parse_date(date)?);
    let (hms, fraction) = time.split_once('.').unwrap_or((time, ""));
    let mut parts = hms.splitn(3, ':');
    let h: i128 = parts.next()?.parse().ok()?;
    let mi: i128 = parts.next()?.parse().ok()?;
    let s: i128 = parts.next()?.parse().ok()?;
    if h > 23 || mi > 59 || s > 59 || fraction.len() > 9 {
        return None;
    }
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let nanos = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<9}").parse::<i128>().ok()?
    };
    Some((((days * 24 + h) * 60 + mi) * 60 + s) * NANOS_PER_SECOND + nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(logical: LogicalType, native: &str) -> ColumnMeta {
        ColumnMeta {
            name: "c".into(),
            logical,
            native_type: native.into(),
            native_id: None,
            origin: None,
        }
    }

    #[test]
    fn text_columns_hold_dates_by_their_native_type() {
        assert_eq!(
            text_temporal(&meta(LogicalType::Text, "date")),
            Some(LogicalType::Date)
        );
        assert_eq!(
            text_temporal(&meta(LogicalType::Text, "DATETIME(6)")),
            Some(LogicalType::Timestamp)
        );
        assert_eq!(
            text_temporal(&meta(LogicalType::Text, "timestamp")),
            Some(LogicalType::Timestamp)
        );
        assert_eq!(
            text_temporal(&meta(LogicalType::Text, "datetime2")),
            Some(LogicalType::Timestamp)
        );
        assert_eq!(
            text_temporal(&meta(LogicalType::Text, "datetimeoffset")),
            Some(LogicalType::TimestampTz)
        );
        assert_eq!(text_temporal(&meta(LogicalType::Text, "varchar")), None);
        // Typed columns are what they are.
        assert_eq!(text_temporal(&meta(LogicalType::Date, "date")), None);
        assert_eq!(
            effective_logical(&meta(LogicalType::Date, "date")),
            LogicalType::Date
        );
    }

    #[test]
    fn dates_read_back_to_the_same_text_and_zero_dates_are_none() {
        for text in ["1970-01-01", "2024-02-29", "0001-01-01", "9999-12-31"] {
            let value = parse(text, LogicalType::Date).unwrap();
            assert_eq!(value.to_string(), text);
        }
        for text in [
            "2026-01-31 10:20:30",
            "1999-12-31 23:59:59.5",
            "2026-01-31 00:00:00.123456",
        ] {
            let value = parse(text, LogicalType::Timestamp).unwrap();
            assert_eq!(value.to_string(), text);
        }
        for text in ["0000-00-00", "2026-00-10", "2026-02-30", "2025-02-29", "x"] {
            assert_eq!(parse(text, LogicalType::Date), None, "{text}");
        }
        assert_eq!(parse("0000-00-00 00:00:00", LogicalType::Timestamp), None);
        assert_eq!(parse("2026-01-31 24:00:00", LogicalType::Timestamp), None);
    }

    #[test]
    fn sql_server_text_reads_to_the_microsecond_and_offsets_to_utc() {
        assert_eq!(
            parse("2026-01-31 10:20:30.1234567", LogicalType::Timestamp)
                .unwrap()
                .to_string(),
            "2026-01-31 10:20:30.123456"
        );
        let Some(Value::Timestamp(utc)) =
            parse("2026-01-31 08:20:30.123456", LogicalType::Timestamp)
        else {
            panic!("a timestamp")
        };
        assert_eq!(
            parse(
                "2026-01-31 10:20:30.1234567 +02:00",
                LogicalType::TimestampTz
            ),
            Some(Value::TimestampTz(utc))
        );
        for bad in [
            "2026-01-31 10:20:30",
            "2026-01-31 10:20:30 02:00",
            "2026-01-31 10:20:30 +2:00",
            "2026-01-31 10:20:30 +15:00",
            "2026-01-31 10:20:30.12x +00:00",
        ] {
            assert_eq!(instant_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn offsets_sort_by_their_instant_and_the_seventh_digit_counts() {
        let key = |t| instant_key(t).unwrap();
        // 10:00 at +02:00 is before 09:00 at +00:00.
        assert!(
            key("2026-01-31 10:00:00.0000000 +02:00") < key("2026-01-31 09:00:00.0000000 +00:00")
        );
        assert!(
            key("2026-01-31 09:00:00.0000000 -05:30") > key("2026-01-31 09:00:00.0000000 +00:00")
        );
        assert!(
            key("2026-01-31 09:00:00.1234567 +00:00") < key("2026-01-31 09:00:00.1234568 +00:00")
        );
        assert_eq!(
            key("2026-02-01 01:30:00.0000000 +02:00"),
            key("2026-01-31 23:30:00.0000000 +00:00")
        );
    }
}
