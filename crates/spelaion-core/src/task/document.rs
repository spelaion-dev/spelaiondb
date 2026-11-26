//! A task's document (ADR-089 point 2): one JSON document with a version, written from and read
//! into the core's own request types, never stored as the UI sent it. It holds options and
//! paths, never a secret, and is checked when saved and every time it is read back.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spelaion_config::{ConnectionId, ConnectionKey};

use super::TaskError;
use crate::Redactor;
use crate::backup::{
    DuckdbExportFormat, DumpOptions, RestoreFormat, RestoreOptions, render_file_name,
    split_extra_args,
};
use crate::export::{CsvOptions, ExportFormat, JsonOptions, SqlOptions, TextEncoding};
use crate::export_stream::{FileNamePattern, StreamExport};
use crate::import::{ColumnMapping, ImportTarget, ParseOptions, check_sql_type};
use crate::script::OnError;

/// The version this program writes and the newest it reads.
pub const TASK_DOCUMENT_VERSION: u32 = 1;

/// The longest query a task keeps, in bytes.
const MAX_SQL: usize = 1024 * 1024;
/// The longest other text a task keeps (a filter, an order, a name, extra arguments), in bytes.
const MAX_TEXT: usize = 64 * 1024;
/// Most tables an export of explorer tables names.
const MAX_TABLES: usize = 10_000;
/// Most columns an import maps.
const MAX_COLUMNS: usize = 10_000;
/// Most folders a personal connection is resolved with (ADR-075's window holds ten).
const MAX_OPEN_FOLDERS: usize = 10;

/// A whole task: its connection and what it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDocument {
    pub version: u32,
    pub connection: TaskConnection,
    pub request: TaskRequest,
}

/// The connection a task runs on (ADR-075's identity), and for a personal connection the
/// repository folders it was saved with, which it is resolved with wherever it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskConnection {
    /// The repository's canonical root; `None` for a personal connection.
    pub folder: Option<PathBuf>,
    /// The connection's id as `ConnectionId` writes it (`prod/orders`, `~/local`).
    pub id: String,
    /// For a personal connection: the repository folders open when the task was saved.
    #[serde(default)]
    pub open_folders: Vec<PathBuf>,
}

impl TaskConnection {
    /// The connection's identity, or why it is not one.
    pub fn key(&self) -> Result<ConnectionKey, TaskError> {
        if self.id.trim().is_empty() {
            return Err(TaskError::Invalid("a task needs a connection".into()));
        }
        ConnectionKey::new(self.folder.clone(), ConnectionId::parse(&self.id)).ok_or_else(|| {
            TaskError::Invalid(format!(
                "the connection {} does not belong to the folder the task names",
                self.id
            ))
        })
    }
}

/// What a task runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum TaskRequest {
    /// A streaming export (ADR-034).
    Export(ExportTask),
    /// An import from a file (ADR-038).
    Import(ImportTask),
    /// A dump with the vendor's tool (ADR-049).
    Dump(TaskDumpRequest),
    /// A restore with the vendor's tool (ADR-049).
    Restore(RestoreTask),
    /// A copy of a SQLite database or an export of a DuckDB one (ADR-049 point 8).
    EmbeddedDump(TaskEmbeddedDump),
    /// A DuckDB export folder loaded into the database (ADR-049 point 8).
    DuckdbImport(TaskDuckdbImport),
    /// The run of a `.sql` file (ADR-049 point 9).
    RunFile(RunFileTask),
}

/// A task's kind, for lists and confirmations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Export,
    Import,
    Dump,
    Restore,
    EmbeddedDump,
    DuckdbImport,
    RunFile,
}

impl TaskKind {
    /// How the kind reads in a list.
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Export => "export",
            TaskKind::Import => "import",
            TaskKind::Dump => "dump",
            TaskKind::Restore => "restore",
            TaskKind::EmbeddedDump => "backup copy",
            TaskKind::DuckdbImport => "DuckDB import",
            TaskKind::RunFile => "SQL file run",
        }
    }
}

