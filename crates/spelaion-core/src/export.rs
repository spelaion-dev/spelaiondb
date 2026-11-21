//! Results written as text formats: to copy a selection to the clipboard or to export a whole
//! result to a file. The rows come from the result store (ADR-010), so typed data is used:
//! numbers stay numbers in JSON, NULL is `NULL` in SQL and literals follow the engine.
//! [`RowWriter`] writes rows one at a time, so streaming exports (ADR-034, `export_stream.rs`)
//! write the same text from batches that are never stored.

use std::collections::HashSet;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spelaion_config::atomic_file::{AtomicWriter, NewFileMode};
use spelaion_driver_api::{ColumnMeta, Dialect, LogicalType, Value};

use crate::edit::{UpdateCopyError, UpdateTable};
use crate::generate::qualified;
use crate::results::StoredResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFormat {
    /// Tab-separated, for pasting into spreadsheets. NULL is an empty cell.
    Tsv,
    /// RFC 4180 (comma, `"` quoting, CRLF). NULL is an empty field.
    Csv,
    /// Array of objects, one per row, keyed by column name.
    Json,
    /// `INSERT`s into `table` ([`SqlOptions::rows_per_insert`] rows each, one by default), with
    /// the engine's literals and quoting, or the engine's upsert ([`SqlOptions::upsert`]).
    SqlInsert { table: String },
    /// One `INSERT` containing every selected row, with the engine's literals and quoting.
    SqlInsertBatch { table: String },
    /// A Markdown (GitHub) table: always with the header row, numbers aligned right.
    Markdown,
    /// The values as an SQL list for `IN`: `(1, 'a')`, or `((1, 'a'), (2, 'b'))` with several
    /// columns; duplicates kept once, in order.
    InList,
    /// An HTML table (header row and one row per result row), for pasting into documents and
    /// spreadsheets. NULL is an empty cell.
    Html,
    /// Newline-delimited JSON: one object per line, keyed by column name (SDB-391).
    Ndjson,
    /// Apache Parquet, typed from each column's logical and native type (SDB-391): written by
    /// `export_binary.rs`, one row group per batch.
    Parquet,
    /// The Arrow IPC file format, with the same types as Parquet (SDB-391).
    ArrowIpc,
    /// An Excel workbook (SDB-390): a sheet per result or table, numbers and dates as real
    /// cells, written by `export_xlsx.rs` a batch at a time.
    Xlsx,
}

impl ExportFormat {
    /// Written a batch at a time by `export_binary.rs` rather than a row at a time.
    pub fn is_binary(&self) -> bool {
        match self {
            ExportFormat::Parquet | ExportFormat::ArrowIpc | ExportFormat::Xlsx => true,
            ExportFormat::Tsv
            | ExportFormat::Csv
            | ExportFormat::Json
            | ExportFormat::SqlInsert { .. }
            | ExportFormat::SqlInsertBatch { .. }
            | ExportFormat::Markdown
            | ExportFormat::InList
            | ExportFormat::Html
            | ExportFormat::Ndjson => false,
        }
    }

    /// A file's extension: `orders.csv`, or `orders.csv.gz` when the file is compressed with
    /// gzip. Parquet compresses its own column chunks instead, so it stays `.parquet`.
    pub fn extension(&self, gzip: bool) -> String {
        let base = match self {
            ExportFormat::Tsv => "tsv",
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::SqlInsert { .. } | ExportFormat::SqlInsertBatch { .. } => "sql",
            ExportFormat::Markdown => "md",
            ExportFormat::InList => "txt",
            ExportFormat::Html => "html",
            ExportFormat::Ndjson => "ndjson",
            ExportFormat::Parquet => return "parquet".into(),
            // A workbook is a zip already.
            ExportFormat::Xlsx => return "xlsx".into(),
            ExportFormat::ArrowIpc => "arrow",
        };
        if gzip {
            format!("{base}.gz")
        } else {
            base.into()
        }
    }
}

/// Most rows one `INSERT` of an export may hold ([`SqlOptions::rows_per_insert`]).
pub const MAX_ROWS_PER_INSERT: usize = 10_000;

/// How SQL keywords are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeywordCase {
    #[default]
    Upper,
    Lower,
}

/// How table and column names are written, before the engine quotes them where it must.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NameCase {
    #[default]
    AsIs,
    Lower,
    Upper,
}

impl NameCase {
    fn apply(self, name: &str) -> String {
        match self {
            NameCase::AsIs => name.to_string(),
            NameCase::Lower => name.to_lowercase(),
            NameCase::Upper => name.to_uppercase(),
        }
    }
}

/// How a SQL export writes its statements (SDB-391). The default is what the clipboard and
/// earlier exports wrote: one `INSERT` per row, keywords in upper case, names as they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlOptions {
    /// Rows per `INSERT`, from 1 to [`MAX_ROWS_PER_INSERT`]; SQL Server's statements hold at
    /// most 1,000 rows (error 10738 beyond), so they are split there.
    pub rows_per_insert: usize,
    /// Leave the table's schema out of its name (a table export writes it by default).
    pub omit_schema: bool,
    pub keyword_case: KeywordCase,
    pub name_case: NameCase,
    /// Each statement in the engine's upsert form, by the table's primary key from the catalog:
    /// `ON CONFLICT … DO UPDATE` (PostgreSQL, SQLite, DuckDB), `ON DUPLICATE KEY UPDATE`
    /// (MySQL, MariaDB) or `MERGE` (SQL Server). Only for a table's export.
    pub upsert: bool,
}

impl Default for SqlOptions {
    fn default() -> Self {
        Self {
            rows_per_insert: 1,
            omit_schema: false,
            keyword_case: KeywordCase::Upper,
            name_case: NameCase::AsIs,
            upsert: false,
        }
    }
}

impl SqlOptions {
    pub fn check(&self) -> Result<(), String> {
        if (1..=MAX_ROWS_PER_INSERT).contains(&self.rows_per_insert) {
            Ok(())
        } else {
            Err(format!(
                "an INSERT holds from 1 to {MAX_ROWS_PER_INSERT} rows"
            ))
        }
    }
}

/// The table a SQL export writes into, as the core resolved it: its schema (`None`: left out)
/// and, for an upsert, its primary key from the catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SqlTarget {
    pub(crate) schema: Option<String>,
    pub(crate) key: Vec<String>,
}

/// Engines whose upsert form a SQL export writes.
pub(crate) fn upsert_supported(engine: &str) -> bool {
    matches!(
        engine,
        "postgres" | "sqlite" | "duckdb" | "mysql" | "sqlserver"
    )
}

/// How JSON and NDJSON write values (SDB-391).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonOptions {
    /// Timestamps in ISO 8601 (`2024-03-04T10:00:00.5`, `…Z` with a time zone) instead of the
    /// database's text (`2024-03-04 10:00:00.5`). Values ISO cannot hold (infinity, BC) keep
    /// the database's text.
    pub iso_dates: bool,
    /// A JSON column's value embedded as JSON; off, it is a string like any text.
    pub json_as_json: bool,
}

impl Default for JsonOptions {
    fn default() -> Self {
        Self {
            iso_dates: false,
            json_as_json: true,
        }
    }
}

/// How CSV and TSV fields are written (ADR-034). The default is what the clipboard uses:
/// a comma, NULL as an empty field and no formula escaping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvOptions {
    /// Field separator of CSV (`,`, `;`, `|`…). TSV always uses a tab.
    pub delimiter: char,
    /// What a NULL is written as.
    pub null_text: String,
    /// A text cell (or column name) that a spreadsheet would run as a formula gets a leading
    /// `'` ([`formula_escaped`]).
    pub escape_formulas: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            delimiter: ',',
            null_text: String::new(),
            escape_formulas: false,
        }
    }
}

impl CsvOptions {
    /// A delimiter that can be read back: not a quote, a line break, a letter or a digit.
    pub fn check(&self) -> Result<(), String> {
        match self.delimiter {
            '"' | '\'' | '\r' | '\n' => Err(format!(
                "{:?} cannot separate fields: it is used for quoting or ends lines",
                self.delimiter
            )),
            c if c.is_alphanumeric() || (c.is_control() && c != '\t') => Err(format!(
                "{c:?} cannot separate fields: use a punctuation mark, a space or a tab"
            )),
            _ => Ok(()),
        }
    }
}

/// The character encoding of an exported file (ADR-034).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    /// UTF-8 starting with a byte order mark, so spreadsheets (Excel) read accents correctly.
    Utf8Bom,
    /// UTF-16 little endian with its byte order mark (what Excel's "Unicode text" is).
    Utf16Le,
    /// Western Windows code page. Characters it does not have become `?` and are counted.
    Windows1252,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// Rows of the result (clipped at its end).
    pub rows: Range<usize>,
    /// Columns of the result, by position and in this order; positions past the end are left
    /// out.
    pub columns: Vec<usize>,
    /// Column names as the first line (TSV/CSV). JSON and SQL always name the columns.
    pub header: bool,
    /// CSV and TSV fields.
    pub csv: CsvOptions,
}

/// For results whose connection is gone: standard SQL quoting (`"x"`) and literals.
pub struct GenericDialect;

impl Dialect for GenericDialect {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn split_statements(&self, _script: &str) -> Vec<spelaion_driver_api::StatementSpan> {
        Vec::new()
    }

