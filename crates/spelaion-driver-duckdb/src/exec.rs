//! Statement execution and conversion of DuckDB's Arrow results to SpelaionDB's columns
//! (ADR-005): the types with their own Arrow column keep it, everything else travels as the
//! text DuckDB would print (`format.rs`).

use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, AsArray, BinaryBuilder, BooleanArray, Date32Array, Float64Builder,
    RecordBatch, StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use arrow::compute::{cast, concat_batches};
use arrow::datatypes::{
    DataType, Date32Type, Float32Type, Float64Type, Int64Type, Schema, Time64MicrosecondType,
    Time64NanosecondType, TimeUnit, TimestampMicrosecondType, TimestampMillisecondType,
    TimestampNanosecondType, TimestampSecondType, UInt64Type,
};
use spelaion_driver_api::thread_cancel::Work;
use spelaion_driver_api::{ColumnMeta, DriverError, ExecEvent, LogicalType, Result, ResultSchema};
use tokio::sync::mpsc;

use crate::format;

pub fn map_error(e: duckdb::Error) -> DriverError {
    match e {
        duckdb::Error::DuckDBFailure(_, Some(message)) => database_error(message),
        duckdb::Error::DuckDBFailure(err, None) => database_error(err.to_string()),
        other => DriverError::Other(other.to_string()),
    }
}

/// DuckDB's messages carry the statement with a caret (`LINE 1: …`): kept as the detail.
fn database_error(message: String) -> DriverError {
    if message.starts_with("INTERRUPT Error") {
        return DriverError::Cancelled;
    }
    let (message, detail) = match message.split_once("\n\nLINE ") {
        Some((head, line)) => (head.trim().to_string(), Some(format!("LINE {line}"))),
        None => (message.trim().to_string(), None),
    };
    DriverError::Database {
        code: None,
        message,
        detail,
        hint: None,
        position: None,
    }
}

/// How a column of DuckDB's Arrow output becomes one of ours.
#[derive(Debug, Clone, Copy)]
enum Conversion {
    Bool,
    /// Signed integers and unsigned up to 32 bits: Int64.
    Int,
    Float,
    Bytes,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    /// Plain strings.
    Utf8,
    /// Written as DuckDB's text with this logical type.
    Text(LogicalType),
}

fn conversion(data_type: &DataType) -> Conversion {
    match data_type {
        DataType::Boolean => Conversion::Bool,
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32 => Conversion::Int,
        DataType::Float16 | DataType::Float32 | DataType::Float64 => Conversion::Float,
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => Conversion::Bytes,
        DataType::Date32 => Conversion::Date,
        DataType::Time64(TimeUnit::Microsecond | TimeUnit::Nanosecond) => Conversion::Time,
        DataType::Timestamp(_, None) => Conversion::Timestamp,
        DataType::Timestamp(_, Some(_)) => Conversion::TimestampTz,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => Conversion::Utf8,
        // UBIGINT, HUGEINT and DECIMAL: exact, as text.
        DataType::UInt64 | DataType::Decimal128(..) | DataType::Decimal256(..) => {
            Conversion::Text(LogicalType::Decimal)
        }
        DataType::Interval(_) => Conversion::Text(LogicalType::Interval),
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) => {
            Conversion::Text(LogicalType::Array)
        }
        // ENUM.
        DataType::Dictionary(..) => Conversion::Text(LogicalType::Text),
        _ => Conversion::Text(LogicalType::Native),
    }
}

fn logical(conversion: Conversion) -> LogicalType {
    match conversion {
        Conversion::Bool => LogicalType::Bool,
        Conversion::Int => LogicalType::Int,
        Conversion::Float => LogicalType::Float,
        Conversion::Bytes => LogicalType::Bytes,
        Conversion::Date => LogicalType::Date,
        Conversion::Time => LogicalType::Time,
        Conversion::Timestamp => LogicalType::Timestamp,
        Conversion::TimestampTz => LogicalType::TimestampTz,
        Conversion::Utf8 => LogicalType::Text,
        Conversion::Text(logical) => logical,
    }
}

/// DuckDB's name for a result column's type. The spatial extension's `GEOMETRY` reaches
/// Arrow as WKB marked `geoarrow.wkb`: named so, the map view knows it (ADR-045).
fn field_native_type(field: &arrow::datatypes::Field) -> String {
    if field
        .metadata()
        .get("ARROW:extension:name")
        .is_some_and(|name| name == "geoarrow.wkb")
    {
        return "GEOMETRY".into();
    }
    native_type(field.data_type())
}