impl TaskRequest {
    pub fn kind(&self) -> TaskKind {
        match self {
            TaskRequest::Export(_) => TaskKind::Export,
            TaskRequest::Import(_) => TaskKind::Import,
            TaskRequest::Dump(_) => TaskKind::Dump,
            TaskRequest::Restore(_) => TaskKind::Restore,
            TaskRequest::EmbeddedDump(_) => TaskKind::EmbeddedDump,
            TaskRequest::DuckdbImport(_) => TaskKind::DuckdbImport,
            TaskRequest::RunFile(_) => TaskKind::RunFile,
        }
    }
}

/// An export: what is read, how it is written and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTask {
    pub source: ExportSource,
    pub settings: ExportSettings,
    /// The folder the file (or the files) go to.
    pub folder: PathBuf,
    /// How each file is named (`{table}`, `{date}`…); an existing file is never overwritten:
    /// the new one is called `orders (2).csv`.
    pub names: FileNamePattern,
}

/// What an export reads (ADR-034 point 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExportSource {
    /// A query, run again: only a single read passes.
    Query { sql: String },
    /// A table or view with a table data tab's filter and order.
    #[serde(rename_all = "camelCase")]
    Table {
        schema: String,
        name: String,
        #[serde(default)]
        filter: String,
        #[serde(default)]
        order_by: String,
    },
    /// Tables and views chosen in the explorer, one file each (or one workbook as XLSX).
    Tables { tables: Vec<TableName> },
}

/// A table or view by schema and name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableName {
    pub schema: String,
    pub name: String,
}

/// How an export writes its file: [`StreamExport`] without its batch size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSettings {
    pub format: ExportFormat,
    #[serde(default)]
    pub header: bool,
    #[serde(default)]
    pub csv: CsvOptions,
    pub encoding: TextEncoding,
    #[serde(default)]
    pub gzip: bool,
    #[serde(default)]
    pub sql: SqlOptions,
    #[serde(default)]
    pub json: JsonOptions,
    /// A query's columns to write, by position and in this order; `None`: all of them.
    #[serde(default)]
    pub columns: Option<Vec<usize>>,
}

impl ExportSettings {
    /// The settings a run streams with.
    pub fn stream(&self) -> StreamExport {
        StreamExport {
            format: self.format.clone(),
            header: self.header,
            csv: self.csv.clone(),
            encoding: self.encoding,
            gzip: self.gzip,
            sql: self.sql.clone(),
            json: self.json,
            columns: self.columns.clone(),
            batch_size: crate::export_stream::DEFAULT_BATCH_ROWS,
        }
    }

    /// The settings of a [`StreamExport`] the dialog built.
    pub fn of(export: &StreamExport) -> Self {
        Self {
            format: export.format.clone(),
            header: export.header,
            csv: export.csv.clone(),
            encoding: export.encoding,
            gzip: export.gzip,
            sql: export.sql.clone(),
            json: export.json,
            columns: export.columns.clone(),
        }
    }
}

/// An import from a file (ADR-038): the file is read when the task runs, as it is then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportTask {
    pub file: PathBuf,
    pub options: ParseOptions,
    pub target: ImportTarget,
    /// One per column of the source.
    pub columns: Vec<ColumnMapping>,
    /// Rows per transaction; 0: everything in one transaction.
    #[serde(default)]
    pub commit_every: u64,
}

impl ImportTask {
    pub fn request(&self) -> crate::import::ImportRequest {
        crate::import::ImportRequest {
            source: crate::import::ImportSource::File(self.file.clone()),
            options: self.options.clone(),
            target: self.target.clone(),
            columns: self.columns.clone(),
            commit_every: self.commit_every,
        }
    }
}

/// Where the vendor's tools are looked for: as the dialog finds them, or only in a folder the
/// user chose. A task never names a program to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskTools {
    Detect,
    Folder { path: PathBuf },
}

impl TaskTools {
    pub fn source(&self) -> crate::backup::ToolSource {
        match self {
            TaskTools::Detect => crate::backup::ToolSource::Detect,
            TaskTools::Folder { path } => crate::backup::ToolSource::Folder(path.clone()),
        }
    }
}

/// A dump with the vendor's tool: [`crate::backup::DumpRequest`] as a task keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDumpRequest {
    pub tools: TaskTools,
    pub folder: PathBuf,
    /// The file name (`{database}`, `{connection}`, `{timestamp}`, `{date}`).
    pub name_pattern: String,
    #[serde(default)]
    pub extra_args: String,
    pub options: DumpOptions,
}

