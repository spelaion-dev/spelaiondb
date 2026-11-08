//! Importing data into a table (ADR-038): a file or pasted text, read as CSV, TSV, a JSON array
//! or a Markdown table, into an existing table or a new one.
//!
//! 1. [`preview`] reads a sample of the source: its columns, the type each one looks like and
//!    the first rows. Nothing touches the database.
//! 2. [`plan_import`] writes, in the core, what will run: `DROP TABLE` (replace), `CREATE TABLE`
//!    or `ALTER TABLE … ADD` for new columns, and the statement the rows go through, and says
//!    which of them the guard asks to confirm (ADR-029). The UI shows them; it never sends SQL.
//! 3. [`run_import`] writes the same statements again, refuses a read-only connection and an
//!    unconfirmed plan that needed a confirmation, runs the DDL, and then loads the rows in
//!    transactions of `commit_every` rows through [`Connection::insert_rows`], which binds
//!    every value (or sends it as `COPY` data): nothing from the source is ever SQL text except
//!    the quoted names of the columns. A cancel or an error rolls back the transaction it
//!    happened in, so only whole committed batches stay.
//!
//! The import runs on a session of its own (never an editor tab's, which may hold a
//! transaction, ADR-031), opened and closed by the caller.
//!
//! [`Connection::insert_rows`]: spelaion_driver_api::Connection::insert_rows

mod detect;
mod source;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use spelaion_config::Safeguards;
use spelaion_driver_api::{
    CancelHandle, Dialect, DriverError, InsertTarget, ObjectKind, TableInfo, TransactionState,
};

pub use detect::ColumnKind;
pub use source::{
    ImportFormat, ImportSource, MAX_COLUMNS, MAX_RECORD_BYTES, MAX_WHOLE_FILE, ParseOptions,
    SourceEncoding, format_for_path, guess_options, text_format,
};

use crate::Session;
use crate::edit::run_one;
use crate::guard::{Risk, script_risks};
use crate::objects::qualified_name;
use detect::{Formats, KindDetector, Normalizer, TargetKind, parse_offset, target_kind};
use source::RowReader;

/// Rows of the source the preview shows.
pub const IMPORT_PREVIEW_ROWS: usize = 50;
/// Characters of a value the preview shows.
pub const IMPORT_PREVIEW_CHARS: usize = 200;
/// Rows read to detect the columns' types, unless asked otherwise.
pub const DEFAULT_SAMPLE_ROWS: usize = 1_000;
/// The most rows a preview may read.
pub const MAX_SAMPLE_ROWS: usize = 100_000;
/// Rows handed to the driver at a time.
pub const IMPORT_BATCH_ROWS: usize = 5_000;
/// Rows per transaction, unless asked otherwise.
pub const DEFAULT_COMMIT_EVERY: u64 = 50_000;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("could not read {path}: {message}")]
    Io { path: String, message: String },
    /// The source cannot be read as asked (a line too long, a quote that never closes…).
    #[error("{0}")]
    Source(String),
    /// The request itself is wrong (a column mapped twice, a type that is not one…).
    #[error("{0}")]
    Invalid(String),
    #[error("a table named {0} already exists")]
    TableExists(String),
    #[error("this connection is read-only: nothing can be imported into it")]
    ReadOnly,
    #[error("this import needs a confirmation before it runs")]
    NeedsConfirmation,
    #[error("{0}")]
    Database(#[from] DriverError),
    /// A statement of the plan failed; the rows were not loaded.
    #[error("{statement} failed: {message}")]
    Statement { statement: String, message: String },
    #[error(
        "the import was cancelled: {committed} rows were committed before; the rest of that batch was rolled back"
    )]
    Cancelled { committed: u64 },
    #[error(
        "rows {first}–{last} could not be loaded: {message}. {committed} rows were committed before; that batch was rolled back"
    )]
    Failed {
        first: u64,
        last: u64,
        committed: u64,
        message: String,
    },
}

