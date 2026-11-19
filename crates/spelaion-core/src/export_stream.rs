//! Exports that stream to a file (ADR-034): a query, a table data tab or explorer tables run
//! again, as a read, and each Arrow batch is written to the file as it arrives and then
//! dropped. Nothing collects the rows, so memory does not grow with the row count.
//!
//! Only reads are streamed, behind two barriers:
//! 1. the statement must be one statement for the engine's splitter and a read for
//!    [`classify_read_only`] (the agents' classifier), before anything is sent;
//! 2. it runs inside the engine's read-only transaction, always rolled back (on SQLite,
//!    `PRAGMA query_only` on the export's own connection).
//!
//! The caller gives the export its own session (never the tab's, which may hold a
//! transaction, ADR-031) and closes it afterwards. The file is written as `<name>.part` and
//! renamed only when complete: a cancel or an error leaves nothing behind.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_driver_api::{Dialect, DriverError, ExecEvent, ExecOptions, LogicalType, Value};

use crate::Session;
use crate::classify::{ReadKind, classify_read_only, reject_mysql_executable_comments};
use crate::export::{
    CsvOptions, ExportFormat, FileSummary, JsonOptions, PartFile, RowWriter, SqlOptions, SqlTarget,
    TextEncoding, part_path, upsert_supported,
};
use crate::export_binary::BinaryFile;
use crate::export_xlsx::XlsxBook;
use crate::guard::check_script;
use crate::job::JobCancel;
use crate::masking::{mask_batch, masked_schema};
use crate::table_data::{PageError, table_export_sql};

/// What a streaming export writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamExport {
    /// Any file format but the clipboard's (`InList`, `SqlInsertBatch`). For tables, the
    /// table of `SqlInsert` is the exported table's name.
    pub format: ExportFormat,
    /// Column names as the first line (CSV and TSV).
    pub header: bool,
    pub csv: CsvOptions,
    pub encoding: TextEncoding,
    /// The file compressed with gzip (Parquet: its column chunks, with its own codec).
    pub gzip: bool,
    /// How SQL statements are written (SDB-391).
    pub sql: SqlOptions,
    /// How JSON and NDJSON write values (SDB-391).
    pub json: JsonOptions,
    /// Columns of the result to write, by position and in this order; `None`: all of them.
    pub columns: Option<Vec<usize>>,
    /// Rows per batch asked of the driver: the most rows the export holds at once
    /// ([`DEFAULT_BATCH_ROWS`] unless a test wants smaller ones).
    pub batch_size: usize,
}

/// Rows per batch of a streaming export.
pub const DEFAULT_BATCH_ROWS: usize = 10_000;

/// Rows and bytes written so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportProgress {
    pub rows: u64,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Refused before anything was sent to the server.
    #[error("only a single read can be exported by running it again: {0}")]
    NotARead(String),
    /// The connection's type confirms execution and the export was not confirmed.
    #[error("this connection's type asks before running a query: confirm the export first")]
    NeedsConfirmation,
    #[error("{0}")]
    Table(#[from] PageError),
    #[error("{0}")]
    Database(#[from] DriverError),
    #[error("could not write {path}: {message}")]
    Io { path: String, message: String },
    #[error("the export was cancelled")]
    Cancelled,
    #[error("{0}")]
    Invalid(String),
}

/// A table or view, with the table data tab's filter and order (both empty from the explorer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSource {
    pub schema: String,
    pub name: String,
    pub filter: String,
    pub order_by: String,
}

/// A table written by [`export_tables`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportedFile {
    pub schema: String,
    pub name: String,
    pub path: PathBuf,
    pub summary: FileSummary,
}

/// [`export_tables`] stopped at `table` (its position): `done` are the files finished before.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct TablesError {
    pub done: Vec<ExportedFile>,
    pub table: usize,
    pub error: Box<ExportError>,
}

/// How often progress is reported.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Makes the export's session find unqualified names where the tab's does: the same database
/// on engines that switch it per session (MySQL, MariaDB, SQL Server) and the same schema on
/// PostgreSQL and DuckDB. The tab is only asked which one it uses (a catalog read); when it
/// cannot say (a transaction that failed, for example), the export keeps its own.
pub async fn follow_session_context(tab: &Session, export: &Session) -> Result<(), String> {
    // A tab still running a statement answers only when it finishes: say so rather than wait.
    let asked = tokio::time::timeout(TAB_ANSWER, async {
        match tab.dialect().name() {
            "mysql" | "sqlserver" => Ok(tab.connection().current_database().await.ok().flatten()),
            "postgres" | "duckdb" => Ok(crate::active_schema(tab).await.ok().flatten()),
            _ => Ok(None),
        }
    })
    .await
    .unwrap_or(Err(TabBusy))
    .map_err(|TabBusy| {
        "this tab is still running a statement: export again when it finishes".to_string()
    })?;
    let Some(name) = asked else {
        return Ok(());
    };
    let per_database = matches!(export.dialect().name(), "mysql" | "sqlserver");
    let current = if per_database {
        export.connection().current_database().await.ok().flatten()
    } else {
        crate::active_schema(export).await?
    };
    if current.as_deref() == Some(name.as_str()) {
        return Ok(());
    }
    if per_database {
        export
            .connection()
            .select_database(&name)
            .await
            .map_err(|e| e.to_string())
    } else {
        crate::set_active_schema(export, &name)
            .await
            .map_err(|e| e.to_string())
    }
}

