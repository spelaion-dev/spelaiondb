//! Column types detected from a sample of the rows, the SQL type each engine gets for them, and
//! the few conversions applied to values on the way in (ADR-038). Everything else is sent as the
//! text it is and converted by the server, so its own rules (and errors) apply.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, TimeZone};

/// What a column's sampled values look like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Integer,
    Decimal,
    /// Numbers with an exponent (`1.5e3`).
    Float,
    Boolean,
    Date,
    Timestamp,
    /// Timestamps that all carry a time zone (`Z`, `+02:00`).
    TimestampTz,
    /// Anything else, a column whose values are of mixed types, or one with no values.
    Text,
}

/// How dates and timestamps are written, from the parse options.
#[derive(Debug, Clone, Default)]
pub(crate) struct Formats {
    pub date: Option<String>,
    pub timestamp: Option<String>,
}

/// One value's kind.
pub(crate) fn value_kind(value: &str, formats: &Formats) -> ColumnKind {
    if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
        return ColumnKind::Boolean;
    }
    if let Some(kind) = number_kind(value) {
        return kind;
    }
    if is_date(value, formats) {
        return ColumnKind::Date;
    }
    match parse_timestamp(value, formats.timestamp.as_deref()) {
        Some(Stamp::Naive(_)) => ColumnKind::Timestamp,
        Some(Stamp::Zoned(_)) => ColumnKind::TimestampTz,
        None => ColumnKind::Text,
    }
}

/// The kind two sets of values share: integers widen to decimals, numbers to floats and dates
/// to timestamps; anything else mixed is text.
pub(crate) fn merge(a: ColumnKind, b: ColumnKind) -> ColumnKind {
    use ColumnKind::{Date, Decimal, Float, Integer, Text, Timestamp};
    match (a, b) {
        _ if a == b => a,
        (Integer, Decimal) | (Decimal, Integer) => Decimal,
        (Integer | Decimal, Float) | (Float, Integer | Decimal) => Float,
        (Date, Timestamp) | (Timestamp, Date) => Timestamp,
        _ => Text,
    }
}

/// `42`, `-7`, `3.25`, `.5`, `1e9`: numbers without thousands separators. A whole number with a
/// leading zero (`007`, a code) is not a number, nor is one too big for 64 bits a whole number.
fn number_kind(value: &str) -> Option<ColumnKind> {
    let unsigned = value.strip_prefix(['-', '+']).unwrap_or(value);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(i) => (&unsigned[..i], Some(&unsigned[i + 1..])),
        None => (unsigned, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (mantissa, None),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !digits(int)
        || !frac.is_none_or(digits)
        || (int.is_empty() && frac.is_none_or(str::is_empty))
    {
        return None;
    }
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        return (!exponent.is_empty() && digits(exponent)).then_some(ColumnKind::Float);
    }
    match frac {
        Some(_) => Some(ColumnKind::Decimal),
        None if int.len() > 1 && int.starts_with('0') => None,
        None if value.parse::<i64>().is_ok() => Some(ColumnKind::Integer),
        None => Some(ColumnKind::Decimal),
    }
}

/// Digits before and after the point of a decimal value.
pub(crate) fn decimal_digits(value: &str) -> (usize, usize) {
    let unsigned = value.strip_prefix(['-', '+']).unwrap_or(value);
    let (int, frac) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    (int.trim_start_matches('0').len().max(1), frac.len())
}

fn is_date(value: &str, formats: &Formats) -> bool {
    match &formats.date {
        Some(format) => NaiveDate::parse_from_str(value, format).is_ok(),
        None => {
            value.len() == 10
                && value.as_bytes()[4] == b'-'
                && NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
        }
    }
}

enum Stamp {
    Naive(NaiveDateTime),
    Zoned(DateTime<FixedOffset>),
}

const ISO_NAIVE: [&str; 4] = [
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%d %H:%M",
];

