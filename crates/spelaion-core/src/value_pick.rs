//! The cell panel's value pickers in an editable result (SDB-58): a date and time picker for
//! date, time and timestamp columns, a list of the allowed values of an enum, a set or a
//! `CHECK (… IN (…))`, and a search of the table a foreign key references.
//!
//! A picker only writes the text of a cell edit: the edit is applied like any other, by
//! `edit.rs` (ADR-014: primary key from the database, one transaction, one row per statement,
//! read-only and Production decided there). What a picker writes is the text the grid shows
//! for that value once it is saved, so a picked value reads back unchanged: the date and time
//! are formatted here with the column's precision and the engine's way of writing it (UTC with
//! `+00` for a column with a time zone, as the drivers show them), an allowed value is the
//! catalog's own label, and a referenced key is the referenced row's value as stored.
//!
//! What a column allows is read from the catalog each time a picker opens (the UI only names
//! the column): `describe_table`'s types, constraints and foreign keys, and PostgreSQL's
//! `pg_enum`. A `CHECK` list is only offered when it can be read reliably: one constraint that
//! is exactly `column IN (…)`, `column = ANY (ARRAY[…])` or `column = … OR column = …` of
//! string or whole-number literals. The foreign key search is a page of the referenced table
//! (at most [`KEY_PAGE_ROWS`] rows, never the whole table) whose filter the core writes from
//! the text searched, like "filter by this value" ([`value_condition`]), and whose SELECT is
//! checked like a table data page.

use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use spelaion_driver_api::{
    ColumnInfo, ConstraintKind, Dialect, ForeignKeyInfo, LogicalType, TableInfo, Value,
};
use sqlparser::ast::{
    BinaryOperator, ColumnOption, Expr, Statement, TableConstraint, Value as SqlValue,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

use crate::Session;
use crate::export::sql_literal;
use crate::filter::FilterOp;
use crate::generate::{qualified, short_rows};
use crate::guard::sql_dialect;
use crate::table_data::{check_projection, push_clauses, value_condition};

/// Rows a page of the foreign key search holds.
pub const KEY_PAGE_ROWS: u64 = 50;
/// Columns of the referenced table shown next to its key, to tell its rows apart.
pub const KEY_LABEL_COLUMNS: usize = 3;
/// Longest text of a label column on a page; the key's own values are always whole.
pub const KEY_LABEL_CHARS: usize = 200;
/// The furthest a time zone offset goes (SQL Server's `datetimeoffset` limit).
pub const MAX_OFFSET_MINUTES: i32 = 14 * 60;

/// What a date and time picker writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalKind {
    Date,
    Time,
    Timestamp,
    /// With a time zone: written in UTC with `+00`, as the grid shows it, or where it is with
    /// its offset when the picker [keeps the offset](TemporalPicker::keeps_offset).
    TimestampTz,
}

/// How the seconds of a time may be picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporalStep {
    /// Up to the picker's `digits` fractional digits.
    Fraction,
    /// SQL Server's `datetime`: thousandths rounded to .000, .003 or .007, as it stores them.
    SqlServerDatetime,
    /// SQL Server's `smalldatetime`: whole minutes, no seconds.
    Minutes,
}

/// A date and time picker for a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemporalPicker {
    pub kind: TemporalKind,
    /// Fractional digits of a second the column keeps and the grid shows (0 to 7: SQL
    /// Server's `datetime2` and `datetimeoffset` keep 100 ns; the rest at most 6).
    pub digits: u8,
    /// The engine writes every one of `digits` digits (MySQL's `TIME(3)`: `10:20:30.120`);
    /// otherwise trailing zeros are dropped (`10:20:30.12`).
    pub padded: bool,
    pub step: TemporalStep,
    /// A time zone is written where it is with its offset (`2024-01-01 10:00:00.1234567
    /// +02:00`), as SQL Server keeps and shows a `datetimeoffset` (SDB-265); otherwise in UTC
    /// with `+00`, as the grid shows a time zone.
    pub keeps_offset: bool,
}

/// The most fractional digits a picker writes: SQL Server's 100 ns.
const MAX_DIGITS: u8 = 7;

/// Where a column's allowed values come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceSource {
    /// An enum type (PostgreSQL, MySQL, MariaDB, DuckDB).
    Enum,
    /// MySQL's and MariaDB's `SET`: several values at once.
    Set,
    /// A `CHECK (column IN (…))` constraint.
    Check,
}

/// The allowed values of a column, in the catalog's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoicePicker {
    pub source: ChoiceSource,
    pub values: Vec<String>,
}

impl ChoicePicker {
    /// Several values at once (a `SET`).
    pub fn multiple(&self) -> bool {
        self.source == ChoiceSource::Set
    }
}

/// The foreign key a column belongs to: picking a row of the referenced table sets every one
/// of `columns` to the value of the matching referenced column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPicker {
    pub name: String,
    pub schema: String,
    pub table: String,
    /// This table's columns, in the key's order.
    pub columns: Vec<String>,
    /// The referenced table's columns, in the same order.
    pub referenced_columns: Vec<String>,
}

/// The picker the cell panel offers for a column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValuePicker {
    Temporal(TemporalPicker),
    Choice(ChoicePicker),
    ForeignKey(KeyPicker),
}

/// A date and time as picked: the parts a picker's kind does not use are ignored (the date of
/// a time, the time of a date, the offset of anything without a time zone).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PickedTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// The fractional digits of the second, as typed (`"5"` is half a second).
    pub fraction: String,
    /// The offset from UTC the date and time were picked in, in minutes (east is positive).
    pub offset_minutes: i32,
}