/// A column of the source as the preview saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewColumn {
    pub name: String,
    pub kind: ColumnKind,
    /// The type a new column gets on this engine.
    pub sql_type: String,
    /// A few distinct values of the sample.
    pub samples: Vec<String>,
    /// NULLs in the sample.
    pub nulls: usize,
}

/// What [`preview`] read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPreview {
    /// The options used (guessed ones, when none were given).
    pub options: ParseOptions,
    /// The encoding the source was read in (never `Auto`).
    pub encoding: SourceEncoding,
    pub columns: Vec<PreviewColumn>,
    /// The first rows, values cut to [`IMPORT_PREVIEW_CHARS`].
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows read to detect the types.
    pub sampled: usize,
    /// The source has more rows than were read.
    pub more: bool,
    /// A name for a new table: the file's name, or `imported` for pasted text.
    pub suggested_table: String,
}

/// Reads the start of `source` (blocking: call it off the async runtime). `options: None`
/// guesses them from the source. `engine` is the dialect name the new columns' types are for.
pub fn preview(
    source: &ImportSource,
    options: Option<&ParseOptions>,
    sample_rows: usize,
    engine: &str,
) -> Result<ImportPreview, ImportError> {
    let options = match options {
        Some(options) => options.clone(),
        None => guess_options(source)?,
    };
    let formats = formats(&options)?;
    let mut reader = RowReader::open(source, &options)?;
    let mut detectors = vec![KindDetector::default(); reader.columns.len()];
    let mut samples: Vec<Vec<String>> = vec![Vec::new(); reader.columns.len()];
    let mut rows = Vec::new();
    let limit = sample_rows.clamp(1, MAX_SAMPLE_ROWS);
    let mut more = false;
    while let Some(row) = reader.next_row()? {
        if reader.rows as usize > limit {
            more = true;
            break;
        }
        for (i, value) in row.iter().enumerate() {
            detectors[i].observe(value.as_deref(), &formats);
            if let Some(value) = value
                && samples[i].len() < 3
                && !samples[i].contains(value)
            {
                samples[i].push(cut(value));
            }
        }
        if rows.len() < IMPORT_PREVIEW_ROWS {
            rows.push(
                row.into_iter()
                    .map(|v| v.map(|v| cut(&v)))
                    .collect::<Vec<_>>(),
            );
        }
    }
    let columns = reader
        .columns
        .iter()
        .zip(detectors)
        .zip(samples)
        .map(|((name, detector), samples)| PreviewColumn {
            name: name.clone(),
            kind: detector.kind(),
            sql_type: detector.sql_type(engine),
            samples,
            nulls: detector.nulls,
        })
        .collect();
    Ok(ImportPreview {
        sampled: (reader.rows as usize).min(limit),
        more,
        encoding: reader.encoding,
        columns,
        rows,
        suggested_table: suggested_table(source),
        options,
    })
}

fn cut(value: &str) -> String {
    match value.char_indices().nth(IMPORT_PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &value[..end]),
        None => value.to_string(),
    }
}

/// A table name from a file's name: letters, digits and `_` (others become `_`), never starting
/// with a digit, lower case.
pub fn suggested_table(source: &ImportSource) -> String {
    let stem = match source {
        ImportSource::File(path) => path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string(),
        ImportSource::Text(_) => String::new(),
    };
    let mut name: String = stem
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or('_')
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .chars()
        .take(60)
        .collect();
    if name.is_empty() {
        name = "imported".into();
    } else if name.starts_with(|c: char| c.is_ascii_digit()) {
        name = format!("t_{name}");
    }
    name
}

fn formats(options: &ParseOptions) -> Result<Formats, ImportError> {
    let nonempty = |f: &Option<String>| f.as_ref().filter(|f| !f.trim().is_empty()).cloned();
    Ok(Formats {
        date: nonempty(&options.date_format),
        timestamp: nonempty(&options.timestamp_format),
    })
}