fn parse_timestamp(value: &str, format: Option<&str>) -> Option<Stamp> {
    if let Some(format) = format {
        if let Ok(zoned) = DateTime::parse_from_str(value, format) {
            return Some(Stamp::Zoned(zoned));
        }
        return NaiveDateTime::parse_from_str(value, format)
            .ok()
            .map(Stamp::Naive);
    }
    if value.len() < 16 || value.as_bytes()[4] != b'-' {
        return None;
    }
    let (naive, offset) = split_zone(value);
    let naive = ISO_NAIVE
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(naive, f).ok())?;
    match offset {
        None => Some(Stamp::Naive(naive)),
        Some(offset) => offset
            .from_local_datetime(&naive)
            .single()
            .map(Stamp::Zoned),
    }
}

/// `2024-01-02 10:00:00+02:00` → the local part and its offset (`Z` is UTC).
fn split_zone(value: &str) -> (&str, Option<FixedOffset>) {
    if let Some(naive) = value.strip_suffix(['Z', 'z']) {
        return (naive.trim_end(), FixedOffset::east_opt(0));
    }
    // The zone starts at the last sign after the date.
    match value[10..].rfind(['+', '-']) {
        Some(i) => {
            let at = 10 + i;
            match parse_offset(&value[at..]) {
                Some(offset) => (value[..at].trim_end(), Some(offset)),
                None => (value, None),
            }
        }
        None => (value, None),
    }
}

/// `+02:00`, `-0530`, `+02`, `UTC`, `Z`.
pub(crate) fn parse_offset(text: &str) -> Option<FixedOffset> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("utc") || text.eq_ignore_ascii_case("z") {
        return FixedOffset::east_opt(0);
    }
    let (sign, rest) = match text.as_bytes().first()? {
        b'+' => (1, &text[1..]),
        b'-' => (-1, &text[1..]),
        _ => return None,
    };
    let rest = rest.replace(':', "");
    if !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (hours, minutes) = match rest.len() {
        2 => (rest.parse::<i32>().ok()?, 0),
        4 => (
            rest[..2].parse::<i32>().ok()?,
            rest[2..].parse::<i32>().ok()?,
        ),
        _ => return None,
    };
    if hours > 15 || minutes > 59 {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

/// Detects a column's kind from its sampled values.
#[derive(Debug, Clone, Default)]
pub(crate) struct KindDetector {
    kind: Option<ColumnKind>,
    int_digits: usize,
    scale: usize,
    max_chars: usize,
    pub values: usize,
    pub nulls: usize,
}

impl KindDetector {
    pub fn observe(&mut self, value: Option<&str>, formats: &Formats) {
        let Some(value) = value else {
            self.nulls += 1;
            return;
        };
        self.values += 1;
        self.max_chars = self.max_chars.max(value.chars().count());
        let kind = value_kind(value, formats);
        if matches!(kind, ColumnKind::Integer | ColumnKind::Decimal) {
            let (int, scale) = decimal_digits(value);
            self.int_digits = self.int_digits.max(int);
            self.scale = self.scale.max(scale);
        }
        self.kind = Some(self.kind.map_or(kind, |seen| merge(seen, kind)));
    }

    pub fn kind(&self) -> ColumnKind {
        self.kind.unwrap_or(ColumnKind::Text)
    }

    /// The SQL type a new column of this kind gets on `engine` (a dialect name).
    pub fn sql_type(&self, engine: &str) -> String {
        sql_type(
            engine,
            self.kind(),
            self.int_digits,
            self.scale,
            self.max_chars,
        )
    }
}

/// The SQL type for a column of `kind`; decimals get room for the digits seen (plus two more
/// before the point) where the engine needs a precision.
pub(crate) fn sql_type(
    engine: &str,
    kind: ColumnKind,
    int_digits: usize,
    scale: usize,
    max_chars: usize,
) -> String {
    let precision = int_digits + 2 + scale;
    let decimal = |max_precision: usize, max_scale: usize, fallback: &str| {
        if precision <= max_precision && scale <= max_scale {
            format!("DECIMAL({precision},{scale})")
        } else {
            fallback.to_string()
        }
    };
    match (engine, kind) {
        (_, ColumnKind::Integer) if engine == "sqlite" => "INTEGER".into(),
        (_, ColumnKind::Integer) => "BIGINT".into(),
        ("postgres", ColumnKind::Decimal) => "NUMERIC".into(),
        ("sqlite", ColumnKind::Decimal) => "NUMERIC".into(),
        ("mysql", ColumnKind::Decimal) => decimal(65, 30, "DOUBLE"),
        (_, ColumnKind::Decimal) => decimal(38, 38, "DOUBLE"),
        ("postgres", ColumnKind::Float) => "DOUBLE PRECISION".into(),
        ("sqlserver", ColumnKind::Float) => "FLOAT".into(),
        ("sqlite", ColumnKind::Float) => "REAL".into(),
        (_, ColumnKind::Float) => "DOUBLE".into(),
        ("sqlserver", ColumnKind::Boolean) => "BIT".into(),
        (_, ColumnKind::Boolean) => "BOOLEAN".into(),
        (_, ColumnKind::Date) => "DATE".into(),
        ("mysql", ColumnKind::Timestamp | ColumnKind::TimestampTz) => "DATETIME(6)".into(),
        ("sqlserver", ColumnKind::Timestamp) => "DATETIME2".into(),
        ("sqlserver", ColumnKind::TimestampTz) => "DATETIMEOFFSET".into(),
        ("sqlite", ColumnKind::Timestamp | ColumnKind::TimestampTz) => "TEXT".into(),
        (_, ColumnKind::Timestamp) => "TIMESTAMP".into(),
        (_, ColumnKind::TimestampTz) => "TIMESTAMP WITH TIME ZONE".into(),
        ("mysql", ColumnKind::Text) if max_chars > 16_000 => "LONGTEXT".into(),
        ("mysql", ColumnKind::Text) => "TEXT".into(),
        ("sqlserver", ColumnKind::Text) => "NVARCHAR(MAX)".into(),
        ("duckdb", ColumnKind::Text) => "VARCHAR".into(),
        (_, ColumnKind::Text) => "TEXT".into(),
    }
}

/// What a target column's type means for the values sent to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    Boolean,
    Date,
    Timestamp,
    TimestampTz,
    Other,
}

