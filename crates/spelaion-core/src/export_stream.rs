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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arrow::array::RecordBatch;
use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_driver_api::{
    CancelHandle, Dialect, DriverError, ExecEvent, ExecOptions, LogicalType, Value,
};

use crate::Session;
use crate::classify::{ReadKind, classify_read_only, reject_mysql_executable_comments};
use crate::export::{
    CsvOptions, ExportFormat, FileSummary, PartFile, RowWriter, TextEncoding, part_path,
};
use crate::guard::check_script;
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

/// Stops a running export: its statement (the engine's cancel) and its writer. Created
/// before the export's session exists, so a cancel that comes while it is still connecting
/// is not lost. After [`ExportError::Cancelled`] the session may still be inside the export's
/// read-only transaction, with the rest of the rows on their way: close it.
#[derive(Default)]
pub struct ExportCancel {
    cancelled: AtomicBool,
    handle: std::sync::Mutex<Option<Arc<dyn CancelHandle>>>,
}

impl ExportCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session whose statement [`ExportCancel::cancel`] stops.
    pub fn attach(&self, session: &Session) {
        *self.handle.lock().expect("lock poisoned") = Some(session.connection().cancel_handle());
    }

    pub async fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let handle = self.handle.lock().expect("lock poisoned").clone();
        if let Some(handle) = handle {
            let _ = handle.cancel().await;
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
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
    cancel: &ExportCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<FileSummary, ExportError> {
    let dialect = session.dialect().clone();
    let sql = checked_read(dialect.as_ref(), sql)?;
    check_script(dialect.as_ref(), &sql, safeguards, confirmed)
        .map_err(|_| ExportError::NeedsConfirmation)?;
    stream_read(session, &sql, path, export, cancel, progress).await
}

/// Streams a table's rows to `path`: the table data tab's checked SELECT without its paging
/// (`table_export_sql`), never SQL from the UI.
pub async fn export_table(
    session: &Session,
    table: &TableSource,
    path: &Path,
    export: &StreamExport,
    cancel: &ExportCancel,
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
    if let ExportFormat::SqlInsert { table: target } = &mut export.format {
        target.clone_from(&table.name);
    }
    stream_read(session, &sql, path, &export, cancel, progress).await
}

/// Streams several tables, one after the other, each to its own file in `folder` named after
/// it ([`file_name`]; never over an existing file). `progress` gets the table's position.
/// A cancel or an error stops at that table; the files finished before stay.
pub async fn export_tables(
    session: &Session,
    tables: &[(String, String)],
    folder: &Path,
    extension: &str,
    export: &StreamExport,
    cancel: &ExportCancel,
    progress: &(dyn Fn(usize, ExportProgress) + Send + Sync),
) -> Result<Vec<ExportedFile>, TablesError> {
    let mut done = Vec::new();
    for (index, (schema, name)) in tables.iter().enumerate() {
        let failed = |done, error| TablesError {
            done,
            table: index,
            error: Box::new(error),
        };
        if cancel.is_cancelled() {
            return Err(failed(done, ExportError::Cancelled));
        }
        // The same name in two schemas: both files say which schema.
        let shared = tables.iter().filter(|(_, other)| other == name).count() > 1;
        let base = if shared {
            format!("{schema}.{name}")
        } else {
            name.clone()
        };
        let path = free_path(folder, &file_name(&base), extension);
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
pub(crate) fn barrier(engine: &str) -> Result<(&'static str, Option<&'static str>), ExportError> {
    Ok(match engine {
        "postgres" => ("begin read only", Some("rollback")),
        "mysql" => ("start transaction read only", Some("rollback")),
        "sqlserver" => ("begin transaction", Some("if @@trancount > 0 rollback")),
        // Not `enable_external_access = false`: clones of a file share one database, so it
        // would switch it off for the user's tabs too (ADR-034).
        "duckdb" => ("begin transaction read only", Some("rollback")),
        // The export's own connection, closed afterwards.
        "sqlite" => ("pragma query_only = on", None),
        other => {
            return Err(ExportError::Invalid(format!(
                "streaming exports are not available for {other}"
            )));
        }
    })
}

async fn stream_read(
    session: &Session,
    sql: &str,
    path: &Path,
    export: &StreamExport,
    cancel: &ExportCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<FileSummary, ExportError> {
    match export.format {
        ExportFormat::Csv
        | ExportFormat::Tsv
        | ExportFormat::Json
        | ExportFormat::SqlInsert { .. }
        | ExportFormat::Markdown
        | ExportFormat::Html => {}
        ExportFormat::InList | ExportFormat::SqlInsertBatch { .. } => {
            return Err(ExportError::Invalid(
                "that format is for the clipboard, not for files".into(),
            ));
        }
    }
    export.csv.check().map_err(ExportError::Invalid)?;
    if cancel.is_cancelled() {
        return Err(ExportError::Cancelled);
    }
    let (begin, end) = barrier(session.dialect().name())?;
    drain(session, begin).await?;
    let result = write_stream(session, sql, path, export, cancel, progress).await;
    match (&result, end) {
        // A cancelled export's session is closed by its owner, which ends the transaction;
        // waiting here for the engine to discard the rest of the rows would only delay it.
        (Err(ExportError::Cancelled), _) | (_, None) => {}
        // Read-only either way; a failure here changes nothing that was written.
        (_, Some(end)) => {
            let _ = drain(session, end).await;
        }
    }
    result
}

/// The file being written: `<path>.part` until [`FileWriter::finish`] renames it. Each call
/// runs on a blocking thread, one batch at a time.
struct FileWriter {
    file: PartFile,
    writer: RowWriter,
    columns: Vec<usize>,
    logicals: Vec<LogicalType>,
    values: Vec<Value>,
}

impl FileWriter {
    fn create(
        path: &Path,
        encoding: TextEncoding,
        mut writer: RowWriter,
        columns: Vec<usize>,
        logicals: Vec<LogicalType>,
        bytes: Arc<AtomicU64>,
    ) -> io::Result<Self> {
        // Dropped on any error: the temporary file goes.
        let mut file = PartFile::create(path, encoding)?;
        file.writer().report_to(bytes);
        writer.begin(file.writer())?;
        Ok(Self {
            file,
            writer,
            values: Vec::with_capacity(columns.len()),
            columns,
            logicals,
        })
    }

    fn write(&mut self, batch: &RecordBatch) -> io::Result<()> {
        for row in 0..batch.num_rows() {
            self.values.clear();
            self.values.extend(
                self.columns
                    .iter()
                    .zip(&self.logicals)
                    .map(|(&c, &logical)| {
                        Value::from_array(batch.column(c).as_ref(), row, logical)
                            .unwrap_or(Value::Null)
                    }),
            );
            self.writer.row(&self.values, self.file.writer())?;
        }
        Ok(())
    }

    fn finish(mut self) -> io::Result<FileSummary> {
        let written = self.writer.finish(self.file.writer())?;
        self.file.commit(written as u64)
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
    path: &Path,
    export: &StreamExport,
    cancel: &ExportCancel,
    progress: &(dyn Fn(ExportProgress) + Send + Sync),
) -> Result<FileSummary, ExportError> {
    let io_error = |e: io::Error| ExportError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    };
    let bytes = Arc::new(AtomicU64::new(0));
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
                    return Ok(summary);
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
                let logicals: Vec<_> = metas.iter().map(|m| m.logical).collect();
                let writer = RowWriter::new(
                    export.format.clone(),
                    export.header,
                    export.csv.clone(),
                    session.dialect().as_ref(),
                    metas,
                );
                let target = path.to_path_buf();
                let (encoding, bytes) = (export.encoding, bytes.clone());
                match blocking(move || {
                    FileWriter::create(&target, encoding, writer, columns, logicals, bytes)
                })
                .await?
                {
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
}