/// Where the rows go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportTarget {
    Existing {
        schema: String,
        table: String,
    },
    /// A table created from the source's columns.
    New {
        schema: String,
        table: String,
        if_exists: IfExists,
    },
}

/// What a new table does when its name is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IfExists {
    Fail,
    /// Load into the table that is there.
    Append,
    /// Drop it and create it again (the guard asks first: a `DROP`).
    Replace,
}

/// Where one column of the source goes, in the source's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnMapping {
    /// Not imported (deleted from the preview).
    Skip,
    /// Into this column of the table.
    Existing(String),
    /// Into a new column: of the new table, or added to the existing one.
    New { name: String, sql_type: String },
}

/// Everything an import needs; the core writes every statement from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRequest {
    pub source: ImportSource,
    pub options: ParseOptions,
    pub target: ImportTarget,
    /// One per column of the source.
    pub columns: Vec<ColumnMapping>,
    /// Rows per transaction; 0: everything in one transaction.
    pub commit_every: u64,
}

/// What will run, to show before it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportPlan {
    /// Run first, in order: `DROP TABLE`, `CREATE TABLE`, `ALTER TABLE … ADD`.
    pub statements: Vec<String>,
    /// How the rows go in: `COPY … FROM STDIN` or `INSERT … VALUES (?, …)` with bound values.
    pub insert: String,
    /// The table and the columns the rows go to.
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    /// The table is created by the import.
    pub creates_table: bool,
    /// What the guard asks to confirm: the position of the statement (`statements.len()` is the
    /// insert) and why.
    pub risks: Vec<(usize, Risk)>,
}

impl ImportPlan {
    pub fn needs_confirmation(&self) -> bool {
        !self.risks.is_empty()
    }
}

/// The plan with what the load needs besides.
struct Resolved {
    plan: ImportPlan,
    target: InsertTarget,
    /// For each target column, the source column it reads.
    sources: Vec<usize>,
    kinds: Vec<TargetKind>,
}

/// Writes what an import will run, after checking the request against the table (blocking file
/// read of the source's header included: it is small). A read-only connection is refused.
pub async fn plan_import(
    session: &Session,
    request: &ImportRequest,
    safeguards: &Safeguards,
    read_only: bool,
) -> Result<ImportPlan, ImportError> {
    Ok(resolve(session, request, safeguards, read_only).await?.plan)
}