    fn quote_identifier(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

/// Rows read per batch from the store while writing (keeps memory bounded on big exports).
const CHUNK: usize = 10_000;

/// Most rows SQL Server accepts in one `INSERT … VALUES` (error 10738 beyond).
const SQL_SERVER_MAX_VALUES_ROWS: usize = 1_000;

/// Writes rows in one format, one at a time: [`RowWriter::begin`], then [`RowWriter::row`]
/// for each row, then [`RowWriter::finish`]. Only an `IN` list keeps anything between rows.
pub(crate) struct RowWriter {
    format: ExportFormat,
    header: bool,
    csv: CsvOptions,
    json: JsonOptions,
    engine: &'static str,
    metas: Vec<ColumnMeta>,
    names: Vec<String>,
    /// What starts a SQL statement, up to its first row: `INSERT INTO t (a, b) VALUES `, or
    /// SQL Server's `MERGE INTO t AS tgt USING (VALUES `.
    statement_head: String,
    /// What ends a SQL statement after its last row, before the `;`: nothing, or the upsert.
    statement_tail: String,
    /// Rows per SQL statement.
    rows_per_statement: usize,
    /// Rows of the SQL statement being written.
    in_statement: usize,
    items: Vec<String>,
    seen: HashSet<String>,
    written: usize,
}

impl RowWriter {
    /// `metas` are the columns written, in order.
    pub(crate) fn new(
        format: ExportFormat,
        header: bool,
        csv: CsvOptions,
        dialect: &dyn Dialect,
        metas: Vec<ColumnMeta>,
    ) -> Self {
        let names = unique_names(metas.iter().map(|m| m.name.as_str()));
        let mut writer = Self {
            format,
            header,
            csv,
            json: JsonOptions::default(),
            engine: dialect.name(),
            metas,
            names,
            statement_head: String::new(),
            statement_tail: String::new(),
            rows_per_statement: 1,
            in_statement: 0,
            items: Vec::new(),
            seen: HashSet::new(),
            written: 0,
        };
        writer.set_sql(dialect, &SqlOptions::default(), &SqlTarget::default());
        writer
    }

    /// How JSON and NDJSON write values.
    pub(crate) fn with_json(mut self, json: JsonOptions) -> Self {
        self.json = json;
        self
    }

    /// How SQL statements are written, into `target` (its schema and, for an upsert, its
    /// primary key, which the caller checked is among the columns written).
    pub(crate) fn with_sql(
        mut self,
        dialect: &dyn Dialect,
        options: &SqlOptions,
        target: &SqlTarget,
    ) -> Self {
        self.set_sql(dialect, options, target);
        self
    }

    fn set_sql(&mut self, dialect: &dyn Dialect, options: &SqlOptions, target: &SqlTarget) {
        let table = match &self.format {
            ExportFormat::SqlInsert { table } | ExportFormat::SqlInsertBatch { table } => {
                table.clone()
            }
            ExportFormat::Tsv
            | ExportFormat::Csv
            | ExportFormat::Json
            | ExportFormat::Markdown
            | ExportFormat::InList
            | ExportFormat::Html
            | ExportFormat::Ndjson
            | ExportFormat::Parquet
            | ExportFormat::ArrowIpc
            | ExportFormat::Xlsx => return,
        };
        let per = match self.format {
            ExportFormat::SqlInsertBatch { .. } => usize::MAX,
            _ => options.rows_per_insert.max(1),
        };
        self.rows_per_statement = if self.engine == "sqlserver" {
            per.min(SQL_SERVER_MAX_VALUES_ROWS)
        } else {
            per
        };
        let kw = |text: &str| match options.keyword_case {
            KeywordCase::Upper => text.to_string(),
            KeywordCase::Lower => text.to_lowercase(),
        };
        let name = |text: &str| dialect.quote_identifier(&options.name_case.apply(text));
        let table_name = match target.schema.as_deref() {
            Some(schema) if !options.omit_schema && !schema.is_empty() => qualified(
                dialect,
                &options.name_case.apply(schema),
                &options.name_case.apply(&table),
            ),
            _ => name(&table),
        };
        let columns: Vec<String> = self.metas.iter().map(|m| name(&m.name)).collect();
        let list = columns.join(", ");
        let others: Vec<&String> = self
            .metas
            .iter()
            .zip(&columns)
            .filter(|(m, _)| !target.key.contains(&m.name))
            .map(|(_, quoted)| quoted)
            .collect();
        let keys: Vec<String> = target.key.iter().map(|k| name(k)).collect();
        if !options.upsert || target.key.is_empty() {
            self.statement_head = format!(
                "{} {table_name} ({list}) {} ",
                kw("INSERT INTO"),
                kw("VALUES")
            );
            self.statement_tail = String::new();
            return;
        }
        match self.engine {
            "sqlserver" => {
                self.statement_head = format!(
                    "{} {table_name} {} tgt {} ({} ",
                    kw("MERGE INTO"),
                    kw("AS"),
                    kw("USING"),
                    kw("VALUES")
                );
                let on: Vec<String> = keys.iter().map(|k| format!("tgt.{k} = src.{k}")).collect();
                let matched = if others.is_empty() {
                    String::new()
                } else {
                    let set: Vec<String> = others
                        .iter()
                        .map(|c| format!("tgt.{c} = src.{c}"))
                        .collect();
                    format!(" {} {}", kw("WHEN MATCHED THEN UPDATE SET"), set.join(", "))
                };
                let sources: Vec<String> = columns.iter().map(|c| format!("src.{c}")).collect();
                self.statement_tail = format!(
                    ") {} src ({list}) {} {}{matched} {} ({list}) {} ({})",
                    kw("AS"),
                    kw("ON"),
                    on.join(&format!(" {} ", kw("AND"))),
                    kw("WHEN NOT MATCHED THEN INSERT"),
                    kw("VALUES"),
                    sources.join(", ")
                );
            }
            "mysql" => {
                self.statement_head = format!(
                    "{} {table_name} ({list}) {} ",
                    kw("INSERT INTO"),
                    kw("VALUES")
                );
                // Every column in the key: a no-op update keeps the row as it is.
                let updated: Vec<&String> = if others.is_empty() {
                    columns.iter().take(1).collect()
                } else {
                    others
                };
                let set: Vec<String> = updated
                    .iter()
                    .map(|c| format!("{c} = {}({c})", kw("VALUES")))
                    .collect();
                self.statement_tail =
                    format!(" {} {}", kw("ON DUPLICATE KEY UPDATE"), set.join(", "));
            }
            // PostgreSQL, SQLite and DuckDB share `ON CONFLICT`.
            _ => {
                self.statement_head = format!(
                    "{} {table_name} ({list}) {} ",
                    kw("INSERT INTO"),
                    kw("VALUES")
                );
                let action = if others.is_empty() {
                    kw("DO NOTHING")
                } else {
                    let set: Vec<String> = others
                        .iter()
                        .map(|c| format!("{c} = {}.{c}", kw("EXCLUDED")))
                        .collect();
                    format!("{} {}", kw("DO UPDATE SET"), set.join(", "))
                };
                self.statement_tail =
                    format!(" {} ({}) {action}", kw("ON CONFLICT"), keys.join(", "));
            }
        }
    }

    pub(crate) fn begin(&mut self, out: &mut dyn Write) -> io::Result<()> {
        match &self.format {
            ExportFormat::Tsv | ExportFormat::Csv if self.header => {
                let fields: Vec<String> = self
                    .names
                    .iter()
                    .map(|n| self.field(Some(n), false))
                    .collect();
                self.write_line(out, &fields)
            }
            ExportFormat::Json => out.write_all(b"["),
            ExportFormat::Html => {
                out.write_all(b"<table>\n  <thead>\n    <tr>")?;
                for name in &self.names {
                    write!(out, "<th>{}</th>", html_text(name))?;
                }
                out.write_all(b"</tr>\n  </thead>\n  <tbody>\n")
            }
            ExportFormat::Markdown => {
                let cells: Vec<String> = self.names.iter().map(|n| markdown_cell(n)).collect();
                writeln!(out, "| {} |", cells.join(" | "))?;
                let rules: Vec<&str> = self
                    .metas
                    .iter()
                    .map(|m| if is_numeric(m.logical) { "---:" } else { "---" })
                    .collect();
                writeln!(out, "| {} |", rules.join(" | "))
            }
            ExportFormat::Parquet | ExportFormat::ArrowIpc | ExportFormat::Xlsx => Err(not_text()),
            ExportFormat::Tsv
            | ExportFormat::Csv
            | ExportFormat::SqlInsert { .. }
            | ExportFormat::SqlInsertBatch { .. }
            | ExportFormat::InList
            | ExportFormat::Ndjson => Ok(()),
        }
    }

    /// One row: `values` follow the columns given to [`RowWriter::new`].
    pub(crate) fn row(&mut self, values: &[Value], out: &mut dyn Write) -> io::Result<()> {
        match &self.format {
            ExportFormat::Tsv | ExportFormat::Csv => {
                let fields: Vec<String> = values
                    .iter()
                    .zip(&self.metas)
                    .map(|(v, m)| self.field(text(v).as_deref(), is_numeric(m.logical)))
                    .collect();
                self.write_line(out, &fields)?;
            }
            ExportFormat::Json => {
                out.write_all(if self.written == 0 { b"\n  " } else { b",\n  " })?;
                self.write_object(values, out)?;
            }
            ExportFormat::Ndjson => {
                self.write_object(values, out)?;
                out.write_all(b"\n")?;
            }
            ExportFormat::SqlInsert { .. } | ExportFormat::SqlInsertBatch { .. } => {
                if self.in_statement == 0 {
                    out.write_all(self.statement_head.as_bytes())?;
                    out.write_all(b"(")?;
                } else {
                    out.write_all(b",\n  (")?;
                }
                out.write_all(self.literals(values).join(", ").as_bytes())?;
                out.write_all(b")")?;
                self.in_statement += 1;
                if self.in_statement >= self.rows_per_statement {
                    self.end_statement(out)?;
                }
            }
            ExportFormat::InList => {
                let literals = self.literals(values);
                let item = if literals.len() == 1 {
                    literals.join("")
                } else {
                    format!("({})", literals.join(", "))
                };
                if self.seen.insert(item.clone()) {
                    self.items.push(item);
                }
            }
            ExportFormat::Html => {
                out.write_all(b"    <tr>")?;
                for value in values {
                    let cell = text(value).map(|t| html_text(&t)).unwrap_or_default();
                    write!(out, "<td>{cell}</td>")?;
                }
                out.write_all(b"</tr>\n")?;
            }
            ExportFormat::Markdown => {
                let cells: Vec<String> = values
                    .iter()
                    .map(|v| markdown_cell(&text(v).unwrap_or_else(|| "NULL".into())))
                    .collect();
                writeln!(out, "| {} |", cells.join(" | "))?;
            }
            ExportFormat::Parquet | ExportFormat::ArrowIpc | ExportFormat::Xlsx => {
                return Err(not_text());
            }
        }
        self.written += 1;
        Ok(())
    }

    /// Closes what the format opened; returns how many rows were written.
    pub(crate) fn finish(&mut self, out: &mut dyn Write) -> io::Result<usize> {
        match &self.format {
            ExportFormat::Json => {
                out.write_all(if self.written == 0 { b"]\n" } else { b"\n]\n" })?
            }
            ExportFormat::InList => write!(out, "({})", self.items.join(", "))?,
            ExportFormat::Html => out.write_all(b"  </tbody>\n</table>\n")?,
            ExportFormat::SqlInsert { .. } | ExportFormat::SqlInsertBatch { .. } => {
                if self.in_statement > 0 {
                    self.end_statement(out)?;
                }
            }
            ExportFormat::Parquet | ExportFormat::ArrowIpc | ExportFormat::Xlsx => {
                return Err(not_text());
            }
            ExportFormat::Tsv
            | ExportFormat::Csv
            | ExportFormat::Markdown
            | ExportFormat::Ndjson => {}
        }
        Ok(self.written)
    }

    fn end_statement(&mut self, out: &mut dyn Write) -> io::Result<()> {
        out.write_all(self.statement_tail.as_bytes())?;
        out.write_all(b";\n")?;
        self.in_statement = 0;
        Ok(())
    }

    /// `{"name": value, …}` for JSON and NDJSON.
    fn write_object(&self, values: &[Value], out: &mut dyn Write) -> io::Result<()> {
        out.write_all(b"{")?;
        for (i, (name, value)) in self.names.iter().zip(values).enumerate() {
            if i > 0 {
                out.write_all(b", ")?;
            }
            write!(
                out,
                "{}: {}",
                json_string(name),
                json_value_with(value, self.metas[i].logical, self.json)
            )?;
        }
        out.write_all(b"}")
    }

    fn literals(&self, values: &[Value]) -> Vec<String> {
        values.iter().map(|v| sql_literal(v, self.engine)).collect()
    }

    fn separator(&self) -> char {
        if self.format == ExportFormat::Tsv {
            '\t'
        } else {
            self.csv.delimiter
        }
    }

    /// One TSV/CSV field. Quoted only when needed, doubling `"` (spreadsheets read both).
    fn field(&self, value: Option<&str>, numeric: bool) -> String {
        let value = match value {
            None => self.csv.null_text.clone(),
            Some(value) if self.csv.escape_formulas && !numeric => formula_escaped(value),
            Some(value) => value.to_string(),
        };
        if value.contains([self.separator(), '"', '\n', '\r']) {
            format!("\"{}\"", value.replace('"', "\"\""))
        } else {
            value
        }
    }

    fn write_line(&self, out: &mut dyn Write, fields: &[String]) -> io::Result<()> {
        let end = if self.format == ExportFormat::Csv {
            "\r\n"
        } else {
            "\n"
        };
        out.write_all(fields.join(&self.separator().to_string()).as_bytes())?;
        out.write_all(end.as_bytes())
    }
}

/// `text` with a leading `'` when a spreadsheet would run it as a formula: it starts with `=`,
/// `+`, `-`, `@`, a tab or a carriage return (OWASP's CSV injection advice). Text that is just
/// a number (`-5`, `+1.5e3`) is left alone, so negative numbers stay numbers (ADR-034).
pub fn formula_escaped(text: &str) -> String {
    let risky = match text.chars().next() {
        Some('=' | '@' | '\t' | '\r') => true,
        Some('+' | '-') => !is_plain_number(text),
        _ => false,
    };
    if risky {
        format!("'{text}")
    } else {
        text.to_string()
    }
}

/// `-5`, `+1.5`, `-2e3`: a sign, digits, at most one point and an exponent. Not `-inf` or
/// `-nan`, which a spreadsheet does not read as numbers.
fn is_plain_number(text: &str) -> bool {
    let digits = &text[1..];
    let (mantissa, exponent) = match digits.find(['e', 'E']) {
        Some(at) => (&digits[..at], Some(&digits[at + 1..])),
        None => (digits, None),
    };
    let mantissa_ok = !mantissa.is_empty()
        && mantissa.chars().any(|c| c.is_ascii_digit())
        && mantissa.chars().all(|c| c.is_ascii_digit() || c == '.')
        && mantissa.matches('.').count() <= 1;
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
    });
    mantissa_ok && exponent_ok
}

/// Writes the selected rows and columns of `result` to `out`; returns how many rows.
pub fn write_result(
    result: &StoredResult,
    options: &ExportOptions,
    dialect: &dyn Dialect,
    out: &mut dyn Write,
) -> io::Result<usize> {
    let schema = result.schema();
    let total_columns = schema.columns.len();
    let columns: Vec<usize> = options
        .columns
        .iter()
        .copied()
        .filter(|&c| c < total_columns)
        .collect();
    let rows = options.rows.start.min(result.rows())..options.rows.end.min(result.rows());
    let metas: Vec<ColumnMeta> = columns.iter().map(|&c| schema.columns[c].clone()).collect();
    let writer = RowWriter::new(
        options.format.clone(),
        options.header,
        options.csv.clone(),
        dialect,
        metas,
    );
    write_rows(result, rows, &columns, writer, out)
}

/// The `rows` of the view, with `columns` in this order, through `writer`.
fn write_rows(
    result: &StoredResult,
    rows: Range<usize>,
    columns: &[usize],
    mut writer: RowWriter,
    out: &mut dyn Write,
) -> io::Result<usize> {
    let schema = result.schema();
    writer.begin(out)?;
    let mut start = rows.start;
    while start < rows.end {
        let len = CHUNK.min(rows.end - start);
        for batch in result.window(start, len) {
            for row in 0..batch.num_rows() {
                let values: Vec<Value> = columns
                    .iter()
                    .map(|&c| {
                        Value::from_array(batch.column(c).as_ref(), row, schema.columns[c].logical)
                            .unwrap_or(Value::Null)
                    })
                    .collect();
                writer.row(&values, out)?;
            }
        }
        start += len;
    }
    writer.finish(out)
}

/// A selection of `result` as clipboard text (the grid's copy). One cell copied as plain text
/// (TSV without a header) is its value exactly: no quotes and no line break after it, so pasted
/// into a terminal it does not run and pasted into a search box or a form it does not submit.
/// A line break that is part of the value is kept. Anything else is [`write_result`]'s text,
/// byte for byte: rows (one row of several cells too) keep their final line break, which a
/// spreadsheet expects when pasting rows.
pub fn copy_text(
    result: &StoredResult,
    options: &ExportOptions,
    dialect: &dyn Dialect,
) -> io::Result<String> {
    if let Some((value, numeric)) = single_cell(result, options) {
        return Ok(match text(&value) {
            None => options.csv.null_text.clone(),
            Some(text) if options.csv.escape_formulas && !numeric => formula_escaped(&text),
            Some(text) => text,
        });
    }
    let mut out = Vec::new();
    write_result(result, options, dialect, &mut out)?;
    String::from_utf8(out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Copies distinct view rows in display order with a single header and formatter.
/// Reject invalid row indices rather than silently copying a different selection.
pub fn copy_selected_rows(
    result: &StoredResult,
    options: &ExportOptions,
    selected: &[usize],
    dialect: &dyn Dialect,
) -> io::Result<String> {
    let mut rows = selected.to_vec();
    rows.sort_unstable();
    rows.dedup();
    if rows.last().is_some_and(|&row| row >= result.rows()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected row is outside the result",
        ));
    }
    if let [row] = rows.as_slice() {
        let mut one = options.clone();
        one.rows = *row..row + 1;
        return copy_text(result, &one, dialect);
    }
    let schema = result.schema();
    let columns: Vec<usize> = options
        .columns
        .iter()
        .copied()
        .filter(|&c| c < schema.columns.len())
        .collect();
    let metas = columns.iter().map(|&c| schema.columns[c].clone()).collect();
    let mut writer = RowWriter::new(
        options.format.clone(),
        options.header,
        options.csv.clone(),
        dialect,
        metas,
    );
    let mut out = Vec::new();
    writer.begin(&mut out)?;
    for row in rows {
        for batch in result.window(row, 1) {
            let values: Vec<Value> = columns
                .iter()
                .map(|&c| {
                    Value::from_array(batch.column(c).as_ref(), 0, schema.columns[c].logical)
                        .unwrap_or(Value::Null)
                })
                .collect();
            writer.row(&values, &mut out)?;
        }
    }
    writer.finish(&mut out)?;
    String::from_utf8(out).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The value of the one cell `options` selects as plain text (TSV, no header), and whether its
/// column is numeric; `None` for any other copy.
fn single_cell(result: &StoredResult, options: &ExportOptions) -> Option<(Value, bool)> {
    if options.format != ExportFormat::Tsv || options.header {
        return None;
    }
    let schema = result.schema();
    let columns: Vec<usize> = options
        .columns
        .iter()
        .copied()
        .filter(|&c| c < schema.columns.len())
        .collect();
    let rows = options.rows.start.min(result.rows())..options.rows.end.min(result.rows());
    let ([column], 1) = (columns.as_slice(), rows.len()) else {
        return None;
    };
    let logical = schema.columns[*column].logical;
    let batch = result.window(rows.start, 1).into_iter().next()?;
    let value =
        Value::from_array(batch.column(*column).as_ref(), 0, logical).unwrap_or(Value::Null);
    Some((value, is_numeric(logical)))
}

/// What writing a file did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileSummary {
    pub rows: u64,
    pub bytes: u64,
    /// Characters the encoding does not have, written as `?`.
    pub replaced: u64,
    /// The most rows a streaming export held in memory at once: one batch (ADR-034). Zero
    /// for a result written from the store.
    pub peak_rows: u64,
}