/// How long the tab may take to say which database or schema it uses.
const TAB_ANSWER: Duration = Duration::from_secs(3);

struct TabBusy;

/// What the export dialog tells the user before a query or a table data tab is run again
/// in a new session (ADR-034).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerunCheck {
    /// Why the query cannot be run again (it is not a single read, so running it again would
    /// run a write twice): only the rows already loaded can be exported.
    pub refused: Option<String>,
    /// The tab's session has an open transaction: its uncommitted changes are not visible to
    /// the export's session, so they will not be in the file.
    pub open_transaction: bool,
}

/// Checks a re-run before it is offered: `sql` is the statement that produced the result
/// (`None` for a table data tab, whose SELECT the core writes), and `tab` the tab's session,
/// whose transaction is read as last reported, without a round trip.
pub fn rerun_check(tab: &Session, sql: Option<&str>) -> RerunCheck {
    let refused = sql.and_then(|sql| match checked_read(tab.dialect().as_ref(), sql) {
        Ok(_) => None,
        Err(e) => Some(e.to_string()),
    });
    RerunCheck {
        refused,
        open_transaction: tab.transaction_status().in_transaction(),
    }
}

/// Runs `sql` again on `session` (the export's own) and streams its rows to `path`. Only a
/// single read passes; a connection type that confirms execution needs `confirmed` (ADR-029).
#[allow(clippy::too_many_arguments)]
pub async fn export_query(
    session: &Session,
    sql: &str,
    safeguards: &Safeguards,
    confirmed: bool,
    path: &Path,
    export: &StreamExport,
    cancel: &JobCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<FileSummary, ExportError> {
    let dialect = session.dialect().clone();
    let sql = checked_read(dialect.as_ref(), sql)?;
    check_script(dialect.as_ref(), &sql, safeguards, confirmed)
        .map_err(|_| ExportError::NeedsConfirmation)?;
    if export.sql.upsert {
        return Err(ExportError::Invalid(
            "an upsert is written for a table: export the table, or turn upsert off".into(),
        ));
    }
    stream_read(
        session,
        &sql,
        Target::file(path, &sheet_of(path)),
        export,
        &SqlTarget::default(),
        cancel,
        progress,
    )
    .await
    .map(Written::summary)
}

/// Streams a table's rows to `path`: the table data tab's checked SELECT without its paging
/// (`table_export_sql`), never SQL from the UI.
pub async fn export_table(
    session: &Session,
    table: &TableSource,
    path: &Path,
    export: &StreamExport,
    cancel: &JobCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<FileSummary, ExportError> {
    let dialect = session.dialect().clone();
    let sql = table_export_sql(
        dialect.as_ref(),
        &table.schema,
        &table.name,
        &table.filter,
        &table.order_by,
    )?;
    let sql = checked_read(dialect.as_ref(), &sql)?;
    let mut export = export.clone();
    let mut target = SqlTarget::default();
    if let ExportFormat::SqlInsert { table: name } = &mut export.format {
        name.clone_from(&table.name);
        target.schema = Some(table.schema.clone());
        if export.sql.upsert {
            target.key = upsert_key(session, table).await?;
        }
    } else if export.sql.upsert {
        return Err(ExportError::Invalid("an upsert is written as SQL".into()));
    }
    stream_read(
        session,
        &sql,
        Target::file(path, &table.name),
        &export,
        &target,
        cancel,
        progress,
    )
    .await
    .map(Written::summary)
}

/// [`export_table`]'s statement and options, for a sheet of a workbook.
async fn table_sheet(
    session: &Session,
    table: &TableSource,
    target: Target,
    export: &StreamExport,
    cancel: &JobCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<Written, ExportError> {
    let dialect = session.dialect().clone();
    let sql = table_export_sql(
        dialect.as_ref(),
        &table.schema,
        &table.name,
        &table.filter,
        &table.order_by,
    )?;
    let sql = checked_read(dialect.as_ref(), &sql)?;
    stream_read(
        session,
        &sql,
        target,
        export,
        &SqlTarget::default(),
        cancel,
        progress,
    )
    .await
}

/// The primary key an upsert of `table` is written by, from the catalog: refused on an engine
/// whose upsert form the export does not write, and for a table without a primary key.
async fn upsert_key(session: &Session, table: &TableSource) -> Result<Vec<String>, ExportError> {
    let engine = session.dialect().name();
    if !upsert_supported(engine) {
        return Err(ExportError::Invalid(format!(
            "an upsert is not written for {engine}: turn upsert off"
        )));
    }
    let info = session
        .connection()
        .describe_table(&table.schema, &table.name)
        .await?;
    if info.primary_key.is_empty() {
        return Err(ExportError::Invalid(format!(
            "{} has no primary key: an upsert needs one to find each row",
            table.name
        )));
    }
    Ok(info.primary_key)
}

/// Streams several tables, one after the other, each to its own file in `folder` named after
/// it ([`file_name`]; never over an existing file). `progress` gets the table's position.
/// A cancel or an error stops at that table; the files finished before stay.
///
/// As XLSX, the tables go into one workbook, a sheet each ([`export_workbook`]).
#[allow(clippy::too_many_arguments)]
pub async fn export_tables(
    session: &Session,
    tables: &[(String, String)],
    folder: &Path,
    names: &FileNamePattern,
    database: Option<&str>,
    export: &StreamExport,
    cancel: &JobCancel,
    progress: &(dyn Fn(usize, ExportProgress) + Send + Sync),
) -> Result<Vec<ExportedFile>, TablesError> {
    if export.format == ExportFormat::Xlsx {
        // Boxed: its future and this one's would otherwise share one large frame.
        return Box::pin(export_workbook(
            session, tables, folder, names, database, export, cancel, progress,
        ))
        .await;
    }
    let mut done = Vec::new();
    let now = chrono::Local::now();
    let extension = export.format.extension(export.gzip);
    for (index, (schema, name)) in tables.iter().enumerate() {
        let failed = |done, error| TablesError {
            done,
            table: index,
            error: Box::new(error),
        };
        if cancel.is_cancelled() {
            return Err(failed(done, ExportError::Cancelled));
        }
        let base = match names.render(
            &NameParts {
                table: name,
                schema,
                database: database.unwrap_or(""),
            },
            now,
        ) {
            Ok(base) => base,
            Err(message) => return Err(failed(done, ExportError::Invalid(message))),
        };
        // The same name in two schemas, and a pattern that does not say the schema: both files
        // say which schema.
        let shared = tables.iter().filter(|(_, other)| other == name).count() > 1;
        let base = if shared && !names.pattern.contains("{schema}") {
            file_name(&format!("{schema}.{base}"))
        } else {
            base
        };
        let path = free_path(folder, &base, &extension);
        let table = TableSource {
            schema: schema.clone(),
            name: name.clone(),
            filter: String::new(),
            order_by: String::new(),
        };
        let report = |p: ExportProgress| progress(index, p);
        match export_table(session, &table, &path, export, cancel, &report).await {
            Ok(summary) => done.push(ExportedFile {
                schema: schema.clone(),
                name: name.clone(),
                path,
                summary,
            }),
            Err(error) => return Err(failed(done, error)),
        }
    }
    Ok(done)
}

/// [`export_tables`] as XLSX: one workbook in `folder`, a sheet per table (the schema in front
/// when two tables share a name), named by the pattern with the database for `{table}`
/// (`tables` when the connection names none) and the tables' schema for `{schema}` when they
/// share one. Each table is still its own read, run again in the read-only transaction and
/// written a batch at a time. The workbook is renamed to its name only when every sheet is
/// written: a cancel or an error leaves no file.
#[allow(clippy::too_many_arguments)]
async fn export_workbook(
    session: &Session,
    tables: &[(String, String)],
    folder: &Path,
    names: &FileNamePattern,
    database: Option<&str>,
    export: &StreamExport,
    cancel: &JobCancel,
    progress: &(dyn Fn(usize, ExportProgress) + Send + Sync),
) -> Result<Vec<ExportedFile>, TablesError> {
    let failed = |table: usize, error| TablesError {
        done: Vec::new(),
        table,
        error: Box::new(error),
    };
    if export.sql.upsert {
        return Err(failed(
            0,
            ExportError::Invalid("an upsert is written as SQL".into()),
        ));
    }
    let database = database.unwrap_or("");
    let schema = match tables.split_first() {
        Some(((first, _), rest)) if rest.iter().all(|(s, _)| s == first) => first.as_str(),
        _ => "",
    };
    let base = names
        .render(
            &NameParts {
                table: if database.is_empty() {
                    "tables"
                } else {
                    database
                },
                schema,
                database,
            },
            chrono::Local::now(),
        )
        .map_err(|message| failed(0, ExportError::Invalid(message)))?;
    let path = free_path(folder, &base, &export.format.extension(false));
    let bytes = Arc::new(AtomicU64::new(0));
    let mut book: Option<Box<XlsxBook>> = None;
    let mut sheets = Vec::with_capacity(tables.len());
    for (index, (schema, name)) in tables.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(failed(index, ExportError::Cancelled));
        }
        let shared = tables.iter().filter(|(_, other)| other == name).count() > 1;
        let sheet = if shared {
            format!("{schema}.{name}")
        } else {
            name.clone()
        };
        let table = TableSource {
            schema: schema.clone(),
            name: name.clone(),
            filter: String::new(),
            order_by: String::new(),
        };
        let target = Target {
            path: path.clone(),
            sheet,
            bytes: bytes.clone(),
            book: book.take(),
            keep_open: true,
        };
        let report = |p: ExportProgress| progress(index, p);
        match Box::pin(table_sheet(
            session, &table, target, export, cancel, &report,
        ))
        .await
        {
            Ok(Written::Sheet {
                book: open,
                rows,
                peak_rows,
            }) => {
                book = Some(open);
                sheets.push((schema.clone(), name.clone(), rows, peak_rows));
            }
            Ok(Written::File(_)) => {
                return Err(failed(
                    index,
                    ExportError::Invalid("the workbook was closed before its last sheet".into()),
                ));
            }
            Err(error) => return Err(failed(index, error)),
        }
    }
    let last = tables.len().saturating_sub(1);
    let Some(book) = book else {
        return Err(failed(
            0,
            ExportError::Invalid("there is no table to export".into()),
        ));
    };
    let summary = blocking(move || book.finish())
        .await
        .map_err(|error| failed(last, error))?
        .map_err(|e| {
            failed(
                last,
                ExportError::Io {
                    path: path.display().to_string(),
                    message: e.to_string(),
                },
            )
        })?;
    Ok(sheets
        .into_iter()
        .map(|(schema, name, rows, peak_rows)| ExportedFile {
            schema,
            name,
            path: path.clone(),
            summary: FileSummary {
                rows,
                bytes: summary.bytes,
                replaced: 0,
                peak_rows,
            },
        })
        .collect())
}

/// Most characters of a [`FileNamePattern`]'s pattern or timestamp format.
pub const MAX_NAME_PATTERN: usize = 200;

/// How export files are named (SDB-391): a pattern with `{table}`, `{schema}`, `{database}`,
/// `{date}` (`2026-11-18`), `{time}` (`14-05-09`) and `{timestamp}` (written with
/// `timestamp_format`, a `strftime` pattern). What it renders is made a safe file name
/// ([`file_name`]): a pattern cannot reach another folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNamePattern {
    pub pattern: String,
    pub timestamp_format: String,
}

