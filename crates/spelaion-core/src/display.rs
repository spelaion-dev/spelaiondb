//! How the grid shows values (SDB-385): date, time and number formats, a display time zone
//! and how binary values read. Display only: it is applied as a window of rows is served,
//! next to each cell's text as the database prints it, which is what everything else keeps
//! using (copies, exports, filters, value pickers, edits, Ctrl+F). A masked column
//! (ADR-051) is never formatted: its cells are the mask.
//!
//! The patterns are `strftime`'s. A value that cannot be read as what its column holds (a
//! MySQL zero date, PostgreSQL's `infinity`) is shown as the database prints it.

use std::sync::Arc;

use arrow::array::{ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::error::ArrowError;
use arrow::ipc::writer::StreamWriter;
use jiff::civil;
use jiff::fmt::strtime;
use jiff::tz::{Offset, TimeZone};
use spelaion_driver_api::{ColumnMeta, LogicalType, Value};

use crate::results::StoredResult;
use crate::temporal;

/// The longest pattern, in bytes.
pub const MAX_PATTERN: usize = 64;
/// The time zone name that means this computer's.
pub const LOCAL_ZONE: &str = "local";
/// Field metadata of a window's display columns: what kind of value the column holds.
pub const KIND_KEY: &str = "spelaion.kind";
/// Field metadata of a window's display columns: the zone its values are shown in.
pub const ZONE_KEY: &str = "spelaion.zone";

/// The character between groups of three digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Grouping {
    #[default]
    None,
    Comma,
    Dot,
    Space,
    Apostrophe,
}

impl Grouping {
    fn mark(self) -> Option<char> {
        match self {
            Self::None => None,
            Self::Comma => Some(','),
            Self::Dot => Some('.'),
            // A space that does not break: a number stays on one line.
            Self::Space => Some('\u{00A0}'),
            Self::Apostrophe => Some('\''),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Comma => "comma",
            Self::Dot => "dot",
            Self::Space => "space",
            Self::Apostrophe => "apostrophe",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::None,
            Self::Comma,
            Self::Dot,
            Self::Space,
            Self::Apostrophe,
        ]
        .into_iter()
        .find(|g| g.as_str() == text)
    }
}

/// The character before a number's decimals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecimalMark {
    #[default]
    Dot,
    Comma,
}

impl DecimalMark {
    fn mark(self) -> char {
        match self {
            Self::Dot => '.',
            Self::Comma => ',',
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dot => "dot",
            Self::Comma => "comma",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Dot, Self::Comma]
            .into_iter()
            .find(|d| d.as_str() == text)
    }
}

/// How a binary value reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BinaryDisplay {
    /// As the database prints it: `\x…`.
    #[default]
    Hex,
    Utf8,
    Latin1,
    Base64,
}

impl BinaryDisplay {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hex => "hex",
            Self::Utf8 => "utf8",
            Self::Latin1 => "latin1",
            Self::Base64 => "base64",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Hex, Self::Utf8, Self::Latin1, Self::Base64]
            .into_iter()
            .find(|b| b.as_str() == text)
    }
}

/// The user's display settings, as they are kept. An empty pattern or zone means "as the
/// database prints it" for that kind of value; `as_stored` means it for all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayFormat {
    pub as_stored: bool,
    pub date: String,
    pub time: String,
    pub timestamp: String,
    pub grouping: Grouping,
    pub decimal: DecimalMark,
    /// An IANA name (`Europe/Madrid`), [`LOCAL_ZONE`] or empty.
    pub time_zone: String,
    pub binary: BinaryDisplay,
}