/// Writes the export to `path` through a temporary file renamed at the end, so an error or
/// a crash never leaves a half-written file under the final name.
pub fn export_to_file(
    result: &StoredResult,
    options: &ExportOptions,
    file_options: &FileOptions,
    dialect: &dyn Dialect,
    path: &Path,
) -> io::Result<FileSummary> {
    let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
    options.csv.check().map_err(invalid)?;
    file_options.sql.check().map_err(invalid)?;
    if file_options.sql.upsert {
        return Err(invalid(
            "an upsert is written for a table: export the table, or turn upsert off".into(),
        ));
    }
    let schema = result.schema();
    let columns: Vec<usize> = options
        .columns
        .iter()
        .copied()
        .filter(|&c| c < schema.columns.len())
        .collect();
    let rows = options.rows.start.min(result.rows())..options.rows.end.min(result.rows());
    if options.format == ExportFormat::Xlsx {
        let fields: Vec<_> = columns
            .iter()
            .map(|&c| schema.arrow.field(c).clone())
            .collect();
        // The sheet is named after the file.
        let sheet = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split('.').next())
            .unwrap_or("Result")
            .to_string();
        let mut book = crate::export_xlsx::XlsxBook::create(path, None)?;
        book.begin_sheet(&sheet, &fields, columns)?;
        let mut start = rows.start;
        while start < rows.end {
            let len = CHUNK.min(rows.end - start);
            for batch in result.window(start, len) {
                book.write(&batch)?;
            }
            start += len;
        }
        return book.finish();
    }
    if options.format.is_binary() {
        let fields: Vec<_> = columns
            .iter()
            .map(|&c| schema.arrow.field(c).clone())
            .collect();
        let mut file = crate::export_binary::BinaryFile::create(
            path,
            &options.format,
            file_options.gzip,
            &fields,
            columns,
            CHUNK,
            None,
        )?;
        let mut start = rows.start;
        while start < rows.end {
            let len = CHUNK.min(rows.end - start);
            for batch in result.window(start, len) {
                file.write(&batch)?;
            }
            start += len;
        }
        return file.finish();
    }
    let mut file = PartFile::create(path, file_options.encoding, file_options.gzip)?;
    let metas: Vec<ColumnMeta> = columns.iter().map(|&c| schema.columns[c].clone()).collect();
    let writer = RowWriter::new(
        options.format.clone(),
        options.header,
        options.csv.clone(),
        dialect,
        metas,
    )
    .with_json(file_options.json)
    .with_sql(dialect, &file_options.sql, &SqlTarget::default());
    match write_rows(result, rows, &columns, writer, file.writer()) {
        Ok(rows) => file.commit(rows as u64),
        Err(e) => {
            file.discard();
            Err(e)
        }
    }
}

/// How an export's file is written, besides its format (SDB-391).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOptions {
    /// Of the text formats; Parquet and Arrow are binary.
    pub encoding: TextEncoding,
    /// The file compressed with gzip (Parquet: its column chunks, with Parquet's own codec).
    pub gzip: bool,
    pub sql: SqlOptions,
    pub json: JsonOptions,
}

impl Default for FileOptions {
    fn default() -> Self {
        Self {
            encoding: TextEncoding::Utf8,
            gzip: false,
            sql: SqlOptions::default(),
            json: JsonOptions::default(),
        }
    }
}

/// A file written as `<path>.part` and renamed to `path` only when complete (ADR-034): an
/// error, a cancel or a crash never leaves a partial file under the final name, and whatever
/// was at `path` stays until the new file is whole. It is `spelaion-config`'s
/// [`AtomicWriter`]: the `.part` file sits beside the file a symbolic link points to (the link
/// stays a link), the old file's mode is kept, and a second export to the same path writes
/// `<path>.<pid>-<n>.part` instead of sharing the first one's. Dropped without
/// [`PartFile::commit`], it deletes the temporary file.
pub(crate) struct PartFile {
    out: Encoder<Sink>,
}

/// Bytes on their way to the temporary file, counted as they reach it (the file's size, after
/// gzip), and shared with whoever reports progress.
pub(crate) struct Counted {
    inner: io::BufWriter<AtomicWriter>,
    bytes: u64,
    progress: Option<Arc<AtomicU64>>,
}

impl Write for Counted {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes += written as u64;
        if let Some(counter) = &self.progress {
            counter.store(self.bytes, Ordering::Relaxed);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Where a file's bytes go: straight to it, or through gzip (SDB-391).
pub(crate) enum Sink {
    Plain(Counted),
    Gzip(flate2::write::GzEncoder<Counted>),
}

impl Sink {
    fn counted(&mut self) -> &mut Counted {
        match self {
            Sink::Plain(counted) => counted,
            Sink::Gzip(gzip) => gzip.get_mut(),
        }
    }

    /// Ends gzip's stream, if any.
    fn finish(self) -> io::Result<Counted> {
        match self {
            Sink::Plain(counted) => Ok(counted),
            Sink::Gzip(gzip) => gzip.finish(),
        }
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::Plain(counted) => counted.write(buf),
            Sink::Gzip(gzip) => gzip.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::Plain(counted) => counted.flush(),
            Sink::Gzip(gzip) => gzip.flush(),
        }
    }
}

impl PartFile {
    /// `gzip`: the file is one gzip stream of what is written.
    pub(crate) fn create(path: &Path, encoding: TextEncoding, gzip: bool) -> io::Result<Self> {
        let file = AtomicWriter::create_with_suffix(path, ".part", NewFileMode::Default)?;
        let counted = Counted {
            inner: io::BufWriter::with_capacity(1 << 20, file),
            bytes: 0,
            progress: None,
        };
        let sink = if gzip {
            Sink::Gzip(flate2::write::GzEncoder::new(
                counted,
                flate2::Compression::default(),
            ))
        } else {
            Sink::Plain(counted)
        };
        let mut part = Self {
            out: Encoder::new(sink, encoding),
        };
        // On an error `part` is dropped, which deletes the temporary file.
        part.writer().write_bom()?;
        Ok(part)
    }

    pub(crate) fn writer(&mut self) -> &mut Encoder<Sink> {
        &mut self.out
    }

    /// The bytes written to the file from now on are also stored in `counter`.
    pub(crate) fn report_to(&mut self, counter: Arc<AtomicU64>) {
        let counted = self.out.inner.counted();
        counter.store(counted.bytes, Ordering::Relaxed);
        counted.progress = Some(counter);
    }

    /// The writer, for a writer that must own it (Parquet's, Arrow's); dropped, it deletes
    /// the temporary file like the [`PartFile`] would.
    pub(crate) fn into_writer(self) -> Encoder<Sink> {
        self.out
    }

    /// The [`PartFile`] of a writer [`PartFile::into_writer`] gave.
    pub(crate) fn from_writer(out: Encoder<Sink>) -> Self {
        Self { out }
    }

    /// Flushes, ends gzip's stream, renames to the final name and says what was written.
    pub(crate) fn commit(self, rows: u64) -> io::Result<FileSummary> {
        let mut out = self.out;
        out.flush()?;
        let replaced = out.replaced();
        let mut counted = out.into_inner().finish()?;
        counted.flush()?;
        let summary = FileSummary {
            rows,
            bytes: counted.bytes,
            replaced,
            peak_rows: 0,
        };
        counted
            .inner
            .into_inner()
            .map_err(|e| e.into_error())?
            .commit()?;
        Ok(summary)
    }

    /// Deletes the temporary file.
    pub(crate) fn discard(self) {
        drop(self);
    }
}

/// Where a file is written until it is complete.
pub(crate) fn part_path(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".part");
    PathBuf::from(temporary)
}

/// Writes UTF-8 text in another encoding, counting the characters it could not encode. Bytes
/// that are not text (Parquet's, Arrow's) go through unchanged in UTF-8.
pub(crate) struct Encoder<W: Write> {
    inner: W,
    encoding: TextEncoding,
    /// The start of a character split between two writes.
    pending: Vec<u8>,
    replaced: u64,
}

impl<W: Write> Encoder<W> {
    pub(crate) fn new(inner: W, encoding: TextEncoding) -> Self {
        Self {
            inner,
            encoding,
            pending: Vec::new(),
            replaced: 0,
        }
    }

    fn write_bom(&mut self) -> io::Result<()> {
        match self.encoding {
            TextEncoding::Utf8Bom => self.emit(&[0xef, 0xbb, 0xbf]),
            TextEncoding::Utf16Le => self.emit(&[0xff, 0xfe]),
            TextEncoding::Utf8 | TextEncoding::Windows1252 => Ok(()),
        }
    }

    pub(crate) fn replaced(&self) -> u64 {
        self.replaced
    }

    pub(crate) fn into_inner(self) -> W {
        self.inner
    }

    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes)
    }

    fn encode(&mut self, text: &str) -> io::Result<()> {
        match self.encoding {
            TextEncoding::Utf8 | TextEncoding::Utf8Bom => self.emit(text.as_bytes()),
            TextEncoding::Utf16Le => {
                let mut out = Vec::with_capacity(text.len() * 2);
                for unit in text.encode_utf16() {
                    out.extend_from_slice(&unit.to_le_bytes());
                }
                self.emit(&out)
            }
            TextEncoding::Windows1252 => {
                let mut out = Vec::with_capacity(text.len());
                for c in text.chars() {
                    match windows_1252(c) {
                        Some(byte) => out.push(byte),
                        None => {
                            out.push(b'?');
                            self.replaced += 1;
                        }
                    }
                }
                self.emit(&out)
            }
        }
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pending.is_empty()
            && matches!(self.encoding, TextEncoding::Utf8 | TextEncoding::Utf8Bom)
        {
            self.emit(buf)?;
            return Ok(buf.len());
        }
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(buf);
        let mut start = 0;
        while start < pending.len() {
            match std::str::from_utf8(&pending[start..]) {
                Ok(text) => {
                    self.encode(text)?;
                    start = pending.len();
                }
                Err(e) => {
                    let valid = start + e.valid_up_to();
                    let text =
                        std::str::from_utf8(&pending[start..valid]).expect("valid up to here");
                    self.encode(text)?;
                    match e.error_len() {
                        // Not UTF-8 (never from our writers): one `?` per bad sequence.
                        Some(bad) => {
                            match self.encoding {
                                TextEncoding::Utf16Le => self.emit(&[b'?', 0])?,
                                TextEncoding::Utf8
                                | TextEncoding::Utf8Bom
                                | TextEncoding::Windows1252 => self.emit(b"?")?,
                            }
                            self.replaced += 1;
                            start = valid + bad;
                        }
                        // A character continues in the next write.
                        None => {
                            start = valid;
                            break;
                        }
                    }
                }
            }
        }
        pending.drain(..start);
        self.pending = pending;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The Windows-1252 byte of `c`, if it has one.
fn windows_1252(c: char) -> Option<u8> {
    let code = c as u32;
    if code < 0x80 || (0xa0..=0xff).contains(&code) {
        return Some(code as u8);
    }
    Some(match c {
        '€' => 0x80,
        '‚' => 0x82,
        'ƒ' => 0x83,
        '„' => 0x84,
        '…' => 0x85,
        '†' => 0x86,
        '‡' => 0x87,
        'ˆ' => 0x88,
        '‰' => 0x89,
        'Š' => 0x8a,
        '‹' => 0x8b,
        'Œ' => 0x8c,
        'Ž' => 0x8e,
        '\u{2018}' => 0x91,
        '\u{2019}' => 0x92,
        '\u{201c}' => 0x93,
        '\u{201d}' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '˜' => 0x98,
        '™' => 0x99,
        'š' => 0x9a,
        '›' => 0x9b,
        'œ' => 0x9c,
        'ž' => 0x9e,
        'Ÿ' => 0x9f,
        _ => return None,
    })
}

/// How [`column_names`] writes the names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameStyle {
    /// `id, name`.
    Plain,
    /// Always quoted, the engine's way: `"id", "name"` (`` `id` `` in MySQL and MariaDB,
    /// `[id]` in SQL Server).
    Quoted,
    /// With the table they come from, quoted: `"public"."users"."id"`.
    Qualified,
    /// One plain name per line.
    List,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CopyError {
    #[error("qualified names need the table the columns come from (open the table's data)")]
    NoTable,
    #[error("the result has no column {0}")]
    NoSuchColumn(usize),
    #[error("the result has no row {0}")]
    NoSuchRow(usize),
    /// "Copy special…" options whose text could not be read back (SDB-388).
    #[error("{0}")]
    Options(String),
    #[error(
        "the column holds more than {MAX_IN_LIST:} different values: too many for an IN list (filter the rows first)"
    )]
    TooManyValues,
}

/// Most different values [`column_in_list`] writes: a longer `IN` list is better done as a
/// join, and some engines refuse it.
pub const MAX_IN_LIST: usize = 10_000;

/// The names of `columns` (by position, in this order) for the clipboard. `table` is the
/// schema and name of the table they come from, for [`NameStyle::Qualified`]; qualified names
/// are written like the explorer's (ADR-026).
pub fn column_names(
    result: &StoredResult,
    columns: &[usize],
    style: NameStyle,
    dialect: &dyn Dialect,
    table: Option<(&str, &str)>,
) -> Result<String, CopyError> {
    let metas = &result.schema().columns;
    let mut names = Vec::with_capacity(columns.len());
    for &column in columns {
        let name = &metas
            .get(column)
            .ok_or(CopyError::NoSuchColumn(column))?
            .name;
        names.push(match style {
            NameStyle::Plain | NameStyle::List => name.clone(),
            NameStyle::Quoted => always_quoted(dialect, name),
            NameStyle::Qualified => {
                let (schema, table) = table.ok_or(CopyError::NoTable)?;
                crate::objects::qualified_name(dialect, schema, &[table.to_string(), name.clone()])
            }
        });
    }
    Ok(names.join(if style == NameStyle::List { "\n" } else { ", " }))
}