/// A restore with the vendor's tool: [`crate::backup::RestoreRequest`] as a task keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RestoreTask {
    pub tools: TaskTools,
    pub input: PathBuf,
    pub format: RestoreFormat,
    #[serde(default)]
    pub extra_args: String,
    pub options: RestoreOptions,
}

/// A SQLite copy (`VACUUM INTO`) or a DuckDB export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEmbeddedDump {
    pub folder: PathBuf,
    pub name_pattern: String,
    #[serde(default)]
    pub duckdb_format: DuckdbExportFormat,
}

/// A DuckDB export folder to load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDuckdbImport {
    pub folder: PathBuf,
}

/// A `.sql` file to run: its path, never its text (the file is the record).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunFileTask {
    pub file: PathBuf,
    /// What a failed statement does; asking is read as stopping, since nobody may be there.
    #[serde(default)]
    pub on_error: OnError,
}

fn invalid(message: impl Into<String>) -> TaskError {
    TaskError::Invalid(message.into())
}

fn absolute(path: &Path, what: &str) -> Result<(), TaskError> {
    if path.as_os_str().is_empty() {
        return Err(invalid(format!("choose the {what}")));
    }
    if !path.is_absolute() {
        return Err(invalid(format!(
            "the {what} must be a full path: {}",
            path.display()
        )));
    }
    if path.as_os_str().len() > MAX_TEXT || path.to_string_lossy().contains('\0') {
        return Err(invalid(format!("the {what} is not a path a task keeps")));
    }
    Ok(())
}

fn bounded(text: &str, max: usize, what: &str) -> Result<(), TaskError> {
    if text.len() > max || text.contains('\0') {
        return Err(invalid(format!("the {what} is too long for a task")));
    }
    Ok(())
}

fn name_part(text: &str, what: &str) -> Result<(), TaskError> {
    if text.trim().is_empty() {
        return Err(invalid(format!("the {what} has no name")));
    }
    bounded(text, MAX_TEXT, what)
}

/// A tool's file name pattern renders into a name today.
fn backup_pattern(pattern: &str) -> Result<(), TaskError> {
    bounded(pattern, crate::export_stream::MAX_NAME_PATTERN, "file name")?;
    render_file_name(pattern, "database", "connection", chrono::Local::now())
        .map(drop)
        .map_err(|e| invalid(format!("the file name: {e}")))
}

fn extra_args(text: &str) -> Result<(), TaskError> {
    bounded(text, MAX_TEXT, "extra arguments")?;
    split_extra_args(text)
        .map(drop)
        .map_err(|e| invalid(format!("the extra arguments: {e}")))
}

impl TaskDocument {
    /// A document of this version.
    pub fn new(connection: TaskConnection, request: TaskRequest) -> Self {
        Self {
            version: TASK_DOCUMENT_VERSION,
            connection,
            request,
        }
    }

    /// The checks a run by hand applies before anything starts, and the bounds of a task.
    pub fn check(&self) -> Result<(), TaskError> {
        if self.version != TASK_DOCUMENT_VERSION {
            return Err(invalid(format!(
                "a task of version {} (this program reads version {TASK_DOCUMENT_VERSION})",
                self.version
            )));
        }
        let key = self.connection.key()?;
        bounded(&self.connection.id, MAX_TEXT, "connection")?;
        if let Some(folder) = key.folder() {
            absolute(folder, "connection's folder")?;
            if !self.connection.open_folders.is_empty() {
                return Err(invalid(
                    "only a personal connection is resolved with other folders",
                ));
            }
        }
        if self.connection.open_folders.len() > MAX_OPEN_FOLDERS {
            return Err(invalid(format!(
                "a task remembers at most {MAX_OPEN_FOLDERS} folders"
            )));
        }
        for folder in &self.connection.open_folders {
            absolute(folder, "folder")?;
        }
        match &self.request {
            TaskRequest::Export(export) => check_export(export),
            TaskRequest::Import(import) => check_import(import),
            TaskRequest::Dump(dump) => {
                check_tools(&dump.tools)?;
                absolute(&dump.folder, "folder")?;
                backup_pattern(&dump.name_pattern)?;
                extra_args(&dump.extra_args)?;
                check_dump_options(&dump.options)
            }
            TaskRequest::Restore(restore) => {
                check_tools(&restore.tools)?;
                absolute(&restore.input, "file to restore")?;
                extra_args(&restore.extra_args)?;
                match &restore.options {
                    RestoreOptions::Postgres(_) => Ok(()),
                    RestoreOptions::Mysql(m) => match &m.database {
                        Some(database) => name_part(database, "database"),
                        None => Ok(()),
                    },
                }
            }
            TaskRequest::EmbeddedDump(dump) => {
                absolute(&dump.folder, "folder")?;
                backup_pattern(&dump.name_pattern)
            }
            TaskRequest::DuckdbImport(import) => absolute(&import.folder, "folder"),
            TaskRequest::RunFile(run) => absolute(&run.file, "SQL file"),
        }
    }