impl Default for FileNamePattern {
    /// The table's name, as exports always named their files.
    fn default() -> Self {
        Self {
            pattern: "{table}".into(),
            timestamp_format: "%Y%m%d-%H%M%S".into(),
        }
    }
}

/// What a [`FileNamePattern`]'s variables stand for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameParts<'a> {
    pub table: &'a str,
    pub schema: &'a str,
    pub database: &'a str,
}

/// The variables a pattern may use.
const NAME_VARIABLES: [&str; 6] = ["table", "schema", "database", "date", "time", "timestamp"];

impl FileNamePattern {
    /// A pattern that names only known variables, closes every `{`, and a timestamp format
    /// `strftime` can write; both bounded and without control characters.
    pub fn check(&self) -> Result<(), String> {
        for (what, text) in [
            ("file name pattern", &self.pattern),
            ("timestamp format", &self.timestamp_format),
        ] {
            if text.chars().count() > MAX_NAME_PATTERN {
                return Err(format!(
                    "the {what} has at most {MAX_NAME_PATTERN} characters"
                ));
            }
            if text.chars().any(char::is_control) {
                return Err(format!("the {what} cannot hold a control character"));
            }
        }
        if self.pattern.trim().is_empty() {
            return Err("the file name pattern is empty".into());
        }
        let mut rest = self.pattern.as_str();
        while let Some(open) = rest.find('{') {
            let after = &rest[open + 1..];
            let close = after
                .find('}')
                .ok_or("a { in the file name pattern is not closed")?;
            let name = &after[..close];
            if !NAME_VARIABLES.contains(&name) {
                return Err(format!(
                    "{{{name}}} is not a variable of file names: use {}",
                    NAME_VARIABLES
                        .iter()
                        .map(|v| format!("{{{v}}}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            rest = &after[close + 1..];
        }
        if rest.contains('}')
            || self.pattern.matches('}').count() != self.pattern.matches('{').count()
        {
            return Err("a } in the file name pattern has no {".into());
        }
        if chrono::format::StrftimeItems::new(&self.timestamp_format)
            .any(|item| matches!(item, chrono::format::Item::Error))
        {
            return Err(format!(
                "{} is not a timestamp format strftime can write",
                self.timestamp_format
            ));
        }
        Ok(())
    }

    /// [`FileNamePattern::render`] at this moment, in this computer's time zone.
    pub fn render_now(&self, parts: &NameParts<'_>) -> Result<String, String> {
        self.render(parts, chrono::Local::now())
    }

    /// The file name (without its extension) for `parts` at `now`, made safe ([`file_name`]).
    pub fn render(
        &self,
        parts: &NameParts<'_>,
        now: chrono::DateTime<chrono::Local>,
    ) -> Result<String, String> {
        self.check()?;
        let mut out = String::new();
        let mut rest = self.pattern.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let close = after.find('}').expect("checked");
            match &after[..close] {
                "table" => out.push_str(parts.table),
                "schema" => out.push_str(parts.schema),
                "database" => out.push_str(parts.database),
                "date" => out.push_str(&now.format("%Y-%m-%d").to_string()),
                "time" => out.push_str(&now.format("%H-%M-%S").to_string()),
                _ => out.push_str(&now.format(&self.timestamp_format).to_string()),
            }
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        Ok(file_name(&out))
    }
}

/// A file name for `name`: characters that file systems refuse become `_`, trailing dots and
/// spaces go, and Windows' reserved device names (`CON`, `NUL`, `COM1`…) get a `_`.
pub fn file_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .take(150)
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    let stem = out
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem[3..].chars().all(|c| c.is_ascii_digit()));
    if out.is_empty() || reserved || out.chars().all(|c| c == '.') {
        out.push('_');
    }
    out
}

/// `folder/base.extension`, or `base (2).extension`… if that file (or one being written)
/// already exists.
fn free_path(folder: &Path, base: &str, extension: &str) -> PathBuf {
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{base}.{extension}")
        } else {
            format!("{base} ({n}).{extension}")
        };
        let path = folder.join(name);
        if !path.exists() && !part_path(&path).exists() {
            return path;
        }
        n += 1;
    }
}

/// `sql` trimmed, if it is exactly one read.
fn checked_read(dialect: &dyn Dialect, sql: &str) -> Result<String, ExportError> {
    let engine = dialect.name();
    let sql = sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return Err(ExportError::NotARead("there is no statement".into()));
    }
    // The splitter that runs scripts must see one statement, like the classifier.
    if dialect.split_statements(sql).len() != 1 {
        return Err(ExportError::NotARead(
            "only one statement can be exported".into(),
        ));
    }
    reject_mysql_executable_comments(engine, sql).map_err(ExportError::NotARead)?;
    let kind = classify_read_only(engine, sql).map_err(ExportError::NotARead)?;
    match kind {
        ReadKind::Query | ReadKind::Show => {}
        ReadKind::Explain if engine == "sqlserver" => {
            return Err(ExportError::NotARead(
                "SQL Server has no EXPLAIN statement to run again".into(),
            ));
        }
        ReadKind::Explain => {}
    }
    Ok(sql.to_string())
}

