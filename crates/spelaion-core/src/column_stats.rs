//! Statistics of a table column (the table data tab's column statistics popover): rows,
//! distinct values, NULLs, minimum, maximum and the most frequent values, counted by the
//! server. Unlike the calc panel (`selection_stats.rs`), they cover the table (with the tab's
//! filter), not the loaded page.
//!
//! What runs is written here and is only ever reads:
//! - the column and the table come from the catalog and are quoted for the engine; the tab's
//!   filter goes through the same check as a table data page (`table_data.rs`), so it cannot
//!   change the query, read another table or run anything else;
//! - each statement is checked as a single read again, and runs inside the engine's read-only
//!   transaction (the same barrier as streaming exports, ADR-034), in a session of its own
//!   that the caller closes afterwards;
//! - a big table is sampled: past `sample_rows` rows, the numbers describe the first
//!   `sample_rows` rows the server returns, and say so.
//!
//! Each number has its own statement, so one the engine refuses for the column's type
//! (PostgreSQL's `json` has no equality, SQL Server's `text` no order) leaves the others.

use futures_util::StreamExt;
use spelaion_driver_api::{Dialect, LogicalType, Value};

use crate::classify::{ReadKind, classify_read_only, reject_mysql_executable_comments};
use crate::export_stream::{ExportCancel, barrier, drain};
use crate::filter::FilterOp;
use crate::generate::qualified;
use crate::session::{CoreEvent, Session};
use crate::table_data::{PageError, check, push_clauses, value_condition};

/// Rows read before the numbers are sampled.
pub const STATS_SAMPLE_ROWS: u64 = 100_000;

/// Most frequent values listed.
pub const TOP_VALUES: u64 = 10;

/// Longest text returned for a value (longer ones end in "…").
pub const STATS_TEXT_CHARS: usize = 200;

/// Longest value a "filter by this value" condition is written for.
const MAX_CONDITION_CHARS: usize = 1_000;

/// One column of a table, with the table data tab's filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnStatsRequest {
    pub schema: String,
    pub table: String,
    pub column: String,
    /// The column's type, as the tab's page reports it.
    pub logical: LogicalType,
    /// What goes after `WHERE` in the tab; empty for every row.
    pub filter: String,
    /// Rows read before sampling ([`STATS_SAMPLE_ROWS`] in the app).
    pub sample_rows: u64,
    /// Most frequent values listed ([`TOP_VALUES`] in the app).
    pub top: u64,
}

/// The statements behind the numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnStatsSql {
    /// Counts up to `sample_rows + 1` rows: whether the table must be sampled.
    pub probe: String,
    /// `COUNT(*)` and `COUNT(column)`.
    pub counts: String,
    pub distinct: String,
    /// `MIN` and `MAX`, for types that have an order in every engine.
    pub range: Option<String>,
    pub top: String,
}

/// A number that may not exist for the column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stat<T> {
    Value(T),
    /// The column's type has no meaningful value here (booleans have no minimum).
    NotForType,
    /// The engine refused it; its message.
    Failed(String),
}