/// The target kind of a column from its type as the engine writes it (`boolean`, `bit`,
/// `tinyint(1)`, `timestamp with time zone`, `datetimeoffset(7)`…).
pub(crate) fn target_kind(native_type: &str) -> TargetKind {
    let t = native_type.trim().to_ascii_lowercase();
    let base = t.split('(').next().unwrap_or("").trim();
    if matches!(base, "bool" | "boolean" | "bit") || t == "tinyint(1)" {
        return TargetKind::Boolean;
    }
    if base == "date" {
        return TargetKind::Date;
    }
    if base.starts_with("datetimeoffset")
        || base == "timestamptz"
        || (t.starts_with("timestamp") && t.contains("with time zone"))
    {
        return TargetKind::TimestampTz;
    }
    if base.starts_with("timestamp") || base.starts_with("datetime") || base == "smalldatetime" {
        return TargetKind::Timestamp;
    }
    TargetKind::Other
}

/// Converts values on their way to their columns: booleans to `1`/`0` (what every engine's
/// boolean, `bit` and `tinyint(1)` take), dates and timestamps written in the chosen formats to
/// ISO, and timestamps without a zone given the chosen time zone where the column keeps one.
/// MySQL and MariaDB have no type that keeps a zone: their timestamps with one are stored in UTC.
#[derive(Debug, Clone)]
pub(crate) struct Normalizer {
    pub kinds: Vec<TargetKind>,
    pub formats: Formats,
    pub zone: Option<FixedOffset>,
    pub mysql: bool,
}