/// A page of the foreign key search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencedRows {
    /// The columns shown: the referenced key's columns first (in the key's order), then up to
    /// [`KEY_LABEL_COLUMNS`] text columns of the referenced table.
    pub columns: Vec<String>,
    /// How many of `columns` are the key's.
    pub key_columns: usize,
    /// Each row's values as text (`None` is NULL); label columns cut at [`KEY_LABEL_CHARS`].
    pub rows: Vec<Vec<Option<String>>>,
    pub offset: u64,
    /// There are rows after this page.
    pub has_more: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PickError {
    #[error("could not read the table: {0}")]
    Catalog(String),
    #[error("{0} is not a column of the table")]
    NoSuchColumn(String),
    #[error("{0} is not part of a foreign key")]
    NotAKey(String),
    #[error("{0} is not a valid date (years 1 to 9999)")]
    BadDate(String),
    #[error("{0} is not a valid time of day")]
    BadTime(String),
    #[error("\"{0}\" is not a fraction of a second: up to {1} digits")]
    BadFraction(String, u8),
    #[error("this column keeps whole minutes: the seconds must be 0")]
    SecondsNotKept,
    #[error("an offset from UTC goes from -14:00 to +14:00")]
    BadOffset,
    #[error("\"{0}\" is not one of the allowed values")]
    NotAllowed(String),
    #[error("pick exactly one value")]
    OneValue,
    #[error("\"{0}\" is picked twice")]
    Twice(String),
    #[error("could not search {table}: {message}")]
    Search { table: String, message: String },
    /// The referenced key has a masked column (ADR-051): picking would write the mask.
    #[error("{0} is masked: its values cannot be picked")]
    Masked(String),
}

/// The picker for `column` of `schema.table`, from the catalog as it is now, or `None` when
/// the column takes no picker. A column of a foreign key takes the key's search, else a
/// column with allowed values takes their list, else a date or time column takes the date
/// and time picker.
pub async fn value_picker(
    session: &Session,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<Option<ValuePicker>, PickError> {
    let described = session
        .connection()
        .describe_table(schema, table)
        .await
        .map_err(|e| PickError::Catalog(e.to_string()))?;
    let info = find_column(&described, column)?;
    if let Some(key) = column_key(&described, &info.name) {
        return Ok(Some(ValuePicker::ForeignKey(KeyPicker {
            name: key.name.clone(),
            schema: key.referenced_schema.clone(),
            table: key.referenced_table.clone(),
            columns: key.columns.clone(),
            referenced_columns: key.referenced_columns.clone(),
        })));
    }
    let engine = session.dialect().name();
    if let Some(choice) = type_choices(engine, &info.native_type) {
        return Ok(Some(ValuePicker::Choice(choice)));
    }
    if engine == "postgres" {
        let labels = postgres_enum_labels(session, schema, table, &info.name).await?;
        if !labels.is_empty() {
            return Ok(Some(ValuePicker::Choice(ChoicePicker {
                source: ChoiceSource::Enum,
                values: labels,
            })));
        }
    }
    let checks = if engine == "sqlite" {
        sqlite_check_lists(session, schema, table, &info.name).await?
    } else {
        check_lists(engine, &described, &info.name)
    };
    if let Some(values) = only_list(checks) {
        return Ok(Some(ValuePicker::Choice(ChoicePicker {
            source: ChoiceSource::Check,
            values,
        })));
    }
    Ok(temporal_picker(engine, &info.native_type).map(ValuePicker::Temporal))
}

fn find_column<'a>(table: &'a TableInfo, column: &str) -> Result<&'a ColumnInfo, PickError> {
    table
        .columns
        .iter()
        .find(|c| c.name == column)
        .ok_or_else(|| PickError::NoSuchColumn(column.to_string()))
}

/// The foreign key `column` belongs to: the one with the fewest columns, then the first in
/// the catalog's order.
fn column_key<'a>(table: &'a TableInfo, column: &str) -> Option<&'a ForeignKeyInfo> {
    table
        .foreign_keys
        .iter()
        .filter(|key| {
            key.columns.iter().any(|c| c == column)
                && !key.columns.is_empty()
                && key.columns.len() == key.referenced_columns.len()
        })
        .min_by_key(|key| key.columns.len())
}