/// The statements around the read: the engine's read-only transaction, always rolled back.
/// Refused on a server that accepts that transaction without enforcing it (SDB-169).
fn barrier(session: &Session) -> Result<(&'static str, Option<&'static str>), ExportError> {
    if let Some(reason) = session.read_only_transaction_refusal() {
        return Err(ExportError::Invalid(reason));
    }
    Ok(match session.dialect().name() {
        "postgres" => ("begin read only", Some("rollback")),
        "mysql" => ("start transaction read only", Some("rollback")),
        "sqlserver" => ("begin transaction", Some("if @@trancount > 0 rollback")),
        // Not `enable_external_access = false`: clones of a file share one database, so it
        // would switch it off for the user's tabs too (ADR-034).
        "duckdb" => ("begin transaction read only", Some("rollback")),
        // The export's own connection, closed afterwards.
        "sqlite" => ("pragma query_only = on", None),
        // No transactions: the session's `readonly` setting, for the rest of the export's own
        // session ([`enter`] sets it only where it is not set already).
        "clickhouse" => (crate::readonly::CLICKHOUSE_READ_ONLY, None),
        "trino" => ("start transaction read only", Some("rollback")),
        other => {
            return Err(ExportError::Invalid(format!(
                "streaming exports are not available for {other}"
            )));
        }
    })
}