async fn resolve(
    session: &Session,
    request: &ImportRequest,
    safeguards: &Safeguards,
    read_only: bool,
) -> Result<Resolved, ImportError> {
    if read_only {
        return Err(ImportError::ReadOnly);
    }
    if !session.connection().capabilities().insert_rows {
        return Err(ImportError::Invalid(
            "this connection cannot import rows (a SQLite file on a server is reached through its \
             sqlite3 shell, which cannot bind values)"
                .into(),
        ));
    }
    let dialect = session.dialect().clone();
    let dialect = dialect.as_ref();
    let source_columns = source_columns(request).await?;
    if request.columns.len() != source_columns.len() {
        return Err(ImportError::Invalid(format!(
            "the source has {} columns, the mapping {}: preview it again",
            source_columns.len(),
            request.columns.len()
        )));
    }
    zone(&request.options)?;
    formats(&request.options)?;
    let (schema, table, if_exists, wanted_new) = match &request.target {
        ImportTarget::Existing { schema, table } => (schema, table, None, false),
        ImportTarget::New {
            schema,
            table,
            if_exists,
        } => (schema, table, Some(*if_exists), true),
    };
    check_name("table", table)?;
    let existing = find_table(session, schema, table).await?;
    let mut statements = Vec::new();
    let table_info = match (existing, if_exists) {
        (None, _) if !wanted_new => {
            return Err(ImportError::Invalid(format!("there is no table {table}")));
        }
        (None, _) => None,
        (Some(_), Some(IfExists::Fail)) => return Err(ImportError::TableExists(table.clone())),
        (Some(name), Some(IfExists::Replace)) => {
            statements.push(format!(
                "DROP TABLE {}",
                qualified_name(dialect, schema, &[name])
            ));
            None
        }
        (Some(name), _) => Some(session.connection().describe_table(schema, &name).await?),
    };
    let table = table_info
        .as_ref()
        .map_or(table.clone(), |t| t.object.name.clone());
    let creates_table = table_info.is_none();
    let qualified = qualified_name(dialect, schema, std::slice::from_ref(&table));

    let mut targets: Vec<String> = Vec::new();
    let mut sources = Vec::new();
    let mut kinds = Vec::new();
    let mut new_columns: Vec<(String, String)> = Vec::new();
    for (i, mapping) in request.columns.iter().enumerate() {
        let (name, kind) = match mapping {
            ColumnMapping::Skip => continue,
            ColumnMapping::Existing(name) => {
                let Some(info) = table_info.as_ref() else {
                    return Err(ImportError::Invalid(format!(
                        "{name}: a new table has no existing columns"
                    )));
                };
                let column = table_column(info, name)
                    .ok_or_else(|| ImportError::Invalid(format!("{table} has no column {name}")))?;
                (column.name.clone(), target_kind(&column.native_type))
            }
            ColumnMapping::New { name, sql_type } => {
                check_name("column", name)?;
                let sql_type = sql_type.trim();
                // Appending to a table that has the column already: it is used as it is.
                if let Some(column) = table_info.as_ref().and_then(|t| table_column(t, name)) {
                    (column.name.clone(), target_kind(&column.native_type))
                } else {
                    check_sql_type(sql_type)?;
                    new_columns.push((name.clone(), sql_type.to_string()));
                    (name.clone(), target_kind(sql_type))
                }
            }
        };
        if targets.iter().any(|t| t.eq_ignore_ascii_case(&name)) {
            return Err(ImportError::Invalid(format!(
                "two columns of the source go to {name}"
            )));
        }
        targets.push(name);
        sources.push(i);
        kinds.push(kind);
    }
    if targets.is_empty() {
        return Err(ImportError::Invalid("no column is imported".into()));
    }
    if creates_table {
        let columns: Vec<String> = new_columns
            .iter()
            .map(|(name, sql_type)| format!("  {} {sql_type}", dialect.quote_identifier(name)))
            .collect();
        statements.push(format!(
            "CREATE TABLE {qualified} (\n{}\n)",
            columns.join(",\n")
        ));
    } else {
        let add = if dialect.name() == "sqlserver" {
            "ADD"
        } else {
            "ADD COLUMN"
        };
        for (name, sql_type) in &new_columns {
            statements.push(format!(
                "ALTER TABLE {qualified} {add} {} {sql_type}",
                dialect.quote_identifier(name)
            ));
        }
    }
    let target = InsertTarget {
        schema: schema.clone(),
        table: table.clone(),
        columns: targets.clone(),
    };
    let insert = insert_display(dialect, &target);
    // The guard reads the rows' statement as the INSERT it is (`COPY` included).
    let guarded = target.insert_sql(dialect, 1, |_| "NULL".into());
    let mut risks = Vec::new();
    for (index, statement) in statements.iter().chain([&guarded]).enumerate() {
        if let Some(risk) = script_risks(dialect, statement, safeguards)
            .into_iter()
            .map(|r| r.risk)
            .next()
        {
            risks.push((index, risk));
        }
    }
    Ok(Resolved {
        plan: ImportPlan {
            statements,
            insert,
            schema: schema.clone(),
            table,
            columns: targets,
            creates_table,
            risks,
        },
        target,
        sources,
        kinds,
    })
}