impl Default for DisplayFormat {
    fn default() -> Self {
        Self {
            as_stored: true,
            date: String::new(),
            time: String::new(),
            timestamp: String::new(),
            grouping: Grouping::None,
            decimal: DecimalMark::Dot,
            time_zone: String::new(),
            binary: BinaryDisplay::Hex,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DisplayError {
    #[error("the {what} format {pattern:?} cannot be used: {detail}")]
    Pattern {
        what: &'static str,
        pattern: String,
        detail: String,
    },
    #[error("there is no time zone called {0:?}")]
    TimeZone(String),
    #[error("the digit grouping and the decimal mark cannot be the same character")]
    SameMarks,
}

/// What kind of value a column holds, for the grid's colors by type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Number,
    Text,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Bool,
    Binary,
    Other,
}

impl ValueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Number => "number",
            Self::Text => "text",
            Self::Date => "date",
            Self::Time => "time",
            Self::Timestamp => "timestamp",
            Self::TimestampTz => "timestamptz",
            Self::Bool => "bool",
            Self::Binary => "binary",
            Self::Other => "other",
        }
    }

    /// The kind of a column's values: a text column that holds dates (MySQL, SQL Server) is
    /// the date type it holds.
    pub fn of(column: &ColumnMeta) -> Self {
        match temporal::effective_logical(column) {
            LogicalType::Int | LogicalType::Float | LogicalType::Decimal => Self::Number,
            LogicalType::Text => Self::Text,
            LogicalType::Date => Self::Date,
            LogicalType::Time => Self::Time,
            LogicalType::Timestamp => Self::Timestamp,
            LogicalType::TimestampTz => Self::TimestampTz,
            LogicalType::Bool => Self::Bool,
            LogicalType::Bytes => Self::Binary,
            LogicalType::Uuid
            | LogicalType::Json
            | LogicalType::Interval
            | LogicalType::Array
            | LogicalType::Native => Self::Other,
        }
    }
}

/// Display settings checked and ready to format with. The default shows everything as the
/// database prints it.
#[derive(Debug, Clone, Default)]
pub struct Display {
    format: DisplayFormat,
    zone: Option<(TimeZone, String)>,
}

/// The offset written after a value shown in a zone, unless its pattern writes one.
const OFFSET: &str = "%:z";

impl DisplayFormat {
    /// Checks the settings: every pattern must format a value of its kind, the zone must
    /// exist, and a number's two marks must differ.
    pub fn compile(&self) -> Result<Display, DisplayError> {
        let sample = civil::date(2026, 11, 16).at(20, 7, 16, 123_456_000);
        let check = |what: &'static str, pattern: &str, result: Result<String, jiff::Error>| {
            if pattern.len() > MAX_PATTERN || pattern.chars().any(char::is_control) {
                return Err(DisplayError::Pattern {
                    what,
                    pattern: pattern.chars().take(MAX_PATTERN).collect(),
                    detail: format!("a format is one line of at most {MAX_PATTERN} characters"),
                });
            }
            result.map(|_| ()).map_err(|e| DisplayError::Pattern {
                what,
                pattern: pattern.to_string(),
                detail: e.to_string(),
            })
        };
        if !self.date.is_empty() {
            check(
                "date",
                &self.date,
                strtime::format(&self.date, sample.date()),
            )?;
        }
        if !self.time.is_empty() {
            check(
                "time",
                &self.time,
                strtime::format(&self.time, sample.time()),
            )?;
        }
        let zone = match self.time_zone.trim() {
            "" => None,
            LOCAL_ZONE => {
                let zone = TimeZone::system();
                let name = zone.iana_name().unwrap_or(LOCAL_ZONE).to_string();
                Some((zone, name))
            }
            name => {
                let zone = TimeZone::get(name)
                    .map_err(|_| DisplayError::TimeZone(name.chars().take(64).collect()))?;
                let name = zone.iana_name().unwrap_or(name).to_string();
                Some((zone, name))
            }
        };
        if !self.timestamp.is_empty() {
            // A timestamp's pattern is also used for values with a zone, which may name it.
            let zoned = sample
                .to_zoned(TimeZone::UTC)
                .map_err(|e| DisplayError::Pattern {
                    what: "timestamp",
                    pattern: self.timestamp.clone(),
                    detail: e.to_string(),
                })?;
            check(
                "timestamp",
                &self.timestamp,
                strtime::format(&self.timestamp, &zoned),
            )?;
        }
        if self.grouping.mark() == Some(self.decimal.mark()) {
            return Err(DisplayError::SameMarks);
        }
        Ok(Display {
            format: self.clone(),
            zone,
        })
    }
}

impl Display {
    pub fn format(&self) -> &DisplayFormat {
        &self.format
    }