/// The date and time picker for a column of `native_type` (as `describe_table` writes it) on
/// `engine` (the dialect's name), or `None` when it is not a date, time or timestamp the
/// picker can write exactly (PostgreSQL's `timetz`, intervals…).
pub fn temporal_picker(engine: &str, native_type: &str) -> Option<TemporalPicker> {
    let lower = native_type.trim().to_ascii_lowercase();
    let (base, precision) = split_precision(&lower);
    let picker = |kind, digits: u8| TemporalPicker {
        kind,
        digits: digits.min(6),
        padded: false,
        step: TemporalStep::Fraction,
        keeps_offset: false,
    };
    let p = |default: u8| precision.unwrap_or(default);
    match engine {
        "postgres" => match base.as_str() {
            "date" => Some(picker(TemporalKind::Date, 0)),
            "time" | "time without time zone" => Some(picker(TemporalKind::Time, p(6))),
            "timestamp" | "timestamp without time zone" => {
                Some(picker(TemporalKind::Timestamp, p(6)))
            }
            "timestamptz" | "timestamp with time zone" => {
                Some(picker(TemporalKind::TimestampTz, p(6)))
            }
            _ => None,
        },
        "mysql" | "mariadb" => match base.as_str() {
            "date" => Some(picker(TemporalKind::Date, 0)),
            // Shown as the server writes them (text with their native type, SDB-122): every
            // digit of the precision (`10:20:30.120`).
            "datetime" | "timestamp" => Some(TemporalPicker {
                padded: true,
                ..picker(TemporalKind::Timestamp, p(0))
            }),
            // TIME is shown as the server writes it: every digit of its precision.
            "time" => Some(TemporalPicker {
                padded: true,
                ..picker(TemporalKind::Time, p(0))
            }),
            _ => None,
        },
        "sqlserver" => match base.as_str() {
            "date" => Some(picker(TemporalKind::Date, 0)),
            "time" => Some(picker(TemporalKind::Time, p(7))),
            // Shown as the server writes them (text with their native type, SDB-265): every
            // digit of the precision, up to 100 ns, and a `datetimeoffset`'s own offset.
            "datetime2" => Some(TemporalPicker {
                digits: p(7).min(MAX_DIGITS),
                padded: true,
                ..picker(TemporalKind::Timestamp, 0)
            }),
            "datetimeoffset" => Some(TemporalPicker {
                digits: p(7).min(MAX_DIGITS),
                padded: true,
                keeps_offset: true,
                ..picker(TemporalKind::TimestampTz, 0)
            }),
            "datetime" => Some(TemporalPicker {
                step: TemporalStep::SqlServerDatetime,
                ..picker(TemporalKind::Timestamp, 3)
            }),
            "smalldatetime" => Some(TemporalPicker {
                step: TemporalStep::Minutes,
                ..picker(TemporalKind::Timestamp, 0)
            }),
            _ => None,
        },
        // SQLite keeps the text it is given: its own functions' formats, to the millisecond.
        "sqlite" => {
            if base.contains("datetime") || base.contains("timestamp") {
                Some(picker(TemporalKind::Timestamp, 3))
            } else if base.contains("date") {
                Some(picker(TemporalKind::Date, 0))
            } else if base.contains("time") {
                Some(picker(TemporalKind::Time, 3))
            } else {
                None
            }
        }
        "duckdb" => match base.as_str() {
            "date" => Some(picker(TemporalKind::Date, 0)),
            "time" => Some(picker(TemporalKind::Time, 6)),
            "timestamp" | "datetime" | "timestamp_us" | "timestamp_ns" => {
                Some(picker(TemporalKind::Timestamp, 6))
            }
            "timestamp_ms" => Some(picker(TemporalKind::Timestamp, 3)),
            "timestamp_s" => Some(picker(TemporalKind::Timestamp, 0)),
            "timestamptz" | "timestamp with time zone" => {
                Some(picker(TemporalKind::TimestampTz, 6))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `timestamp(3) with time zone` → (`timestamp with time zone`, 3).
fn split_precision(native: &str) -> (String, Option<u8>) {
    if let (Some(open), Some(close)) = (native.find('('), native.find(')'))
        && open < close
    {
        let inside = native[open + 1..close].trim();
        if let Ok(digits) = inside.parse::<u8>() {
            let base = format!("{} {}", &native[..open], &native[close + 1..]);
            let base = base.split_whitespace().collect::<Vec<_>>().join(" ");
            return (base, Some(digits));
        }
    }
    (
        native.split_whitespace().collect::<Vec<_>>().join(" "),
        None,
    )
}

/// The text a picked date and time is written and shown as in a column with this picker.
pub fn picked_time_text(picker: &TemporalPicker, picked: &PickedTime) -> Result<String, PickError> {
    // A date has no time to check.
    let fraction = match picker.kind {
        TemporalKind::Date => "",
        TemporalKind::Time | TemporalKind::Timestamp | TemporalKind::TimestampTz => {
            picked.fraction.trim()
        }
    };
    let digits = match picker.step {
        TemporalStep::Minutes => 0,
        TemporalStep::Fraction | TemporalStep::SqlServerDatetime => picker.digits.min(MAX_DIGITS),
    };
    if !fraction.bytes().all(|b| b.is_ascii_digit()) || fraction.len() > usize::from(digits) {
        return Err(PickError::BadFraction(fraction.to_string(), digits));
    }
    if picker.step == TemporalStep::Minutes
        && picker.kind != TemporalKind::Date
        && picked.second != 0
    {
        return Err(PickError::SecondsNotKept);
    }
    let date = || {
        let shown = format!("{:04}-{:02}-{:02}", picked.year, picked.month, picked.day);
        if !(1..=9999).contains(&picked.year) {
            return Err(PickError::BadDate(shown));
        }
        NaiveDate::from_ymd_opt(picked.year, picked.month, picked.day)
            .ok_or(PickError::BadDate(shown))
    };
    let time = || {
        NaiveTime::from_hms_opt(picked.hour, picked.minute, picked.second).ok_or_else(|| {
            PickError::BadTime(format!(
                "{:02}:{:02}:{:02}",
                picked.hour, picked.minute, picked.second
            ))
        })
    };
    // The fraction in 100 ns steps, and as the thousandths SQL Server's datetime keeps.
    let mut steps: u32 = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<7}").parse().unwrap_or(0)
    };
    if picker.step == TemporalStep::SqlServerDatetime {
        steps = sqlserver_datetime_millis(steps / 10_000) * 10_000;
    }
    let fraction_text = |steps: u32| -> String {
        let all = format!("{steps:07}");
        let kept = &all[..usize::from(digits)];
        let kept = if picker.padded {
            kept
        } else {
            kept.trim_end_matches('0')
        };
        if kept.is_empty() {
            String::new()
        } else {
            format!(".{kept}")
        }
    };
    let time_text = |t: NaiveTime| t.format("%H:%M:%S").to_string();
    Ok(match picker.kind {
        TemporalKind::Date => date()?.format("%Y-%m-%d").to_string(),
        TemporalKind::Time => format!("{}{}", time_text(time()?), fraction_text(steps)),
        TemporalKind::Timestamp => format!(
            "{} {}{}",
            date()?.format("%Y-%m-%d"),
            time_text(time()?),
            fraction_text(steps)
        ),
        TemporalKind::TimestampTz => {
            if picked.offset_minutes.abs() > MAX_OFFSET_MINUTES {
                return Err(PickError::BadOffset);
            }
            let local = NaiveDateTime::new(date()?, time()?);
            let utc = local
                .checked_sub_signed(Duration::minutes(picked.offset_minutes.into()))
                .filter(|utc| (1..=9999).contains(&chrono::Datelike::year(&utc.date())))
                .ok_or_else(|| PickError::BadDate(local.format("%Y-%m-%d %H:%M").to_string()))?;
            if picker.keeps_offset {
                let minutes = picked.offset_minutes.unsigned_abs();
                let sign = if picked.offset_minutes < 0 { "-" } else { "+" };
                format!(
                    "{} {}{} {sign}{:02}:{:02}",
                    local.date().format("%Y-%m-%d"),
                    time_text(local.time()),
                    fraction_text(steps),
                    minutes / 60,
                    minutes % 60
                )
            } else {
                format!(
                    "{} {}{}+00",
                    utc.date().format("%Y-%m-%d"),
                    time_text(utc.time()),
                    fraction_text(steps)
                )
            }
        }
    })
}

/// SQL Server's `datetime` keeps thousandths ending in 0, 3 or 7: the nearest one, as it
/// rounds them (.999 is kept as .997 rather than carried into the next second).
fn sqlserver_datetime_millis(millis: u32) -> u32 {
    let ticks = (f64::from(millis) * 0.3).round();
    let rounded = (ticks / 0.3).round() as u32;
    rounded.min(997)
}

/// The text of the values picked from a column's allowed values: one value, or for a `SET`
/// the values picked in the order the set lists them, joined with commas (as MySQL writes
/// them; none is the empty set).
pub fn picked_choice_text(picker: &ChoicePicker, chosen: &[String]) -> Result<String, PickError> {
    for (i, value) in chosen.iter().enumerate() {
        if !picker.values.contains(value) {
            return Err(PickError::NotAllowed(value.clone()));
        }
        if chosen[..i].contains(value) {
            return Err(PickError::Twice(value.clone()));
        }
    }
    if picker.multiple() {
        Ok(picker
            .values
            .iter()
            .filter(|v| chosen.contains(v))
            .cloned()
            .collect::<Vec<_>>()
            .join(","))
    } else {
        match chosen {
            [one] => Ok(one.clone()),
            _ => Err(PickError::OneValue),
        }
    }
}

/// The values a type lists itself: MySQL's and MariaDB's `enum('a','b')` and `set('a','b')`,
/// DuckDB's `ENUM('a', 'b')`.
fn type_choices(engine: &str, native_type: &str) -> Option<ChoicePicker> {
    let native = native_type.trim();
    let lower = native.to_ascii_lowercase();
    let (source, rest) = match engine {
        "mysql" | "mariadb" if lower.starts_with("enum(") => (ChoiceSource::Enum, &native[5..]),
        "mysql" | "mariadb" if lower.starts_with("set(") => (ChoiceSource::Set, &native[4..]),
        "duckdb" if lower.starts_with("enum(") => (ChoiceSource::Enum, &native[5..]),
        _ => return None,
    };
    let values = quoted_list(rest.strip_suffix(')')?)?;
    (!values.is_empty()).then_some(ChoicePicker { source, values })
}

/// `'a','b''c', 'd'` → `a`, `b'c`, `d`; `None` for anything else.
fn quoted_list(list: &str) -> Option<Vec<String>> {
    let mut values = Vec::new();
    let mut chars = list.trim().chars().peekable();
    loop {
        if chars.next()? != '\'' {
            return None;
        }
        let mut value = String::new();
        loop {
            match chars.next()? {
                '\'' if chars.peek() == Some(&'\'') => {
                    chars.next();
                    value.push('\'');
                }
                '\'' => break,
                c => value.push(c),
            }
        }
        values.push(value);
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => return Some(values),
            Some(',') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            Some(_) => return None,
        }
    }
}

/// The labels of a PostgreSQL enum column (or of a domain over an enum), in their order.
async fn postgres_enum_labels(
    session: &Session,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<Vec<String>, PickError> {
    let dialect = session.dialect();
    let regclass = sql_literal(
        &Value::Text(qualified(dialect.as_ref(), schema, table)),
        "postgres",
    );
    let sql = format!(
        "SELECT e.enumlabel::text FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
         JOIN pg_catalog.pg_enum e ON e.enumtypid = \
         CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END \
         WHERE a.attrelid = pg_catalog.to_regclass({regclass}) AND a.attname = {} \
         AND NOT a.attisdropped ORDER BY e.enumsortorder",
        sql_literal(&Value::Text(column.to_string()), "postgres")
    );
    let (_, rows) = short_rows(session, &sql, true)
        .await
        .map_err(PickError::Catalog)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(Value::Text(label)) => Some(label),
            _ => None,
        })
        .collect())
}