/// Runs the first statement of [`barrier`]: as it is, except ClickHouse's, which is set only
/// where the session is not read-only already (a user whose profile is read-only cannot set it).
async fn enter(session: &Session, begin: &str) -> Result<(), DriverError> {
    if begin == crate::readonly::CLICKHOUSE_READ_ONLY {
        session.clickhouse_read_only().await.map(|_| ())
    } else {
        drain(session, begin).await
    }
}

async fn stream_read(
    session: &Session,
    sql: &str,
    destination: Target,
    export: &StreamExport,
    target: &SqlTarget,
    cancel: &JobCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<Written, ExportError> {
    match export.format {
        ExportFormat::Csv
        | ExportFormat::Tsv
        | ExportFormat::Json
        | ExportFormat::SqlInsert { .. }
        | ExportFormat::Markdown
        | ExportFormat::Html
        | ExportFormat::Ndjson
        | ExportFormat::Parquet
        | ExportFormat::ArrowIpc
        | ExportFormat::Xlsx => {}
        ExportFormat::InList | ExportFormat::SqlInsertBatch { .. } => {
            return Err(ExportError::Invalid(
                "that format is for the clipboard, not for files".into(),
            ));
        }
    }
    export.csv.check().map_err(ExportError::Invalid)?;
    export.sql.check().map_err(ExportError::Invalid)?;
    let Some(_attached) = cancel.attach(session) else {
        return Err(ExportError::Cancelled);
    };
    read_only_transaction(
        session,
        write_stream(session, sql, destination, export, target, cancel, progress),
        // A cancelled export's session is closed by its owner, which ends the transaction;
        // waiting here for the engine to discard the rest of the rows would only delay it.
        |result| matches!(result, Err(ExportError::Cancelled)),
    )
    .await
}

/// Runs `read` inside the engine's read-only transaction: refused where the server accepts
/// that transaction without enforcing it ([`barrier`]), entered ([`enter`]), and ended once
/// `read` is over (read-only either way: nothing to keep or lose), unless `left_to_owner` says
/// the session's owner ends it by closing the session. Every read the core runs again in a
/// session of its own goes through here: an export, a data search's tables, a column's
/// statistics.
pub(crate) async fn read_only_transaction<T, E>(
    session: &Session,
    read: impl Future<Output = Result<T, E>>,
    left_to_owner: impl FnOnce(&Result<T, E>) -> bool,
) -> Result<T, E>
where
    E: From<ExportError> + From<DriverError>,
{
    let (begin, end) = barrier(session)?;
    enter(session, begin).await?;
    let result = read.await;
    if let Some(end) = end
        && !left_to_owner(&result)
    {
        let _ = drain(session, end).await;
    }
    result
}

/// Where a read is written: a file of its own, or one more sheet of a workbook being written.
struct Target {
    /// The file (for errors, and to create it).
    path: PathBuf,
    /// The sheet's name, when the file is a workbook.
    sheet: String,
    /// The bytes written so far, for progress.
    bytes: Arc<AtomicU64>,
    /// The open workbook of [`export_tables`], when the format is XLSX.
    book: Option<Box<XlsxBook>>,
    /// The workbook is handed back open ([`Written::Sheet`]) rather than finished.
    keep_open: bool,
}

impl Target {
    fn file(path: &Path, sheet: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            sheet: sheet.to_string(),
            bytes: Arc::new(AtomicU64::new(0)),
            book: None,
            keep_open: false,
        }
    }
}

/// What [`write_stream`] wrote.
enum Written {
    File(FileSummary),
    /// A sheet of a workbook still open.
    Sheet {
        book: Box<XlsxBook>,
        rows: u64,
        peak_rows: u64,
    },
}

impl Written {
    fn summary(self) -> FileSummary {
        match self {
            Written::File(summary) => summary,
            // A file of its own is always finished.
            Written::Sheet {
                rows, peak_rows, ..
            } => FileSummary {
                rows,
                bytes: 0,
                replaced: 0,
                peak_rows,
            },
        }
    }
}