    /// Nothing is shown otherwise than the database prints it.
    pub fn is_as_stored(&self) -> bool {
        let f = &self.format;
        f.as_stored
            || (f.date.is_empty()
                && f.time.is_empty()
                && f.timestamp.is_empty()
                && f.grouping == Grouping::None
                && f.decimal == DecimalMark::Dot
                && self.zone.is_none()
                && f.binary == BinaryDisplay::Hex)
    }

    /// The zone a column's values are shown in: only for values that have a zone, and only
    /// when one is set.
    pub fn zone_of(&self, column: &ColumnMeta) -> Option<&str> {
        if self.format.as_stored || ValueKind::of(column) != ValueKind::TimestampTz {
            return None;
        }
        self.zone.as_ref().map(|(_, name)| name.as_str())
    }

    /// How a cell is shown, when that is not how the database prints it; `None` otherwise.
    /// `value` is the stored value of a column `meta` that is not masked.
    pub fn cell(&self, meta: &ColumnMeta, value: &Value) -> Option<String> {
        if self.format.as_stored {
            return None;
        }
        let shown = match value {
            Value::Null => None,
            Value::Int(_) | Value::Float(_) | Value::Decimal(_) => self.number(&value.to_string()),
            Value::Date(days) => self.date(*days),
            Value::Time(micros) => self.time(*micros),
            Value::Timestamp(micros) => self.timestamp(*micros),
            Value::TimestampTz(micros) => self.instant(*micros, None),
            Value::Bytes(bytes) => self.binary(bytes),
            // A text column that holds dates (MySQL, SQL Server) is read as the date it is.
            Value::Text(text) | Value::Native(text) => match temporal::text_temporal(meta) {
                Some(LogicalType::TimestampTz) => {
                    match temporal::parse(text, LogicalType::TimestampTz) {
                        Some(Value::TimestampTz(micros)) => {
                            self.instant(micros, written_offset(text))
                        }
                        _ => None,
                    }
                }
                Some(logical) => match temporal::parse(text, logical) {
                    Some(Value::Date(days)) => self.date(days),
                    Some(Value::Time(micros)) => self.time(micros),
                    Some(Value::Timestamp(micros)) => self.timestamp(micros),
                    _ => None,
                },
                None => None,
            },
            Value::Bool(_)
            | Value::Uuid(_)
            | Value::Json(_)
            | Value::Interval(_)
            | Value::Array(_) => None,
        };
        // The same text is not another way to show it.
        shown.filter(|text| *text != value.to_string())
    }

    fn date(&self, days: i32) -> Option<String> {
        if self.format.date.is_empty() {
            return None;
        }
        strtime::format(&self.format.date, civil_date(days)?).ok()
    }

    fn time(&self, micros: i64) -> Option<String> {
        if self.format.time.is_empty() {
            return None;
        }
        let nanos = i128::from(micros).checked_mul(1_000)?;
        let seconds = i64::try_from(nanos.div_euclid(1_000_000_000)).ok()?;
        let time = civil::Time::new(
            i8::try_from(seconds / 3600).ok()?,
            i8::try_from(seconds / 60 % 60).ok()?,
            i8::try_from(seconds % 60).ok()?,
            i32::try_from(nanos.rem_euclid(1_000_000_000)).ok()?,
        )
        .ok()?;
        strtime::format(&self.format.time, time).ok()
    }

    fn timestamp(&self, micros: i64) -> Option<String> {
        if self.format.timestamp.is_empty() {
            return None;
        }
        let instant = jiff::Timestamp::from_microsecond(micros).ok()?;
        // A timestamp without a zone is what it says: it is formatted, never converted.
        let civil = TimeZone::UTC.to_datetime(instant);
        strtime::format(&self.format.timestamp, civil).ok()
    }

