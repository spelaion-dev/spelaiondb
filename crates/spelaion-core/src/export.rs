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

use crate::results::StoredResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFormat {
    /// Tab-separated, for pasting into spreadsheets. NULL is an empty cell.
    Tsv,
    /// RFC 4180 (comma, `"` quoting, CRLF). NULL is an empty field.
    Csv,
    /// Array of objects, one per row, keyed by column name.
    Json,
    /// One `INSERT` per row into `table`, with the engine's literals and quoting.
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
    engine: &'static str,
    metas: Vec<ColumnMeta>,
    names: Vec<String>,
    insert_prefix: String,
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
        let insert_prefix = match &format {
            ExportFormat::SqlInsert { table } | ExportFormat::SqlInsertBatch { table } => format!(
                "INSERT INTO {} ({}) VALUES (",
                dialect.quote_identifier(table),
                metas
                    .iter()
                    .map(|m| dialect.quote_identifier(&m.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            _ => String::new(),
        };
        Self {
            format,
            header,
            csv,
            engine: dialect.name(),
            metas,
            names,
            insert_prefix,
            items: Vec::new(),
            seen: HashSet::new(),
            written: 0,
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
            ExportFormat::Tsv
            | ExportFormat::Csv
            | ExportFormat::SqlInsert { .. }
            | ExportFormat::SqlInsertBatch { .. }
            | ExportFormat::InList => Ok(()),
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
                out.write_all(if self.written == 0 {
                    b"\n  {"
                } else {
                    b",\n  {"
                })?;
                for (i, (name, value)) in self.names.iter().zip(values).enumerate() {
                    if i > 0 {
                        out.write_all(b", ")?;
                    }
                    write!(
                        out,
                        "{}: {}",
                        json_string(name),
                        json_value(value, self.metas[i].logical)
                    )?;
                }
                out.write_all(b"}")?;
            }
            ExportFormat::SqlInsert { .. } => {
                out.write_all(self.insert_prefix.as_bytes())?;
                out.write_all(self.literals(values).join(", ").as_bytes())?;
                out.write_all(b");\n")?;
            }
            ExportFormat::SqlInsertBatch { .. } => {
                if self.written == 0 {
                    out.write_all(self.insert_prefix.as_bytes())?;
                } else if self.engine == "sqlserver"
                    && self.written.is_multiple_of(SQL_SERVER_MAX_VALUES_ROWS)
                {
                    // SQL Server takes at most 1000 rows per VALUES: another INSERT.
                    out.write_all(b";\n")?;
                    out.write_all(self.insert_prefix.as_bytes())?;
                } else {
                    out.write_all(b",\n  (")?;
                }
                out.write_all(self.literals(values).join(", ").as_bytes())?;
                out.write_all(b")")?;
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
            ExportFormat::SqlInsertBatch { .. } if self.written > 0 => out.write_all(b";\n")?,
            ExportFormat::Tsv
            | ExportFormat::Csv
            | ExportFormat::SqlInsert { .. }
            | ExportFormat::SqlInsertBatch { .. }
            | ExportFormat::Markdown => {}
        }
        Ok(self.written)
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
    let mut writer = RowWriter::new(
        options.format.clone(),
        options.header,
        options.csv.clone(),
        dialect,
        metas,
    );
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
    encoding: TextEncoding,
    dialect: &dyn Dialect,
    path: &Path,
) -> io::Result<FileSummary> {
    options
        .csv
        .check()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut file = PartFile::create(path, encoding)?;
    match write_result(result, options, dialect, file.writer()) {
        Ok(rows) => file.commit(rows as u64),
        Err(e) => {
            file.discard();
            Err(e)
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
    out: Encoder<io::BufWriter<AtomicWriter>>,
}

impl PartFile {
    pub(crate) fn create(path: &Path, encoding: TextEncoding) -> io::Result<Self> {
        let file = AtomicWriter::create_with_suffix(path, ".part", NewFileMode::Default)?;
        let mut part = Self {
            out: Encoder::new(io::BufWriter::with_capacity(1 << 20, file), encoding),
        };
        // On an error `part` is dropped, which deletes the temporary file.
        part.writer().write_bom()?;
        Ok(part)
    }

    pub(crate) fn writer(&mut self) -> &mut Encoder<io::BufWriter<AtomicWriter>> {
        &mut self.out
    }

    /// Flushes, renames to the final name and says what was written.
    pub(crate) fn commit(self, rows: u64) -> io::Result<FileSummary> {
        let mut out = self.out;
        out.flush()?;
        let summary = FileSummary {
            rows,
            bytes: out.bytes(),
            replaced: out.replaced(),
            peak_rows: 0,
        };
        out.into_inner()
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

/// Writes UTF-8 text in another encoding, counting the bytes written and the characters it
/// could not encode.
pub(crate) struct Encoder<W: Write> {
    inner: W,
    encoding: TextEncoding,
    /// The start of a character split between two writes.
    pending: Vec<u8>,
    bytes: u64,
    replaced: u64,
    /// Shared with whoever reports progress.
    progress: Option<Arc<AtomicU64>>,
}

impl<W: Write> Encoder<W> {
    pub(crate) fn new(inner: W, encoding: TextEncoding) -> Self {
        Self {
            inner,
            encoding,
            pending: Vec::new(),
            bytes: 0,
            replaced: 0,
            progress: None,
        }
    }

    /// Bytes written from now on are also stored in `counter`.
    pub(crate) fn report_to(&mut self, counter: Arc<AtomicU64>) {
        counter.store(self.bytes, Ordering::Relaxed);
        self.progress = Some(counter);
    }

    fn write_bom(&mut self) -> io::Result<()> {
        match self.encoding {
            TextEncoding::Utf8Bom => self.emit(&[0xef, 0xbb, 0xbf]),
            TextEncoding::Utf16Le => self.emit(&[0xff, 0xfe]),
            TextEncoding::Utf8 | TextEncoding::Windows1252 => Ok(()),
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn replaced(&self) -> u64 {
        self.replaced
    }

    pub(crate) fn into_inner(self) -> W {
        self.inner
    }

    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_all(bytes)?;
        self.bytes += bytes.len() as u64;
        if let Some(counter) = &self.progress {
            counter.store(self.bytes, Ordering::Relaxed);
        }
        Ok(())
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

fn text(value: &Value) -> Option<String> {
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

/// Numbers and booleans as JSON values; JSON columns embedded as JSON; exact decimals and
/// everything else as strings (a JSON number could lose a decimal's precision).
fn json_value(value: &Value, logical: LogicalType) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => value.to_string(),
        other => {
            let text = other.to_string();
            let is_json = logical == LogicalType::Json
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
        other => quoted(&other.to_string()),
    }
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
            TextEncoding::Utf8Bom,
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
            TextEncoding::Utf8,
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
                TextEncoding::Utf8Bom,
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
                TextEncoding::Utf8,
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
        let mut first = PartFile::create(&path, TextEncoding::Utf8).unwrap();
        let mut second = PartFile::create(&path, TextEncoding::Utf8).unwrap();
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
        let mut cancelled = PartFile::create(&path, TextEncoding::Utf8).unwrap();
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
            let bytes = encoder.bytes();
            let out = encoder.into_inner();
            assert_eq!(bytes, out.len() as u64);
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
            TextEncoding::Utf16Le,
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
            TextEncoding::Utf8,
            &Quoted("postgres"),
            &path,
        );
        assert!(refused.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