/// An identifier quoted even where the engine would not need it.
fn always_quoted(dialect: &dyn Dialect, name: &str) -> String {
    match dialect.name() {
        "mysql" | "mariadb" => format!("`{}`", name.replace('`', "``")),
        "sqlserver" => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// Every different value of one column, in the order of the current view, as an SQL list for
/// `IN`: `(1, 2, 3)` or `('a', 'b')`, with the engine's literals for each type. Reads the
/// whole column in the core (a million rows included); more than [`MAX_IN_LIST`] different
/// values is refused rather than written.
pub fn column_in_list(
    result: &StoredResult,
    column: usize,
    dialect: &dyn Dialect,
) -> Result<String, CopyError> {
    let meta = result
        .schema()
        .columns
        .get(column)
        .ok_or(CopyError::NoSuchColumn(column))?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut items: Vec<String> = Vec::new();
    for position in 0..result.rows() {
        let (batch, row) = result.locate(result.original_row(position));
        let value = Value::from_array(batch.column(column).as_ref(), row, meta.logical)
            .unwrap_or(Value::Null);
        let literal = sql_literal(&value, dialect.name());
        if seen.insert(literal.clone()) {
            if items.len() == MAX_IN_LIST {
                return Err(CopyError::TooManyValues);
            }
            items.push(literal);
        }
    }
    Ok(format!("({})", items.join(", ")))
}

/// The rows of the view at `rows` (positions, in display order, each once) as one `UPDATE`
/// each (SDB-388): `SET` the selected `columns` that are not part of the primary key, `WHERE`
/// every key column equals the row's value, with names quoted and values written as literals
/// by the engine's dialect ([`sql_literal`], MySQL's backslash rule included). The table and
/// its key are [`UpdateTable`]'s, from the catalog; the values are the stored, typed result's.
/// A masked column, in the key or in the selection, is refused (ADR-051): the result holds
/// the mask, and an UPDATE that left it out would not be the one the user selected.
pub fn copy_updates(
    update: &UpdateTable,
    result: &StoredResult,
    rows: &[usize],
    columns: &[usize],
) -> Result<String, UpdateCopyError> {
    update.target.check(result)?;
    let dialect = update.target.dialect();
    let table = &update.table;
    let metas = &result.schema().columns;
    let position = |name: &str| metas.iter().position(|m| m.name == name);
    let mut keys = Vec::with_capacity(table.primary_key.len());
    for key in &table.primary_key {
        let at = position(key).ok_or_else(|| UpdateCopyError::KeyNotInResult(key.clone()))?;
        if result.is_masked(at) {
            return Err(UpdateCopyError::Masked(key.clone()));
        }
        keys.push((at, key.as_str()));
    }
    let mut set: Vec<usize> = Vec::new();
    for &column in columns {
        let meta = metas
            .get(column)
            .ok_or_else(|| UpdateCopyError::Failed(format!("the result has no column {column}")))?;
        if table.primary_key.contains(&meta.name) || set.contains(&column) {
            continue;
        }
        if !table.columns.iter().any(|c| c.name == meta.name) {
            return Err(UpdateCopyError::NotATableColumn(meta.name.clone()));
        }
        if result.is_masked(column) {
            return Err(UpdateCopyError::Masked(meta.name.clone()));
        }
        set.push(column);
    }
    if set.is_empty() {
        return Err(UpdateCopyError::OnlyKeyColumns);
    }
    let name = qualified(dialect, &table.object.schema, &table.object.name);
    let engine = dialect.name();
    let mut out = String::new();
    let mut done = HashSet::new();
    for &row in rows {
        if !done.insert(row) {
            continue;
        }
        let value = |column: usize| {
            result
                .value_at(row, column)
                .ok_or(UpdateCopyError::NoSuchRow(row))
        };
        let mut assignments = Vec::with_capacity(set.len());
        for &column in &set {
            assignments.push(format!(
                "{} = {}",
                dialect.quote_identifier(&metas[column].name),
                sql_literal(&value(column)?, engine)
            ));
        }
        let mut conditions = Vec::with_capacity(keys.len());
        for &(column, key) in &keys {
            let value = value(column)?;
            if value == Value::Null {
                return Err(UpdateCopyError::NullKey(key.to_string()));
            }
            conditions.push(format!(
                "{} = {}",
                dialect.quote_identifier(key),
                sql_literal(&value, engine)
            ));
        }
        out.push_str(&format!(
            "UPDATE {name} SET {} WHERE {};\n",
            assignments.join(", "),
            conditions.join(" AND ")
        ));
    }
    Ok(out)
}

/// The line break between rows of [`CopySpecial`] text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowDelimiter {
    /// `\n`.
    Lf,
    /// `\r\n`, what Windows programs and RFC 4180 expect.
    CrLf,
}

impl RowDelimiter {
    pub fn as_str(self) -> &'static str {
        match self {
            RowDelimiter::Lf => "\n",
            RowDelimiter::CrLf => "\r\n",
        }
    }
}

/// Most characters of [`CopySpecial::null_text`].
pub const MAX_NULL_TEXT: usize = 32;

/// How "Copy special…" writes a selection as delimited text (SDB-388); the user's choices are
/// the copy defaults (`History::set_copy_defaults`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopySpecial {
    /// Between fields: a punctuation mark, a space or a tab.
    pub delimiter: char,
    /// Around a field that holds the delimiter, the quote or a line break, doubled inside it;
    /// `None` never quotes (such a field then reads as several).
    pub quote: Option<char>,
    pub row_delimiter: RowDelimiter,
    /// What a NULL is written as.
    pub null_text: String,
    /// The column names as the first row.
    pub header: bool,
    /// A first column with each row's number as the grid shows it (`#` in the header).
    pub row_numbers: bool,
}

impl Default for CopySpecial {
    /// What the grid's plain copy writes: tab-separated with a header, `"` quoting, `\n`
    /// between rows and NULL as an empty field.
    fn default() -> Self {
        Self {
            delimiter: '\t',
            quote: Some('"'),
            row_delimiter: RowDelimiter::Lf,
            null_text: String::new(),
            header: true,
            row_numbers: false,
        }
    }
}

impl CopySpecial {
    /// Options whose text can be read back: a delimiter and a quote that are neither letters,
    /// digits, line breaks nor each other, and a NULL text that holds neither of them.
    pub fn check(&self) -> Result<(), String> {
        let d = self.delimiter;
        if matches!(d, '\r' | '\n') || d.is_alphanumeric() || (d.is_control() && d != '\t') {
            return Err(format!(
                "{d:?} cannot separate fields: use a punctuation mark, a space or a tab"
            ));
        }
        if let Some(q) = self.quote {
            if q.is_alphanumeric() || q.is_whitespace() || q.is_control() {
                return Err(format!("{q:?} cannot quote fields: use a punctuation mark"));
            }
            if q == d {
                return Err("the delimiter and the quote must be different characters".into());
            }
        }
        if self.null_text.chars().count() > MAX_NULL_TEXT {
            return Err(format!(
                "the NULL text has at most {MAX_NULL_TEXT} characters"
            ));
        }
        if self.null_text.chars().any(char::is_control) {
            return Err("the NULL text cannot hold a tab or a line break".into());
        }
        if self.null_text.contains(d) || self.quote.is_some_and(|q| self.null_text.contains(q)) {
            return Err("the NULL text cannot hold the delimiter or the quote".into());
        }
        Ok(())
    }

    fn field(&self, value: &str) -> String {
        match self.quote {
            Some(q) if value.contains([self.delimiter, q, '\r', '\n']) => {
                let doubled: String = [q, q].iter().collect();
                format!("{q}{}{q}", value.replace(q, &doubled))
            }
            _ => value.to_string(),
        }
    }
}

/// The rows of the view at `rows` (positions, in display order, each once) and the result's
/// `columns` as delimited text, written as `options` say (SDB-388). The values are the
/// database's own text (a masked column's is the mask, ADR-051); a NULL is
/// [`CopySpecial::null_text`]. Every row ends with the row delimiter, except that one cell
/// without a header or a row number is its value alone, as the grid's plain copy writes it.
pub fn copy_special(
    result: &StoredResult,
    rows: &[usize],
    columns: &[usize],
    options: &CopySpecial,
) -> Result<String, CopyError> {
    options.check().map_err(CopyError::Options)?;
    let metas = &result.schema().columns;
    if let Some(&column) = columns.iter().find(|&&c| c >= metas.len()) {
        return Err(CopyError::NoSuchColumn(column));
    }
    if let Some(&row) = rows.iter().find(|&&r| r >= result.rows()) {
        return Err(CopyError::NoSuchRow(row));
    }
    let cell = |row: usize, column: usize| match result.value_at(row, column) {
        Some(Value::Null) | None => options.null_text.clone(),
        Some(value) => options.field(&value.to_string()),
    };
    if let ([row], [column], false, false) = (rows, columns, options.header, options.row_numbers) {
        return Ok(cell(*row, *column));
    }
    let separator = options.delimiter.to_string();
    let end = options.row_delimiter.as_str();
    let mut out = String::new();
    if options.header {
        let mut fields: Vec<String> = Vec::with_capacity(columns.len() + 1);
        if options.row_numbers {
            fields.push(options.field("#"));
        }
        fields.extend(columns.iter().map(|&c| options.field(&metas[c].name)));
        out.push_str(&fields.join(&separator));
        out.push_str(end);
    }
    let mut done = HashSet::new();
    for &row in rows {
        if !done.insert(row) {
            continue;
        }
        let mut fields: Vec<String> = Vec::with_capacity(columns.len() + 1);
        if options.row_numbers {
            fields.push((row + 1).to_string());
        }
        fields.extend(columns.iter().map(|&c| cell(row, c)));
        out.push_str(&fields.join(&separator));
        out.push_str(end);
    }
    Ok(out)
}

/// Text for an HTML cell: `&`, `<`, `>` and quotes escaped, line breaks as `<br>`.
fn html_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out.replace("\r\n", "<br>").replace(['\n', '\r'], "<br>")
}

/// A Markdown table cell: `|` escaped, line breaks as `<br>`, backslashes kept literal.
fn markdown_cell(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

fn is_numeric(logical: LogicalType) -> bool {
    match logical {
        LogicalType::Int | LogicalType::Float | LogicalType::Decimal => true,
        LogicalType::Bool
        | LogicalType::Text
        | LogicalType::Bytes
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => false,
    }
}

/// Repeated column names (`id`, `id`) become `id`, `id_2` so JSON keys stay distinct.
fn unique_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let mut candidate = name.to_string();
        let mut n = 2;
        while out.contains(&candidate) {
            candidate = format!("{name}_{n}");
            n += 1;
        }
        out.push(candidate);
    }
    out
}

pub(crate) fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// What [`RowWriter::begin`], `row` and `finish` say for a format written in batches.
fn not_text() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "Parquet and Arrow are written a batch at a time, not as text",
    )
}

/// A timestamp's ISO 8601 text: `2024-03-04T10:00:00.5`, or `…Z` with a time zone (the core
/// keeps those in UTC). `None` for what ISO cannot hold (infinity, BC) and other values.
fn iso_timestamp(value: &Value) -> Option<String> {
    let text = value.to_string();
    if text.contains("infinity") || text.ends_with(" BC") {
        return None;
    }
    match value {
        Value::Timestamp(_) => Some(text.replacen(' ', "T", 1)),
        Value::TimestampTz(_) => Some(format!(
            "{}Z",
            text.strip_suffix("+00")?.replacen(' ', "T", 1)
        )),
        _ => None,
    }
}

/// Numbers and booleans as JSON values; JSON columns embedded as JSON (unless `options` says
/// otherwise); exact decimals and everything else as strings (a JSON number could lose a
/// decimal's precision); timestamps in ISO 8601 when `options` asks.
fn json_value_with(value: &Value, logical: LogicalType, options: JsonOptions) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => value.to_string(),
        Value::Timestamp(_) | Value::TimestampTz(_) if options.iso_dates => {
            json_string(&iso_timestamp(value).unwrap_or_else(|| value.to_string()))
        }
        other => {
            let text = other.to_string();
            let is_json = options.json_as_json
                && logical == LogicalType::Json
                && matches!(
                    text.trim_start().chars().next(),
                    Some('{' | '[' | '"' | 't' | 'f' | 'n' | '-' | '0'..='9')
                )
                && json_is_valid(&text);
            if is_json { text } else { json_string(&text) }
        }
    }
}

/// Minimal check that a JSON column's text can be embedded as is: balanced brackets outside
/// strings. The server already validated it as JSON; this only guards against surprises.
fn json_is_valid(text: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_string
}

/// A literal for `INSERT`, following the engine (`dialect` is the [`Dialect::name`]).
///
/// MySQL and MariaDB read a `\` in a string as an escape, unless the `sql_mode` has
/// `NO_BACKSLASH_ESCAPES` (SDB-161), and the session the literal runs on (or the server an
/// exported file is loaded into) may have either. A string that holds a backslash is written
/// as its UTF-8 bytes with a character set introducer, `_utf8mb4 X'…'`: a string in every mode,
/// compared with the column's collation like any literal. Strings without one keep `'…'`,
/// which both modes read the same.
pub(crate) fn sql_literal(value: &Value, dialect: &str) -> String {
    let quoted = |text: &str| {
        if dialect == "mysql" && text.contains('\\') {
            let hex: String = text.bytes().map(|b| format!("{b:02X}")).collect();
            return format!("_utf8mb4 X'{hex}'");
        }
        // Where a backslash escapes in a string, `\` and `'` are escaped with one, or a value
        // ending in `\` would end its literal early (SDB-435). Not `''`: Spanner and HiveQL
        // read it as two strings.
        if spelaion_dialect::backslash_escapes(dialect) {
            return format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"));
        }
        let text = text.replace('\'', "''");
        // SQL Server: `N'…'` keeps every character (plain `'…'` goes through a code page).
        if dialect == "sqlserver" {
            format!("N'{text}'")
        } else {
            format!("'{text}'")
        }
    };
    match value {
        Value::Null => "NULL".into(),
        // T-SQL has no TRUE or FALSE: `bit` takes 1 and 0.
        Value::Bool(b) if dialect == "sqlserver" => if *b { "1" } else { "0" }.into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => value.to_string(),
        Value::Decimal(d) => d.clone(),
        Value::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            match dialect {
                "postgres" => format!("'\\x{hex}'"),
                "duckdb" => format!("from_hex('{hex}')"),
                "sqlserver" => format!("0x{hex}"),
                _ => format!("X'{hex}'"),
            }
        }
        Value::Date(_) | Value::Timestamp(_) | Value::TimestampTz(_) if dialect == "sqlserver" => {
            tsql_date_literal(value).unwrap_or_else(|| quoted(&value.to_string()))
        }
        other => quoted(&other.to_string()),
    }
}