/// The statement the rows go through, as shown: what the driver sends, values apart.
fn insert_display(dialect: &dyn Dialect, target: &InsertTarget) -> String {
    match dialect.name() {
        "postgres" => format!(
            "COPY {} {} FROM STDIN",
            target.qualified(dialect),
            target.column_list(dialect)
        ),
        "sqlserver" => target.insert_sql(dialect, 1, |n| format!("@P{n}")),
        _ => target.insert_sql(dialect, 1, |_| "?".into()),
    }
}

/// The source's column names (its header, or `columnN`).
async fn source_columns(request: &ImportRequest) -> Result<Vec<String>, ImportError> {
    let source = request.source.clone();
    let options = request.options.clone();
    tokio::task::spawn_blocking(move || RowReader::open(&source, &options).map(|r| r.columns))
        .await
        .map_err(|e| ImportError::Source(e.to_string()))?
}

/// The table's name as the catalog writes it, when there is one with this name (compared
/// without case except on PostgreSQL, where quoted names keep it).
async fn find_table(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<Option<String>, ImportError> {
    let objects = session.connection().list_objects(schema).await?;
    let same = |name: &str| {
        if session.dialect().name() == "postgres" {
            name == table
        } else {
            name.eq_ignore_ascii_case(table)
        }
    };
    let found = objects
        .iter()
        .find(|o| o.name == table)
        .or_else(|| objects.iter().find(|o| same(&o.name)));
    match found {
        None => Ok(None),
        Some(o) if matches!(o.kind, ObjectKind::Table | ObjectKind::PartitionedTable) => {
            Ok(Some(o.name.clone()))
        }
        Some(o) => Err(ImportError::Invalid(format!(
            "{} is a view, not a table: rows cannot be imported into it",
            o.name
        ))),
    }
}

fn table_column<'a>(
    table: &'a TableInfo,
    name: &str,
) -> Option<&'a spelaion_driver_api::ColumnInfo> {
    table.columns.iter().find(|c| c.name == name).or_else(|| {
        table
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    })
}

/// A table or column name: not empty, no control characters, at most 128 characters. It is
/// always quoted with the engine's quoting.
fn check_name(what: &str, name: &str) -> Result<(), ImportError> {
    if name.trim().is_empty() {
        return Err(ImportError::Invalid(format!("the {what} needs a name")));
    }
    if name.chars().any(char::is_control) || name.chars().count() > 128 {
        return Err(ImportError::Invalid(format!(
            "{name:?} cannot be a {what} name: at most 128 characters and no control characters"
        )));
    }
    Ok(())
}

/// A column type typed by the user goes into `CREATE TABLE`/`ALTER TABLE` as written, so it may
/// only be words, digits and one parenthesized list of numbers (`numeric(12, 2)`,
/// `timestamp with time zone`, `nvarchar(max)`): no quotes, punctuation or comments that could
/// end the statement or start another.
pub fn check_sql_type(sql_type: &str) -> Result<(), ImportError> {
    let invalid = || {
        ImportError::Invalid(format!(
            "{sql_type:?} is not a column type: use words, digits and one list of sizes in parentheses"
        ))
    };
    let sql_type = sql_type.trim();
    if sql_type.is_empty()
        || sql_type.len() > 64
        || !sql_type.starts_with(|c: char| c.is_ascii_alphabetic())
    {
        return Err(invalid());
    }
    let (words, rest) = match sql_type.split_once('(') {
        Some((words, rest)) => (words, Some(rest)),
        None => (sql_type, None),
    };
    let word = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ' ')
    };
    if !word(words) {
        return Err(invalid());
    }
    if let Some(rest) = rest {
        let (sizes, after) = rest.split_once(')').ok_or_else(invalid)?;
        let sizes_ok = sizes.split(',').all(|s| {
            let s = s.trim();
            !s.is_empty()
                && (s.bytes().all(|b| b.is_ascii_digit()) || s.eq_ignore_ascii_case("max"))
        });
        if !sizes_ok || !word(after) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn zone(options: &ParseOptions) -> Result<Option<chrono::FixedOffset>, ImportError> {
    match options.time_zone.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(text) => parse_offset(text).map(Some).ok_or_else(|| {
            ImportError::Invalid(format!(
                "{text:?} is not a time zone: use UTC or an offset such as +02:00"
            ))
        }),
    }
}