/// DuckDB's name for the type behind an Arrow type (`INTEGER`, `VARCHAR[]`…).
pub fn native_type(data_type: &DataType) -> String {
    match data_type {
        DataType::Null => "\"NULL\"".into(),
        DataType::Boolean => "BOOLEAN".into(),
        DataType::Int8 => "TINYINT".into(),
        DataType::Int16 => "SMALLINT".into(),
        DataType::Int32 => "INTEGER".into(),
        DataType::Int64 => "BIGINT".into(),
        DataType::UInt8 => "UTINYINT".into(),
        DataType::UInt16 => "USMALLINT".into(),
        DataType::UInt32 => "UINTEGER".into(),
        DataType::UInt64 => "UBIGINT".into(),
        DataType::Float16 | DataType::Float32 => "FLOAT".into(),
        DataType::Float64 => "DOUBLE".into(),
        DataType::Decimal128(38, 0) => "HUGEINT".into(),
        DataType::Decimal128(p, s) | DataType::Decimal256(p, s) => format!("DECIMAL({p},{s})"),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "VARCHAR".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "BLOB".into(),
        DataType::Date32 | DataType::Date64 => "DATE".into(),
        DataType::Time32(_) | DataType::Time64(_) => "TIME".into(),
        DataType::Timestamp(_, Some(_)) => "TIMESTAMP WITH TIME ZONE".into(),
        DataType::Timestamp(TimeUnit::Second, None) => "TIMESTAMP_S".into(),
        DataType::Timestamp(TimeUnit::Millisecond, None) => "TIMESTAMP_MS".into(),
        DataType::Timestamp(TimeUnit::Microsecond, None) => "TIMESTAMP".into(),
        DataType::Timestamp(TimeUnit::Nanosecond, None) => "TIMESTAMP_NS".into(),
        DataType::Interval(_) => "INTERVAL".into(),
        DataType::List(f) | DataType::LargeList(f) => format!("{}[]", native_type(f.data_type())),
        DataType::FixedSizeList(f, n) => format!("{}[{n}]", native_type(f.data_type())),
        DataType::Struct(fields) => format!(
            "STRUCT({})",
            fields
                .iter()
                .map(|f| format!("{} {}", f.name(), native_type(f.data_type())))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        DataType::Map(entries, _) => match entries.data_type() {
            DataType::Struct(kv) if kv.len() == 2 => format!(
                "MAP({}, {})",
                native_type(kv[0].data_type()),
                native_type(kv[1].data_type())
            ),
            _ => "MAP".into(),
        },
        DataType::Union(fields, _) => format!(
            "UNION({})",
            fields
                .iter()
                .map(|(_, f)| format!("{} {}", f.name(), native_type(f.data_type())))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        DataType::Dictionary(..) => "ENUM".into(),
        other => other.to_string(),
    }
}

/// What a statement without a result set answers: DuckDB replies to them with one `Count`
/// column (rows changed; no rows for DDL) or one `Success` column.
enum Status {
    Count,
    Success,
}

/// Statements whose results are always rows, whatever their columns are called.
const QUERY_KEYWORDS: &[&str] = &[
    "select",
    "with",
    "from",
    "values",
    "table",
    "show",
    "describe",
    "desc",
    "summarize",
    "pragma",
    "explain",
    "call",
    "pivot",
    "unpivot",
    "(",
];

fn first_keyword(sql: &str) -> String {
    let mut rest = sql.trim_start();
    // Leading comments.
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    if rest.starts_with('(') {
        return "(".into();
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn status(sql: &str, schema: &Schema) -> Option<Status> {
    if QUERY_KEYWORDS.contains(&first_keyword(sql).as_str()) {
        return None;
    }
    match schema
        .fields()
        .iter()
        .map(|f| (f.name().as_str(), f.data_type()))
        .collect::<Vec<_>>()[..]
    {
        [("Count", DataType::Int64)] => Some(Status::Count),
        [("Success", DataType::Boolean)] => Some(Status::Success),
        _ => None,
    }
}

type Events = mpsc::Sender<Result<ExecEvent>>;

/// Sends an event; `false` if nobody is listening any more (the statement was abandoned).
fn send(events: &Events, event: ExecEvent) -> bool {
    events.blocking_send(Ok(event)).is_ok()
}

/// Runs one statement and sends its events: `Columns`, `Batch`*, `Done`.
pub fn execute(
    conn: &duckdb::Connection,
    sql: &str,
    params: &[String],
    batch_size: usize,
    events: &Events,
    work: &Work,
) -> Result<()> {
    let mut statement = conn.prepare(sql).map_err(map_error)?;
    // DuckDB clears an interrupt when a statement starts: a cancel that came meanwhile is
    // caught here, or by the interrupts `thread_cancel` keeps sending while it runs.
    work.check()?;
    // `$1`, `$2`…: every value bound as text, never written into the SQL (ADR-043).
    let mut stream = statement
        .stream_arrow(duckdb::params_from_iter(params.iter().map(String::as_str)))
        .map_err(map_error)?;
    let schema = stream.get_schema();
    work.check()?;

    if let Some(status) = status(sql, &schema) {
        let mut rows_affected = None;
        for batch in stream.by_ref() {
            let batch = batch.map_err(map_error)?;
            work.check()?;
            if matches!(status, Status::Count) && batch.num_rows() > 0 {
                let counts = batch.column(0).as_primitive::<Int64Type>();
                rows_affected = Some(u64::try_from(counts.value(0)).unwrap_or(0));
            }
        }
        send(events, ExecEvent::Done { rows_affected });
        return Ok(());
    }

    let conversions: Vec<Conversion> = schema
        .fields()
        .iter()
        .map(|f| conversion(f.data_type()))
        .collect();
    let columns = schema
        .fields()
        .iter()
        .zip(&conversions)
        .map(|(field, conversion)| ColumnMeta {
            name: field.name().clone(),
            logical: logical(*conversion),
            native_type: field_native_type(field),
            native_id: None,
        })
        .collect();
    let result = Arc::new(ResultSchema::new(columns));
    if !send(events, ExecEvent::Columns(result.clone())) {
        return Ok(());
    }

    // DuckDB hands out chunks of 2048 rows: regrouped into batches of exactly `batch_size`
    // (the last one smaller). `false` once nobody listens any more.
    let mut pending: Vec<RecordBatch> = Vec::new();
    let mut pending_rows = 0;
    let mut total_rows: u64 = 0;
    let flush = |pending: &mut Vec<RecordBatch>, all: bool| -> Result<bool> {
        if pending.is_empty() {
            return Ok(true);
        }
        let joined = concat_batches(&result.arrow, pending.iter())
            .map_err(|e| DriverError::Other(format!("could not join result batches: {e}")))?;
        pending.clear();
        let mut offset = 0;
        while joined.num_rows() - offset >= batch_size || (all && offset < joined.num_rows()) {
            let length = batch_size.min(joined.num_rows() - offset);
            if !send(events, ExecEvent::Batch(joined.slice(offset, length))) {
                return Ok(false);
            }
            offset += length;
        }
        if offset < joined.num_rows() {
            pending.push(joined.slice(offset, joined.num_rows() - offset));
        }
        Ok(true)
    };
    for batch in stream {
        let batch = batch.map_err(map_error)?;
        work.check()?;
        let arrays = batch
            .columns()
            .iter()
            .zip(&conversions)
            .map(|(array, conversion)| convert(array, *conversion))
            .collect::<Result<Vec<ArrayRef>>>()?;
        let converted = RecordBatch::try_new(result.arrow.clone(), arrays)
            .map_err(|e| DriverError::Other(format!("could not build a result batch: {e}")))?;
        pending_rows += converted.num_rows();
        total_rows += converted.num_rows() as u64;
        pending.push(converted);
        if pending_rows >= batch_size {
            if !flush(&mut pending, false)? {
                return Ok(());
            }
            pending_rows = pending.iter().map(RecordBatch::num_rows).sum();
        }
    }
    if flush(&mut pending, true)? {
        // Like the other drivers, a query reports the rows it returned.
        send(
            events,
            ExecEvent::Done {
                rows_affected: Some(total_rows),
            },
        );
    }
    Ok(())
}

fn convert(array: &ArrayRef, conversion: Conversion) -> Result<ArrayRef> {
    let failed = |e: arrow::error::ArrowError| {
        DriverError::Other(format!("could not convert a column: {e}"))
    };
    let rows = array.len();
    Ok(match conversion {
        Conversion::Bool => {
            let bools = array.as_any().downcast_ref::<BooleanArray>().cloned();
            match bools {
                Some(b) => Arc::new(b),
                None => cast(array, &DataType::Boolean).map_err(failed)?,
            }
        }
        Conversion::Int => cast(array, &DataType::Int64).map_err(failed)?,
        Conversion::Float => {
            let mut out = Float64Builder::with_capacity(rows);
            for i in 0..rows {
                if array.is_null(i) {
                    out.append_null();
                } else {
                    out.append_value(match array.data_type() {
                        // Through the shortest text of the f32: 0.1 stays 0.1.
                        DataType::Float32 => {
                            let x = array.as_primitive::<Float32Type>().value(i);
                            x.to_string().parse().unwrap_or(f64::from(x))
                        }
                        DataType::Float64 => array.as_primitive::<Float64Type>().value(i),
                        _ => cast(array, &DataType::Float64)
                            .map_err(failed)?
                            .as_primitive::<Float64Type>()
                            .value(i),
                    });
                }
            }
            Arc::new(out.finish())
        }
        Conversion::Bytes => {
            let mut out = BinaryBuilder::with_capacity(rows, 0);
            let binary = cast(array, &DataType::Binary).map_err(failed)?;
            let binary = binary.as_binary::<i32>();
            for i in 0..rows {
                if binary.is_null(i) {
                    out.append_null();
                } else {
                    out.append_value(binary.value(i));
                }
            }
            Arc::new(out.finish())
        }
        Conversion::Date => {
            let dates = cast(array, &DataType::Date32).map_err(failed)?;
            let dates = dates.as_any().downcast_ref::<Date32Array>().cloned();
            // DuckDB's -infinity is -i32::MAX; ours is i32::MIN.
            Arc::new(dates.map_or_else(
                || Date32Array::new_null(rows),
                |d| d.unary::<_, Date32Type>(|d| if d == NEG_INFINITY_DATE { i32::MIN } else { d }),
            ))
        }
        Conversion::Time => {
            let mut out = Time64MicrosecondBuilder::with_capacity(rows);
            for i in 0..rows {
                if array.is_null(i) {
                    out.append_null();
                } else if let DataType::Time64(TimeUnit::Nanosecond) = array.data_type() {
                    out.append_value(array.as_primitive::<Time64NanosecondType>().value(i) / 1_000);
                } else {
                    out.append_value(array.as_primitive::<Time64MicrosecondType>().value(i));
                }
            }
            Arc::new(out.finish())
        }
        Conversion::Timestamp | Conversion::TimestampTz => {
            let mut out = TimestampMicrosecondBuilder::with_capacity(rows);
            for i in 0..rows {
                if array.is_null(i) {
                    out.append_null();
                    continue;
                }
                let DataType::Timestamp(unit, _) = array.data_type() else {
                    out.append_null();
                    continue;
                };
                // DuckDB's ±infinity is ±i64::MAX in every unit; ours is i64::MAX/MIN.
                let micros = match unit {
                    TimeUnit::Second => {
                        keep_infinity(array.as_primitive::<TimestampSecondType>().value(i), |v| {
                            v.checked_mul(1_000_000)
                        })
                    }
                    TimeUnit::Millisecond => keep_infinity(
                        array.as_primitive::<TimestampMillisecondType>().value(i),
                        |v| v.checked_mul(1_000),
                    ),
                    TimeUnit::Microsecond => keep_infinity(
                        array.as_primitive::<TimestampMicrosecondType>().value(i),
                        Some,
                    ),
                    TimeUnit::Nanosecond => keep_infinity(
                        array.as_primitive::<TimestampNanosecondType>().value(i),
                        |v| Some(v.div_euclid(1_000)),
                    ),
                };
                out.append_value(micros);
            }
            let built = out.finish();
            if matches!(conversion, Conversion::TimestampTz) {
                Arc::new(built.with_timezone("UTC"))
            } else {
                Arc::new(built)
            }
        }
        Conversion::Utf8 => cast(array, &DataType::Utf8).map_err(failed)?,
        Conversion::Text(_) => {
            let mut out = StringBuilder::with_capacity(rows, rows * 8);
            for i in 0..rows {
                if array.is_null(i) {
                    out.append_null();
                } else if let DataType::UInt64 = array.data_type() {
                    out.append_value(array.as_primitive::<UInt64Type>().value(i).to_string());
                } else {
                    out.append_value(format::text(array.as_ref(), i));
                }
            }
            Arc::new(out.finish())
        }
    })
}

/// DuckDB's `-infinity`: `-i64::MAX` for timestamps, `-i32::MAX` for dates.
const NEG_INFINITY: i64 = -i64::MAX;
const NEG_INFINITY_DATE: i32 = -i32::MAX;

fn keep_infinity(value: i64, to_micros: impl Fn(i64) -> Option<i64>) -> i64 {
    match value {
        i64::MAX => i64::MAX,
        i64::MIN | NEG_INFINITY => i64::MIN,
        _ => to_micros(value).unwrap_or(if value > 0 { i64::MAX } else { i64::MIN }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_keyword_skips_comments() {
        assert_eq!(first_keyword("  -- note\n/* x */ INSERT into t"), "insert");
        assert_eq!(first_keyword("(select 1)"), "(");
        assert_eq!(
            first_keyword("WITH x AS (select 1) select * from x"),
            "with"
        );
    }
}