/// One of the most frequent values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopValue {
    /// Its text as the grid shows it, cut to [`STATS_TEXT_CHARS`]; `None` for NULL.
    pub text: Option<String>,
    pub count: u64,
    /// The condition that keeps the rows with this value, written like "filter by this
    /// value"; `None` when the value is too long to filter by.
    pub condition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnStats {
    /// Rows the numbers describe (`sample_rows` when sampled).
    pub rows: u64,
    /// The table (with the filter) has more rows than `sample_rows`.
    pub sampled: bool,
    pub nulls: u64,
    pub distinct: Stat<u64>,
    /// Minimum and maximum (each `None` when every value is NULL).
    pub range: Stat<(Option<String>, Option<String>)>,
    pub top: Stat<Vec<TopValue>>,
    /// What ran, in order: every statement a read.
    pub statements: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ColumnStatsError {
    #[error(transparent)]
    Invalid(#[from] PageError),
    #[error("{0}")]
    NotARead(String),
    #[error("{0}")]
    Failed(String),
    #[error("cancelled")]
    Cancelled,
}

/// Whether `MIN` and `MAX` mean something for the type in every engine.
fn has_order(logical: LogicalType) -> bool {
    match logical {
        LogicalType::Int
        | LogicalType::Float
        | LogicalType::Decimal
        | LogicalType::Text
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz => true,
        // Booleans and UUIDs have no MIN in PostgreSQL; the rest have no useful order.
        LogicalType::Bool
        | LogicalType::Uuid
        | LogicalType::Bytes
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => false,
    }
}

/// `SELECT column FROM table WHERE (filter)`, checked like a table data page, and the same
/// with the core's limit (`TOP` in SQL Server).
fn source(
    dialect: &dyn Dialect,
    request: &ColumnStatsRequest,
    limit: Option<u64>,
) -> Result<String, PageError> {
    let column = dialect.quote_identifier(&request.column);
    let mut from = format!(
        "\nFROM {}",
        qualified(dialect, &request.schema, &request.table)
    );
    push_clauses(&mut from, &request.filter, "");
    let plain = format!("SELECT {column}{from}");
    check(dialect, &plain, None, !request.filter.trim().is_empty())?;
    Ok(match limit {
        None => plain,
        Some(n) if dialect.name() == "sqlserver" => format!("SELECT TOP ({n}) {column}{from}"),
        Some(n) => format!("{plain}\nLIMIT {n}"),
    })
}

/// The statements for `request`, over the first `sample_rows` rows when `sampled`.
pub fn column_stats_sql(
    dialect: &dyn Dialect,
    request: &ColumnStatsRequest,
    sampled: bool,
) -> Result<ColumnStatsSql, PageError> {
    if request.sample_rows == 0 || request.top == 0 {
        return Err(PageError::BadLimit);
    }
    let column = dialect.quote_identifier(&request.column);
    let probe_source = source(dialect, request, Some(request.sample_rows + 1))?;
    let rows = source(dialect, request, sampled.then_some(request.sample_rows))?;
    let over = |select: &str| format!("SELECT {select}\nFROM (\n{rows}\n) AS s");
    let top = if dialect.name() == "sqlserver" {
        format!(
            "SELECT TOP ({}) {column}, COUNT(*)\nFROM (\n{rows}\n) AS s\nGROUP BY {column}\nORDER BY COUNT(*) DESC, {column}",
            request.top
        )
    } else {
        format!(
            "{}\nGROUP BY {column}\nORDER BY COUNT(*) DESC, {column}\nLIMIT {}",
            over(&format!("{column}, COUNT(*)")),
            request.top
        )
    };
    Ok(ColumnStatsSql {
        probe: format!("SELECT COUNT(*)\nFROM (\n{probe_source}\n) AS s"),
        counts: over(&format!("COUNT(*), COUNT({column})")),
        distinct: over(&format!("COUNT(DISTINCT {column})")),
        range: has_order(request.logical).then(|| over(&format!("MIN({column}), MAX({column})"))),
        top,
    })
}

/// `sql` if it is a single read (checked again even though the core wrote it).
fn checked(dialect: &dyn Dialect, sql: &str) -> Result<(), ColumnStatsError> {
    let engine = dialect.name();
    if dialect.split_statements(sql).len() != 1 {
        return Err(ColumnStatsError::NotARead(
            "column statistics run one statement at a time".into(),
        ));
    }
    reject_mysql_executable_comments(engine, sql).map_err(ColumnStatsError::NotARead)?;
    match classify_read_only(engine, sql).map_err(ColumnStatsError::NotARead)? {
        ReadKind::Query => Ok(()),
        ReadKind::Show | ReadKind::Explain => Err(ColumnStatsError::NotARead(
            "column statistics only run SELECT".into(),
        )),
    }
}

/// Every row of the first result of `sql` (a handful), read inside the engine's read-only
/// transaction. The result is freed, also on failure.
async fn read_rows(
    session: &Session,
    sql: &str,
    cancel: &ExportCancel,
) -> Result<Vec<Vec<Value>>, ColumnStatsError> {
    checked(session.dialect().as_ref(), sql)?;
    if cancel.is_cancelled() {
        return Err(ColumnStatsError::Cancelled);
    }
    let (begin, end) =
        barrier(session.dialect().name()).map_err(|e| ColumnStatsError::Failed(e.to_string()))?;
    drain(session, begin)
        .await
        .map_err(|e| ColumnStatsError::Failed(e.to_string()))?;
    let mut events = session.run_generated(sql);
    let mut results = Vec::new();
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result: id, .. } => results.push(id),
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    drop(events);
    if let Some(end) = end {
        // Read-only either way: nothing to keep or lose.
        let _ = drain(session, end).await;
    }
    let store = session.results();
    let rows = results.first().and_then(|id| {
        let stored = store.get(*id)?;
        let stored = stored.read().ok()?;
        let schema = stored.schema().clone();
        let mut rows = Vec::new();
        for batch in stored.window(0, stored.rows()) {
            for row in 0..batch.num_rows() {
                rows.push(
                    schema
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(i, meta)| {
                            Value::from_array(batch.column(i).as_ref(), row, meta.logical)
                                .unwrap_or(Value::Null)
                        })
                        .collect(),
                );
            }
        }
        Some(rows)
    });
    for id in results {
        store.remove(id);
    }
    if cancel.is_cancelled() {
        return Err(ColumnStatsError::Cancelled);
    }
    match failure {
        Some(message) => Err(ColumnStatsError::Failed(message)),
        None => {
            rows.ok_or_else(|| ColumnStatsError::Failed("the statement returned no rows".into()))
        }
    }
}