/// Rows read and committed so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportProgress {
    pub read: u64,
    pub committed: u64,
}

/// What a finished import did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub schema: String,
    pub table: String,
    pub rows: u64,
    pub created_table: bool,
    pub elapsed: Duration,
}

/// Stops a running import: the statement running (the engine's cancel) and the load, which
/// rolls back the batch it was in. Created before the import's session exists, so a cancel that
/// comes while it is still connecting is not lost.
#[derive(Default)]
pub struct ImportCancel {
    cancelled: AtomicBool,
    handle: std::sync::Mutex<Option<Arc<dyn CancelHandle>>>,
}

impl ImportCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// The session whose statement [`ImportCancel::cancel`] stops.
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

/// How often progress is reported.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Runs an import on `session` (the import's own): the core writes the plan again from the
/// request, refuses a read-only connection and a plan the guard asks to confirm unless
/// `confirmed`, runs its statements, and loads the rows.
#[allow(clippy::too_many_arguments)]
pub async fn run_import(
    session: &Session,
    request: &ImportRequest,
    safeguards: &Safeguards,
    read_only: bool,
    confirmed: bool,
    cancel: &ImportCancel,
    progress: &(dyn Fn(ImportProgress) + Send + Sync),
) -> Result<ImportSummary, ImportError> {
    let started = Instant::now();
    let resolved = resolve(session, request, safeguards, read_only).await?;
    if resolved.plan.needs_confirmation() && !confirmed {
        return Err(ImportError::NeedsConfirmation);
    }
    // The import is a transaction of its own (ADR-031).
    if let Ok(TransactionState::Open | TransactionState::Failed) =
        session.connection().transaction_state().await
    {
        return Err(ImportError::Invalid(
            "a transaction is open on this session: commit or roll it back first".into(),
        ));
    }
    if cancel.is_cancelled() {
        return Err(ImportError::Cancelled { committed: 0 });
    }
    for statement in &resolved.plan.statements {
        run_one(session, statement)
            .await
            .map_err(|message| ImportError::Statement {
                statement: first_line(statement),
                message,
            })?;
    }
    let normalizer = Normalizer {
        kinds: resolved.kinds.clone(),
        formats: formats(&request.options)?,
        zone: zone(&request.options)?,
        mysql: session.dialect().name() == "mysql",
    };
    let rows = load(session, request, &resolved, &normalizer, cancel, progress).await?;
    Ok(ImportSummary {
        schema: resolved.plan.schema,
        table: resolved.plan.table,
        rows,
        created_table: resolved.plan.creates_table,
        elapsed: started.elapsed(),
    })
}

fn first_line(statement: &str) -> String {
    let line = statement.lines().next().unwrap_or("");
    if line.len() < statement.len() {
        format!("{} …", line.trim_end_matches(" ("))
    } else {
        line.to_string()
    }
}

type Batch = Result<Vec<Vec<Option<String>>>, ImportError>;