/// A sheet's name for a file: its name without the extension.
fn sheet_of(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Result");
    name.split('.').next().unwrap_or(name).to_string()
}

/// The file being written: `<path>.part` until [`FileWriter::finish`] renames it. Each call
/// runs on a blocking thread, one batch at a time.
enum FileWriter {
    /// A text format, written a row at a time.
    Text(TextFile),
    /// Parquet or Arrow, written a batch at a time.
    Binary(BinaryFile),
    /// A sheet of a workbook, written a batch at a time.
    Xlsx(Box<XlsxBook>),
}

struct TextFile {
    file: PartFile,
    writer: RowWriter,
    columns: Vec<usize>,
    logicals: Vec<LogicalType>,
    values: Vec<Value>,
}

impl FileWriter {
    fn text(
        path: &Path,
        encoding: TextEncoding,
        gzip: bool,
        mut writer: RowWriter,
        columns: Vec<usize>,
        logicals: Vec<LogicalType>,
        bytes: Arc<AtomicU64>,
    ) -> io::Result<Self> {
        // Dropped on any error: the temporary file goes.
        let mut file = PartFile::create(path, encoding, gzip)?;
        file.report_to(bytes);
        writer.begin(file.writer())?;
        Ok(Self::Text(TextFile {
            file,
            writer,
            values: Vec::with_capacity(columns.len()),
            columns,
            logicals,
        }))
    }

    fn write(&mut self, batch: &RecordBatch) -> io::Result<()> {
        let text = match self {
            FileWriter::Binary(file) => return file.write(batch),
            FileWriter::Xlsx(book) => return book.write(batch),
            FileWriter::Text(text) => text,
        };
        for row in 0..batch.num_rows() {
            text.values.clear();
            text.values.extend(
                text.columns
                    .iter()
                    .zip(&text.logicals)
                    .map(|(&c, &logical)| {
                        Value::from_array(batch.column(c).as_ref(), row, logical)
                            .unwrap_or(Value::Null)
                    }),
            );
            text.writer.row(&text.values, text.file.writer())?;
        }
        Ok(())
    }

    fn finish(self) -> io::Result<FileSummary> {
        match self {
            FileWriter::Binary(file) => file.finish(),
            FileWriter::Xlsx(book) => book.finish(),
            FileWriter::Text(mut text) => {
                let written = text.writer.finish(text.file.writer())?;
                text.file.commit(written as u64)
            }
        }
    }
}

/// Runs `work` on a blocking thread (file writes must not stall the async runtime).
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ExportError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| ExportError::Invalid(format!("the writer stopped: {e}")))
}