impl Normalizer {
    /// Whether any value of a column of this kind can change.
    fn touches(&self, kind: TargetKind) -> bool {
        match kind {
            TargetKind::Boolean => true,
            TargetKind::Date => self.formats.date.is_some(),
            TargetKind::Timestamp => self.formats.timestamp.is_some() || self.mysql,
            TargetKind::TimestampTz => {
                self.formats.timestamp.is_some() || self.zone.is_some() || self.mysql
            }
            TargetKind::Other => false,
        }
    }

    pub fn value(&self, column: usize, value: String) -> Result<String, String> {
        let kind = self.kinds.get(column).copied().unwrap_or(TargetKind::Other);
        if !self.touches(kind) {
            return Ok(value);
        }
        match kind {
            TargetKind::Other => Ok(value),
            TargetKind::Boolean => Ok(if value.eq_ignore_ascii_case("true") {
                "1".into()
            } else if value.eq_ignore_ascii_case("false") {
                "0".into()
            } else {
                value
            }),
            TargetKind::Date => {
                let format = self.formats.date.as_deref().unwrap_or("%Y-%m-%d");
                NaiveDate::parse_from_str(&value, format)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .map_err(|_| format!("\"{value}\" is not a date written as {format}"))
            }
            TargetKind::Timestamp | TargetKind::TimestampTz => {
                let stamp = match parse_timestamp(&value, self.formats.timestamp.as_deref()) {
                    Some(stamp) => stamp,
                    None if self.formats.timestamp.is_some() => {
                        return Err(format!(
                            "\"{value}\" is not a timestamp written as {}",
                            self.formats.timestamp.as_deref().unwrap_or_default()
                        ));
                    }
                    // Not ISO and no format chosen: the server reads it as it can.
                    None => return Ok(value),
                };
                const NAIVE: &str = "%Y-%m-%d %H:%M:%S%.f";
                const ZONED: &str = "%Y-%m-%d %H:%M:%S%.f%:z";
                Ok(match (stamp, kind) {
                    (Stamp::Zoned(zoned), _) if self.mysql => {
                        zoned.naive_utc().format(NAIVE).to_string()
                    }
                    (Stamp::Zoned(zoned), TargetKind::TimestampTz) => {
                        zoned.format(ZONED).to_string()
                    }
                    (Stamp::Zoned(zoned), _) => zoned.naive_local().format(NAIVE).to_string(),
                    (Stamp::Naive(naive), TargetKind::TimestampTz) => match self.zone {
                        Some(zone) if !self.mysql => {
                            match zone.from_local_datetime(&naive).single() {
                                Some(zoned) => zoned.format(ZONED).to_string(),
                                None => naive.format(NAIVE).to_string(),
                            }
                        }
                        _ => naive.format(NAIVE).to_string(),
                    },
                    (Stamp::Naive(naive), _) => naive.format(NAIVE).to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(values: &[Option<&str>]) -> ColumnKind {
        detect_with(values, &Formats::default())
    }

    fn detect_with(values: &[Option<&str>], formats: &Formats) -> ColumnKind {
        let mut detector = KindDetector::default();
        for v in values {
            detector.observe(*v, formats);
        }
        detector.kind()
    }

    #[test]
    fn single_kinds_are_detected() {
        use ColumnKind::*;
        assert_eq!(detect(&[Some("1"), Some("-20"), None, Some("+3")]), Integer);
        assert_eq!(detect(&[Some("1.5"), Some(".25"), Some("-0.5")]), Decimal);
        assert_eq!(detect(&[Some("1e3"), Some("2.5E-4")]), Float);
        assert_eq!(
            detect(&[Some("true"), Some("FALSE"), Some("True")]),
            Boolean
        );
        assert_eq!(detect(&[Some("2024-02-29"), Some("1999-12-31")]), Date);
        assert_eq!(
            detect(&[
                Some("2024-02-29 10:00:00"),
                Some("2024-03-01T23:59:59.123456")
            ]),
            Timestamp
        );
        assert_eq!(
            detect(&[
                Some("2024-02-29T10:00:00Z"),
                Some("2024-03-01 23:59:59+02:00")
            ]),
            TimestampTz
        );
        assert_eq!(detect(&[Some("hello")]), Text);
        // No values at all: text.
        assert_eq!(detect(&[None, None]), Text);
    }

    #[test]
    fn mixed_types_become_text_and_numbers_widen() {
        use ColumnKind::*;
        assert_eq!(detect(&[Some("1"), Some("2.5")]), Decimal);
        assert_eq!(detect(&[Some("1"), Some("2.5"), Some("1e10")]), Float);
        assert_eq!(
            detect(&[Some("2024-01-01"), Some("2024-01-01 10:00")]),
            Timestamp
        );
        assert_eq!(detect(&[Some("1"), Some("abc")]), Text);
        assert_eq!(detect(&[Some("1"), Some("true")]), Text);
        assert_eq!(detect(&[Some("2024-01-01"), Some("12")]), Text);
        assert_eq!(
            detect(&[Some("2024-01-01 10:00"), Some("2024-01-01 10:00Z")]),
            Text
        );
    }

    #[test]
    fn things_that_look_like_numbers_but_are_not() {
        use ColumnKind::*;
        // Codes with leading zeros, thousands separators, a lone sign or point, hex, too big.
        assert_eq!(detect(&[Some("007"), Some("012")]), Text);
        assert_eq!(detect(&[Some("1,000")]), Text);
        assert_eq!(detect(&[Some("-")]), Text);
        assert_eq!(detect(&[Some(".")]), Text);
        assert_eq!(detect(&[Some("0x1F")]), Text);
        assert_eq!(detect(&[Some("1e")]), Text);
        assert_eq!(detect(&[Some("0"), Some("10")]), Integer);
        assert_eq!(detect(&[Some("99999999999999999999")]), Decimal);
        assert_eq!(detect(&[Some("2024-13-01")]), Text);
        assert_eq!(detect(&[Some("2023-02-29")]), Text);
    }

    #[test]
    fn custom_date_and_timestamp_formats() {
        let formats = Formats {
            date: Some("%d/%m/%Y".into()),
            timestamp: Some("%d/%m/%Y %H:%M".into()),
        };
        assert_eq!(
            detect_with(&[Some("31/12/2024")], &formats),
            ColumnKind::Date
        );
        assert_eq!(
            detect_with(&[Some("31/12/2024 10:30")], &formats),
            ColumnKind::Timestamp
        );
        // ISO is not the chosen format any more.
        assert_eq!(
            detect_with(&[Some("2024-12-31")], &formats),
            ColumnKind::Text
        );
    }

    #[test]
    fn each_engine_gets_its_types() {
        let mut d = KindDetector::default();
        d.observe(Some("12345.678"), &Formats::default());
        d.observe(Some("1.5"), &Formats::default());
        assert_eq!(d.sql_type("postgres"), "NUMERIC");
        assert_eq!(d.sql_type("mysql"), "DECIMAL(10,3)");
        assert_eq!(d.sql_type("sqlserver"), "DECIMAL(10,3)");
        assert_eq!(d.sql_type("duckdb"), "DECIMAL(10,3)");
        assert_eq!(d.sql_type("sqlite"), "NUMERIC");
        assert_eq!(
            sql_type("sqlserver", ColumnKind::Text, 0, 0, 10),
            "NVARCHAR(MAX)"
        );
        assert_eq!(sql_type("sqlserver", ColumnKind::Boolean, 0, 0, 0), "BIT");
        assert_eq!(
            sql_type("mysql", ColumnKind::TimestampTz, 0, 0, 0),
            "DATETIME(6)"
        );
        assert_eq!(sql_type("duckdb", ColumnKind::Decimal, 40, 2, 0), "DOUBLE");
    }

    #[test]
    fn target_kinds_from_native_types() {
        assert_eq!(target_kind("boolean"), TargetKind::Boolean);
        assert_eq!(target_kind("BIT"), TargetKind::Boolean);
        assert_eq!(target_kind("tinyint(1)"), TargetKind::Boolean);
        assert_eq!(target_kind("tinyint(4)"), TargetKind::Other);
        assert_eq!(
            target_kind("timestamp with time zone"),
            TargetKind::TimestampTz
        );
        assert_eq!(
            target_kind("timestamp without time zone"),
            TargetKind::Timestamp
        );
        assert_eq!(target_kind("TIMESTAMPTZ"), TargetKind::TimestampTz);
        assert_eq!(target_kind("datetimeoffset(7)"), TargetKind::TimestampTz);
        assert_eq!(target_kind("datetime2(7)"), TargetKind::Timestamp);
        assert_eq!(target_kind("DATE"), TargetKind::Date);
        assert_eq!(target_kind("character varying(80)"), TargetKind::Other);
    }

    #[test]
    fn values_are_normalized_for_their_columns() {
        let normalizer = Normalizer {
            kinds: vec![
                TargetKind::Boolean,
                TargetKind::Date,
                TargetKind::TimestampTz,
                TargetKind::Timestamp,
                TargetKind::Other,
            ],
            formats: Formats {
                date: Some("%d/%m/%Y".into()),
                timestamp: Some("%d/%m/%Y %H:%M".into()),
            },
            zone: parse_offset("+02:00"),
            mysql: false,
        };
        assert_eq!(normalizer.value(0, "TRUE".into()).unwrap(), "1");
        assert_eq!(normalizer.value(0, "false".into()).unwrap(), "0");
        assert_eq!(normalizer.value(0, "1".into()).unwrap(), "1");
        assert_eq!(
            normalizer.value(1, "31/12/2024".into()).unwrap(),
            "2024-12-31"
        );
        assert!(normalizer.value(1, "2024-12-31".into()).is_err());
        assert_eq!(
            normalizer.value(2, "31/12/2024 10:30".into()).unwrap(),
            "2024-12-31 10:30:00+02:00"
        );
        assert_eq!(
            normalizer.value(3, "31/12/2024 10:30".into()).unwrap(),
            "2024-12-31 10:30:00"
        );
        assert_eq!(
            normalizer.value(4, "31/12/2024".into()).unwrap(),
            "31/12/2024"
        );

        // ISO timestamps with a time zone go to UTC on MySQL, keep it elsewhere.
        let iso = Normalizer {
            kinds: vec![TargetKind::Timestamp, TargetKind::TimestampTz],
            formats: Formats::default(),
            zone: parse_offset("UTC"),
            mysql: true,
        };
        assert_eq!(
            iso.value(0, "2024-01-01T10:00:00+02:00".into()).unwrap(),
            "2024-01-01 08:00:00"
        );
        let pg = Normalizer {
            mysql: false,
            ..iso
        };
        assert_eq!(
            pg.value(1, "2024-01-01 10:00:00".into()).unwrap(),
            "2024-01-01 10:00:00+00:00"
        );
        assert_eq!(
            pg.value(1, "2024-01-01T10:00:00.5-05:30".into()).unwrap(),
            "2024-01-01 10:00:00.500-05:30"
        );
        // Not a timestamp and no format: sent as it is.
        assert_eq!(pg.value(1, "yesterday".into()).unwrap(), "yesterday");
    }

    #[test]
    fn offsets() {
        assert_eq!(parse_offset("+02:00"), FixedOffset::east_opt(7200));
        assert_eq!(parse_offset("-0530"), FixedOffset::east_opt(-19800));
        assert_eq!(parse_offset("+02"), FixedOffset::east_opt(7200));
        assert_eq!(parse_offset("utc"), FixedOffset::east_opt(0));
        assert_eq!(parse_offset("Europe/Madrid"), None);
        assert_eq!(parse_offset("+25:00"), None);
    }
}