/// The value lists of the table's `CHECK` constraints that are lists for `column`.
fn check_lists(engine: &str, table: &TableInfo, column: &str) -> Vec<Vec<String>> {
    table
        .constraints
        .iter()
        .filter(|c| c.kind == ConstraintKind::Check)
        .filter_map(|c| check_list(engine, &c.definition, column))
        .collect()
}

/// The one list of values the checks allow, when exactly one check is a list.
fn only_list(lists: Vec<Vec<String>>) -> Option<Vec<String>> {
    match <[Vec<String>; 1]>::try_from(lists) {
        Ok([values]) if !values.is_empty() => Some(values),
        _ => None,
    }
}

/// SQLite keeps no catalog of its checks: they are read from the table's own `CREATE TABLE`
/// text (column and table constraints alike), and a text that cannot be read gives none.
async fn sqlite_check_lists(
    session: &Session,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<Vec<Vec<String>>, PickError> {
    let sql = format!(
        "SELECT sql FROM {}.sqlite_master WHERE type = 'table' AND name = {}",
        session.dialect().quote_identifier(schema),
        sql_literal(&Value::Text(table.to_string()), "sqlite")
    );
    let (_, rows) = short_rows(session, &sql, true)
        .await
        .map_err(PickError::Catalog)?;
    let Some(Value::Text(create)) = rows.into_iter().next().and_then(|r| r.into_iter().next())
    else {
        return Ok(Vec::new());
    };
    Ok(create_table_checks(&create)
        .iter()
        .filter_map(|expr| expr_list(expr, column))
        .collect())
}

/// Every `CHECK` condition of a SQLite `CREATE TABLE` statement.
pub fn create_table_checks(create: &str) -> Vec<Expr> {
    let Ok(statements) = Parser::parse_sql(sql_dialect("sqlite").as_ref(), create) else {
        return Vec::new();
    };
    let [Statement::CreateTable(table)] = statements.as_slice() else {
        return Vec::new();
    };
    let columns = table
        .columns
        .iter()
        .flat_map(|c| &c.options)
        .filter_map(|o| match &o.option {
            ColumnOption::Check(check) => Some(check.expr.as_ref().clone()),
            _ => None,
        });
    let constraints = table.constraints.iter().filter_map(|c| match c {
        TableConstraint::Check(check) => Some(check.expr.as_ref().clone()),
        _ => None,
    });
    columns.chain(constraints).collect()
}

/// The values of a check definition (`CHECK (…)`, as `describe_table` writes it) that is
/// exactly a list of literals for `column`; `None` for any other condition.
pub fn check_list(engine: &str, definition: &str, column: &str) -> Option<Vec<String>> {
    let body = definition.trim();
    let body = body
        .get(..5)
        .filter(|start| start.eq_ignore_ascii_case("check"))
        .map_or(body, |_| &body[5..]);
    // MySQL's `information_schema` writes the clause's quotes escaped: `_utf8mb4\'new\'`.
    let unescaped;
    let body = if matches!(engine, "mysql" | "mariadb") && body.contains("\\'") {
        unescaped = mysql_unescape(body);
        unescaped.as_str()
    } else {
        body
    };
    let parser = sql_dialect(engine);
    let mut p = Parser::new(parser.as_ref()).try_with_sql(body).ok()?;
    let expr = p.parse_expr().ok()?;
    if p.peek_token().token != Token::EOF {
        return None;
    }
    expr_list(&expr, column)
}

/// `\x` → `x` throughout.
fn mysql_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The values of a check condition that is exactly a list of literals for `column`.
fn expr_list(expr: &Expr, column: &str) -> Option<Vec<String>> {
    let expr = unwrap(expr);
    let values = match expr {
        Expr::InList {
            expr,
            list,
            negated: false,
        } if is_column(expr, column) => list.iter().map(literal).collect::<Option<Vec<_>>>()?,
        Expr::AnyOp {
            left,
            compare_op: BinaryOperator::Eq,
            right,
            ..
        } if is_column(left, column) => match unwrap(right) {
            Expr::Array(array) => array.elem.iter().map(literal).collect::<Option<Vec<_>>>()?,
            _ => return None,
        },
        Expr::BinaryOp {
            op: BinaryOperator::Or | BinaryOperator::Eq,
            ..
        } => {
            let mut values = Vec::new();
            or_equalities(expr, column, &mut values)?;
            values
        }
        _ => return None,
    };
    let mut distinct: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        if !distinct.contains(&value) {
            distinct.push(value);
        }
    }
    Some(distinct)
}