/// Reads the source in a blocking thread, a batch at a time: at most two batches wait.
fn read_batches(request: &ImportRequest) -> tokio::sync::mpsc::Receiver<Batch> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Batch>(2);
    let source = request.source.clone();
    let options = request.options.clone();
    tokio::task::spawn_blocking(move || {
        let mut reader = match RowReader::open(&source, &options) {
            Ok(reader) => reader,
            Err(e) => {
                let _ = tx.blocking_send(Err(e));
                return;
            }
        };
        loop {
            let mut batch = Vec::with_capacity(IMPORT_BATCH_ROWS);
            let mut failed = None;
            while batch.len() < IMPORT_BATCH_ROWS {
                match reader.next_row() {
                    Ok(Some(row)) => batch.push(row),
                    Ok(None) => break,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            let done = batch.len() < IMPORT_BATCH_ROWS;
            if !batch.is_empty() && tx.blocking_send(Ok(batch)).is_err() {
                return;
            }
            if let Some(e) = failed {
                let _ = tx.blocking_send(Err(e));
                return;
            }
            if done {
                return;
            }
        }
    });
    rx
}

async fn load(
    session: &Session,
    request: &ImportRequest,
    resolved: &Resolved,
    normalizer: &Normalizer,
    cancel: &ImportCancel,
    progress: &(dyn Fn(ImportProgress) + Send + Sync),
) -> Result<u64, ImportError> {
    let dialect = session.dialect().name();
    let begin = match dialect {
        "sqlserver" => "BEGIN TRANSACTION",
        "mysql" => "START TRANSACTION",
        _ => "BEGIN",
    };
    let mut batches = read_batches(request);
    let mut read = 0u64;
    let mut committed = 0u64;
    let mut in_transaction = 0u64;
    let mut open = false;
    let mut reported = Instant::now();
    let rollback = |open: bool| async move {
        if open {
            // Best effort: if even this fails, closing the session rolls it back.
            let _ = run_one(session, "ROLLBACK").await;
        }
    };
    while let Some(batch) = batches.recv().await {
        let batch = match batch {
            Ok(batch) => batch,
            Err(e) => {
                rollback(open).await;
                return Err(match e {
                    ImportError::Source(message) => ImportError::Failed {
                        first: read + 1,
                        last: read + 1,
                        committed,
                        message,
                    },
                    other => other,
                });
            }
        };
        let mut rows = Vec::with_capacity(batch.len());
        for (offset, row) in batch.into_iter().enumerate() {
            let mut values = Vec::with_capacity(resolved.sources.len());
            for (column, &source) in resolved.sources.iter().enumerate() {
                let Some(text) = row[source].clone() else {
                    values.push(None);
                    continue;
                };
                match normalizer.value(column, text) {
                    Ok(text) => values.push(Some(text)),
                    Err(message) => {
                        rollback(open).await;
                        let row = read + offset as u64 + 1;
                        return Err(ImportError::Failed {
                            first: row,
                            last: row,
                            committed,
                            message: format!("{}: {message}", resolved.target.columns[column]),
                        });
                    }
                }
            }
            rows.push(values);
        }
        // Cut where a transaction ends, so commits fall every `commit_every` rows.
        let mut rows = rows.into_iter().peekable();
        while rows.peek().is_some() {
            if cancel.is_cancelled() {
                rollback(open).await;
                return Err(ImportError::Cancelled { committed });
            }
            let room = if request.commit_every == 0 {
                u64::MAX
            } else {
                request.commit_every - in_transaction
            };
            let part: Vec<_> = rows
                .by_ref()
                .take(usize::try_from(room).unwrap_or(usize::MAX))
                .collect();
            let count = part.len() as u64;
            if !open {
                run_one(session, begin)
                    .await
                    .map_err(|message| ImportError::Statement {
                        statement: begin.into(),
                        message,
                    })?;
                open = true;
            }
            let inserted = session
                .connection()
                .insert_rows(&resolved.target, part)
                .await;
            if let Err(e) = inserted {
                rollback(open).await;
                if cancel.is_cancelled() {
                    return Err(ImportError::Cancelled { committed });
                }
                return Err(ImportError::Failed {
                    first: read + 1,
                    last: read + count,
                    committed,
                    message: e.to_string(),
                });
            }
            read += count;
            in_transaction += count;
            if request.commit_every > 0 && in_transaction >= request.commit_every {
                if cancel.is_cancelled() {
                    rollback(open).await;
                    return Err(ImportError::Cancelled { committed });
                }
                commit(
                    session,
                    &mut open,
                    read,
                    &mut committed,
                    &mut in_transaction,
                )
                .await?;
            }
            if reported.elapsed() >= PROGRESS_EVERY {
                progress(ImportProgress { read, committed });
                reported = Instant::now();
            }
        }
    }
    if cancel.is_cancelled() {
        rollback(open).await;
        return Err(ImportError::Cancelled { committed });
    }
    if open {
        commit(
            session,
            &mut open,
            read,
            &mut committed,
            &mut in_transaction,
        )
        .await?;
    }
    progress(ImportProgress { read, committed });
    Ok(committed)
}

async fn commit(
    session: &Session,
    open: &mut bool,
    read: u64,
    committed: &mut u64,
    in_transaction: &mut u64,
) -> Result<(), ImportError> {
    let result = run_one(session, "COMMIT").await;
    *open = false;
    match result {
        Ok(_) => {
            *committed = read;
            *in_transaction = 0;
            Ok(())
        }
        Err(message) => {
            let _ = run_one(session, "ROLLBACK").await;
            Err(ImportError::Failed {
                first: *committed + 1,
                last: read,
                committed: *committed,
                message: format!("the commit failed: {message}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_typed_types_cannot_end_the_statement() {
        for ok in [
            "INTEGER",
            "numeric(12, 2)",
            "timestamp with time zone",
            "NVARCHAR(MAX)",
            "double precision",
            "DATETIME(6)",
            "varchar(80) binary",
        ] {
            assert!(check_sql_type(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "int); drop table users; --",
            "int -- comment",
            "text default 'x'",
            "int /* */",
            "int references users(id)",
            "int check (a > 0)",
            "(int)",
            "int(1) (2)",
            "int;",
            "\"int\"",
            "int\nprimary key",
            "int(1,)",
        ] {
            assert!(check_sql_type(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn names_for_new_tables() {
        assert_eq!(
            suggested_table(&ImportSource::File("C:/data/Sales 2024 (Q1).csv".into())),
            "sales_2024__q1"
        );
        assert_eq!(
            suggested_table(&ImportSource::File("/tmp/2024.csv".into())),
            "t_2024"
        );
        assert_eq!(
            suggested_table(&ImportSource::Text("a,b".into())),
            "imported"
        );
        assert!(check_name("column", "a\u{0}b").is_err());
        assert!(check_name("column", "  ").is_err());
        assert!(check_name("column", "Robert'); DROP TABLE students;--").is_ok());
    }

    #[test]
    fn the_preview_detects_types_and_cuts_long_values() {
        let long = "y".repeat(IMPORT_PREVIEW_CHARS + 50);
        let text =
            format!("id,amount,flag,code,mixed,note\n1,2.50,true,007,1,{long}\n2,3,false,012,x,\n");
        let preview = preview(&ImportSource::Text(text), None, 10, "postgres").unwrap();
        let kinds: Vec<ColumnKind> = preview.columns.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                ColumnKind::Integer,
                ColumnKind::Decimal,
                ColumnKind::Boolean,
                ColumnKind::Text,
                ColumnKind::Text,
                ColumnKind::Text,
            ]
        );
        assert_eq!(preview.columns[1].sql_type, "NUMERIC");
        assert_eq!(preview.columns[5].nulls, 1);
        assert_eq!(preview.columns[0].samples, ["1", "2"]);
        assert_eq!(preview.sampled, 2);
        assert!(!preview.more);
        let cell = preview.rows[0][5].as_deref().unwrap();
        assert_eq!(cell.chars().count(), IMPORT_PREVIEW_CHARS + 1);
        assert!(cell.ends_with('…'));
        assert_eq!(preview.suggested_table, "imported");

        let many = (0..30).fold(String::from("n\n"), |s, i| format!("{s}{i}\n"));
        let preview = super::preview(&ImportSource::Text(many), None, 10, "mysql").unwrap();
        assert_eq!((preview.sampled, preview.more), (10, true));
    }
}
