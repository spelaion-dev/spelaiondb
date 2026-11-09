//! Data search (ADR-043): where does a value appear in the tables the user chose?
//!
//! Each table is read once, with one statement the core writes from the catalog and from the
//! columns' types; the value the user typed is **bound as a parameter** (`$1`, `@P1`, `?1` or
//! MySQL's `@spelaion_p1`, see [`bound_placeholder`]), never written into the SQL text. Like
//! column statistics and streaming exports (ADR-034):
//! - every statement is checked again as a single read, and runs inside the engine's read-only
//!   transaction, in a session of its own that the caller opens and closes, so the search also
//!   works on read-only connections and can never change data;
//! - it is always bounded: the tables are the user's choice (at most [`MAX_SEARCH_TABLES`]),
//!   each table returns at most `rows_per_table` rows, and the whole search stops at
//!   [`MAX_SEARCH_HITS`] hits;
//! - it can be cancelled at any point: [`ExportCancel::cancel`] stops the running statement and
//!   no further table is read.
//!
//! Which columns are searched follows the options: text (and UUID) columns always, by a
//! substring match (case sensitive or not); numeric columns only when asked and when the value
//! is a number, by equality; large text (JSON, XML, MySQL's `TEXT`, SQL Server's `text`) and
//! binary columns only when asked, binary ones by the value's bytes. Other types (dates,
//! booleans, intervals, arrays, the engine's own types) are not searched.

use futures_util::StreamExt;
use spelaion_driver_api::{
    ColumnMeta, DriverError, ExecEvent, ExecOptions, LogicalType, Value, bound_placeholder,
};

use crate::classify::{ReadKind, classify_read_only, reject_mysql_executable_comments};
use crate::export_stream::{ExportCancel, barrier, drain};
use crate::filter::FilterOp;
use crate::generate::qualified;
use crate::session::Session;
use crate::table_data::{key_filter, value_condition};

/// Most tables one search reads.
pub const MAX_SEARCH_TABLES: usize = 1_000;
/// Most rows a table may return (the user chooses up to this).
pub const MAX_ROWS_PER_TABLE: u32 = 1_000;
/// Rows a table returns unless the user chooses otherwise.
pub const DEFAULT_ROWS_PER_TABLE: u32 = 100;
/// The search stops after this many hits in all.
pub const MAX_SEARCH_HITS: usize = 1_000;
/// Longest value searched for.
pub const MAX_SEARCH_VALUE_CHARS: usize = 1_000;
/// Characters of a column's text shown around a match.
pub const SNIPPET_CHARS: usize = 120;
/// Characters shown before the match in a snippet.
const SNIPPET_BEFORE: usize = 40;
/// Longest value a "no primary key" hit is filtered by (as "filter by this value" does).
const MAX_CONDITION_CHARS: usize = 1_000;

/// A table or view to search.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchTable {
    pub schema: String,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSearchOptions {
    /// Text matches only with the same case.
    pub case_sensitive: bool,
    /// Numeric columns too, by equality, when the value is a number.
    pub numbers: bool,
    /// Large text (JSON, XML, `TEXT`…) and binary columns too.
    pub large: bool,
    /// Rows a table returns at most (1 to [`MAX_ROWS_PER_TABLE`]).
    pub rows_per_table: u32,
}

impl Default for DataSearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            numbers: false,
            large: false,
            rows_per_table: DEFAULT_ROWS_PER_TABLE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSearchRequest {
    pub value: String,
    pub tables: Vec<SearchTable>,
    pub options: DataSearchOptions,
}

/// How a column is searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchColumnKind {
    /// A substring of its text.
    Text,
    /// A substring of its text, only with [`DataSearchOptions::large`].
    LargeText,
    /// Equal to the value, only with [`DataSearchOptions::numbers`] and a numeric value.
    Number,
    /// Contains the value's bytes, only with [`DataSearchOptions::large`].
    Binary,
}

/// A column a table's statement searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchColumn {
    pub name: String,
    pub logical: LogicalType,
    pub kind: SearchColumnKind,
}

/// The statement that searches one table, and how to read its rows: the key columns first,
/// then for each searched column a match flag, its snippet and (without a primary key, for
/// text and numbers) its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSearchSql {
    pub sql: String,
    /// Bound to the placeholders, in order: only ever the value.
    pub params: Vec<String>,
    /// The primary key's columns and types (empty when the table has none).
    pub key: Vec<(String, LogicalType)>,
    pub columns: Vec<SearchColumn>,
}

/// One searched column of a hit that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMatch {
    pub column: String,
    /// Up to [`SNIPPET_CHARS`] of its text around the match; `None` for binary columns.
    pub text: Option<String>,
}