    /// A value that is a point in time: shown in the display zone when there is one, else in
    /// the offset it was written with (`written`), else in UTC as the database prints it.
    fn instant(&self, micros: i64, written: Option<Offset>) -> Option<String> {
        let zone = match (&self.zone, written) {
            (Some((zone, _)), _) => zone.clone(),
            (None, _) if self.format.timestamp.is_empty() => return None,
            (None, Some(offset)) => TimeZone::fixed(offset),
            (None, None) => TimeZone::UTC,
        };
        let zoned = jiff::Timestamp::from_microsecond(micros)
            .ok()?
            .to_zoned(zone);
        let pattern = if self.format.timestamp.is_empty() {
            "%Y-%m-%d %H:%M:%S%.f"
        } else {
            &self.format.timestamp
        };
        let mut shown = strtime::format(pattern, &zoned).ok()?;
        // It always says where it is shown: its offset, unless the pattern writes one.
        if !names_zone(pattern) {
            shown.push(' ');
            shown.push_str(&strtime::format(OFFSET, &zoned).ok()?);
        }
        Some(shown)
    }

    fn number(&self, text: &str) -> Option<String> {
        let grouping = self.format.grouping.mark();
        let decimal = self.format.decimal.mark();
        if grouping.is_none() && decimal == '.' {
            return None;
        }
        let (sign, digits) = match text.strip_prefix('-') {
            Some(rest) => ("-", rest),
            None => ("", text),
        };
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        // Only plain digits: `NaN`, `Infinity` and `1e+20` are shown as they are.
        let plain = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() || !plain(whole) || !plain(fraction) {
            return None;
        }
        let mut out = String::with_capacity(text.len() + whole.len() / 3 + 1);
        out.push_str(sign);
        for (i, digit) in whole.chars().enumerate() {
            if i > 0 && (whole.len() - i) % 3 == 0 {
                out.extend(grouping);
            }
            out.push(digit);
        }
        if digits.contains('.') {
            out.push(decimal);
            out.push_str(fraction);
        }
        Some(out)
    }

    fn binary(&self, bytes: &[u8]) -> Option<String> {
        let text = match self.format.binary {
            BinaryDisplay::Hex => return None,
            BinaryDisplay::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            BinaryDisplay::Latin1 => bytes.iter().map(|b| char::from(*b)).collect(),
            BinaryDisplay::Base64 => return Some(base64(bytes)),
        };
        // One line in a cell: what would break it or cannot be drawn is a replacement mark.
        Some(
            text.chars()
                .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
                .collect(),
        )
    }
}

/// Whether a pattern writes the zone or the offset itself.
fn names_zone(pattern: &str) -> bool {
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            continue;
        }
        // Flags and colons between the percent sign and the directive.
        while chars.peek().is_some_and(|c| {
            matches!(c, ':' | '-' | '_' | '0' | '^' | '#' | '.') || c.is_ascii_digit()
        }) {
            chars.next();
        }
        if let Some('z' | 'Z' | 'Q') = chars.next() {
            return true;
        }
    }
    false
}

fn civil_date(days: i32) -> Option<civil::Date> {
    let instant = jiff::Timestamp::from_second(i64::from(days).checked_mul(86_400)?).ok()?;
    Some(TimeZone::UTC.to_datetime(instant).date())
}