/// A SQL Server date or time literal that means the same under every language (SDB-123).
/// `'YYYY-MM-DD hh:mm:ss'` is read by the session's DATEFORMAT for `datetime` and
/// `smalldatetime` (the 4th of March is the 3rd of April under `dmy`), and a `datetimeoffset`
/// needs `+hh:mm`. ISO 8601 with `T` (`'2024-03-04T10:00:00.5'`, `+00:00` for a time zone) and
/// an unseparated date (`'20240304'`) are read alike by every type and language. `None` for
/// what SQL Server never has (infinity, BC): the generic text.
fn tsql_date_literal(value: &Value) -> Option<String> {
    let text = value.to_string();
    if text.contains("infinity") || text.ends_with(" BC") {
        return None;
    }
    let iso = match value {
        Value::Date(_) => text.replace('-', ""),
        Value::Timestamp(_) => text.replacen(' ', "T", 1),
        Value::TimestampTz(_) => {
            format!("{}+00:00", text.strip_suffix("+00")?.replacen(' ', "T", 1))
        }
        _ => return None,
    };
    Some(format!("'{iso}'"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        ArrayRef, BinaryArray, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
    };
    use spelaion_driver_api::{ColumnMeta, ResultSchema, StatementSpan};

    use super::*;
    use crate::ResultStore;

    struct Quoted(&'static str);

    impl Dialect for Quoted {
        fn name(&self) -> &'static str {
            self.0
        }
        fn split_statements(&self, _script: &str) -> Vec<StatementSpan> {
            Vec::new()
        }
        fn quote_identifier(&self, ident: &str) -> String {
            match self.0 {
                "mysql" => format!("`{ident}`"),
                _ => format!("\"{ident}\""),
            }
        }
    }

    fn sample() -> Arc<std::sync::RwLock<StoredResult>> {
        let meta = |name: &str, logical| ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
            origin: None,
        };
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int),
            meta("name", LogicalType::Text),
            meta("price", LogicalType::Float),
            meta("ok", LogicalType::Bool),
            meta("doc", LogicalType::Json),
            meta("raw", LogicalType::Bytes),
            meta("amount", LogicalType::Decimal),
            meta("id", LogicalType::Int),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(2), None])),
            Arc::new(StringArray::from(vec![
                Some("plain"),
                Some("a,b \"q\"\tc\nd"),
                Some("it's \\ x"),
            ])),
            Arc::new(Float64Array::from(vec![Some(1.5), Some(f64::NAN), None])),
            Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
            Arc::new(StringArray::from(vec![
                Some("{\"a\": [1, 2]}"),
                Some("not json {"),
                None,
            ])),
            Arc::new(BinaryArray::from(vec![
                Some(&[0xde_u8, 0xad][..]),
                None,
                Some(&[][..]),
            ])),
            Arc::new(StringArray::from(vec![
                Some("12345678901234567890.1234"),
                None,
                Some("0"),
            ])),
            Arc::new(Int64Array::from(vec![Some(10), Some(20), Some(30)])),
        ];
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
            )
            .unwrap();
        store.get(id).unwrap()
    }

    fn render(
        format: ExportFormat,
        rows: Range<usize>,
        columns: Range<usize>,
        header: bool,
        dialect: &'static str,
    ) -> String {
        let result = sample();
        let result = result.read().unwrap();
        let mut out = Vec::new();
        write_result(
            &result,
            &ExportOptions {
                format,
                rows,
                columns: columns.collect(),
                header,
                csv: CsvOptions::default(),
            },
            &Quoted(dialect),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// A text column and a number column, one row per pair.
    fn copy_sample(rows: &[(Option<&str>, Option<i64>)]) -> Arc<std::sync::RwLock<StoredResult>> {
        let meta = |name: &str, logical| ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
            origin: None,
        };
        let schema = Arc::new(ResultSchema::new(vec![
            meta("value", LogicalType::Text),
            meta("n", LogicalType::Int),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(
                rows.iter().map(|(text, _)| *text).collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            )),
        ];
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
            )
            .unwrap();
        store.get(id).unwrap()
    }

    fn copy(
        result: &StoredResult,
        format: ExportFormat,
        rows: Range<usize>,
        columns: Vec<usize>,
        header: bool,
        csv: CsvOptions,
    ) -> String {
        let options = ExportOptions {
            format,
            rows,
            columns,
            header,
            csv,
        };
        copy_text(result, &options, &Quoted("postgres")).unwrap()
    }

    #[test]
    fn sparse_copy_preserves_view_order_and_excludes_other_rows() {
        let result = copy_sample(&[
            (Some("one"), Some(1)),
            (Some("two"), Some(2)),
            (Some("three"), Some(3)),
            (Some("four"), Some(4)),
        ]);
        let result = result.read().unwrap();
        let options = ExportOptions {
            format: ExportFormat::Tsv,
            rows: 0..4,
            columns: vec![0],
            header: true,
            csv: CsvOptions::default(),
        };
        assert_eq!(
            copy_selected_rows(&result, &options, &[3, 1, 3], &Quoted("postgres")).unwrap(),
            "value\ntwo\nfour\n"
        );
        assert!(copy_selected_rows(&result, &options, &[4], &Quoted("postgres")).is_err());
    }

    #[test]
    fn sparse_copy_uses_one_formatter_for_every_copy_format() {
        let stored = copy_sample(&[
            (Some("alpha"), Some(1)),
            (Some("bravo"), Some(2)),
            (Some("charlie"), Some(3)),
            (Some("delta"), Some(4)),
        ]);
        let result = stored.read().unwrap();
        for format in [
            ExportFormat::Tsv,
            ExportFormat::Csv,
            ExportFormat::Json,
            ExportFormat::SqlInsert { table: "t".into() },
            ExportFormat::SqlInsertBatch { table: "t".into() },
            ExportFormat::Markdown,
            ExportFormat::Html,
            ExportFormat::InList,
        ] {
            for header in [false, true] {
                let options = ExportOptions {
                    format: format.clone(),
                    rows: 0..4,
                    columns: vec![0],
                    header,
                    csv: CsvOptions::default(),
                };
                let text =
                    copy_selected_rows(&result, &options, &[3, 1], &Quoted("postgres")).unwrap();
                assert!(
                    text.contains("bravo") && text.contains("delta"),
                    "{format:?}: {text}"
                );
                assert!(
                    !text.contains("alpha") && !text.contains("charlie"),
                    "{format:?}: {text}"
                );
                assert!(
                    text.find("bravo") < text.find("delta"),
                    "{format:?}: {text}"
                );
            }
        }
    }

    /// What `write_result` writes for the same options: today's copy, byte for byte.
    fn written(
        result: &StoredResult,
        format: ExportFormat,
        rows: Range<usize>,
        columns: Vec<usize>,
        header: bool,
    ) -> String {
        let mut out = Vec::new();
        let options = ExportOptions {
            format,
            rows,
            columns,
            header,
            csv: CsvOptions::default(),
        };
        write_result(result, &options, &Quoted("postgres"), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    const COPY_VALUES: [(Option<&str>, Option<i64>); 6] = [
        (Some("plain"), Some(1)),
        (Some("ends with a line break\n"), Some(-2)),
        (Some("windows line\r\n"), None),
        (Some("tab\tand \"quote\", comma\nsecond line"), Some(4)),
        (None, Some(5)),
        (Some("=1+1"), Some(6)),
    ];

    #[test]
    fn one_cell_copies_as_its_value_alone() {
        let result = copy_sample(&COPY_VALUES);
        let result = result.read().unwrap();
        let one = |row: usize, column: usize| {
            copy(
                &result,
                ExportFormat::Tsv,
                row..row + 1,
                vec![column],
                false,
                CsvOptions::default(),
            )
        };
        // No line break after it, no quotes around it.
        assert_eq!(one(0, 0), "plain");
        assert_eq!(one(0, 1), "1");
        assert_eq!(one(1, 1), "-2");
        // A line break that is part of the value stays, Windows' too.
        assert_eq!(one(1, 0), "ends with a line break\n");
        assert_eq!(one(2, 0), "windows line\r\n");
        assert_eq!(one(3, 0), "tab\tand \"quote\", comma\nsecond line");
        // NULL is the null text (empty by default), and nothing else.
        assert_eq!(one(4, 0), "");
        assert_eq!(one(2, 1), "");
        assert_eq!(one(5, 0), "=1+1");
        // A hidden column left out of the selection, a row past the end: still one cell.
        let hidden = copy(
            &result,
            ExportFormat::Tsv,
            0..1,
            vec![1, 99],
            false,
            CsvOptions::default(),
        );
        assert_eq!(hidden, "1");
        let past_end = copy(
            &result,
            ExportFormat::Tsv,
            5..9,
            vec![0],
            false,
            CsvOptions::default(),
        );
        assert_eq!(past_end, "=1+1");
        // The copy's own options still apply to its value.
        let options = CsvOptions {
            null_text: "NULL".into(),
            escape_formulas: true,
            ..CsvOptions::default()
        };
        let with = |row: usize, column: usize| {
            copy(
                &result,
                ExportFormat::Tsv,
                row..row + 1,
                vec![column],
                false,
                options.clone(),
            )
        };
        assert_eq!(with(4, 0), "NULL");
        assert_eq!(with(5, 0), "'=1+1");
        assert_eq!(with(1, 1), "-2");
    }

    #[test]
    fn every_other_copy_is_written_as_before_byte_for_byte() {
        let result = copy_sample(&COPY_VALUES);
        let result = result.read().unwrap();
        let same = |format: ExportFormat, rows: Range<usize>, columns: Vec<usize>, header: bool| {
            let copied = copy(
                &result,
                format.clone(),
                rows.clone(),
                columns.clone(),
                header,
                CsvOptions::default(),
            );
            assert_eq!(copied, written(&result, format, rows, columns, header));
            copied
        };
        // Two rows of one column: each row ends in its line break, quoted values stay quoted.
        assert_eq!(
            same(ExportFormat::Tsv, 0..2, vec![0], false),
            "plain\n\"ends with a line break\n\"\n"
        );
        // One row of several cells keeps its final line break (spreadsheets paste rows so).
        assert_eq!(
            same(ExportFormat::Tsv, 0..1, vec![0, 1], false),
            "plain\t1\n"
        );
        // One cell with its column's name is a header and a row.
        assert_eq!(
            same(ExportFormat::Tsv, 0..1, vec![0], true),
            "value\nplain\n"
        );
        // Other formats keep theirs, CSV's Windows line endings included.
        assert_eq!(same(ExportFormat::Csv, 0..1, vec![0], false), "plain\r\n");
        assert_eq!(
            same(ExportFormat::Csv, 0..2, vec![0, 1], false),
            "plain,1\r\n\"ends with a line break\n\",-2\r\n"
        );
        same(ExportFormat::Json, 0..1, vec![0], false);
        same(ExportFormat::Markdown, 0..1, vec![0], false);
        same(ExportFormat::Html, 0..1, vec![0], false);
        same(ExportFormat::InList, 0..1, vec![0], false);
        same(
            ExportFormat::SqlInsertBatch { table: "t".into() },
            0..1,
            vec![0],
            false,
        );
        // Nothing selected is nothing.
        assert_eq!(same(ExportFormat::Tsv, 9..12, vec![0], false), "");
        assert_eq!(same(ExportFormat::Tsv, 0..1, vec![99], false), "\n");
    }

    #[test]
    fn tsv_quotes_only_what_spreadsheets_need() {
        assert_eq!(
            render(ExportFormat::Tsv, 0..3, 0..2, true, "postgres"),
            "id\tname\n1\tplain\n2\t\"a,b \"\"q\"\"\tc\nd\"\n\tit's \\ x\n"
        );
    }

    #[test]
    fn columns_are_taken_in_the_order_given_and_hidden_ones_left_out() {
        let result = sample();
        let result = result.read().unwrap();
        let mut out = Vec::new();
        write_result(
            &result,
            &ExportOptions {
                format: ExportFormat::Tsv,
                rows: 0..2,
                // The grid hides "name": the selection id..price sends 0 and 2; 99 is gone.
                columns: vec![2, 0, 99],
                header: true,
                csv: CsvOptions::default(),
            },
            &Quoted("postgres"),
            &mut out,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "price\tid\n1.5\t1\nNaN\t2\n"
        );
    }

    #[test]
    fn markdown_tables_escape_pipes_and_line_breaks() {
        assert_eq!(
            render(ExportFormat::Markdown, 0..3, 0..3, false, "postgres"),
            "| id | name | price |\n\
             | ---: | --- | ---: |\n\
             | 1 | plain | 1.5 |\n\
             | 2 | a,b \"q\"\tc<br>d | NaN |\n\
             | NULL | it's \\\\ x | NULL |\n"
        );
        assert_eq!(
            render(ExportFormat::Markdown, 0..0, 1..2, false, "postgres"),
            "| name |\n| --- |\n"
        );
    }

    #[test]
    fn in_lists_use_literals_and_drop_repeats() {
        assert_eq!(
            render(ExportFormat::InList, 0..3, 1..2, false, "postgres"),
            "('plain', 'a,b \"q\"\tc\nd', 'it''s \\ x')"
        );
        assert_eq!(
            render(ExportFormat::InList, 0..3, 0..1, false, "postgres"),
            "(1, 2, NULL)"
        );
        assert_eq!(
            render(ExportFormat::InList, 0..2, 0..2, false, "mysql"),
            "((1, 'plain'), (2, 'a,b \"q\"\tc\nd'))"
        );
    }

    #[test]
    fn csv_follows_rfc_4180() {
        assert_eq!(
            render(ExportFormat::Csv, 0..2, 0..3, false, "postgres"),
            "1,plain,1.5\r\n2,\"a,b \"\"q\"\"\tc\nd\",NaN\r\n"
        );
    }

    #[test]
    fn json_keeps_types_and_distinct_keys() {
        let json = render(ExportFormat::Json, 0..3, 0..8, false, "postgres");
        assert!(json.starts_with("[\n  {\"id\": 1, \"name\": \"plain\", \"price\": 1.5, \"ok\": true, \"doc\": {\"a\": [1, 2]}"), "{json}");
        assert!(json.contains("\"raw\": \"\\\\xdead\""), "{json}");
        assert!(
            json.contains("\"amount\": \"12345678901234567890.1234\""),
            "{json}"
        );
        assert!(json.contains("\"id_2\": 10"), "{json}");
        // NaN is not a JSON number; invalid JSON text is a string, not embedded.
        assert!(json.contains("\"price\": \"NaN\""), "{json}");
        assert!(json.contains("\"doc\": \"not json {\""), "{json}");
        assert!(
            json.contains("{\"id\": null, \"name\": \"it's \\\\ x\""),
            "{json}"
        );
        assert!(json.ends_with("}\n]\n"), "{json}");
        assert_eq!(
            render(ExportFormat::Json, 3..3, 0..8, false, "postgres"),
            "[]\n"
        );
    }

    #[test]
    fn inserts_use_each_engine_literals() {
        let table = || ExportFormat::SqlInsert {
            table: "items".into(),
        };
        let pg = render(table(), 0..3, 0..7, false, "postgres");
        assert_eq!(
            pg.lines().next().unwrap(),
            "INSERT INTO \"items\" (\"id\", \"name\", \"price\", \"ok\", \"doc\", \"raw\", \"amount\") VALUES (1, 'plain', 1.5, TRUE, '{\"a\": [1, 2]}', '\\xdead', 12345678901234567890.1234);"
        );
        // A value may contain a newline: statements end with ");\n", not lines.
        assert!(
            pg.contains("'NaN', FALSE, 'not json {', NULL, NULL);\n"),
            "{pg}"
        );
        assert!(
            pg.ends_with("VALUES (NULL, 'it''s \\ x', NULL, NULL, NULL, '\\x', 0);\n"),
            "{pg}"
        );
        let mysql = render(table(), 2..3, 0..2, false, "mysql");
        assert_eq!(
            mysql,
            // `it's \ x` holds a backslash: bytes with an introducer, read alike in every
            // sql_mode (SDB-161).
            "INSERT INTO `items` (`id`, `name`) VALUES (NULL, _utf8mb4 X'69742773205C2078');\n"
        );
        let sqlite = render(table(), 0..1, 5..6, false, "sqlite");
        assert_eq!(
            sqlite,
            "INSERT INTO \"items\" (\"raw\") VALUES (X'dead');\n"
        );
    }

    #[test]
    fn copied_insert_groups_rows_and_preserves_literals() {
        let batch = || ExportFormat::SqlInsertBatch {
            table: "items".into(),
        };
        assert_eq!(
            render(batch(), 0..3, 0..2, false, "mysql"),
            "INSERT INTO `items` (`id`, `name`) VALUES (1, 'plain'),\n  (2, 'a,b \"q\"\tc\nd'),\n  (NULL, _utf8mb4 X'69742773205C2078');\n"
        );
        assert_eq!(
            render(batch(), 0..1, 5..6, false, "sqlite"),
            "INSERT INTO \"items\" (\"raw\") VALUES (X'dead');\n"
        );
        assert_eq!(render(batch(), 3..3, 0..2, false, "postgres"), "");
    }

    /// SQL Server takes 1000 rows per VALUES: 2500 rows become three INSERTs.
    #[test]
    fn copied_insert_splits_at_sql_servers_limit() {
        let schema = Arc::new(ResultSchema::new(vec![ColumnMeta {
            name: "id".into(),
            logical: LogicalType::Int,
            native_type: "int".into(),
            native_id: None,
            origin: None,
        }]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let values: ArrayRef = Arc::new(Int64Array::from_iter_values(0..2_500));
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), vec![values]).unwrap(),
            )
            .unwrap();
        let result = store.get(id).unwrap();
        let mut out = Vec::new();
        write_result(
            &result.read().unwrap(),
            &ExportOptions {
                format: ExportFormat::SqlInsertBatch {
                    table: "items".into(),
                },
                rows: 0..2_500,
                columns: vec![0],
                header: false,
                csv: CsvOptions::default(),
            },
            &Quoted("sqlserver"),
            &mut out,
        )
        .unwrap();
        let sql = String::from_utf8(out).unwrap();
        let statements: Vec<&str> = sql
            .split(
                ";
",
            )
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(statements.len(), 3);
        assert_eq!(
            statements[0]
                .matches(
                    "),
  ("
                )
                .count(),
            999
        );
        assert!(statements[1].starts_with("INSERT INTO \"items\" (\"id\") VALUES (1000)"));
        assert!(sql.ends_with(
            "(2499);
"
        ));
    }

    #[test]
    fn copied_insert_stays_one_statement_across_chunks() {
        let schema = Arc::new(ResultSchema::new(vec![ColumnMeta {
            name: "id".into(),
            logical: LogicalType::Int,
            native_type: "integer".into(),
            native_id: None,
            origin: None,
        }]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let values: ArrayRef = Arc::new(Int64Array::from_iter_values(0..=CHUNK as i64));
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), vec![values]).unwrap(),
            )
            .unwrap();
        let result = store.get(id).unwrap();
        let mut out = Vec::new();
        let count = write_result(
            &result.read().unwrap(),
            &ExportOptions {
                format: ExportFormat::SqlInsertBatch {
                    table: "items".into(),
                },
                rows: 0..CHUNK + 1,
                columns: vec![0],
                header: false,
                csv: CsvOptions::default(),
            },
            &Quoted("sqlite"),
            &mut out,
        )
        .unwrap();
        let sql = String::from_utf8(out).unwrap();
        assert_eq!(count, CHUNK + 1);
        assert_eq!(sql.matches("INSERT INTO").count(), 1);
        assert!(sql.ends_with(&format!("  ({CHUNK});\n")));
        assert!(sql.contains(&format!("  ({}),\n  ({CHUNK})", CHUNK - 1)));
    }

    #[test]
    fn html_tables_escape_markup_and_leave_null_empty() {
        let result = sample();
        let result = result.read().unwrap();
        let mut out = Vec::new();
        write_result(
            &result,
            &ExportOptions {
                format: ExportFormat::Html,
                rows: 0..3,
                columns: vec![0, 1],
                header: false,
                csv: CsvOptions::default(),
            },
            &Quoted("postgres"),
            &mut out,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "<table>\n  <thead>\n    <tr><th>id</th><th>name</th></tr>\n  </thead>\n  <tbody>\n    \
             <tr><td>1</td><td>plain</td></tr>\n    \
             <tr><td>2</td><td>a,b &quot;q&quot;\tc<br>d</td></tr>\n    \
             <tr><td></td><td>it&#39;s \\ x</td></tr>\n  </tbody>\n</table>\n"
        );
        assert_eq!(html_text("<b>&</b>"), "&lt;b&gt;&amp;&lt;/b&gt;");
    }

    #[test]
    fn column_names_plain_quoted_qualified_and_as_a_list() {
        let result = sample();
        let result = result.read().unwrap();
        let names =
            |style, dialect, table| column_names(&result, &[1, 0], style, &Quoted(dialect), table);
        assert_eq!(
            names(NameStyle::Plain, "postgres", None),
            Ok("name, id".into())
        );
        assert_eq!(
            names(NameStyle::List, "postgres", None),
            Ok("name\nid".into())
        );
        assert_eq!(
            names(NameStyle::Quoted, "mysql", None),
            Ok("`name`, `id`".into())
        );
        // Quoted even where the engine would not need it.
        assert_eq!(
            names(NameStyle::Quoted, "sqlite", None),
            Ok("\"name\", \"id\"".into())
        );
        assert_eq!(
            names(NameStyle::Quoted, "sqlserver", None),
            Ok("[name], [id]".into())
        );
        assert_eq!(always_quoted(&Quoted("postgres"), "a\"b"), "\"a\"\"b\"");
        assert_eq!(always_quoted(&Quoted("sqlserver"), "a]b"), "[a]]b]");
        assert_eq!(
            names(NameStyle::Qualified, "postgres", Some(("public", "items"))),
            Ok("\"public\".\"items\".\"name\", \"public\".\"items\".\"id\"".into())
        );
        assert_eq!(
            names(NameStyle::Qualified, "postgres", None),
            Err(CopyError::NoTable)
        );
        assert_eq!(
            column_names(&result, &[9], NameStyle::Plain, &Quoted("postgres"), None),
            Err(CopyError::NoSuchColumn(9))
        );
    }

    #[test]
    fn a_columns_values_as_an_in_list_once_each_and_capped() {
        let result = sample();
        let result = result.read().unwrap();
        assert_eq!(
            column_in_list(&result, 0, &Quoted("postgres")),
            Ok("(1, 2, NULL)".into())
        );
        assert_eq!(
            column_in_list(&result, 1, &Quoted("mysql")),
            Ok("('plain', 'a,b \"q\"\tc\nd', _utf8mb4 X'69742773205C2078')".into())
        );
        assert_eq!(
            column_in_list(&result, 3, &Quoted("sqlserver")),
            Ok("(1, 0, NULL)".into())
        );
        assert_eq!(
            column_in_list(&result, 9, &Quoted("postgres")),
            Err(CopyError::NoSuchColumn(9))
        );

        // A column with more different values than an IN list takes is refused, not cut.
        let schema = Arc::new(ResultSchema::new(vec![ColumnMeta {
            name: "n".into(),
            logical: LogicalType::Int,
            native_type: "int8".into(),
            native_id: None,
            origin: None,
        }]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let values: ArrayRef = Arc::new(Int64Array::from_iter_values(
            (0..MAX_IN_LIST as i64 + 1).chain(0..5),
        ));
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), vec![values]).unwrap(),
            )
            .unwrap();
        let big = store.get(id).unwrap();
        let big = big.read().unwrap();
        assert_eq!(
            column_in_list(&big, 0, &Quoted("postgres")),
            Err(CopyError::TooManyValues)
        );
        // Repeated values count once: exactly the limit fits.
        let id = store.create(schema.clone());
        let values: ArrayRef = Arc::new(Int64Array::from_iter_values(
            (0..MAX_IN_LIST as i64).chain(0..MAX_IN_LIST as i64),
        ));
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), vec![values]).unwrap(),
            )
            .unwrap();
        let fits = store.get(id).unwrap();
        let list = column_in_list(&fits.read().unwrap(), 0, &Quoted("postgres")).unwrap();
        assert_eq!(list.matches(", ").count(), MAX_IN_LIST - 1);
    }

    #[test]
    fn files_are_written_whole_with_a_bom_for_csv() {
        let dir = std::env::temp_dir().join(format!("spelaion-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = sample();
        let result = result.read().unwrap();
        let options = |format| ExportOptions {
            format,
            rows: 0..usize::MAX,
            columns: vec![0, 1],
            header: true,
            csv: CsvOptions::default(),
        };
        let csv = dir.join("out.csv");
        let summary = export_to_file(
            &result,
            &options(ExportFormat::Csv),
            &FileOptions {
                encoding: TextEncoding::Utf8Bom,
                ..FileOptions::default()
            },
            &Quoted("postgres"),
            &csv,
        )
        .unwrap();
        assert_eq!(summary.rows, 3);
        assert_eq!(summary.bytes, std::fs::metadata(&csv).unwrap().len());
        let bytes = std::fs::read(&csv).unwrap();
        assert!(bytes.starts_with("\u{feff}id,name\r\n1,plain".as_bytes()));
        assert!(!dir.join("out.csv.part").exists());
        let json = dir.join("out.json");
        export_to_file(
            &result,
            &options(ExportFormat::Json),
            &FileOptions {
                encoding: TextEncoding::Utf8,
                ..FileOptions::default()
            },
            &Quoted("postgres"),
            &json,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&json)
                .unwrap()
                .starts_with("[\n  {")
        );
        // A failure leaves neither the file nor the temporary one.
        let missing = dir.join("no-such-dir").join("out.csv");
        assert!(
            export_to_file(
                &result,
                &options(ExportFormat::Csv),
                &FileOptions {
                    encoding: TextEncoding::Utf8Bom,
                    ..FileOptions::default()
                },
                &Quoted("postgres"),
                &missing
            )
            .is_err()
        );
        assert!(!missing.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_file_export_keeps_the_files_mode_and_writes_through_a_link() {
        let result = sample();
        let result = result.read().unwrap();
        let options = ExportOptions {
            format: ExportFormat::Csv,
            rows: 0..usize::MAX,
            columns: vec![0, 1],
            header: true,
            csv: CsvOptions::default(),
        };
        crate::test_files::assert_keeps_mode_and_links("data", |path| {
            export_to_file(
                &result,
                &options,
                &FileOptions {
                    encoding: TextEncoding::Utf8,
                    ..FileOptions::default()
                },
                &Quoted("postgres"),
                path,
            )
            .unwrap();
        });
    }

    #[test]
    fn two_file_exports_to_one_path_never_share_the_part_file() {
        let dir =
            std::env::temp_dir().join(format!("spelaion-export-parts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.csv");
        let mut first = PartFile::create(&path, TextEncoding::Utf8, false).unwrap();
        let mut second = PartFile::create(&path, TextEncoding::Utf8, false).unwrap();
        assert!(
            dir.join("out.csv.part").exists(),
            "the first is <name>.part (ADR-034)"
        );
        first.writer().write_all(b"first").unwrap();
        second.writer().write_all(b"second").unwrap();
        first.commit(1).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first");
        second.commit(1).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        // A cancelled one leaves nothing and the file as it was.
        let mut cancelled = PartFile::create(&path, TextEncoding::Utf8, false).unwrap();
        cancelled.writer().write_all(b"partial").unwrap();
        cancelled.discard();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ranges_are_clipped_to_the_result() {
        assert_eq!(
            render(ExportFormat::Tsv, 2..99, 7..99, false, "postgres"),
            "30\n"
        );
        assert_eq!(render(ExportFormat::Tsv, 5..9, 0..2, false, "postgres"), "");
    }

    fn render_csv(format: ExportFormat, columns: Vec<usize>, csv: CsvOptions) -> String {
        let result = sample();
        let result = result.read().unwrap();
        let mut out = Vec::new();
        write_result(
            &result,
            &ExportOptions {
                format,
                rows: 0..3,
                columns,
                header: true,
                csv,
            },
            &Quoted("postgres"),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn csv_options_change_the_delimiter_and_the_null_text() {
        let csv = CsvOptions {
            delimiter: ';',
            null_text: "NULL".into(),
            escape_formulas: false,
        };
        assert_eq!(
            render_csv(ExportFormat::Csv, vec![0, 1, 2], csv.clone()),
            "id;name;price\r\n1;plain;1.5\r\n2;\"a,b \"\"q\"\"\tc\nd\";NaN\r\nNULL;it's \\ x;NULL\r\n"
        );
        // A NULL text holding the delimiter is quoted like any field.
        let semicolon_null = CsvOptions {
            null_text: "n;a".into(),
            ..csv
        };
        assert!(render_csv(ExportFormat::Csv, vec![0], semicolon_null).ends_with("\"n;a\"\r\n"));
        // TSV keeps the tab whatever the CSV delimiter says.
        let tsv = CsvOptions {
            delimiter: ';',
            null_text: "\\N".into(),
            escape_formulas: false,
        };
        assert_eq!(
            render_csv(ExportFormat::Tsv, vec![0, 2], tsv),
            "id\tprice\n1\t1.5\n2\tNaN\n\\N\t\\N\n"
        );
    }

    #[test]
    fn delimiters_that_cannot_be_read_back_are_refused() {
        for bad in ['"', '\'', '\n', '\r', 'a', '7', '\u{0}'] {
            let csv = CsvOptions {
                delimiter: bad,
                ..CsvOptions::default()
            };
            assert!(csv.check().is_err(), "{bad:?}");
        }
        for good in [',', ';', '\t', '|', ' ', '^'] {
            let csv = CsvOptions {
                delimiter: good,
                ..CsvOptions::default()
            };
            assert!(csv.check().is_ok(), "{good:?}");
        }
    }

    #[test]
    fn formulas_are_escaped_but_numbers_are_not() {
        for (cell, escaped) in [
            ("=1+1", "'=1+1"),
            ("=HYPERLINK(\"http://x\")", "'=HYPERLINK(\"http://x\")"),
            ("+cmd|' /C calc'!A0", "'+cmd|' /C calc'!A0"),
            ("-2+3+cmd|' /C calc'!A0", "'-2+3+cmd|' /C calc'!A0"),
            ("@SUM(A1)", "'@SUM(A1)"),
            ("\t=1", "'\t=1"),
            ("\r=1", "'\r=1"),
            ("-", "'-"),
            ("-inf", "'-inf"),
            ("- 5", "'- 5"),
            ("-5", "-5"),
            ("+1.5", "+1.5"),
            ("-2e3", "-2e3"),
            ("-1.5E-3", "-1.5E-3"),
            ("plain", "plain"),
            ("a=b", "a=b"),
            ("", ""),
        ] {
            assert_eq!(formula_escaped(cell), escaped, "{cell:?}");
        }
        // In a file: text cells and column names are escaped, number columns never.
        let meta = |name: &str, logical| ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
            origin: None,
        };
        let schema = Arc::new(ResultSchema::new(vec![
            meta("=total", LogicalType::Text),
            meta("delta", LogicalType::Int),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        store
            .append(
                id,
                RecordBatch::try_new(
                    schema.arrow.clone(),
                    vec![
                        Arc::new(StringArray::from(vec!["=WEBSERVICE(\"x\")", "-5", "@x"])),
                        Arc::new(Int64Array::from(vec![-1, 2, -3])),
                    ],
                )
                .unwrap(),
            )
            .unwrap();
        let result = store.get(id).unwrap();
        let render = |escape_formulas| {
            let mut out = Vec::new();
            write_result(
                &result.read().unwrap(),
                &ExportOptions {
                    format: ExportFormat::Csv,
                    rows: 0..3,
                    columns: vec![0, 1],
                    header: true,
                    csv: CsvOptions {
                        escape_formulas,
                        ..CsvOptions::default()
                    },
                },
                &Quoted("postgres"),
                &mut out,
            )
            .unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(
            render(true),
            "'=total,delta\r\n\"'=WEBSERVICE(\"\"x\"\")\",-1\r\n-5,2\r\n'@x,-3\r\n"
        );
        assert_eq!(
            render(false),
            "=total,delta\r\n\"=WEBSERVICE(\"\"x\"\")\",-1\r\n-5,2\r\n@x,-3\r\n"
        );
    }

    #[test]
    fn encodings_write_their_mark_and_count_what_they_lack() {
        let encode = |encoding, pieces: &[&[u8]]| {
            let mut encoder = Encoder::new(Vec::new(), encoding);
            encoder.write_bom().unwrap();
            for piece in pieces {
                encoder.write_all(piece).unwrap();
            }
            let replaced = encoder.replaced();
            let out = encoder.into_inner();
            (out, replaced)
        };
        let text = "año €5 — 日";
        assert_eq!(
            encode(TextEncoding::Utf8, &[text.as_bytes()]),
            (text.as_bytes().to_vec(), 0)
        );
        let (bom, _) = encode(TextEncoding::Utf8Bom, &[text.as_bytes()]);
        assert_eq!(&bom[..3], &[0xef, 0xbb, 0xbf]);
        assert_eq!(&bom[3..], text.as_bytes());
        let (utf16, replaced) = encode(TextEncoding::Utf16Le, &[text.as_bytes()]);
        let mut expected = vec![0xff, 0xfe];
        for unit in text.encode_utf16() {
            expected.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!((utf16, replaced), (expected, 0));
        // Windows-1252: accents and the euro sign kept, the CJK character becomes `?`.
        assert_eq!(
            encode(TextEncoding::Windows1252, &[text.as_bytes()]),
            (
                vec![b'a', 0xf1, b'o', b' ', 0x80, b'5', b' ', 0x97, b' ', b'?'],
                1
            )
        );
        // A character split between two writes is still one character.
        let bytes = "ñ€".as_bytes();
        assert_eq!(
            encode(
                TextEncoding::Windows1252,
                &[&bytes[..1], &bytes[1..3], &bytes[3..]]
            ),
            (vec![0xf1, 0x80], 0)
        );
    }

    #[test]
    fn a_file_in_another_encoding_reports_what_it_replaced() {
        let dir = std::env::temp_dir().join(format!("spelaion-encoding-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = sample();
        let result = result.read().unwrap();
        let path = dir.join("out.csv");
        // An old file stays until the new one is complete.
        std::fs::write(&path, "old").unwrap();
        let summary = export_to_file(
            &result,
            &ExportOptions {
                format: ExportFormat::Csv,
                rows: 0..3,
                columns: vec![1],
                header: false,
                csv: CsvOptions::default(),
            },
            &FileOptions {
                encoding: TextEncoding::Utf16Le,
                ..FileOptions::default()
            },
            &Quoted("postgres"),
            &path,
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xfe]);
        assert_eq!(summary.bytes, bytes.len() as u64);
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(String::from_utf16(&units).unwrap().starts_with("plain\r\n"));
        // A bad delimiter is refused before the file is touched.
        let refused = export_to_file(
            &result,
            &ExportOptions {
                format: ExportFormat::Csv,
                rows: 0..3,
                columns: vec![1],
                header: false,
                csv: CsvOptions {
                    delimiter: '"',
                    ..CsvOptions::default()
                },
            },
            &FileOptions {
                encoding: TextEncoding::Utf8,
                ..FileOptions::default()
            },
            &Quoted("postgres"),
            &path,
        );
        assert!(refused.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sqlserver_date_literals_do_not_depend_on_the_language() {
        // 2024-03-04 and 2024-03-04 10:00:00.5 (days since 1970, microseconds).
        let day = 19_786;
        let micros = i64::from(day) * 86_400_000_000 + 36_000_500_000;
        assert_eq!(sql_literal(&Value::Date(day), "sqlserver"), "'20240304'");
        assert_eq!(
            sql_literal(&Value::Timestamp(micros), "sqlserver"),
            "'2024-03-04T10:00:00.5'"
        );
        assert_eq!(
            sql_literal(&Value::TimestampTz(micros), "sqlserver"),
            "'2024-03-04T10:00:00.5+00:00'"
        );
        // What SQL Server never has keeps the generic text; other engines are unchanged.
        assert_eq!(
            sql_literal(
                // i64::MAX is the driver API's infinity.
                &Value::Timestamp(i64::MAX),
                "sqlserver"
            ),
            "N'infinity'"
        );
        assert_eq!(
            sql_literal(&Value::Timestamp(micros), "postgres"),
            "'2024-03-04 10:00:00.5'"
        );
        assert_eq!(
            sql_literal(&Value::TimestampTz(micros), "mysql"),
            "'2024-03-04 10:00:00.5+00'"
        );
    }

    fn special(
        result: &StoredResult,
        rows: &[usize],
        columns: &[usize],
        options: &CopySpecial,
    ) -> String {
        copy_special(result, rows, columns, options).unwrap()
    }

    #[test]
    fn copy_special_writes_the_chosen_delimiters_quotes_and_null() {
        let result = copy_sample(&[
            (Some("plain"), Some(1)),
            (Some("a;b"), None),
            (None, Some(3)),
            (Some("say \"hi\"\nthere"), Some(4)),
        ]);
        let result = result.read().unwrap();
        let options = CopySpecial {
            delimiter: ';',
            quote: Some('"'),
            row_delimiter: RowDelimiter::CrLf,
            null_text: "NULL".into(),
            header: true,
            row_numbers: false,
        };
        assert_eq!(
            special(&result, &[0, 1, 2, 3], &[0, 1], &options),
            "value;n\r\nplain;1\r\n\"a;b\";NULL\r\nNULL;3\r\n\"say \"\"hi\"\"\nthere\";4\r\n"
        );
        // Another quote, doubled inside; the old one is plain text then.
        let single = CopySpecial {
            quote: Some('\''),
            row_delimiter: RowDelimiter::Lf,
            header: false,
            ..options.clone()
        };
        assert_eq!(
            special(&result, &[1, 3], &[0], &single),
            "'a;b'\n'say \"hi\"\nthere'\n"
        );
        // No quote: fields are written as they are.
        let unquoted = CopySpecial {
            quote: None,
            ..single.clone()
        };
        assert_eq!(special(&result, &[1], &[0, 1], &unquoted), "a;b;NULL\n");
        // Row numbers are the grid's, in the order given; a row twice is copied once.
        let numbered = CopySpecial {
            delimiter: '\t',
            row_numbers: true,
            header: true,
            ..options.clone()
        };
        assert_eq!(
            special(&result, &[2, 0, 2], &[1], &numbered),
            "#\tn\r\n3\t3\r\n1\t1\r\n"
        );
        // One cell without a header or a number is its value alone.
        let bare = CopySpecial {
            header: false,
            ..options.clone()
        };
        assert_eq!(special(&result, &[1], &[0], &bare), "\"a;b\"");
        assert_eq!(special(&result, &[2], &[0], &bare), "NULL");
        // Rows and columns outside the result are refused, never copied as something else.
        assert_eq!(
            copy_special(&result, &[4], &[0], &options),
            Err(CopyError::NoSuchRow(4))
        );
        assert_eq!(
            copy_special(&result, &[0], &[2], &options),
            Err(CopyError::NoSuchColumn(2))
        );
    }

    #[test]
    fn copy_special_refuses_options_that_cannot_be_read_back() {
        let result = copy_sample(&[(Some("x"), Some(1))]);
        let result = result.read().unwrap();
        let base = CopySpecial::default();
        assert_eq!(base.check(), Ok(()));
        for options in [
            CopySpecial {
                delimiter: '\n',
                ..base.clone()
            },
            CopySpecial {
                delimiter: 'a',
                ..base.clone()
            },
            CopySpecial {
                delimiter: '7',
                ..base.clone()
            },
            CopySpecial {
                delimiter: '"',
                ..base.clone()
            },
            CopySpecial {
                quote: Some(' '),
                ..base.clone()
            },
            CopySpecial {
                quote: Some('q'),
                ..base.clone()
            },
            CopySpecial {
                null_text: "a\tb".into(),
                ..base.clone()
            },
            CopySpecial {
                delimiter: ',',
                null_text: "a,b".into(),
                ..base.clone()
            },
            CopySpecial {
                null_text: "\"N\"".into(),
                ..base.clone()
            },
            CopySpecial {
                null_text: "n".repeat(MAX_NULL_TEXT + 1),
                ..base.clone()
            },
        ] {
            assert!(options.check().is_err(), "{options:?}");
            assert!(
                matches!(
                    copy_special(&result, &[0], &[0, 1], &options),
                    Err(CopyError::Options(_))
                ),
                "{options:?}"
            );
        }
        // A space, `|` and a quote other than `"` are fine.
        for options in [
            CopySpecial {
                delimiter: ' ',
                ..base.clone()
            },
            CopySpecial {
                delimiter: '|',
                quote: Some('\''),
                null_text: "\\N".into(),
                ..base.clone()
            },
        ] {
            assert_eq!(options.check(), Ok(()), "{options:?}");
        }
    }

    /// The SQL export of `rows` and `columns` of the sample through `options`, into `target`.
    fn sql_export(
        engine: &'static str,
        options: &SqlOptions,
        target: &SqlTarget,
        rows: Range<usize>,
        columns: Vec<usize>,
    ) -> String {
        let result = sample();
        let result = result.read().unwrap();
        let metas = columns
            .iter()
            .map(|&c| result.schema().columns[c].clone())
            .collect();
        let dialect = Quoted(engine);
        let writer = RowWriter::new(
            ExportFormat::SqlInsert {
                table: "items".into(),
            },
            false,
            CsvOptions::default(),
            &dialect,
            metas,
        )
        .with_sql(&dialect, options, target);
        let mut out = Vec::new();
        write_rows(&result, rows, &columns, writer, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn sql_options_group_rows_qualify_and_change_case() {
        let in_public = SqlTarget {
            schema: Some("public".into()),
            key: vec![],
        };
        let two = SqlOptions {
            rows_per_insert: 2,
            ..SqlOptions::default()
        };
        assert_eq!(
            sql_export("postgres", &two, &in_public, 0..3, vec![0, 1]),
            "INSERT INTO \"public\".\"items\" (\"id\", \"name\") VALUES (1, 'plain'),\n  (2, 'a,b \"q\"\tc\nd');\nINSERT INTO \"public\".\"items\" (\"id\", \"name\") VALUES (NULL, 'it''s \\ x');\n"
        );
        let styled = SqlOptions {
            omit_schema: true,
            keyword_case: KeywordCase::Lower,
            name_case: NameCase::Upper,
            ..SqlOptions::default()
        };
        assert_eq!(
            sql_export("postgres", &styled, &in_public, 0..1, vec![0, 1]),
            "insert into \"ITEMS\" (\"ID\", \"NAME\") values (1, 'plain');\n"
        );
        // SQL Server's statements hold at most 1,000 rows, whatever was asked.
        let many = SqlOptions {
            rows_per_insert: MAX_ROWS_PER_INSERT,
            ..SqlOptions::default()
        };
        let result = sample();
        let result = result.read().unwrap();
        let metas = vec![result.schema().columns[0].clone()];
        for (engine, per) in [("sqlserver", 1_000), ("postgres", MAX_ROWS_PER_INSERT)] {
            let writer = RowWriter::new(
                ExportFormat::SqlInsert { table: "t".into() },
                false,
                CsvOptions::default(),
                &Quoted(engine),
                metas.clone(),
            )
            .with_sql(&Quoted(engine), &many, &SqlTarget::default());
            assert_eq!(writer.rows_per_statement, per, "{engine}");
        }
        assert!(
            SqlOptions {
                rows_per_insert: 0,
                ..SqlOptions::default()
            }
            .check()
            .is_err()
        );
        assert!(
            SqlOptions {
                rows_per_insert: MAX_ROWS_PER_INSERT + 1,
                ..SqlOptions::default()
            }
            .check()
            .is_err()
        );
    }

    #[test]
    fn upserts_are_written_in_each_engines_form() {
        let upsert = SqlOptions {
            upsert: true,
            ..SqlOptions::default()
        };
        let by_id = SqlTarget {
            schema: None,
            key: vec!["id".into()],
        };
        let grouped = SqlOptions {
            rows_per_insert: 2,
            ..upsert.clone()
        };
        assert_eq!(
            sql_export("postgres", &upsert, &by_id, 0..1, vec![0, 1]),
            "INSERT INTO \"items\" (\"id\", \"name\") VALUES (1, 'plain') ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\";\n"
        );
        assert_eq!(
            sql_export("mysql", &upsert, &by_id, 0..1, vec![0, 1]),
            "INSERT INTO `items` (`id`, `name`) VALUES (1, 'plain') ON DUPLICATE KEY UPDATE `name` = VALUES(`name`);\n"
        );
        assert_eq!(
            sql_export("sqlserver", &grouped, &by_id, 0..2, vec![0, 1]),
            "MERGE INTO \"items\" AS tgt USING (VALUES (1, N'plain'),\n  (2, N'a,b \"q\"\tc\nd')) AS src (\"id\", \"name\") ON tgt.\"id\" = src.\"id\" WHEN MATCHED THEN UPDATE SET tgt.\"name\" = src.\"name\" WHEN NOT MATCHED THEN INSERT (\"id\", \"name\") VALUES (src.\"id\", src.\"name\");\n"
        );
        // Every column in the key: nothing to update.
        assert_eq!(
            sql_export("sqlite", &upsert, &by_id, 0..1, vec![0]),
            "INSERT INTO \"items\" (\"id\") VALUES (1) ON CONFLICT (\"id\") DO NOTHING;\n"
        );
        assert_eq!(
            sql_export("mysql", &upsert, &by_id, 0..1, vec![0]),
            "INSERT INTO `items` (`id`) VALUES (1) ON DUPLICATE KEY UPDATE `id` = VALUES(`id`);\n"
        );
        assert_eq!(
            sql_export("sqlserver", &upsert, &by_id, 0..1, vec![0]),
            "MERGE INTO \"items\" AS tgt USING (VALUES (1)) AS src (\"id\") ON tgt.\"id\" = src.\"id\" WHEN NOT MATCHED THEN INSERT (\"id\") VALUES (src.\"id\");\n"
        );
        // Lower-case keywords reach every part of it.
        let lower = SqlOptions {
            keyword_case: KeywordCase::Lower,
            ..upsert.clone()
        };
        assert_eq!(
            sql_export("duckdb", &lower, &by_id, 0..1, vec![0, 1]),
            "insert into \"items\" (\"id\", \"name\") values (1, 'plain') on conflict (\"id\") do update set \"name\" = excluded.\"name\";\n"
        );
        // Without a key there is no upsert to write: plain INSERTs.
        assert_eq!(
            sql_export("postgres", &upsert, &SqlTarget::default(), 0..1, vec![0]),
            "INSERT INTO \"items\" (\"id\") VALUES (1);\n"
        );
    }

    #[test]
    fn ndjson_is_one_object_per_line_and_json_options_apply() {
        let result = sample();
        let result = result.read().unwrap();
        let ndjson = render(ExportFormat::Ndjson, 0..3, 0..5, false, "postgres");
        let lines: Vec<&str> = ndjson.lines().collect();
        assert_eq!(lines.len(), 3, "{ndjson}");
        for line in &lines {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(value.is_object(), "{line}");
        }
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["id"], 1);
        assert_eq!(first["doc"]["a"][1], 2);
        // JSON columns as strings, on request.
        let metas: Vec<_> = (0..5).map(|c| result.schema().columns[c].clone()).collect();
        let writer = RowWriter::new(
            ExportFormat::Ndjson,
            false,
            CsvOptions::default(),
            &Quoted("postgres"),
            metas,
        )
        .with_json(JsonOptions {
            iso_dates: false,
            json_as_json: false,
        });
        let mut out = Vec::new();
        write_rows(&result, 0..1, &[0, 1, 2, 3, 4], writer, &mut out).unwrap();
        let first: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(first["doc"], "{\"a\": [1, 2]}");
        // ISO dates: a `T`, and `Z` for a time zone; infinity stays as the database writes it.
        let iso = JsonOptions {
            iso_dates: true,
            json_as_json: true,
        };
        let at = Value::Timestamp(1_000_000);
        assert_eq!(
            json_value_with(&at, LogicalType::Timestamp, iso),
            format!("\"{}\"", at.to_string().replacen(' ', "T", 1))
        );
        let zoned = Value::TimestampTz(1_000_000);
        let text = zoned.to_string();
        assert_eq!(
            json_value_with(&zoned, LogicalType::TimestampTz, iso),
            format!(
                "\"{}Z\"",
                text.strip_suffix("+00").unwrap().replacen(' ', "T", 1)
            )
        );
        assert_eq!(
            json_value_with(&Value::Timestamp(i64::MAX), LogicalType::Timestamp, iso),
            "\"infinity\""
        );
        assert_eq!(
            json_value_with(&at, LogicalType::Timestamp, JsonOptions::default()),
            format!("\"{at}\"")
        );
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-export-formats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn all_of_sample(format: ExportFormat) -> ExportOptions {
        ExportOptions {
            format,
            rows: 0..3,
            columns: (0..7).collect(),
            header: true,
            csv: CsvOptions::default(),
        }
    }

    #[test]
    fn gzip_files_hold_exactly_the_text() {
        use std::io::Read;
        let result = sample();
        let result = result.read().unwrap();
        let plain = temp("plain.csv");
        let packed = temp("packed.csv.gz");
        let options = all_of_sample(ExportFormat::Csv);
        let plain_summary = export_to_file(
            &result,
            &options,
            &FileOptions::default(),
            &Quoted("postgres"),
            &plain,
        )
        .unwrap();
        let summary = export_to_file(
            &result,
            &options,
            &FileOptions {
                gzip: true,
                ..FileOptions::default()
            },
            &Quoted("postgres"),
            &packed,
        )
        .unwrap();
        let bytes = std::fs::read(&packed).unwrap();
        assert_eq!(&bytes[..2], &[0x1f, 0x8b], "a gzip stream");
        // The summary says the file's size, not the text's.
        assert_eq!(summary.bytes, bytes.len() as u64);
        assert_eq!(
            plain_summary.bytes,
            std::fs::metadata(&plain).unwrap().len()
        );
        let mut text = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, std::fs::read_to_string(&plain).unwrap());
        // Nothing is left behind under a temporary name.
        assert!(!part_path(&packed).exists());
        // An upsert needs a table: a loaded result cannot be one.
        let refused = export_to_file(
            &result,
            &all_of_sample(ExportFormat::SqlInsert { table: "t".into() }),
            &FileOptions {
                sql: SqlOptions {
                    upsert: true,
                    ..SqlOptions::default()
                },
                ..FileOptions::default()
            },
            &Quoted("postgres"),
            &temp("upsert.sql"),
        );
        assert!(refused.is_err());
    }

    /// A result with a decimal column whose native type declares its precision and scale.
    fn decimal_sample(
        values: &[Option<&str>],
        native: &str,
    ) -> Arc<std::sync::RwLock<StoredResult>> {
        let schema = Arc::new(ResultSchema::new(vec![ColumnMeta {
            name: "amount".into(),
            logical: LogicalType::Decimal,
            native_type: native.into(),
            native_id: None,
            origin: None,
        }]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        store
            .append(
                id,
                RecordBatch::try_new(
                    schema.arrow.clone(),
                    vec![Arc::new(StringArray::from(values.to_vec())) as ArrayRef],
                )
                .unwrap(),
            )
            .unwrap();
        store.get(id).unwrap()
    }

    fn read_parquet(path: &Path) -> Vec<RecordBatch> {
        let file = std::fs::File::open(path).unwrap();
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn read_arrow(path: &Path) -> Vec<RecordBatch> {
        let file = std::fs::File::open(path).unwrap();
        arrow::ipc::reader::FileReader::try_new(file, None)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn parquet_and_arrow_files_read_back_with_their_types() {
        use arrow::datatypes::DataType;
        let result = sample();
        let result = result.read().unwrap();
        for (format, name) in [
            (ExportFormat::Parquet, "sample.parquet"),
            (ExportFormat::ArrowIpc, "sample.arrow"),
        ] {
            let path = temp(name);
            let summary = export_to_file(
                &result,
                &all_of_sample(format.clone()),
                &FileOptions::default(),
                &Quoted("postgres"),
                &path,
            )
            .unwrap();
            assert_eq!(summary.rows, 3);
            assert_eq!(summary.bytes, std::fs::metadata(&path).unwrap().len());
            let batches = if format == ExportFormat::Parquet {
                read_parquet(&path)
            } else {
                read_arrow(&path)
            };
            let schema = batches[0].schema();
            let types: Vec<DataType> = schema
                .fields()
                .iter()
                .map(|f| f.data_type().clone())
                .collect();
            assert_eq!(
                types,
                vec![
                    DataType::Int64,
                    DataType::Utf8,
                    DataType::Float64,
                    DataType::Boolean,
                    DataType::Utf8,
                    DataType::Binary,
                    // A decimal without a declared precision stays exact text.
                    DataType::Utf8,
                ],
                "{name}"
            );
            // The core's metadata travels with the columns.
            assert_eq!(
                schema
                    .field(0)
                    .metadata()
                    .get("spelaion.logical")
                    .map(String::as_str),
                Some("int"),
                "{name}"
            );
            let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
            assert_eq!(rows, 3);
            // Every cell reads back as the store holds it.
            for (c, field) in schema.fields().iter().enumerate() {
                let logical = result.schema().columns[c].logical;
                for row in 0..3 {
                    let back = Value::from_array(batches[0].column(c).as_ref(), row, logical);
                    assert_eq!(
                        back.map(|v| v.to_string()),
                        result.value_at(row, c).map(|v| v.to_string()),
                        "{name} {} row {row}",
                        field.name()
                    );
                }
            }
        }
        // A declared decimal is a Decimal128 of that precision and scale, exact.
        let amounts = decimal_sample(
            &[Some("12345678901234567890.1234"), None, Some("-0.5")],
            "numeric(30,4)",
        );
        let amounts = amounts.read().unwrap();
        let path = temp("amounts.parquet");
        let options = ExportOptions {
            format: ExportFormat::Parquet,
            rows: 0..3,
            columns: vec![0],
            header: true,
            csv: CsvOptions::default(),
        };
        export_to_file(
            &amounts,
            &options,
            &FileOptions::default(),
            &Quoted("postgres"),
            &path,
        )
        .unwrap();
        let batches = read_parquet(&path);
        assert_eq!(
            batches[0].schema().field(0).data_type(),
            &DataType::Decimal128(30, 4)
        );
        let decimals = batches[0]
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Decimal128Array>()
            .unwrap();
        assert_eq!(decimals.value_as_string(0), "12345678901234567890.1234");
        assert!(arrow::array::Array::is_null(decimals, 1));
        assert_eq!(decimals.value_as_string(2), "-0.5000");
        // A value the declared type cannot hold stops the export, and leaves no file.
        let nan = decimal_sample(&[Some("1.5"), Some("NaN")], "numeric(10,2)");
        let nan = nan.read().unwrap();
        let path = temp("nan.parquet");
        let error = export_to_file(
            &nan,
            &ExportOptions {
                rows: 0..2,
                ..options.clone()
            },
            &FileOptions::default(),
            &Quoted("postgres"),
            &path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("amount: NaN"), "{error}");
        assert!(!path.exists() && !part_path(&path).exists());
        // gzip compresses Parquet's column chunks: the file is still Parquet.
        let path = temp("packed.parquet");
        export_to_file(
            &result,
            &all_of_sample(ExportFormat::Parquet),
            &FileOptions {
                gzip: true,
                ..FileOptions::default()
            },
            &Quoted("postgres"),
            &path,
        )
        .unwrap();
        assert_eq!(&std::fs::read(&path).unwrap()[..4], b"PAR1");
        assert_eq!(read_parquet(&path)[0].num_rows(), 3);
    }

    #[test]
    fn extensions_follow_the_format_and_gzip() {
        assert_eq!(ExportFormat::Csv.extension(false), "csv");
        assert_eq!(ExportFormat::Csv.extension(true), "csv.gz");
        assert_eq!(ExportFormat::Ndjson.extension(true), "ndjson.gz");
        assert_eq!(ExportFormat::ArrowIpc.extension(true), "arrow.gz");
        // Parquet compresses inside the file.
        assert_eq!(ExportFormat::Parquet.extension(true), "parquet");
        assert_eq!(
            ExportFormat::SqlInsert { table: "t".into() }.extension(false),
            "sql"
        );
    }
}