/// A row that contains the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The primary key's columns and values as text (empty without a primary key).
    pub key: Vec<(String, String)>,
    pub matches: Vec<ColumnMatch>,
    /// What goes after `WHERE` in the table data tab that shows this row: its primary key or,
    /// without one, the value of a matching column. `None` when neither can be written.
    pub filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableStatus {
    Searched {
        /// Columns searched.
        columns: u32,
        hits: Vec<SearchHit>,
        /// The table returned `rows_per_table` rows: there may be more.
        more: bool,
    },
    /// No column of the table is searched with these options.
    NoColumns,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableOutcome {
    pub schema: String,
    pub table: String,
    pub status: TableStatus,
}

/// What a search tells while it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataSearchEvent {
    /// Table `index` (from 0) of `total` is being searched.
    Started {
        index: usize,
        total: usize,
        schema: String,
        table: String,
    },
    Finished(TableOutcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSearchSummary {
    /// Tables read to the end (searched, without columns or failed).
    pub tables: usize,
    pub hits: usize,
    pub cancelled: bool,
    /// The search stopped at [`MAX_SEARCH_HITS`] hits.
    pub stopped: bool,
    /// What ran, in order: every statement a read.
    pub statements: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DataSearchError {
    #[error("type a value to search for")]
    EmptyValue,
    #[error("the value is longer than {MAX_SEARCH_VALUE_CHARS} characters")]
    LongValue,
    #[error("the value cannot contain a NUL character")]
    NulInValue,
    #[error("choose at least one table")]
    NoTables,
    #[error("choose at most {MAX_SEARCH_TABLES} tables")]
    TooManyTables,
    #[error("a table returns from 1 to {MAX_ROWS_PER_TABLE} rows")]
    BadLimit,
    #[error("this connection cannot search data: it cannot bind parameters")]
    Unsupported,
    #[error("{0}")]
    NotARead(String),
    #[error("{0}")]
    Failed(String),
}

impl DataSearchRequest {
    /// The request's own limits, checked before anything connects.
    pub fn check(&self) -> Result<(), DataSearchError> {
        check_value(&self.value)?;
        if self.tables.is_empty() {
            return Err(DataSearchError::NoTables);
        }
        if self.tables.len() > MAX_SEARCH_TABLES {
            return Err(DataSearchError::TooManyTables);
        }
        if self.options.rows_per_table == 0 || self.options.rows_per_table > MAX_ROWS_PER_TABLE {
            return Err(DataSearchError::BadLimit);
        }
        Ok(())
    }
}

fn check_value(value: &str) -> Result<(), DataSearchError> {
    if value.trim().is_empty() {
        return Err(DataSearchError::EmptyValue);
    }
    if value.chars().count() > MAX_SEARCH_VALUE_CHARS {
        return Err(DataSearchError::LongValue);
    }
    if value.contains('\0') {
        return Err(DataSearchError::NulInValue);
    }
    Ok(())
}

/// A plain decimal every engine reads exactly: an optional sign, up to 28 digits and up to 10
/// decimals (it fits `DECIMAL(38, 10)`).
pub fn is_search_number(value: &str) -> bool {
    let digits = value.trim();
    let digits = digits.strip_prefix(['+', '-']).unwrap_or(digits);
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    !whole.is_empty()
        && whole.len() <= 28
        && all_digits(whole)
        && fraction.is_none_or(|f| !f.is_empty() && f.len() <= 10 && all_digits(f))
}

/// Native types of large text, per engine (besides JSON, which is large everywhere).
fn is_large_text(engine: &str, native: &str) -> bool {
    let native = native.to_ascii_lowercase();
    match engine {
        "postgres" => native == "xml",
        "mysql" => native == "text",
        "sqlserver" => matches!(native.as_str(), "text" | "ntext" | "xml"),
        _ => false,
    }
}

/// How a SQLite column read as text is searched, by its declared type and SQLite's affinity
/// rules: text affinity (and no declared type) as text, JSON as large text, other NUMERIC
/// affinity as a number unless it holds dates, times or booleans, which are not searched.
fn sqlite_declared(declared: &str) -> Option<SearchColumnKind> {
    let d = declared.to_ascii_uppercase();
    // No declared type (a view's expression): the probe names its storage class, `NULL`.
    if d.is_empty() || d == "NULL" || ["CHAR", "CLOB", "TEXT"].iter().any(|k| d.contains(k)) {
        Some(SearchColumnKind::Text)
    } else if d.contains("JSON") {
        Some(SearchColumnKind::LargeText)
    } else if ["DATE", "TIME", "BOOL"].iter().any(|k| d.contains(k)) {
        None
    } else {
        Some(SearchColumnKind::Number)
    }
}

/// How `column` is searched with `options`, or `None` when it is not.
pub fn column_kind(
    engine: &str,
    column: &ColumnMeta,
    options: &DataSearchOptions,
    value_is_number: bool,
) -> Option<SearchColumnKind> {
    let kind = match column.logical {
        // SQLite types a column by its values, and an empty probe has none: a declared type of
        // NUMERIC affinity (DECIMAL, DATE, BOOLEAN…) reads as text there, so it decides.
        LogicalType::Text if engine == "sqlite" => sqlite_declared(&column.native_type)?,
        LogicalType::Text if is_large_text(engine, &column.native_type) => {
            SearchColumnKind::LargeText
        }
        LogicalType::Text | LogicalType::Uuid => SearchColumnKind::Text,
        LogicalType::Json => SearchColumnKind::LargeText,
        LogicalType::Int | LogicalType::Float | LogicalType::Decimal => SearchColumnKind::Number,
        LogicalType::Bytes => SearchColumnKind::Binary,
        LogicalType::Bool
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => return None,
    };
    let wanted = match kind {
        SearchColumnKind::Text => true,
        SearchColumnKind::LargeText | SearchColumnKind::Binary => options.large,
        SearchColumnKind::Number => options.numbers && value_is_number,
    };
    wanted.then_some(kind)
}

/// The column as text, for the engine.
fn as_text(engine: &str, column: &str) -> String {
    match engine {
        "postgres" => format!("CAST({column} AS text)"),
        "mysql" => format!("CONVERT(CAST({column} AS CHAR) USING utf8mb4)"),
        "sqlserver" => format!("CAST({column} AS NVARCHAR(MAX))"),
        "duckdb" => format!("CAST({column} AS VARCHAR)"),
        _ => format!("CAST({column} AS TEXT)"),
    }
}

/// Where the value starts in the column's text (from 1; 0 when it is not there).
fn position(engine: &str, text: &str, value: &str, case_sensitive: bool) -> String {
    match (engine, case_sensitive) {
        ("postgres" | "duckdb", true) => format!("strpos({text}, {value})"),
        ("postgres" | "duckdb", false) => format!("strpos(lower({text}), lower({value}))"),
        ("mysql", true) => format!(
            "LOCATE(CONVERT({value} USING utf8mb4) COLLATE utf8mb4_bin, {text} COLLATE utf8mb4_bin)"
        ),
        ("mysql", false) => format!(
            "LOCATE(LOWER(CONVERT({value} USING utf8mb4)) COLLATE utf8mb4_bin, LOWER({text}) COLLATE utf8mb4_bin)"
        ),
        ("sqlserver", true) => {
            format!("CHARINDEX({value}, {text} COLLATE Latin1_General_100_CS_AS)")
        }
        ("sqlserver", false) => {
            format!("CHARINDEX({value}, {text} COLLATE Latin1_General_100_CI_AS)")
        }
        (_, true) => format!("instr({text}, {value})"),
        (_, false) => format!("instr(lower({text}), lower({value}))"),
    }
}

/// Up to [`SNIPPET_CHARS`] of `text` from a little before `position`.
fn snippet(engine: &str, text: &str, position: &str) -> String {
    let start = match engine {
        "sqlserver" => {
            format!("IIF({position} > {SNIPPET_BEFORE}, {position} - {SNIPPET_BEFORE}, 1)")
        }
        "sqlite" => format!("max({position} - {SNIPPET_BEFORE}, 1)"),
        _ => format!("GREATEST({position} - {SNIPPET_BEFORE}, 1)"),
    };
    match engine {
        "mysql" | "sqlserver" => format!("SUBSTRING({text}, {start}, {SNIPPET_CHARS})"),
        _ => format!("substr({text}, {start}, {SNIPPET_CHARS})"),
    }
}

/// The value as a number every engine compares exactly.
fn number(engine: &str, value: &str) -> String {
    match engine {
        "postgres" => format!("CAST({value} AS numeric)"),
        "mysql" => format!("CAST({value} AS DECIMAL(38, 10))"),
        "sqlserver" | "duckdb" => format!("TRY_CAST({value} AS DECIMAL(38, 10))"),
        _ => format!("CAST({value} AS NUMERIC)"),
    }
}

/// The column contains the value's bytes (UTF-8; SQL Server's code page).
fn contains_bytes(engine: &str, column: &str, value: &str) -> String {
    match engine {
        "postgres" => format!("position(convert_to({value}, 'UTF8') in {column}) > 0"),
        "mysql" => format!("LOCATE(CAST({value} AS BINARY), {column}) > 0"),
        "sqlserver" => format!(
            "CHARINDEX(CAST({value} AS VARCHAR(MAX)) COLLATE Latin1_General_100_BIN2, CAST(CAST({column} AS VARBINARY(MAX)) AS VARCHAR(MAX)) COLLATE Latin1_General_100_BIN2) > 0"
        ),
        "duckdb" => format!("contains(CAST({column} AS VARCHAR), {value})"),
        _ => format!("instr({column}, CAST({value} AS BLOB)) > 0"),
    }
}

/// The statement that searches `schema.table`, whose columns are `columns` (as a `SELECT *` of
/// the table reports them) and whose primary key is `primary_key`; `None` when no column is
/// searched with these options.
pub fn table_search_sql(
    dialect: &dyn spelaion_driver_api::Dialect,
    table: &SearchTable,
    columns: &[ColumnMeta],
    primary_key: &[String],
    value: &str,
    options: &DataSearchOptions,
) -> Result<Option<TableSearchSql>, DataSearchError> {
    check_value(value)?;
    if options.rows_per_table == 0 || options.rows_per_table > MAX_ROWS_PER_TABLE {
        return Err(DataSearchError::BadLimit);
    }
    let engine = dialect.name();
    let is_number = is_search_number(value);
    let searched: Vec<SearchColumn> = columns
        .iter()
        .filter_map(|c| {
            column_kind(engine, c, options, is_number).map(|kind| SearchColumn {
                name: c.name.clone(),
                logical: c.logical,
                kind,
            })
        })
        .collect();
    if searched.is_empty() {
        return Ok(None);
    }
    // Every key column must be in the table as reported; otherwise no key is used.
    let key: Vec<(String, LogicalType)> = primary_key
        .iter()
        .map(|name| {
            columns
                .iter()
                .find(|c| &c.name == name)
                .map(|c| (name.clone(), c.logical))
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    let placeholder = bound_placeholder(engine, 1);
    let mut select = Vec::new();
    for (i, (name, _)) in key.iter().enumerate() {
        select.push(format!(
            "{} AS spelaion_k{}",
            dialect.quote_identifier(name),
            i + 1
        ));
    }
    let mut conditions = Vec::new();
    for (i, column) in searched.iter().enumerate() {
        let n = i + 1;
        let quoted = dialect.quote_identifier(&column.name);
        let (condition, shown) = match column.kind {
            SearchColumnKind::Text | SearchColumnKind::LargeText => {
                let text = as_text(engine, &quoted);
                let at = position(engine, &text, &placeholder, options.case_sensitive);
                (format!("{at} > 0"), snippet(engine, &text, &at))
            }
            SearchColumnKind::Number => (
                format!("{quoted} = {}", number(engine, &placeholder)),
                as_text(engine, &quoted),
            ),
            SearchColumnKind::Binary => {
                (contains_bytes(engine, &quoted, &placeholder), "NULL".into())
            }
        };
        select.push(format!(
            "CASE WHEN {condition} THEN 1 ELSE 0 END AS spelaion_m{n}"
        ));
        select.push(format!("{shown} AS spelaion_s{n}"));
        if key.is_empty() {
            let raw = match column.kind {
                SearchColumnKind::Text | SearchColumnKind::Number => quoted.clone(),
                SearchColumnKind::LargeText | SearchColumnKind::Binary => "NULL".into(),
            };
            select.push(format!("{raw} AS spelaion_r{n}"));
        }
        conditions.push(condition);
    }
    let limit = options.rows_per_table;
    let top = if engine == "sqlserver" {
        format!("TOP ({limit}) ")
    } else {
        String::new()
    };
    let mut sql = format!(
        "SELECT {top}{}\nFROM {}\nWHERE {}",
        select.join(",\n  "),
        qualified(dialect, &table.schema, &table.table),
        conditions.join("\n   OR ")
    );
    if engine != "sqlserver" {
        sql.push_str(&format!("\nLIMIT {limit}"));
    }
    Ok(Some(TableSearchSql {
        sql,
        params: vec![value.to_string()],
        key,
        columns: searched,
    }))
}

/// `SELECT * FROM table WHERE 1 = 0`: the table's columns and their types, without rows.
pub fn probe_sql(dialect: &dyn spelaion_driver_api::Dialect, table: &SearchTable) -> String {
    format!(
        "SELECT *\nFROM {}\nWHERE 1 = 0",
        qualified(dialect, &table.schema, &table.table)
    )
}

/// `sql` if it is a single read (checked again even though the core wrote it).
fn checked(dialect: &dyn spelaion_driver_api::Dialect, sql: &str) -> Result<(), DataSearchError> {
    let engine = dialect.name();
    if dialect.split_statements(sql).len() != 1 {
        return Err(DataSearchError::NotARead(
            "a data search runs one statement at a time".into(),
        ));
    }
    reject_mysql_executable_comments(engine, sql).map_err(DataSearchError::NotARead)?;
    match classify_read_only(engine, sql).map_err(DataSearchError::NotARead)? {
        ReadKind::Query => Ok(()),
        ReadKind::Show | ReadKind::Explain => Err(DataSearchError::NotARead(
            "a data search only runs SELECT".into(),
        )),
    }
}

/// What one statement returned.
struct Read {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Value>>,
}

/// Runs `sql` (with `params` bound when there are any) and reads every row: a handful, the
/// statement's own limit.
async fn read(session: &Session, sql: &str, params: Vec<String>) -> Result<Read, DriverError> {
    let options = ExecOptions::default();
    let mut events = if params.is_empty() {
        session.connection().execute(sql, &options).await?
    } else {
        session
            .connection()
            .execute_bound(sql, params, &options)
            .await?
    };
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    while let Some(event) = events.next().await {
        match event? {
            ExecEvent::Columns(schema) => columns = schema.columns.clone(),
            ExecEvent::Batch(batch) => {
                for row in 0..batch.num_rows() {
                    rows.push(
                        columns
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
            ExecEvent::Notice(_) | ExecEvent::Done { .. } => {}
        }
    }
    Ok(Read { columns, rows })
}

fn cut(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

fn shown(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        v => Some(cut(&v.to_string(), SNIPPET_CHARS)),
    }
}

/// The hits of a table's rows, read as [`TableSearchSql`] lays them out.
fn hits(
    dialect: &dyn spelaion_driver_api::Dialect,
    search: &TableSearchSql,
    rows: &[Vec<Value>],
) -> Vec<SearchHit> {
    let keys = search.key.len();
    let per_column = if keys == 0 { 3 } else { 2 };
    rows.iter()
        .map(|row| {
            let at = |i: usize| row.get(i).cloned().unwrap_or(Value::Null);
            let key_values: Vec<(String, LogicalType, Value)> = search
                .key
                .iter()
                .enumerate()
                .map(|(i, (name, logical))| (name.clone(), *logical, at(i)))
                .collect();
            let mut matches = Vec::new();
            let mut fallback = None;
            for (i, column) in search.columns.iter().enumerate() {
                let base = keys + i * per_column;
                let matched = matches!(at(base), Value::Int(1))
                    || matches!(at(base), Value::Decimal(ref d) if d.trim() == "1");
                if !matched {
                    continue;
                }
                matches.push(ColumnMatch {
                    column: column.name.clone(),
                    text: match column.kind {
                        SearchColumnKind::Binary => None,
                        SearchColumnKind::Text
                        | SearchColumnKind::LargeText
                        | SearchColumnKind::Number => shown(&at(base + 1)),
                    },
                });
                if keys == 0 && fallback.is_none() {
                    let raw = at(base + 2);
                    let fits = !matches!(raw, Value::Null)
                        && raw.to_string().chars().count() <= MAX_CONDITION_CHARS;
                    if fits {
                        fallback = value_condition(
                            dialect,
                            &column.name,
                            column.logical,
                            FilterOp::Eq,
                            &raw,
                        )
                        .ok();
                    }
                }
            }
            let filter = if keys > 0 {
                key_filter(dialect, &key_values).ok()
            } else {
                fallback
            };
            SearchHit {
                key: key_values
                    .into_iter()
                    .map(|(name, _, value)| (name, value.to_string()))
                    .collect(),
                matches,
                filter,
            }
        })
        .collect()
}

/// How a table's search ended, before it becomes a [`TableStatus`].
enum Searched {
    Done(TableStatus),
    Cancelled,
}

/// Searches one table inside the engine's read-only transaction: its columns, then its rows.
async fn search_table(
    session: &Session,
    table: &SearchTable,
    request: &DataSearchRequest,
    limit: u32,
    cancel: &ExportCancel,
    statements: &mut Vec<String>,
) -> Searched {
    let dialect = session.dialect().clone();
    let failed = |message: String| {
        if cancel.is_cancelled() {
            Searched::Cancelled
        } else {
            Searched::Done(TableStatus::Failed(message))
        }
    };
    let primary_key = match session
        .connection()
        .describe_table(&table.schema, &table.table)
        .await
    {
        Ok(info) => info.primary_key,
        Err(e) => return failed(e.to_string()),
    };
    let (begin, end) = match barrier(dialect.name()) {
        Ok(statements) => statements,
        Err(e) => return failed(e.to_string()),
    };
    if let Err(e) = drain(session, begin).await {
        return failed(e.to_string());
    }
    let outcome = async {
        let probe = probe_sql(dialect.as_ref(), table);
        checked(dialect.as_ref(), &probe).map_err(|e| e.to_string())?;
        statements.push(probe.clone());
        let columns = read(session, &probe, Vec::new())
            .await
            .map_err(|e| e.to_string())?
            .columns;
        let options = DataSearchOptions {
            rows_per_table: limit,
            ..request.options.clone()
        };
        let Some(search) = table_search_sql(
            dialect.as_ref(),
            table,
            &columns,
            &primary_key,
            &request.value,
            &options,
        )
        .map_err(|e| e.to_string())?
        else {
            return Ok(TableStatus::NoColumns);
        };
        checked(dialect.as_ref(), &search.sql).map_err(|e| e.to_string())?;
        if cancel.is_cancelled() {
            return Err("cancelled".to_string());
        }
        statements.push(search.sql.clone());
        let rows = read(session, &search.sql, search.params.clone())
            .await
            .map_err(|e| e.to_string())?
            .rows;
        Ok(TableStatus::Searched {
            columns: u32::try_from(search.columns.len()).unwrap_or(u32::MAX),
            more: rows.len() >= limit as usize,
            hits: hits(dialect.as_ref(), &search, &rows),
        })
    }
    .await;
    if let Some(end) = end {
        // Read-only either way: nothing to keep or lose.
        let _ = drain(session, end).await;
    }
    match outcome {
        Ok(status) => Searched::Done(status),
        Err(message) => failed(message),
    }
}

/// Searches the request's tables one after the other on `session` (a session of its own,
/// opened for reads only, that the caller closes afterwards), telling `progress` as each table
/// starts and ends. A table the engine refuses (permissions, a view that fails) is reported and
/// the search goes on.
pub async fn data_search(
    session: &Session,
    request: &DataSearchRequest,
    cancel: &ExportCancel,
    progress: &(dyn Fn(DataSearchEvent) + Send + Sync),
) -> Result<DataSearchSummary, DataSearchError> {
    request.check()?;
    if !session.connection().capabilities().bound_params {
        return Err(DataSearchError::Unsupported);
    }
    let total = request.tables.len();
    let mut summary = DataSearchSummary {
        tables: 0,
        hits: 0,
        cancelled: false,
        stopped: false,
        statements: Vec::new(),
    };
    for (index, table) in request.tables.iter().enumerate() {
        if cancel.is_cancelled() {
            summary.cancelled = true;
            break;
        }
        if summary.hits >= MAX_SEARCH_HITS {
            summary.stopped = true;
            break;
        }
        progress(DataSearchEvent::Started {
            index,
            total,
            schema: table.schema.clone(),
            table: table.table.clone(),
        });
        // A table never returns more than the hits still allowed.
        let left = u32::try_from(MAX_SEARCH_HITS - summary.hits).unwrap_or(u32::MAX);
        let limit = request.options.rows_per_table.min(left);
        let status = match search_table(
            session,
            table,
            request,
            limit,
            cancel,
            &mut summary.statements,
        )
        .await
        {
            Searched::Done(status) => status,
            Searched::Cancelled => {
                summary.cancelled = true;
                progress(DataSearchEvent::Finished(TableOutcome {
                    schema: table.schema.clone(),
                    table: table.table.clone(),
                    status: TableStatus::Cancelled,
                }));
                break;
            }
        };
        summary.tables += 1;
        if let TableStatus::Searched { hits, .. } = &status {
            summary.hits += hits.len();
        }
        progress(DataSearchEvent::Finished(TableOutcome {
            schema: table.schema.clone(),
            table: table.table.clone(),
            status,
        }));
    }
    if cancel.is_cancelled() {
        summary.cancelled = true;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_api::Dialect;

    fn d(engine: &str) -> Box<dyn Dialect> {
        match engine {
            "postgres" => Box::new(spelaion_driver_postgres::PostgresDialect),
            "mysql" => Box::new(spelaion_driver_mysql::MysqlDialect),
            "sqlserver" => Box::new(spelaion_driver_sqlserver::SqlServerDialect),
            "duckdb" => Box::new(spelaion_driver_duckdb::DuckdbDialect),
            _ => Box::new(spelaion_driver_sqlite::SqliteDialect),
        }
    }

    const ENGINES: [&str; 5] = ["postgres", "mysql", "sqlserver", "sqlite", "duckdb"];

    fn meta(name: &str, logical: LogicalType, native: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            logical,
            native_type: native.into(),
            native_id: None,
        }
    }

    fn table() -> SearchTable {
        SearchTable {
            schema: "public".into(),
            table: "orders".into(),
        }
    }

    fn columns() -> Vec<ColumnMeta> {
        vec![
            meta("id", LogicalType::Int, "int4"),
            meta("note", LogicalType::Text, "varchar"),
            meta("payload", LogicalType::Json, "json"),
            meta("photo", LogicalType::Bytes, "bytea"),
            meta("placed", LogicalType::Date, "date"),
            meta("total", LogicalType::Decimal, "numeric"),
        ]
    }

    #[test]
    fn the_value_is_bound_never_written() {
        let value = "x'); DROP TABLE orders; --";
        let options = DataSearchOptions {
            numbers: true,
            large: true,
            ..DataSearchOptions::default()
        };
        for engine in ENGINES {
            let search = table_search_sql(
                d(engine).as_ref(),
                &table(),
                &columns(),
                &["id".into()],
                value,
                &options,
            )
            .unwrap()
            .unwrap();
            assert!(!search.sql.contains("DROP"), "{engine}: {}", search.sql);
            assert_eq!(search.params, vec![value.to_string()]);
            assert!(
                search.sql.contains(&bound_placeholder(engine, 1)),
                "{engine}: {}",
                search.sql
            );
            assert!(checked(d(engine).as_ref(), &search.sql).is_ok(), "{engine}");
            assert!(
                checked(d(engine).as_ref(), &probe_sql(d(engine).as_ref(), &table())).is_ok(),
                "{engine}"
            );
        }
    }

    #[test]
    fn options_choose_the_columns() {
        let pick = |options: &DataSearchOptions, value: &str| {
            table_search_sql(
                d("postgres").as_ref(),
                &table(),
                &columns(),
                &["id".into()],
                value,
                options,
            )
            .unwrap()
            .map(|s| s.columns.into_iter().map(|c| c.name).collect::<Vec<_>>())
        };
        let plain = DataSearchOptions::default();
        assert_eq!(pick(&plain, "42"), Some(vec!["note".into()]));
        let numbers = DataSearchOptions {
            numbers: true,
            ..plain.clone()
        };
        assert_eq!(
            pick(&numbers, "42"),
            Some(vec!["id".into(), "note".into(), "total".into()])
        );
        // Not a number: numeric columns are left out even when asked.
        assert_eq!(pick(&numbers, "4x2"), Some(vec!["note".into()]));
        let large = DataSearchOptions {
            large: true,
            ..plain.clone()
        };
        assert_eq!(
            pick(&large, "abc"),
            Some(vec!["note".into(), "payload".into(), "photo".into()])
        );
        // A table with only dates has nothing to search.
        let dates = vec![meta("placed", LogicalType::Date, "date")];
        assert_eq!(
            table_search_sql(
                d("postgres").as_ref(),
                &table(),
                &dates,
                &[],
                "2024",
                &plain
            ),
            Ok(None)
        );
    }

    #[test]
    fn large_text_is_told_apart_by_engine() {
        let options = DataSearchOptions::default();
        let text = |engine: &str, native: &str| {
            column_kind(
                engine,
                &meta("c", LogicalType::Text, native),
                &options,
                false,
            )
        };
        assert_eq!(text("mysql", "text"), None);
        assert_eq!(text("mysql", "varchar"), Some(SearchColumnKind::Text));
        assert_eq!(text("sqlserver", "ntext"), None);
        assert_eq!(text("sqlserver", "nvarchar"), Some(SearchColumnKind::Text));
        assert_eq!(text("postgres", "text"), Some(SearchColumnKind::Text));
        assert_eq!(text("postgres", "xml"), None);
        // SQLite: the declared type decides for columns read as text.
        assert_eq!(text("sqlite", "VARCHAR(40)"), Some(SearchColumnKind::Text));
        assert_eq!(text("sqlite", "TEXT"), Some(SearchColumnKind::Text));
        assert_eq!(text("sqlite", "DATE"), None);
        assert_eq!(text("sqlite", "DECIMAL(10, 2)"), None);
        let numbers = DataSearchOptions {
            numbers: true,
            ..DataSearchOptions::default()
        };
        assert_eq!(
            column_kind(
                "sqlite",
                &meta("c", LogicalType::Text, "DECIMAL(10, 2)"),
                &numbers,
                true
            ),
            Some(SearchColumnKind::Number)
        );
    }

    #[test]
    fn sql_server_uses_top_and_the_rest_limit() {
        let options = DataSearchOptions {
            rows_per_table: 7,
            ..DataSearchOptions::default()
        };
        for engine in ENGINES {
            let sql = table_search_sql(
                d(engine).as_ref(),
                &table(),
                &columns(),
                &[],
                "abc",
                &options,
            )
            .unwrap()
            .unwrap()
            .sql;
            if engine == "sqlserver" {
                assert!(sql.starts_with("SELECT TOP (7) "), "{sql}");
                assert!(!sql.contains("LIMIT"));
            } else {
                assert!(sql.ends_with("\nLIMIT 7"), "{engine}: {sql}");
            }
        }
    }

    #[test]
    fn names_are_quoted() {
        let mut odd = columns();
        odd[1].name = "a\"b".into();
        let sql = table_search_sql(
            d("postgres").as_ref(),
            &table(),
            &odd,
            &[],
            "abc",
            &DataSearchOptions::default(),
        )
        .unwrap()
        .unwrap()
        .sql;
        assert!(sql.contains("CAST(\"a\"\"b\" AS text)"), "{sql}");
        odd[1].name = "x] FROM t; --".into();
        let sql = table_search_sql(
            d("sqlserver").as_ref(),
            &table(),
            &odd,
            &[],
            "abc",
            &DataSearchOptions::default(),
        )
        .unwrap()
        .unwrap()
        .sql;
        assert!(
            sql.contains("CAST([x]] FROM t; --] AS NVARCHAR(MAX))"),
            "{sql}"
        );
    }

    #[test]
    fn requests_are_bounded() {
        let mut request = DataSearchRequest {
            value: "abc".into(),
            tables: vec![table()],
            options: DataSearchOptions::default(),
        };
        assert_eq!(request.check(), Ok(()));
        request.value = "  ".into();
        assert_eq!(request.check(), Err(DataSearchError::EmptyValue));
        request.value = "x".repeat(MAX_SEARCH_VALUE_CHARS + 1);
        assert_eq!(request.check(), Err(DataSearchError::LongValue));
        request.value = "a\0b".into();
        assert_eq!(request.check(), Err(DataSearchError::NulInValue));
        request.value = "abc".into();
        request.tables.clear();
        assert_eq!(request.check(), Err(DataSearchError::NoTables));
        request.tables = vec![table(); MAX_SEARCH_TABLES + 1];
        assert_eq!(request.check(), Err(DataSearchError::TooManyTables));
        request.tables = vec![table()];
        request.options.rows_per_table = 0;
        assert_eq!(request.check(), Err(DataSearchError::BadLimit));
        request.options.rows_per_table = MAX_ROWS_PER_TABLE + 1;
        assert_eq!(request.check(), Err(DataSearchError::BadLimit));
    }

    #[test]
    fn numbers_fit_every_engine() {
        for yes in ["42", "-1", "+3.25", "0.0000000001", &"9".repeat(28)] {
            assert!(is_search_number(yes), "{yes}");
        }
        for no in [
            "",
            "-",
            "1.",
            ".5",
            "1e5",
            "0x10",
            "1,5",
            "NaN",
            &"9".repeat(29),
            "1.00000000001",
        ] {
            assert!(!is_search_number(no), "{no}");
        }
    }

    #[test]
    fn hits_carry_the_key_filter_and_the_matching_columns() {
        let dialect = d("postgres");
        let search = table_search_sql(
            dialect.as_ref(),
            &table(),
            &columns(),
            &["id".into()],
            "abc",
            &DataSearchOptions::default(),
        )
        .unwrap()
        .unwrap();
        let rows = vec![
            vec![Value::Int(7), Value::Int(1), Value::Text("xx abc".into())],
            vec![Value::Int(8), Value::Int(0), Value::Null],
        ];
        let hits = hits(dialect.as_ref(), &search, &rows);
        assert_eq!(hits[0].key, vec![("id".to_string(), "7".to_string())]);
        assert_eq!(hits[0].filter.as_deref(), Some("id = 7"));
        assert_eq!(
            hits[0].matches,
            vec![ColumnMatch {
                column: "note".into(),
                text: Some("xx abc".into())
            }]
        );
        assert!(hits[1].matches.is_empty());
    }

    #[test]
    fn without_a_key_a_hit_is_filtered_by_its_value() {
        let dialect = d("postgres");
        let search = table_search_sql(
            dialect.as_ref(),
            &table(),
            &columns(),
            &[],
            "abc",
            &DataSearchOptions::default(),
        )
        .unwrap()
        .unwrap();
        let rows = vec![vec![
            Value::Int(1),
            Value::Text("it's abc".into()),
            Value::Text("it's abc".into()),
        ]];
        let hits = hits(dialect.as_ref(), &search, &rows);
        assert!(hits[0].key.is_empty());
        assert_eq!(hits[0].filter.as_deref(), Some("note = 'it''s abc'"));
    }
}