async fn write_stream(
    session: &Session,
    sql: &str,
    destination: Target,
    export: &StreamExport,
    target: &SqlTarget,
    cancel: &JobCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<Written, ExportError> {
    let path = destination.path.clone();
    let path = path.as_path();
    let io_error = |e: io::Error| ExportError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    };
    let bytes = destination.bytes.clone();
    let sheet = destination.sheet.clone();
    // A workbook of several tables: this read is one more sheet of it, and the book is
    // handed back open.
    let keep_open = destination.keep_open;
    let mut book = destination.book;
    let mut rows = 0u64;
    let mut peak_rows = 0u64;
    if cancel.is_cancelled() {
        return Err(ExportError::Cancelled);
    }
    let options = ExecOptions {
        batch_size: export.batch_size.max(1),
        // Masking matches each column's origin too (ADR-052).
        origins: !session.masking().is_empty(),
    };
    let mut events = match session.connection().execute(sql, &options).await {
        Ok(events) => events,
        Err(_) if cancel.is_cancelled() => return Err(ExportError::Cancelled),
        Err(e) => return Err(e.into()),
    };
    let mut file: Option<FileWriter> = None;
    // The masked columns (ADR-051) and the schema they are written with: every batch is
    // masked before it is written, and its original values dropped with it.
    let mut masking: Option<(Arc<spelaion_driver_api::ResultSchema>, Vec<usize>)> = None;
    let mut reported = Instant::now();
    // Every exit but the end of the rows drops `file`, which deletes the temporary file.
    let failure = loop {
        if cancel.is_cancelled() {
            break ExportError::Cancelled;
        }
        let Some(event) = events.next().await else {
            break match file.take() {
                Some(FileWriter::Xlsx(mut book)) if keep_open => {
                    let ended = blocking(move || book.end_sheet().map(|()| book))
                        .await?
                        .map_err(io_error)?;
                    progress(ExportProgress {
                        rows,
                        bytes: bytes.load(Ordering::Relaxed),
                    });
                    return Ok(Written::Sheet {
                        book: ended,
                        rows,
                        peak_rows,
                    });
                }
                Some(writer) => {
                    let summary = blocking(move || writer.finish()).await?.map_err(io_error)?;
                    let summary = FileSummary {
                        peak_rows,
                        ..summary
                    };
                    progress(ExportProgress {
                        rows: summary.rows,
                        bytes: summary.bytes,
                    });
                    return Ok(Written::File(summary));
                }
                None => ExportError::Invalid("the statement returned no rows to export".into()),
            };
        };
        match event {
            Err(_) if cancel.is_cancelled() => break ExportError::Cancelled,
            Err(e) => {
                // The statement ended there: nothing left to stop.
                drop(events);
                drop(file);
                return Err(e.into());
            }
            Ok(ExecEvent::Columns(schema)) => {
                if file.is_some() {
                    break ExportError::Invalid(
                        "the statement returned more than one result".into(),
                    );
                }
                let (schema, masked) = masked_schema(session.masking(), &schema);
                masking = Some((schema.clone(), masked));
                let columns: Vec<usize> = match &export.columns {
                    Some(chosen) => chosen
                        .iter()
                        .copied()
                        .filter(|&c| c < schema.columns.len())
                        .collect(),
                    None => (0..schema.columns.len()).collect(),
                };
                if columns.is_empty() {
                    break ExportError::Invalid(
                        "none of the chosen columns is in the result".into(),
                    );
                }
                let metas: Vec<_> = columns.iter().map(|&c| schema.columns[c].clone()).collect();
                // An upsert finds each row by the whole primary key.
                if let Some(missing) = target
                    .key
                    .iter()
                    .find(|key| !metas.iter().any(|m| &m.name == *key))
                {
                    break ExportError::Invalid(format!(
                        "an upsert needs the primary key column {missing} in the export"
                    ));
                }
                let destination = path.to_path_buf();
                let (encoding, gzip, bytes) = (export.encoding, export.gzip, bytes.clone());
                let created = if export.format == ExportFormat::Xlsx {
                    let fields: Vec<_> = columns
                        .iter()
                        .map(|&c| schema.arrow.field(c).clone())
                        .collect();
                    let (open, sheet) = (book.take(), sheet.clone());
                    blocking(move || {
                        let mut book = match open {
                            Some(book) => book,
                            None => Box::new(XlsxBook::create(&destination, Some(bytes))?),
                        };
                        book.begin_sheet(&sheet, &fields, columns)?;
                        Ok(FileWriter::Xlsx(book))
                    })
                    .await?
                } else if export.format.is_binary() {
                    let fields: Vec<_> = columns
                        .iter()
                        .map(|&c| schema.arrow.field(c).clone())
                        .collect();
                    let (format, batch_rows) = (export.format.clone(), export.batch_size.max(1));
                    blocking(move || {
                        BinaryFile::create(
                            &destination,
                            &format,
                            gzip,
                            &fields,
                            columns,
                            batch_rows,
                            Some(bytes),
                        )
                        .map(FileWriter::Binary)
                    })
                    .await?
                } else {
                    let logicals: Vec<_> = metas.iter().map(|m| m.logical).collect();
                    let dialect = session.dialect();
                    let writer = RowWriter::new(
                        export.format.clone(),
                        export.header,
                        export.csv.clone(),
                        dialect.as_ref(),
                        metas,
                    )
                    .with_json(export.json)
                    .with_sql(dialect.as_ref(), &export.sql, target);
                    blocking(move || {
                        FileWriter::text(
                            &destination,
                            encoding,
                            gzip,
                            writer,
                            columns,
                            logicals,
                            bytes,
                        )
                    })
                    .await?
                };
                match created {
                    Ok(created) => file = Some(created),
                    Err(e) => break io_error(e),
                }
            }
            Ok(ExecEvent::Batch(batch)) => {
                let Some(mut writer) = file.take() else {
                    continue;
                };
                let batch = match &masking {
                    Some((schema, masked)) if !masked.is_empty() => {
                        mask_batch(&batch, masked, schema).0
                    }
                    _ => batch,
                };
                // The only batch the export holds: written and dropped before the next one.
                let held = batch.num_rows() as u64;
                peak_rows = peak_rows.max(held);
                let (writer, written) = blocking(move || {
                    let written = writer.write(&batch);
                    drop(batch);
                    (writer, written)
                })
                .await?;
                file = Some(writer);
                if let Err(e) = written {
                    break io_error(e);
                }
                rows += held;
                if reported.elapsed() >= PROGRESS_EVERY {
                    progress(ExportProgress {
                        rows,
                        bytes: bytes.load(Ordering::Relaxed),
                    });
                    reported = Instant::now();
                }
            }
            Ok(ExecEvent::Notice(_) | ExecEvent::Done { .. }) => {}
        }
    };
    // The rest of the rows will not be read: stop the statement rather than wait for it.
    if !cancel.is_cancelled() {
        let _ = session.cancel().await;
    }
    drop(events);
    drop(file);
    Err(failure)
}