/// `a = 'x' OR a = 'y' OR …` (in any nesting of ORs and parentheses), each value in order.
fn or_equalities(expr: &Expr, column: &str, values: &mut Vec<String>) -> Option<()> {
    match unwrap(expr) {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            or_equalities(left, column, values)?;
            or_equalities(right, column, values)
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } if is_column(left, column) => {
            values.push(literal(right)?);
            Some(())
        }
        _ => None,
    }
}

/// Without the parentheses and casts the engines add around a check's parts
/// (`((status)::text = ANY ((ARRAY['a'::character varying])::text[]))`).
fn unwrap(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unwrap(inner),
        Expr::Cast { expr, .. } => unwrap(expr),
        other => other,
    }
}

fn is_column(expr: &Expr, column: &str) -> bool {
    match unwrap(expr) {
        Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case(column),
        _ => false,
    }
}

/// A string literal, or a whole number written plainly; nothing else is a reliable value.
fn literal(expr: &Expr) -> Option<String> {
    match unwrap(expr) {
        Expr::Value(value) => match &value.value {
            SqlValue::SingleQuotedString(text) | SqlValue::NationalStringLiteral(text) => {
                Some(text.clone())
            }
            SqlValue::Number(text, _)
                if !text.is_empty()
                    && text
                        .trim_start_matches('-')
                        .bytes()
                        .all(|b| b.is_ascii_digit())
                    && text.parse::<i64>().is_ok() =>
            {
                Some(text.to_string())
            }
            _ => None,
        },
        // MySQL's charset introducer: `_utf8mb4'paid'`.
        Expr::Prefixed { value, .. } => literal(value),
        Expr::UnaryOp {
            op: sqlparser::ast::UnaryOperator::Minus,
            expr,
        } => literal(expr)
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            .map(|n| format!("-{n}")),
        _ => None,
    }
}

/// A page of the table `column`'s foreign key references, `offset` rows in, keeping the rows
/// whose key or label columns contain `search` (every row when it is blank). The key is read
/// from the catalog again; the page holds at most [`KEY_PAGE_ROWS`] rows, in the key's order.
pub async fn search_referenced(
    session: &Session,
    schema: &str,
    table: &str,
    column: &str,
    search: &str,
    offset: u64,
) -> Result<ReferencedRows, PickError> {
    let conn = session.connection();
    let described = conn
        .describe_table(schema, table)
        .await
        .map_err(|e| PickError::Catalog(e.to_string()))?;
    find_column(&described, column)?;
    let key = column_key(&described, column)
        .ok_or_else(|| PickError::NotAKey(column.to_string()))?
        .clone();
    let masking = session.masking();
    if let Some(masked) = key
        .referenced_columns
        .iter()
        .find(|c| masking.masks_table_column(&key.referenced_table, c))
        .or_else(|| {
            key.columns
                .iter()
                .find(|c| masking.masks_table_column(table, c))
        })
    {
        return Err(PickError::Masked(masked.clone()));
    }
    let referenced = conn
        .describe_table(&key.referenced_schema, &key.referenced_table)
        .await
        .map_err(|e| PickError::Catalog(e.to_string()))?;
    let dialect = session.dialect();
    let sql = referenced_page_sql(dialect.as_ref(), &key, &referenced, search, offset)?;
    let (_, rows) = short_rows(session, &sql.sql, false)
        .await
        .map_err(|message| PickError::Search {
            table: key.referenced_table.clone(),
            message,
        })?;
    let has_more = rows.len() as u64 > KEY_PAGE_ROWS;
    let key_columns = key.referenced_columns.len();
    let rows = rows
        .into_iter()
        .take(KEY_PAGE_ROWS as usize)
        .map(|row| {
            row.into_iter()
                .enumerate()
                .map(|(i, value)| match value {
                    Value::Null => None,
                    value if i < key_columns => Some(value.to_string()),
                    value => Some(cut(value.to_string())),
                })
                .collect()
        })
        .collect();
    Ok(ReferencedRows {
        columns: sql.columns,
        key_columns,
        rows,
        offset,
        has_more,
    })
}