    /// Refuses a document any text of which holds one of `secrets` (the connection's resolved
    /// secrets, the password typed for it), and a tool's extra arguments that hold a
    /// `key=value` named like a secret (ADR-089 point 2). Says where it found one, never what.
    pub fn check_secrets(&self, secrets: &Redactor) -> Result<(), TaskError> {
        let value = serde_json::to_value(self).map_err(|e| invalid(e.to_string()))?;
        if let Some(path) = find_secret(&value, secrets, "the task") {
            return Err(TaskError::Secret(path));
        }
        let extra = match &self.request {
            TaskRequest::Dump(dump) => Some(&dump.extra_args),
            TaskRequest::Restore(restore) => Some(&restore.extra_args),
            TaskRequest::Export(_)
            | TaskRequest::Import(_)
            | TaskRequest::EmbeddedDump(_)
            | TaskRequest::DuckdbImport(_)
            | TaskRequest::RunFile(_) => None,
        };
        if let Some(extra) = extra
            && secrets.redact(extra) != *extra
        {
            return Err(TaskError::Secret("the extra arguments".into()));
        }
        Ok(())
    }

    /// The document as the store keeps it.
    pub fn to_json(&self) -> Result<String, TaskError> {
        serde_json::to_string(self).map_err(|e| invalid(format!("the task cannot be saved: {e}")))
    }

    /// A stored document read and checked; why not, when it cannot be used. A newer version
    /// than this program reads is never guessed at.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("it is not a task document: {e}"))?;
        match value.get("version").and_then(serde_json::Value::as_u64) {
            Some(v) if v == u64::from(TASK_DOCUMENT_VERSION) => {}
            Some(v) if v > u64::from(TASK_DOCUMENT_VERSION) => {
                return Err(format!(
                    "it was saved by a newer SpelaionDB (version {v}); update to run it"
                ));
            }
            Some(v) => return Err(format!("it has an unknown version ({v})")),
            None => return Err("it has no version".into()),
        }
        let document: Self = serde_json::from_value(value)
            .map_err(|e| format!("it does not read as a task: {e}"))?;
        document.check().map_err(|e| e.to_string())?;
        Ok(document)
    }
}

/// The first string of `value` that holds a known secret, named by where it is.
fn find_secret(value: &serde_json::Value, secrets: &Redactor, at: &str) -> Option<String> {
    match value {
        serde_json::Value::String(text) => secrets.holds_secret(text).then(|| at.to_string()),
        serde_json::Value::Array(items) => {
            items.iter().find_map(|item| find_secret(item, secrets, at))
        }
        serde_json::Value::Object(fields) => fields.iter().find_map(|(name, item)| {
            let at = match name.as_str() {
                "sql" => "the query".to_string(),
                "filter" => "the filter".to_string(),
                "orderBy" => "the order".to_string(),
                "extraArgs" => "the extra arguments".to_string(),
                "nullText" => "the NULL text".to_string(),
                "file" | "folder" | "input" | "path" => "a path".to_string(),
                _ => at.to_string(),
            };
            find_secret(item, secrets, &at)
        }),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => None,
    }
}

fn check_tools(tools: &TaskTools) -> Result<(), TaskError> {
    match tools {
        TaskTools::Detect => Ok(()),
        TaskTools::Folder { path } => absolute(path, "tools folder"),
    }
}