/// Runs a control statement and discards what it returns.
pub(crate) async fn drain(session: &Session, sql: &str) -> Result<(), DriverError> {
    let mut events = session
        .connection()
        .execute(sql, &ExecOptions::default())
        .await?;
    while let Some(event) = events.next().await {
        event?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_safe_on_every_file_system() {
        assert_eq!(file_name("orders"), "orders");
        assert_eq!(file_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(file_name("a:b*c?\"d\"<e>|f\\g/h"), "a_b_c__d__e__f_g_h");
        assert_eq!(file_name("line\nbreak"), "line_break");
        assert_eq!(file_name("trailing. . "), "trailing");
        assert_eq!(file_name(".."), "_");
        assert_eq!(file_name(""), "_");
        assert_eq!(file_name("con"), "con_");
        assert_eq!(file_name("Com1"), "Com1_");
        assert_eq!(file_name("nul.backup"), "nul.backup_");
        assert_eq!(file_name("console"), "console");
        assert_eq!(file_name("COM10"), "COM10");
        assert_eq!(file_name(&"x".repeat(400)).len(), 150);
    }

    #[test]
    fn free_paths_never_reuse_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("spelaion-free-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_path(&dir, "orders", "csv"), dir.join("orders.csv"));
        std::fs::write(dir.join("orders.csv"), "x").unwrap();
        assert_eq!(free_path(&dir, "orders", "csv"), dir.join("orders (2).csv"));
        std::fs::write(dir.join("orders (2).csv.part"), "x").unwrap();
        assert_eq!(free_path(&dir, "orders", "csv"), dir.join("orders (3).csv"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_single_reads_pass() {
        use spelaion_dialect::PostgresDialect;
        use spelaion_dialect::SqlServerDialect;
        let pg = PostgresDialect;
        assert_eq!(checked_read(&pg, " select 1 ; ").unwrap(), "select 1");
        for sql in [
            "",
            ";",
            "delete from t",
            "select 1; delete from t",
            "select 1; select 2",
            "with gone as (delete from t returning *) select * from gone",
            "with x as (update t set a = 1 returning *) select * from x",
            "with x as (insert into t values (1) returning *) select * from x",
            "select * into copy from t",
            "select * from t for update",
            "select pg_terminate_backend(1)",
            "select set_config('x', 'y', false)",
            "select * from dblink('host=x', 'delete from t') as d(a int)",
            "explain analyze delete from t",
            "explain analyze select 1",
            "copy t to '/tmp/x'",
            "call p()",
            "do $$ begin delete from t; end $$",
            "create table x as select 1",
            "insert into t select * from t",
            "truncate t",
            "select 1 /* ; */; drop table t",
        ] {
            assert!(checked_read(&pg, sql).is_err(), "{sql}");
        }
        let mssql = SqlServerDialect;
        assert!(checked_read(&mssql, "select 1").is_ok());
        for sql in [
            "select 1\ngo\ndelete from t",
            "select * into #x from t",
            "exec sp_who",
            "select * from srv.db.dbo.t",
            "select * from openrowset('SQLNCLI', 'x', 'select 1')",
            "explain select 1",
            "begin delete from t end",
            "with x as (select 1 as a) delete from t",
        ] {
            assert!(checked_read(&mssql, sql).is_err(), "T-SQL: {sql}");
        }
        use spelaion_dialect::MysqlDialect;
        let mysql = MysqlDialect;
        for sql in [
            "select * from t /*!50000 into outfile '/tmp/x' */",
            "select * from t into outfile '/tmp/x'",
            "select load_file('/etc/passwd')",
            "select get_lock('x', 1)",
            "select 1; delete from t",
        ] {
            assert!(checked_read(&mysql, sql).is_err(), "MySQL: {sql}");
        }
        use spelaion_dialect::DuckdbDialect;
        for sql in [
            "copy t to 'x.csv'",
            "select * from read_csv('x.csv')",
            "attach 'x.db'",
            "install httpfs",
        ] {
            assert!(checked_read(&DuckdbDialect, sql).is_err(), "DuckDB: {sql}");
        }
        use spelaion_dialect::SqliteDialect;
        for sql in [
            "select writefile('x', 'y')",
            "select load_extension('x')",
            "attach 'x.db' as x",
            "pragma writable_schema = on",
            "vacuum into 'x.db'",
        ] {
            assert!(checked_read(&SqliteDialect, sql).is_err(), "SQLite: {sql}");
        }
    }

    #[test]
    fn file_name_patterns_render_safe_names() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 11, 18, 14, 5, 9)
            .single()
            .unwrap();
        let parts = NameParts {
            table: "orders",
            schema: "sales",
            database: "shop",
        };
        let render = |pattern: &str, format: &str| {
            FileNamePattern {
                pattern: pattern.into(),
                timestamp_format: format.into(),
            }
            .render(&parts, now)
        };
        assert_eq!(
            FileNamePattern::default().render(&parts, now).unwrap(),
            "orders"
        );
        assert_eq!(
            render("{database}_{schema}.{table}_{date}_{time}", "%Y").unwrap(),
            "shop_sales.orders_2026-11-18_14-05-09"
        );
        assert_eq!(
            render("{table}-{timestamp}", "%Y%m%d-%H%M%S").unwrap(),
            "orders-20261118-140509"
        );
        // Whatever the pattern and the names hold, the result is one file name in the folder.
        assert_eq!(render("../../{table}", "%Y").unwrap(), ".._.._orders");
        assert_eq!(render("{timestamp}", "%Y/%m/%d").unwrap(), "2026_11_18");
        let odd = NameParts {
            table: "..\\..\\x",
            schema: "a/b",
            database: "c:d",
        };
        assert_eq!(
            FileNamePattern {
                pattern: "{database}{schema}{table}".into(),
                timestamp_format: "%Y".into(),
            }
            .render(&odd, now)
            .unwrap(),
            "c_da_b.._.._x"
        );
        assert_eq!(render("{table}", "%Y").unwrap(), "orders");
        // Refused: unknown or open variables, an empty pattern, a format strftime cannot
        // write, control characters, and too long.
        for (pattern, format) in [
            ("{tabel}", "%Y"),
            ("{table", "%Y"),
            ("table}", "%Y"),
            ("}{table}{", "%Y"),
            ("", "%Y"),
            ("  ", "%Y"),
            ("{table}", "%Q"),
            ("{table}\n", "%Y"),
            ("{table}", "%Y\t"),
        ] {
            assert!(render(pattern, format).is_err(), "{pattern:?} {format:?}");
        }
        assert!(render(&"x".repeat(MAX_NAME_PATTERN + 1), "%Y").is_err());
        assert!(render("{table}", &"%Y".repeat(MAX_NAME_PATTERN)).is_err());
    }
}
