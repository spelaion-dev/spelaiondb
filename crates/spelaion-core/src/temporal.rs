//! Text columns that hold dates (SDB-122). MySQL and MariaDB `DATE`, `DATETIME` and `TIMESTAMP`
//! columns arrive as text with their native type (ADR-006's 2026-11-12 note): their legacy zero
//! dates (`0000-00-00`) are no date, so a typed column would fail on them. Their text sorts
//! and compares as the date does, so sorting, filters and literals need nothing more; what
//! places values on a time line (a chart's X axis) or writes "now" in the column's own form
//! reads them as dates through here.

use spelaion_driver_api::{ColumnMeta, LogicalType, Value};

/// The date type a text column's values are, from its native type: `Date` for `date`,
/// `Timestamp` for `datetime` and `timestamp` (with or without a precision). `None` for any
/// other column, typed ones included.
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
        "datetime" | "timestamp" => Some(LogicalType::Timestamp),
        _ => None,
    }
}

/// The column's logical type, with a text column that holds dates read as its date type.
pub(crate) fn effective_logical(column: &ColumnMeta) -> LogicalType {
    text_temporal(column).unwrap_or(column.logical)
}

/// A text value of a date column as that date: `YYYY-MM-DD` as a [`Value::Date`] and
/// `YYYY-MM-DD hh:mm:ss[.ffffff]` as a [`Value::Timestamp`]. `None` for a zero or impossible
/// date (`0000-00-00`, `2024-00-10`, `2024-02-30`), which is no point in time.
pub(crate) fn parse(text: &str, logical: LogicalType) -> Option<Value> {
    match logical {
        LogicalType::Date => parse_date(text).map(Value::Date),
        LogicalType::Timestamp => parse_timestamp(text).map(Value::Timestamp),
        _ => None,
    }
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

fn parse_timestamp(text: &str) -> Option<i64> {
    let (date, time) = text.split_once(' ')?;
    let days = i64::from(parse_date(date)?);
    let (hms, fraction) = time.split_once('.').unwrap_or((time, ""));
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
        format!("{fraction:0<6}").parse::<i64>().ok()?
    };
    Some(((days * 24 + h) * 60 + mi) * 60_000_000 + s * 1_000_000 + micros)
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
}