fn check_export(export: &ExportTask) -> Result<(), TaskError> {
    absolute(&export.folder, "folder")?;
    export.names.check().map_err(invalid)?;
    let settings = &export.settings;
    match &settings.format {
        ExportFormat::InList | ExportFormat::SqlInsertBatch { .. } => {
            return Err(invalid(
                "this format is only copied, never exported to a file",
            ));
        }
        ExportFormat::SqlInsert { table } => bounded(table, MAX_TEXT, "table name")?,
        ExportFormat::Tsv
        | ExportFormat::Csv
        | ExportFormat::Json
        | ExportFormat::Markdown
        | ExportFormat::Html
        | ExportFormat::Ndjson
        | ExportFormat::Parquet
        | ExportFormat::ArrowIpc
        | ExportFormat::Xlsx => {}
    }
    settings.csv.check().map_err(invalid)?;
    bounded(&settings.csv.null_text, MAX_TEXT, "NULL text")?;
    settings.sql.check().map_err(invalid)?;
    if let Some(columns) = &settings.columns
        && columns.len() > MAX_COLUMNS
    {
        return Err(invalid("too many columns for a task"));
    }
    match &export.source {
        ExportSource::Query { sql } => {
            if sql.trim().is_empty() {
                return Err(invalid("the export has no query"));
            }
            bounded(sql, MAX_SQL, "query")?;
            if settings.sql.upsert {
                return Err(invalid(
                    "an upsert is written for a table: export the table, or turn upsert off",
                ));
            }
        }
        ExportSource::Table {
            schema,
            name,
            filter,
            order_by,
        } => {
            bounded(schema, MAX_TEXT, "schema")?;
            name_part(name, "table")?;
            bounded(filter, MAX_TEXT, "filter")?;
            bounded(order_by, MAX_TEXT, "order")?;
        }
        ExportSource::Tables { tables } => {
            if tables.is_empty() {
                return Err(invalid("the export names no table"));
            }
            if tables.len() > MAX_TABLES {
                return Err(invalid(format!(
                    "a task exports at most {MAX_TABLES} tables"
                )));
            }
            for table in tables {
                bounded(&table.schema, MAX_TEXT, "schema")?;
                name_part(&table.name, "table")?;
            }
        }
    }
    Ok(())
}

fn check_import(import: &ImportTask) -> Result<(), TaskError> {
    absolute(&import.file, "file to import")?;
    let (schema, table) = match &import.target {
        ImportTarget::Existing { schema, table } | ImportTarget::New { schema, table, .. } => {
            (schema, table)
        }
    };
    bounded(schema, MAX_TEXT, "schema")?;
    name_part(table, "table")?;
    if import.columns.is_empty() || import.columns.len() > MAX_COLUMNS {
        return Err(invalid("the import maps no column, or too many"));
    }
    for column in &import.columns {
        match column {
            ColumnMapping::Skip => {}
            ColumnMapping::Existing(name) => name_part(name, "column")?,
            ColumnMapping::New { name, sql_type } => {
                name_part(name, "column")?;
                check_sql_type(sql_type).map_err(|e| invalid(e.to_string()))?;
            }
        }
    }
    let options = &import.options;
    for (text, what) in [
        (options.null_text.as_deref(), "NULL text"),
        (options.date_format.as_deref(), "date format"),
        (options.timestamp_format.as_deref(), "timestamp format"),
        (options.time_zone.as_deref(), "time zone"),
        (options.sheet.as_deref(), "sheet"),
    ] {
        if let Some(text) = text {
            bounded(text, MAX_TEXT, what)?;
        }
    }
    Ok(())
}

fn check_dump_options(options: &DumpOptions) -> Result<(), TaskError> {
    match options {
        DumpOptions::Postgres(pg) => {
            for schema in &pg.schemas {
                name_part(schema, "schema")?;
            }
            for (schema, table) in &pg.tables {
                bounded(schema, MAX_TEXT, "schema")?;
                name_part(table, "table")?;
            }
            if let Some(encoding) = &pg.encoding {
                bounded(encoding, 64, "encoding")?;
            }
            if pg.compression.is_some_and(|level| level > 9) {
                return Err(invalid("compression goes from 0 to 9"));
            }
        }
        DumpOptions::Mysql(m) => {
            for database in &m.databases {
                name_part(database, "database")?;
            }
            for table in &m.tables {
                name_part(table, "table")?;
            }
        }
    }
    Ok(())
}