/// The search's SELECT and the columns it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencedPageSql {
    pub sql: String,
    pub columns: Vec<String>,
}

/// The SELECT of a page of the foreign key search: the key's columns and the label columns
/// of `referenced`, rows containing `search` in any of them, in the key's order, one row more
/// than a page (to know whether there is another), checked like a table data page.
pub fn referenced_page_sql(
    dialect: &dyn Dialect,
    key: &ForeignKeyInfo,
    referenced: &TableInfo,
    search: &str,
    offset: u64,
) -> Result<ReferencedPageSql, PickError> {
    let failed = |message: String| PickError::Search {
        table: key.referenced_table.clone(),
        message,
    };
    let mut columns: Vec<(String, LogicalType)> = key
        .referenced_columns
        .iter()
        .map(|name| {
            let native = referenced
                .columns
                .iter()
                .find(|c| &c.name == name)
                .map_or("", |c| c.native_type.as_str());
            (name.clone(), text_like(native))
        })
        .collect();
    columns.extend(
        referenced
            .columns
            .iter()
            .filter(|c| !key.referenced_columns.contains(&c.name))
            .filter(|c| text_like(&c.native_type) == LogicalType::Text)
            .take(KEY_LABEL_COLUMNS)
            .map(|c| (c.name.clone(), LogicalType::Text)),
    );
    let search = search.trim();
    let filter = if search.is_empty() {
        String::new()
    } else {
        columns
            .iter()
            .map(|(name, logical)| {
                value_condition(
                    dialect,
                    name,
                    *logical,
                    FilterOp::Contains,
                    &Value::Text(search.to_string()),
                )
                .map_err(|e| failed(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(" OR ")
    };
    let order_by = key
        .referenced_columns
        .iter()
        .map(|c| dialect.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let limit = KEY_PAGE_ROWS + 1;
    let mut sql = format!(
        "SELECT {}\nFROM {}",
        columns
            .iter()
            .map(|(name, _)| dialect.quote_identifier(name))
            .collect::<Vec<_>>()
            .join(", "),
        qualified(dialect, &referenced.object.schema, &referenced.object.name)
    );
    push_clauses(&mut sql, &filter, &order_by);
    if dialect.name() == "sqlserver" {
        sql.push_str(&format!(
            "\nOFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
        ));
    } else {
        sql.push_str(&format!("\nLIMIT {limit} OFFSET {offset}"));
    }
    check_projection(
        dialect,
        &sql,
        Some((limit, offset)),
        !filter.is_empty(),
        columns.len(),
    )
    .map_err(|e| failed(e.to_string()))?;
    Ok(ReferencedPageSql {
        sql,
        columns: columns.into_iter().map(|(name, _)| name).collect(),
    })
}

/// Text columns are searched as they are; anything else as its text (a cast).
fn text_like(native_type: &str) -> LogicalType {
    let lower = native_type.to_ascii_lowercase();
    let texty = ["char", "text", "string", "clob", "citext", "enum(", "set("]
        .iter()
        .any(|word| lower.contains(word))
        || lower == "name";
    if texty {
        LogicalType::Text
    } else {
        LogicalType::Native
    }
}

fn cut(text: String) -> String {
    if text.chars().count() <= KEY_LABEL_CHARS {
        text
    } else {
        let mut cut: String = text.chars().take(KEY_LABEL_CHARS).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picked(date: (i32, u32, u32), time: (u32, u32, u32), fraction: &str) -> PickedTime {
        PickedTime {
            year: date.0,
            month: date.1,
            day: date.2,
            hour: time.0,
            minute: time.1,
            second: time.2,
            fraction: fraction.into(),
            offset_minutes: 0,
        }
    }

    #[test]
    fn types_map_to_pickers_per_engine() {
        let p = |engine, native| temporal_picker(engine, native);
        let t = |kind, digits| {
            Some(TemporalPicker {
                kind,
                digits,
                padded: false,
                step: TemporalStep::Fraction,
                keeps_offset: false,
            })
        };
        assert_eq!(p("postgres", "date"), t(TemporalKind::Date, 0));
        assert_eq!(
            p("postgres", "timestamp(3) without time zone"),
            t(TemporalKind::Timestamp, 3)
        );
        assert_eq!(
            p("postgres", "timestamp with time zone"),
            t(TemporalKind::TimestampTz, 6)
        );
        assert_eq!(
            p("postgres", "time(0) without time zone"),
            t(TemporalKind::Time, 0)
        );
        assert_eq!(p("postgres", "time with time zone"), None);
        assert_eq!(p("postgres", "interval"), None);
        // MySQL's dates are the server's text (SDB-122): every digit of the precision.
        let padded = |kind, digits| {
            Some(TemporalPicker {
                padded: true,
                ..t(kind, digits).unwrap()
            })
        };
        assert_eq!(
            p("mysql", "datetime(6)"),
            padded(TemporalKind::Timestamp, 6)
        );
        assert_eq!(p("mysql", "datetime"), padded(TemporalKind::Timestamp, 0));
        assert_eq!(
            p("mariadb", "timestamp(3)"),
            padded(TemporalKind::Timestamp, 3)
        );
        assert_eq!(
            p("mysql", "time(3)"),
            Some(TemporalPicker {
                padded: true,
                ..t(TemporalKind::Time, 3).unwrap()
            })
        );
        assert_eq!(p("mysql", "year"), None);
        // SQL Server's datetime2 and datetimeoffset are the server's text (SDB-265): every
        // digit, up to the seventh, and a datetimeoffset's own offset.
        assert_eq!(
            p("sqlserver", "datetime2(7)"),
            padded(TemporalKind::Timestamp, 7)
        );
        assert_eq!(
            p("sqlserver", "datetime2"),
            padded(TemporalKind::Timestamp, 7)
        );
        assert_eq!(
            p("sqlserver", "datetimeoffset(2)"),
            Some(TemporalPicker {
                keeps_offset: true,
                ..padded(TemporalKind::TimestampTz, 2).unwrap()
            })
        );
        assert_eq!(p("sqlserver", "time(7)"), t(TemporalKind::Time, 6));
        assert_eq!(
            p("sqlserver", "smalldatetime").map(|p| p.step),
            Some(TemporalStep::Minutes)
        );
        assert_eq!(p("sqlite", "DATETIME"), t(TemporalKind::Timestamp, 3));
        assert_eq!(p("sqlite", "date"), t(TemporalKind::Date, 0));
        assert_eq!(p("sqlite", "TEXT"), None);
        assert_eq!(p("duckdb", "TIMESTAMP_MS"), t(TemporalKind::Timestamp, 3));
        assert_eq!(
            p("duckdb", "TIMESTAMP WITH TIME ZONE"),
            t(TemporalKind::TimestampTz, 6)
        );
    }

    #[test]
    fn picked_times_are_written_with_the_column_precision() {
        let ts = |digits, padded| TemporalPicker {
            kind: TemporalKind::Timestamp,
            digits,
            padded,
            step: TemporalStep::Fraction,
            keeps_offset: false,
        };
        let at = picked((2026, 2, 28), (9, 5, 7), "12");
        assert_eq!(
            picked_time_text(&ts(3, false), &at).unwrap(),
            "2026-02-28 09:05:07.12"
        );
        assert_eq!(
            picked_time_text(&ts(3, true), &at).unwrap(),
            "2026-02-28 09:05:07.120"
        );
        assert_eq!(
            picked_time_text(&ts(0, false), &picked((2026, 2, 28), (9, 5, 7), "")).unwrap(),
            "2026-02-28 09:05:07"
        );
        assert_eq!(
            picked_time_text(&ts(2, false), &at.clone()).unwrap(),
            "2026-02-28 09:05:07.12"
        );
        // More digits than the column keeps, or not digits at all, are refused.
        assert!(picked_time_text(&ts(1, false), &at).is_err());
        assert!(picked_time_text(&ts(6, false), &picked((2026, 1, 1), (0, 0, 0), "1a")).is_err());
        // Impossible dates and times are refused, leap days are not.
        assert!(picked_time_text(&ts(0, false), &picked((2026, 2, 29), (0, 0, 0), "")).is_err());
        assert!(picked_time_text(&ts(0, false), &picked((2024, 2, 29), (0, 0, 0), "")).is_ok());
        assert!(picked_time_text(&ts(0, false), &picked((2024, 1, 1), (24, 0, 0), "")).is_err());
        assert!(picked_time_text(&ts(0, false), &picked((0, 1, 1), (0, 0, 0), "")).is_err());
        let date = TemporalPicker {
            kind: TemporalKind::Date,
            ..ts(0, false)
        };
        assert_eq!(
            picked_time_text(&date, &picked((7, 3, 4), (23, 0, 0), "999")).unwrap(),
            "0007-03-04"
        );
    }

    #[test]
    fn a_time_zone_is_written_in_utc() {
        let tz = TemporalPicker {
            kind: TemporalKind::TimestampTz,
            digits: 6,
            padded: false,
            step: TemporalStep::Fraction,
            keeps_offset: false,
        };
        let mut at = picked((2026, 1, 1), (1, 30, 0), "5");
        at.offset_minutes = 120;
        assert_eq!(
            picked_time_text(&tz, &at).unwrap(),
            "2025-12-31 23:30:00.5+00"
        );
        at.offset_minutes = -(5 * 60 + 30);
        assert_eq!(
            picked_time_text(&tz, &at).unwrap(),
            "2026-01-01 07:00:00.5+00"
        );
        at.offset_minutes = 15 * 60;
        assert_eq!(picked_time_text(&tz, &at), Err(PickError::BadOffset));
        // The offset of a column without a time zone is ignored.
        let ts = TemporalPicker {
            kind: TemporalKind::Timestamp,
            ..tz
        };
        at.offset_minutes = 120;
        assert_eq!(picked_time_text(&ts, &at).unwrap(), "2026-01-01 01:30:00.5");
    }

    #[test]
    fn sqlserver_datetime2_and_datetimeoffset_keep_seven_digits_and_the_offset() {
        let datetime2 = temporal_picker("sqlserver", "datetime2(7)").unwrap();
        let at = picked((2024, 1, 1), (10, 0, 0), "1234567");
        assert_eq!(
            picked_time_text(&datetime2, &at).unwrap(),
            "2024-01-01 10:00:00.1234567"
        );
        assert_eq!(
            picked_time_text(&datetime2, &picked((2024, 1, 1), (10, 0, 0), "5")).unwrap(),
            "2024-01-01 10:00:00.5000000"
        );
        assert!(
            picked_time_text(&datetime2, &picked((2024, 1, 1), (10, 0, 0), "12345678")).is_err()
        );
        let offset = temporal_picker("sqlserver", "datetimeoffset").unwrap();
        let mut at = picked((2026, 1, 1), (1, 30, 0), "0000007");
        at.offset_minutes = 120;
        assert_eq!(
            picked_time_text(&offset, &at).unwrap(),
            "2026-01-01 01:30:00.0000007 +02:00"
        );
        at.offset_minutes = -(5 * 60 + 30);
        assert_eq!(
            picked_time_text(&offset, &at).unwrap(),
            "2026-01-01 01:30:00.0000007 -05:30"
        );
        at.offset_minutes = 15 * 60;
        assert_eq!(picked_time_text(&offset, &at), Err(PickError::BadOffset));
        // Its instant in UTC must still be a date the server keeps.
        let mut first = picked((1, 1, 1), (0, 30, 0), "");
        first.offset_minutes = 60;
        assert!(picked_time_text(&offset, &first).is_err());
        // A picker from the app with more digits than any column keeps writes seven at most.
        let wide = TemporalPicker {
            digits: 9,
            ..datetime2
        };
        assert_eq!(
            picked_time_text(&wide, &picked((2024, 1, 1), (10, 0, 0), "1234567")).unwrap(),
            "2024-01-01 10:00:00.1234567"
        );
    }

    #[test]
    fn sqlserver_datetime_and_smalldatetime_round_as_the_server() {
        let datetime = temporal_picker("sqlserver", "datetime").unwrap();
        let text = |fraction: &str| {
            picked_time_text(&datetime, &picked((2024, 1, 1), (10, 0, 0), fraction)).unwrap()
        };
        assert_eq!(text(""), "2024-01-01 10:00:00");
        assert_eq!(text("001"), "2024-01-01 10:00:00");
        assert_eq!(text("002"), "2024-01-01 10:00:00.003");
        assert_eq!(text("005"), "2024-01-01 10:00:00.007");
        assert_eq!(text("009"), "2024-01-01 10:00:00.01");
        assert_eq!(text("999"), "2024-01-01 10:00:00.997");
        let small = temporal_picker("sqlserver", "smalldatetime").unwrap();
        assert_eq!(
            picked_time_text(&small, &picked((2024, 1, 1), (10, 7, 0), "")).unwrap(),
            "2024-01-01 10:07:00"
        );
        assert_eq!(
            picked_time_text(&small, &picked((2024, 1, 1), (10, 7, 30), "")),
            Err(PickError::SecondsNotKept)
        );
        assert!(picked_time_text(&small, &picked((2024, 1, 1), (10, 7, 0), "5")).is_err());
    }

    #[test]
    fn types_list_their_values() {
        assert_eq!(
            type_choices("mysql", "enum('a','b''c','d e')"),
            Some(ChoicePicker {
                source: ChoiceSource::Enum,
                values: vec!["a".into(), "b'c".into(), "d e".into()]
            })
        );
        assert_eq!(
            type_choices("mysql", "set('x','y')").map(|c| c.source),
            Some(ChoiceSource::Set)
        );
        assert_eq!(
            type_choices("duckdb", "ENUM('sad', 'ok', 'happy')").map(|c| c.values),
            Some(vec!["sad".into(), "ok".into(), "happy".into()])
        );
        assert_eq!(type_choices("postgres", "enum('a')"), None);
        assert_eq!(type_choices("mysql", "varchar(20)"), None);
        assert_eq!(type_choices("mysql", "enum('a'"), None);
        assert_eq!(type_choices("mysql", "enum('a' 'b')"), None);
    }

    #[test]
    fn picked_choices_keep_the_catalog_order() {
        let set = ChoicePicker {
            source: ChoiceSource::Set,
            values: vec!["x".into(), "y".into(), "z".into()],
        };
        assert_eq!(
            picked_choice_text(&set, &["z".into(), "x".into()]).unwrap(),
            "x,z"
        );
        assert_eq!(picked_choice_text(&set, &[]).unwrap(), "");
        assert_eq!(
            picked_choice_text(&set, &["x".into(), "x".into()]),
            Err(PickError::Twice("x".into()))
        );
        assert_eq!(
            picked_choice_text(&set, &["w".into()]),
            Err(PickError::NotAllowed("w".into()))
        );
        let one = ChoicePicker {
            source: ChoiceSource::Enum,
            ..set
        };
        assert_eq!(picked_choice_text(&one, &["y".into()]).unwrap(), "y");
        assert_eq!(
            picked_choice_text(&one, &["x".into(), "y".into()]),
            Err(PickError::OneValue)
        );
    }

    #[test]
    fn check_lists_are_read_only_when_reliable() {
        let list = |engine, definition| check_list(engine, definition, "status");
        let abc = Some(vec!["new".to_string(), "paid".to_string()]);
        assert_eq!(list("sqlite", "CHECK (status IN ('new', 'paid'))"), abc);
        assert_eq!(list("duckdb", "CHECK((status IN ('new', 'paid')))"), abc);
        assert_eq!(
            list(
                "postgres",
                "CHECK ((status = ANY (ARRAY['new'::text, 'paid'::text])))"
            ),
            abc
        );
        assert_eq!(
            list(
                "postgres",
                "CHECK (((status)::text = ANY ((ARRAY['new'::character varying, 'paid'::character varying])::text[])))"
            ),
            abc
        );
        assert_eq!(
            list("sqlserver", "CHECK ([status]='new' OR [status]='paid')"),
            abc
        );
        assert_eq!(
            list(
                "mysql",
                "CHECK ((`status` in (_utf8mb4'new',_utf8mb4'paid')))"
            ),
            abc
        );
        // As MySQL's information_schema writes it, quotes escaped.
        assert_eq!(
            list(
                "mysql",
                r"CHECK ((`status` in (_utf8mb4\'new\',_utf8mb4\'paid\')))"
            ),
            abc
        );
        assert_eq!(
            list(
                "mysql",
                r"CHECK ((`status` in (_utf8mb4\'o\'\'k\',_latin1\'a\\\\b\')))"
            ),
            Some(vec!["o'k".to_string(), "a\\b".to_string()])
        );
        assert_eq!(list("mariadb", "CHECK (`status` in ('new','paid'))"), abc);
        assert_eq!(
            list("sqlite", "CHECK (level IN (1, 2))"),
            None,
            "another column"
        );
        assert_eq!(
            check_list("sqlite", "CHECK (level IN (1, -2, 3))", "level"),
            Some(vec!["1".into(), "-2".into(), "3".into()])
        );
        // Anything that is not exactly a list of literals for the column is ignored.
        for definition in [
            "CHECK (status NOT IN ('new', 'paid'))",
            "CHECK (status IN ('new', 'paid') AND total > 0)",
            "CHECK (status IN ('new', lower('PAID')))",
            "CHECK (status = 'new' OR other = 'paid')",
            "CHECK (status IN (1.5, 2))",
            "CHECK (status <> 'new')",
            "CHECK (status IN ('new')) garbage",
            "CHECK (status IN (SELECT x FROM t))",
        ] {
            assert_eq!(list("sqlite", definition), None, "{definition}");
        }
    }
}
