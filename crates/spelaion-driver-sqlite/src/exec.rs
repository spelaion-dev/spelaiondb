//! Statement execution through rusqlite. Columns are typed and built by
//! `spelaion-sqlite-schema` (ADR-006, ADR-058), from the declared types and the first batch.

use rusqlite::types::ValueRef;
use rusqlite::{ErrorCode, Statement};
use spelaion_driver_api::thread_cancel::Work;
use spelaion_driver_api::{ColumnOrigin, DriverError, ExecEvent, Result};
use spelaion_sqlite_schema::typing::{is_dml, locate_name};
use spelaion_sqlite_schema::{ResultBuilder, ResultColumn, SqlValue};
use tokio::sync::mpsc;

pub fn map_error(e: rusqlite::Error) -> DriverError {
    match e {
        rusqlite::Error::SqliteFailure(err, message) => {
            if err.code == ErrorCode::OperationInterrupted {
                return DriverError::Cancelled;
            }
            DriverError::Database {
                code: Some(err.extended_code.to_string()),
                message: message.unwrap_or_else(|| err.to_string()),
                detail: None,
                hint: None,
                position: None,
            }
        }
        // Syntax errors: they carry the position (in bytes) within the statement.
        rusqlite::Error::SqlInputError {
            error,
            msg,
            sql,
            offset,
        } => DriverError::Database {
            code: Some(error.extended_code.to_string()),
            message: msg,
            detail: None,
            hint: None,
            position: usize::try_from(offset)
                .ok()
                .and_then(|o| sql.get(..o))
                .map(|prefix| prefix.chars().count() as u32 + 1),
        },
        rusqlite::Error::MultipleStatement => {
            DriverError::Other("expected a single statement (the core splits scripts)".into())
        }
        other => DriverError::Other(other.to_string()),
    }
}

/// A value read by rusqlite, owned (TEXT that is not UTF-8 is kept as a BLOB).
pub(crate) fn owned(v: ValueRef<'_>) -> SqlValue {
    match v {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(i) => SqlValue::Integer(i),
        ValueRef::Real(x) => SqlValue::Real(x),
        ValueRef::Text(t) => SqlValue::text_from_bytes(t),
        ValueRef::Blob(b) => SqlValue::Blob(b.to_vec()),
    }
}

/// Adds one cell to the result, straight from rusqlite's buffer.
fn append(result: &mut ResultBuilder, column: usize, v: ValueRef<'_>) -> Result<()> {
    match v {
        ValueRef::Null => result.append_null(column),
        ValueRef::Integer(i) => result.append_integer(column, i),
        ValueRef::Real(x) => result.append_real(column, x),
        ValueRef::Text(t) => result.append_text(column, t),
        ValueRef::Blob(b) => result.append_blob(column, b),
    }
}

fn send(events: &mpsc::Sender<Result<ExecEvent>>, event: ExecEvent) -> Result<()> {
    // If the receiver is gone (tab closed, stream dropped), stop reading.
    events
        .blocking_send(Ok(event))
        .map_err(|_| DriverError::Cancelled)
}

pub fn execute(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[String],
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
    work: &Work,
) -> Result<()> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| locate_name(map_error(e), sql))?;
    // An interrupt while preparing lands on no running statement: a cancel that came meanwhile
    // is caught here, or by the interrupts `thread_cancel` keeps sending while it runs.
    work.check()?;
    // `?1`, `?2`…: every value bound as text, never written into the SQL (ADR-043).
    for (i, param) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, param.as_str())
            .map_err(map_error)?;
    }
    if stmt.column_count() == 0 {
        let readonly = stmt.readonly();
        stmt.raw_execute().map_err(map_error)?;
        // Only INSERT/UPDATE/DELETE have a count; changes() repeats the last one for the rest.
        let counts = !readonly && is_dml(sql);
        return send(
            events,
            ExecEvent::Done {
                rows_affected: counts.then(|| conn.changes()),
            },
        );
    }
    match stream_rows(&mut stmt, batch_size, events) {
        Err(DriverError::Cancelled) if events.is_closed() => Ok(()),
        other => other,
    }
}

fn stream_rows(
    stmt: &mut Statement<'_>,
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> Result<()> {
    let names: Vec<String> = stmt.column_names().map(str::to_string).collect();
    let declared: Vec<Option<String>> = stmt
        .columns()
        .map(|c| c.decl_type().map(str::to_string))
        .collect();
    let ncols = names.len();
    // The table column each one reads (ADR-052), from SQLite's column metadata; an expression
    // has none. Names that are not UTF-8 give no origin rather than a panic.
    let origins: Vec<Option<ColumnOrigin>> = (0..ncols)
        .map(|i| match stmt.column_metadata(i) {
            Ok(Some((_, table, column, ..))) => match (table.to_str(), column.to_str()) {
                (Ok(table), Ok(column)) => Some(ColumnOrigin {
                    table: table.to_string(),
                    column: column.to_string(),
                }),
                _ => None,
            },
            Ok(None) | Err(_) => None,
        })
        .collect();
    let columns: Vec<ResultColumn> = names
        .into_iter()
        .zip(declared)
        .zip(origins)
        .map(|((name, declared), origin)| ResultColumn {
            name,
            declared,
            origin,
        })
        .collect();
    let mut rows = stmt.raw_query();

    // First batch in memory to decide the types (ADR-006).
    let mut first: Vec<Vec<SqlValue>> = Vec::with_capacity(batch_size.min(4096));
    while first.len() < batch_size {
        let Some(row) = rows.next().map_err(map_error)? else {
            break;
        };
        first.push(
            (0..ncols)
                .map(|i| row.get_ref(i).map(owned))
                .collect::<rusqlite::Result<_>>()
                .map_err(map_error)?,
        );
    }
    let mut result = ResultBuilder::new(columns, &first)?;
    drop(first);
    send(events, ExecEvent::Columns(result.schema()))?;
    if result.pending() == batch_size {
        send(events, ExecEvent::Batch(result.take_batch()?))?;
    }

    while let Some(row) = rows.next().map_err(map_error)? {
        result.begin_row();
        for i in 0..ncols {
            append(&mut result, i, row.get_ref(i).map_err(map_error)?)?;
        }
        result.end_row();
        if result.pending() == batch_size {
            send(events, ExecEvent::Batch(result.take_batch()?))?;
        }
    }
    if result.pending() > 0 {
        send(events, ExecEvent::Batch(result.take_batch()?))?;
    }
    send(
        events,
        ExecEvent::Done {
            rows_affected: Some(result.rows()),
        },
    )
}