/// The offset a `datetimeoffset`'s text ends with (` +02:00`).
fn written_offset(text: &str) -> Option<Offset> {
    let (_, offset) = text.rsplit_once(' ')?;
    let sign = match offset.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = offset[1..].split_once(':')?;
    let seconds = (hours.parse::<i32>().ok()? * 60 + minutes.parse::<i32>().ok()?) * 60;
    Offset::from_seconds(sign * seconds).ok()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Every time zone name the display zone can be, in order.
pub fn time_zones() -> Vec<String> {
    let mut names: Vec<String> = jiff::tz::db()
        .available()
        .map(|name| name.as_str().to_string())
        .collect();
    names.sort();
    names
}

/// Rows `[start, start + len)` of a result's current view as Arrow IPC, every cell as text:
/// first each column as the database prints it, then each column as `display` shows it, with
/// a cell there only where that differs. The display columns carry what kind of value the
/// column holds and the zone it is shown in. A masked column is never formatted.
pub fn window_ipc(
    stored: &StoredResult,
    start: usize,
    len: usize,
    display: &Display,
) -> Result<Vec<u8>, ArrowError> {
    let columns = &stored.schema().columns;
    let mut fields: Vec<Field> = columns
        .iter()
        .map(|c| Field::new(&c.name, DataType::Utf8, true))
        .collect();
    for (i, column) in columns.iter().enumerate() {
        let masked = stored.is_masked(i);
        let kind = if masked {
            ValueKind::Other
        } else {
            ValueKind::of(column)
        };
        let mut metadata =
            std::collections::HashMap::from([(KIND_KEY.to_string(), kind.as_str().to_string())]);
        if let Some(zone) = display.zone_of(column).filter(|_| !masked) {
            metadata.insert(ZONE_KEY.to_string(), zone.to_string());
        }
        fields.push(
            Field::new(format!("\u{1}shown{i}"), DataType::Utf8, true).with_metadata(metadata),
        );
    }
    let schema = Arc::new(Schema::new(fields));
    let mut out = Vec::new();
    let mut writer = StreamWriter::try_new(&mut out, &schema)?;
    let formats = !display.is_as_stored();
    for batch in stored.window(start, len) {
        let mut stored_text: Vec<ArrayRef> = Vec::with_capacity(columns.len());
        let mut shown_text: Vec<ArrayRef> = Vec::with_capacity(columns.len());
        for (i, meta) in columns.iter().enumerate() {
            let column = batch.column(i);
            let formatted = formats && !stored.is_masked(i);
            let mut as_stored: Vec<Option<String>> = Vec::with_capacity(batch.num_rows());
            let mut as_shown: Vec<Option<String>> = Vec::with_capacity(batch.num_rows());
            for row in 0..batch.num_rows() {
                match Value::from_array(column.as_ref(), row, meta.logical) {
                    Some(Value::Null) | None => {
                        as_stored.push(None);
                        as_shown.push(None);
                    }
                    Some(value) => {
                        as_shown.push(if formatted {
                            display.cell(meta, &value)
                        } else {
                            None
                        });
                        as_stored.push(Some(value.to_string()));
                    }
                }
            }
            stored_text.push(Arc::new(StringArray::from(as_stored)));
            shown_text.push(Arc::new(StringArray::from(as_shown)));
        }
        stored_text.extend(shown_text);
        writer.write(&RecordBatch::try_new(schema.clone(), stored_text)?)?;
    }
    writer.finish()?;
    drop(writer);
    Ok(out)
}

/// The cells of `rows` (positions in the current view, in the order given: a selection of rows
/// that are not next to each other too) and `columns`, as the grid shows them, tab-separated
/// with one row per line: what "copy as displayed" puts on the clipboard. A tab or a line
/// break inside a value becomes a space. A NULL is empty. A row past the end is left out.
pub fn displayed_text(
    stored: &StoredResult,
    rows: &[usize],
    columns: &[usize],
    header: bool,
    display: &Display,
) -> String {
    let schema = stored.schema().clone();
    let flat = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if matches!(c, '\t' | '\n' | '\r') {
                    ' '
                } else {
                    c
                }
            })
            .collect()
    };
    let mut out = String::new();
    if header {
        let names: Vec<String> = columns
            .iter()
            .filter_map(|c| schema.columns.get(*c).map(|meta| flat(&meta.name)))
            .collect();
        out.push_str(&names.join("\t"));
        out.push('\n');
    }
    for &row in rows.iter().filter(|&&row| row < stored.rows()) {
        let cells: Vec<String> = columns
            .iter()
            .map(|&c| {
                let Some(meta) = schema.columns.get(c) else {
                    return String::new();
                };
                match stored.value_at(row, c) {
                    Some(Value::Null) | None => String::new(),
                    Some(value) => {
                        let shown = if stored.is_masked(c) {
                            None
                        } else {
                            display.cell(meta, &value)
                        };
                        flat(&shown.unwrap_or_else(|| value.to_string()))
                    }
                }
            })
            .collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out
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

    fn display(change: impl FnOnce(&mut DisplayFormat)) -> Display {
        let mut format = DisplayFormat {
            as_stored: false,
            ..DisplayFormat::default()
        };
        change(&mut format);
        format.compile().unwrap()
    }

    /// 2026-11-16 20:07:16.5 UTC.
    const INSTANT: i64 = 1_794_859_636_500_000;

    #[test]
    fn by_default_everything_is_as_the_database_prints_it() {
        let default = DisplayFormat::default().compile().unwrap();
        assert!(default.is_as_stored());
        for value in [
            Value::Int(1_234_567),
            Value::Date(20_773),
            Value::TimestampTz(INSTANT),
            Value::Bytes(vec![1, 2]),
        ] {
            assert_eq!(default.cell(&meta(LogicalType::Int, "x"), &value), None);
        }
        // The switch wins over every setting.
        let off = DisplayFormat {
            as_stored: true,
            date: "%d/%m/%Y".into(),
            grouping: Grouping::Dot,
            decimal: DecimalMark::Comma,
            time_zone: "Europe/Madrid".into(),
            binary: BinaryDisplay::Base64,
            ..DisplayFormat::default()
        }
        .compile()
        .unwrap();
        assert!(off.is_as_stored());
        assert_eq!(
            off.cell(&meta(LogicalType::Int, "x"), &Value::Int(1_234_567)),
            None
        );
        assert_eq!(
            off.zone_of(&meta(LogicalType::TimestampTz, "timestamptz")),
            None
        );
    }

    #[test]
    fn dates_times_and_timestamps_follow_their_patterns() {
        let d = display(|f| {
            f.date = "%d/%m/%Y".into();
            f.time = "%I:%M %p".into();
            f.timestamp = "%d %b %Y %H:%M:%S".into();
        });
        let date = Value::Date(20_773);
        assert_eq!(date.to_string(), "2026-11-16");
        assert_eq!(
            d.cell(&meta(LogicalType::Date, "date"), &date).as_deref(),
            Some("16/11/2026")
        );
        assert_eq!(
            d.cell(
                &meta(LogicalType::Time, "time"),
                &Value::Time(72_436_000_000)
            )
            .as_deref(),
            Some("08:07 PM")
        );
        assert_eq!(
            d.cell(
                &meta(LogicalType::Timestamp, "timestamp"),
                &Value::Timestamp(INSTANT)
            )
            .as_deref(),
            Some("16 Nov 2026 20:07:16")
        );
        // A pattern that gives the database's own text is no other way to show it.
        let same = display(|f| f.date = "%Y-%m-%d".into());
        assert_eq!(same.cell(&meta(LogicalType::Date, "date"), &date), None);
    }

    #[test]
    fn a_value_with_a_zone_is_shown_converted_and_says_where() {
        let stored = Value::TimestampTz(INSTANT);
        assert_eq!(stored.to_string(), "2026-11-16 20:07:16.5+00");
        let column = meta(LogicalType::TimestampTz, "timestamptz");
        let madrid = display(|f| f.time_zone = "Europe/Madrid".into());
        assert_eq!(
            madrid.cell(&column, &stored).as_deref(),
            Some("2026-11-16 21:07:16.5 +01:00")
        );
        assert_eq!(madrid.zone_of(&column), Some("Europe/Madrid"));
        // Summer time is the zone's, not a fixed offset.
        let summer = Value::TimestampTz(INSTANT - 150 * 86_400_000_000);
        assert_eq!(
            madrid.cell(&column, &summer).as_deref(),
            Some("2026-06-19 22:07:16.5 +02:00")
        );
        // With a pattern, and one that names the zone itself.
        let pattern = display(|f| {
            f.time_zone = "America/New_York".into();
            f.timestamp = "%d/%m/%Y %H:%M".into();
        });
        assert_eq!(
            pattern.cell(&column, &stored).as_deref(),
            Some("16/11/2026 15:07 -05:00")
        );
        let named = display(|f| {
            f.time_zone = "America/New_York".into();
            f.timestamp = "%d/%m/%Y %H:%M %Z".into();
        });
        assert_eq!(
            named.cell(&column, &stored).as_deref(),
            Some("16/11/2026 15:07 EST")
        );
        // A pattern without a zone still says the value is in UTC.
        let utc = display(|f| f.timestamp = "%d/%m/%Y %H:%M".into());
        assert_eq!(
            utc.cell(&column, &stored).as_deref(),
            Some("16/11/2026 20:07 +00:00")
        );
        assert_eq!(utc.zone_of(&column), None);
        // A timestamp without a zone is never converted, and has no zone to say.
        let plain = meta(LogicalType::Timestamp, "timestamp");
        assert_eq!(madrid.cell(&plain, &Value::Timestamp(INSTANT)), None);
        assert_eq!(madrid.zone_of(&plain), None);
        assert_eq!(
            pattern.cell(&plain, &Value::Timestamp(INSTANT)).as_deref(),
            Some("16/11/2026 20:07")
        );
    }

    #[test]
    fn text_columns_that_hold_dates_are_formatted_as_the_dates_they_are() {
        let d = display(|f| {
            f.date = "%d/%m/%Y".into();
            f.timestamp = "%d/%m/%Y %H:%M:%S".into();
            f.time_zone = "Europe/Madrid".into();
        });
        let text = |s: &str| Value::Text(s.into());
        assert_eq!(
            d.cell(&meta(LogicalType::Text, "date"), &text("2026-11-16"))
                .as_deref(),
            Some("16/11/2026")
        );
        assert_eq!(
            d.cell(
                &meta(LogicalType::Text, "datetime"),
                &text("2026-11-16 20:07:16")
            )
            .as_deref(),
            Some("16/11/2026 20:07:16")
        );
        // A datetimeoffset is a point in time: converted, whatever offset it was written with.
        assert_eq!(
            d.cell(
                &meta(LogicalType::Text, "datetimeoffset"),
                &text("2026-11-16 15:07:16.0000000 -05:00")
            )
            .as_deref(),
            Some("16/11/2026 21:07:16 +01:00")
        );
        // Without a display zone it stays in the offset it was written with.
        let own = display(|f| f.timestamp = "%d/%m/%Y %H:%M".into());
        assert_eq!(
            own.cell(
                &meta(LogicalType::Text, "datetimeoffset"),
                &text("2026-11-16 15:07:16.0000000 -05:00")
            )
            .as_deref(),
            Some("16/11/2026 15:07 -05:00")
        );
        // MySQL's zero date is no date: as the database prints it. Ordinary text is text.
        assert_eq!(
            d.cell(&meta(LogicalType::Text, "date"), &text("0000-00-00")),
            None
        );
        assert_eq!(
            d.cell(&meta(LogicalType::Text, "varchar"), &text("2026-11-16")),
            None
        );
    }

    #[test]
    fn values_out_of_range_are_as_the_database_prints_them() {
        let d = display(|f| {
            f.timestamp = "%d/%m/%Y".into();
            f.date = "%d/%m/%Y".into();
            f.time_zone = "Europe/Madrid".into();
        });
        for micros in [i64::MAX, i64::MIN] {
            assert_eq!(
                d.cell(
                    &meta(LogicalType::Timestamp, "timestamp"),
                    &Value::Timestamp(micros)
                ),
                None
            );
            assert_eq!(
                d.cell(
                    &meta(LogicalType::TimestampTz, "timestamptz"),
                    &Value::TimestampTz(micros)
                ),
                None
            );
        }
        for days in [i32::MAX, i32::MIN] {
            assert_eq!(
                d.cell(&meta(LogicalType::Date, "date"), &Value::Date(days)),
                None
            );
        }
    }

    #[test]
    fn numbers_take_the_grouping_and_the_decimal_mark() {
        let european = display(|f| {
            f.grouping = Grouping::Dot;
            f.decimal = DecimalMark::Comma;
        });
        let int = meta(LogicalType::Int, "int8");
        let cases = [
            (Value::Int(1_234_567), Some("1.234.567")),
            (Value::Int(-1_000), Some("-1.000")),
            (Value::Int(999), None),
            (
                Value::Decimal("1234567.8900".into()),
                Some("1.234.567,8900"),
            ),
            (Value::Decimal("-0.5".into()), Some("-0,5")),
            (Value::Decimal("12".into()), None),
            (Value::Float(1234.5), Some("1.234,5")),
            (Value::Float(f64::NAN), None),
            (Value::Float(f64::INFINITY), None),
        ];
        for (value, expected) in cases {
            assert_eq!(
                european.cell(&int, &value).as_deref(),
                expected,
                "{value:?}"
            );
        }
        let grouped = display(|f| f.grouping = Grouping::Comma);
        assert_eq!(
            grouped
                .cell(&int, &Value::Decimal("1234567.891234".into()))
                .as_deref(),
            Some("1,234,567.891234")
        );
        let spaced = display(|f| f.grouping = Grouping::Space);
        assert_eq!(
            spaced.cell(&int, &Value::Int(1_234_567)).as_deref(),
            Some("1\u{a0}234\u{a0}567")
        );
        // No digit is ever dropped or rounded: only marks change.
        let digits = |text: &str| {
            text.chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
        };
        let long = Value::Decimal("98765432109876543210.123456789012345678".into());
        assert_eq!(
            digits(&european.cell(&int, &long).unwrap()),
            digits(&long.to_string())
        );
    }

    #[test]
    fn binary_reads_as_text_or_base64() {
        let column = meta(LogicalType::Bytes, "bytea");
        let bytes = Value::Bytes(b"caf\xc3\xa9\n\xff".to_vec());
        assert_eq!(bytes.to_string(), "\\x636166c3a90aff");
        assert_eq!(
            display(|f| f.binary = BinaryDisplay::Utf8)
                .cell(&column, &bytes)
                .as_deref(),
            Some("café\u{fffd}\u{fffd}")
        );
        assert_eq!(
            display(|f| f.binary = BinaryDisplay::Latin1)
                .cell(&column, &bytes)
                .as_deref(),
            Some("cafÃ©\u{fffd}ÿ")
        );
        assert_eq!(
            display(|f| f.binary = BinaryDisplay::Base64)
                .cell(&column, &bytes)
                .as_deref(),
            Some("Y2Fmw6kK/w==")
        );
        assert_eq!(
            display(|f| f.binary = BinaryDisplay::Hex).cell(&column, &bytes),
            None
        );
        for (input, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
        ] {
            assert_eq!(base64(input.as_bytes()), encoded);
        }
    }

    #[test]
    fn settings_that_cannot_be_used_are_refused() {
        let with = |change: &dyn Fn(&mut DisplayFormat)| {
            let mut format = DisplayFormat::default();
            change(&mut format);
            format.compile().map(|_| ())
        };
        // A date has no hour to print; an unknown directive; a pattern that is too long.
        assert!(matches!(
            with(&|f| f.date = "%H:%M".into()),
            Err(DisplayError::Pattern { what: "date", .. })
        ));
        assert!(matches!(
            with(&|f| f.time = "%Y".into()),
            Err(DisplayError::Pattern { what: "time", .. })
        ));
        assert!(matches!(
            with(&|f| f.timestamp = "%!".into()),
            Err(DisplayError::Pattern { .. })
        ));
        assert!(matches!(
            with(&|f| f.date = "%Y".repeat(40)),
            Err(DisplayError::Pattern { .. })
        ));
        assert!(matches!(
            with(&|f| f.date = "%Y\n%m".into()),
            Err(DisplayError::Pattern { .. })
        ));
        assert_eq!(
            with(&|f| f.time_zone = "Mars/Olympus".into()),
            Err(DisplayError::TimeZone("Mars/Olympus".into()))
        );
        assert_eq!(
            with(&|f| {
                f.grouping = Grouping::Comma;
                f.decimal = DecimalMark::Comma;
            }),
            Err(DisplayError::SameMarks)
        );
        // This computer's zone always exists, and the list has the usual names.
        assert!(with(&|f| f.time_zone = LOCAL_ZONE.into()).is_ok());
        let zones = time_zones();
        assert!(zones.iter().any(|z| z == "Europe/Madrid") && zones.iter().any(|z| z == "UTC"));
    }

    #[test]
    fn a_pattern_that_names_the_zone_gets_no_second_offset() {
        for pattern in ["%H:%M %z", "%H:%M %:z", "%H:%M %Z", "%H %::z", "%Q"] {
            assert!(names_zone(pattern), "{pattern}");
        }
        for pattern in ["%H:%M", "%Y-%m-%d z", "100%% Z", "%5d"] {
            assert!(!names_zone(pattern), "{pattern}");
        }
    }
}