fn count(value: Option<&Value>) -> Result<u64, ColumnStatsError> {
    match value {
        Some(Value::Int(n)) => u64::try_from(*n).map_err(|_| unexpected(value)),
        Some(Value::Decimal(text) | Value::Text(text)) => {
            text.trim().parse().map_err(|_| unexpected(value))
        }
        _ => Err(unexpected(value)),
    }
}

fn unexpected(value: Option<&Value>) -> ColumnStatsError {
    ColumnStatsError::Failed(format!("unexpected count: {value:?}"))
}

/// `text` cut to [`STATS_TEXT_CHARS`] characters, with "…" when it was longer.
fn short(text: &str) -> String {
    match text.char_indices().nth(STATS_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn shown(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(v) => Some(short(&v.to_string())),
    }
}

/// A number the engine may refuse: its failure becomes [`Stat::Failed`]; a cancel stops all.
fn stat<T>(outcome: Result<T, ColumnStatsError>) -> Result<Stat<T>, ColumnStatsError> {
    match outcome {
        Ok(value) => Ok(Stat::Value(value)),
        Err(ColumnStatsError::Cancelled) => Err(ColumnStatsError::Cancelled),
        Err(e) => Ok(Stat::Failed(e.to_string())),
    }
}

/// Counts the column's statistics on `session` (a session of its own: each statement opens and
/// rolls back the engine's read-only transaction, and SQLite's switches to `query_only`).
pub async fn column_stats(
    session: &Session,
    request: &ColumnStatsRequest,
    cancel: &ExportCancel,
) -> Result<ColumnStats, ColumnStatsError> {
    let dialect = session.dialect().clone();
    let probe = column_stats_sql(dialect.as_ref(), request, false)?.probe;
    let mut statements = vec![probe.clone()];
    let seen = count(
        read_rows(session, &probe, cancel)
            .await?
            .first()
            .and_then(|r| r.first()),
    )?;
    let sampled = seen > request.sample_rows;
    let sql = column_stats_sql(dialect.as_ref(), request, sampled)?;

    statements.push(sql.counts.clone());
    let counts = read_rows(session, &sql.counts, cancel).await?;
    let first = counts.first();
    let rows = count(first.and_then(|r| r.first()))?;
    let values = count(first.and_then(|r| r.get(1)))?;

    statements.push(sql.distinct.clone());
    let distinct = stat(
        read_rows(session, &sql.distinct, cancel)
            .await
            .and_then(|r| count(r.first().and_then(|r| r.first()))),
    )?;

    let range = match &sql.range {
        None => Stat::NotForType,
        Some(range) => {
            statements.push(range.clone());
            stat(read_rows(session, range, cancel).await.map(|r| {
                let row = r.first();
                (
                    shown(row.and_then(|r| r.first())),
                    shown(row.and_then(|r| r.get(1))),
                )
            }))?
        }
    };

    statements.push(sql.top.clone());
    let top = stat(read_rows(session, &sql.top, cancel).await.and_then(|rows| {
        rows.iter()
            .map(|row| {
                let value = row.first().cloned().unwrap_or(Value::Null);
                let condition = match &value {
                    Value::Null => value_condition(
                        dialect.as_ref(),
                        &request.column,
                        request.logical,
                        FilterOp::IsNull,
                        &value,
                    )
                    .ok(),
                    v if v.to_string().chars().count() > MAX_CONDITION_CHARS => None,
                    v => value_condition(
                        dialect.as_ref(),
                        &request.column,
                        request.logical,
                        FilterOp::Eq,
                        v,
                    )
                    .ok(),
                };
                Ok(TopValue {
                    text: shown(Some(&value)),
                    count: count(row.get(1))?,
                    condition,
                })
            })
            .collect()
    }))?;

    Ok(ColumnStats {
        rows,
        sampled,
        nulls: rows.saturating_sub(values),
        distinct,
        range,
        top,
        statements,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real dialect of each engine (its splitter and quoting).
    fn d(engine: &str) -> Box<dyn Dialect> {
        match engine {
            "postgres" => Box::new(spelaion_driver_postgres::PostgresDialect),
            "mysql" => Box::new(spelaion_driver_mysql::MysqlDialect),
            "sqlserver" => Box::new(spelaion_driver_sqlserver::SqlServerDialect),
            "duckdb" => Box::new(spelaion_driver_duckdb::DuckdbDialect),
            _ => Box::new(spelaion_driver_sqlite::SqliteDialect),
        }
    }

    fn request(filter: &str, logical: LogicalType) -> ColumnStatsRequest {
        ColumnStatsRequest {
            schema: "public".into(),
            table: "orders".into(),
            column: "total".into(),
            logical,
            filter: filter.into(),
            sample_rows: 100,
            top: 5,
        }
    }

    #[test]
    fn statements_read_the_column_with_the_filter() {
        let sql = column_stats_sql(
            d("postgres").as_ref(),
            &request("id > 3", LogicalType::Int),
            false,
        )
        .unwrap();
        let rows = "SELECT total\nFROM public.orders\nWHERE (\nid > 3\n)";
        assert_eq!(
            sql.probe,
            format!("SELECT COUNT(*)\nFROM (\n{rows}\nLIMIT 101\n) AS s")
        );
        assert_eq!(
            sql.counts,
            format!("SELECT COUNT(*), COUNT(total)\nFROM (\n{rows}\n) AS s")
        );
        assert_eq!(
            sql.distinct,
            format!("SELECT COUNT(DISTINCT total)\nFROM (\n{rows}\n) AS s")
        );
        assert_eq!(
            sql.range.as_deref(),
            Some(format!("SELECT MIN(total), MAX(total)\nFROM (\n{rows}\n) AS s").as_str())
        );
        assert_eq!(
            sql.top,
            format!(
                "SELECT total, COUNT(*)\nFROM (\n{rows}\n) AS s\nGROUP BY total\nORDER BY COUNT(*) DESC, total\nLIMIT 5"
            )
        );
        for statement in [&sql.probe, &sql.counts, &sql.distinct, &sql.top] {
            assert_eq!(
                classify_read_only("postgres", statement),
                Ok(ReadKind::Query)
            );
        }
    }

    #[test]
    fn sampled_statements_read_the_first_rows_and_sql_server_uses_top() {
        let sql = column_stats_sql(
            d("sqlserver").as_ref(),
            &request("", LogicalType::Text),
            true,
        )
        .unwrap();
        let rows = "SELECT TOP (100) total\nFROM [public].orders";
        assert_eq!(
            sql.counts,
            format!("SELECT COUNT(*), COUNT(total)\nFROM (\n{rows}\n) AS s")
        );
        assert!(sql.probe.contains("SELECT TOP (101) total"));
        assert!(sql.top.starts_with("SELECT TOP (5) total, COUNT(*)"));
        assert!(!sql.top.contains("LIMIT"));
        let sql =
            column_stats_sql(d("mysql").as_ref(), &request("", LogicalType::Text), true).unwrap();
        assert!(
            sql.counts.ends_with("`orders`\nLIMIT 100\n) AS s"),
            "{}",
            sql.counts
        );
    }

    #[test]
    fn types_without_an_order_have_no_range() {
        for logical in [LogicalType::Bool, LogicalType::Json, LogicalType::Uuid] {
            let sql =
                column_stats_sql(d("postgres").as_ref(), &request("", logical), false).unwrap();
            assert_eq!(sql.range, None);
        }
    }

    /// The filter is the user's text: it must not turn the statements into something else.
    #[test]
    fn a_filter_cannot_change_the_statements() {
        for filter in [
            "1 = 1) UNION SELECT password FROM users --",
            "1 = 1; DROP TABLE orders",
            "1 = 1)\nLIMIT 1000000000 --",
            "1 = 1) GROUP BY total HAVING (1 = 1",
            "id IN (SELECT id FROM orders) ) , other_table WHERE (1=1",
            "(1 = 1)) AS s; DELETE FROM orders; SELECT * FROM (SELECT 1",
        ] {
            for engine in ["postgres", "mysql", "sqlserver", "sqlite", "duckdb"] {
                assert!(
                    column_stats_sql(
                        d(engine).as_ref(),
                        &request(filter, LogicalType::Int),
                        false
                    )
                    .is_err(),
                    "{engine}: {filter}"
                );
            }
        }
        // A plain condition is fine, also with subqueries that only read.
        assert!(
            column_stats_sql(
                d("postgres").as_ref(),
                &request("total > (SELECT 1)", LogicalType::Int),
                false
            )
            .is_ok()
        );
    }

    #[test]
    fn names_are_quoted() {
        let mut r = request("", LogicalType::Int);
        r.column = "a\"b".into();
        let sql = column_stats_sql(d("postgres").as_ref(), &r, false).unwrap();
        assert!(sql.counts.contains("COUNT(\"a\"\"b\")"), "{}", sql.counts);
        r.column = "x] FROM t; --".into();
        let sql = column_stats_sql(d("sqlserver").as_ref(), &r, false).unwrap();
        assert!(sql.counts.contains("COUNT([x]] FROM t; --])"));
    }

    #[test]
    fn zero_rows_or_values_are_refused() {
        let mut r = request("", LogicalType::Int);
        r.sample_rows = 0;
        assert_eq!(
            column_stats_sql(d("postgres").as_ref(), &r, false),
            Err(PageError::BadLimit)
        );
    }

    #[test]
    fn only_selects_pass_the_second_check() {
        let sqlite = spelaion_driver_sqlite::SqliteDialect;
        assert!(checked(&sqlite, "SELECT 1").is_ok());
        assert!(checked(&sqlite, "SELECT 1; SELECT 2").is_err());
        assert!(checked(&sqlite, "DELETE FROM t").is_err());
        assert!(checked(d("mysql").as_ref(), "SELECT /*! 1 */ 2").is_err());
    }
}
