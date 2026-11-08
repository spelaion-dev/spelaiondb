//! Reading what is imported (ADR-038): a file or the clipboard's text, as CSV, TSV, a JSON array
//! or a Markdown table. The input is untrusted: every limit here (the record size, the columns,
//! the size of what is read whole) stops a file that would otherwise take all the memory, and
//! nothing read from it ever becomes SQL text (names are quoted by the engine's dialect, values
//! are bound).

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read};
use std::path::{Path, PathBuf};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

use super::ImportError;
use crate::sql_file::{decode_utf16, windows_1252_char};

/// How the source is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Csv,
    Tsv,
    /// An array of objects (their keys are the columns) or of arrays.
    Json,
    /// A pipe table: a header row, a `|---|` line, then the rows.
    Markdown,
}

/// The text encoding of a file. The clipboard is always text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceEncoding {
    /// A byte order mark says it; otherwise UTF-8 when the start of the file is valid UTF-8,
    /// else Windows-1252.
    Auto,
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

/// Where the rows come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSource {
    File(PathBuf),
    /// Pasted or clipboard text.
    Text(String),
}

/// How the source is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseOptions {
    pub format: ImportFormat,
    pub encoding: SourceEncoding,
    /// The CSV separator; TSV always uses a tab.
    pub delimiter: char,
    /// The character that quotes a value (CSV and TSV); `None`: values are never quoted.
    pub quote: Option<char>,
    /// The character that escapes the next one; `None`: a quote inside a quoted value is
    /// written twice.
    pub escape: Option<char>,
    /// The first row names the columns (CSV, TSV and JSON arrays of arrays).
    pub header: bool,
    /// A value written exactly like this (after trimming) is NULL, `\N` or `NULL` for example.
    pub null_text: Option<String>,
    /// An empty value is NULL.
    pub empty_as_null: bool,
    /// Spaces around values are removed.
    pub trim: bool,
    /// How dates are written, in `strftime` notation (`%d/%m/%Y`); `None`: ISO (`2024-03-31`).
    pub date_format: Option<String>,
    /// How timestamps are written (`%d/%m/%Y %H:%M`); `None`: ISO.
    pub timestamp_format: Option<String>,
    /// The time zone of timestamps that have none, for columns that keep one: `UTC` or an
    /// offset such as `+02:00`.
    pub time_zone: Option<String>,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            format: ImportFormat::Csv,
            encoding: SourceEncoding::Auto,
            delimiter: ',',
            quote: Some('"'),
            escape: None,
            header: true,
            null_text: None,
            empty_as_null: true,
            trim: false,
            date_format: None,
            timestamp_format: None,
            time_zone: None,
        }
    }
}

/// Column names and every row, for the formats that are read whole.
type Table = (Vec<String>, Vec<Vec<Option<String>>>);

/// The most columns a source may have.
pub const MAX_COLUMNS: usize = 2_048;
/// The most a single record (all its values, a quoted value spanning lines included) may take:
/// a quote that never closes stops here instead of reading the whole file into one value.
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
/// JSON, Markdown and files that are not UTF-8 are read whole: up to this size.
pub const MAX_WHOLE_FILE: u64 = 512 * 1024 * 1024;
/// How much of a file is looked at to guess its encoding and separator.
const SNIFF_BYTES: usize = 64 * 1024;

/// The format a file's extension says, if it says one.
pub fn format_for_path(path: &Path) -> Option<ImportFormat> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "csv" => Some(ImportFormat::Csv),
        "tsv" | "tab" => Some(ImportFormat::Tsv),
        "json" => Some(ImportFormat::Json),
        "md" | "markdown" => Some(ImportFormat::Markdown),
        _ => None,
    }
}

/// The options to read `source` with when nothing was chosen yet: its format from the file's
/// extension or the text itself, and the CSV separator from its first line.
pub fn guess_options(source: &ImportSource) -> Result<ParseOptions, ImportError> {
    let sample = match source {
        ImportSource::Text(text) => text.chars().take(SNIFF_BYTES).collect::<String>(),
        ImportSource::File(path) => {
            let mut head = vec![0; SNIFF_BYTES];
            let mut file = File::open(path).map_err(|e| io_error(path, &e))?;
            let read = read_up_to(&mut file, &mut head).map_err(|e| io_error(path, &e))?;
            head.truncate(read);
            let (encoding, skip) = sniff_encoding(&head);
            match encoding {
                SourceEncoding::Utf16Le => {
                    decode_utf16(even(&head[skip..]), u16::from_le_bytes).unwrap_or_default()
                }
                SourceEncoding::Utf16Be => {
                    decode_utf16(even(&head[skip..]), u16::from_be_bytes).unwrap_or_default()
                }
                SourceEncoding::Windows1252 => head.iter().map(|b| windows_1252_char(*b)).collect(),
                SourceEncoding::Auto | SourceEncoding::Utf8 => {
                    String::from_utf8_lossy(&head[skip..]).into_owned()
                }
            }
        }
    };
    let format = match source {
        ImportSource::File(path) => format_for_path(path).unwrap_or_else(|| text_format(&sample)),
        ImportSource::Text(_) => text_format(&sample),
    };
    let delimiter = match format {
        ImportFormat::Csv => csv_delimiter(&sample),
        ImportFormat::Tsv => '\t',
        ImportFormat::Json | ImportFormat::Markdown => ',',
    };
    Ok(ParseOptions {
        format,
        delimiter,
        ..ParseOptions::default()
    })
}

/// What pasted text looks like: a Markdown table, a JSON array, tab-separated values (what a
/// spreadsheet copies) or CSV.
pub fn text_format(text: &str) -> ImportFormat {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('[') {
        return ImportFormat::Json;
    }
    let mut lines = trimmed.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("");
    if first.starts_with('|') && lines.next().is_some_and(is_markdown_separator) {
        return ImportFormat::Markdown;
    }
    if first.contains('\t') {
        ImportFormat::Tsv
    } else {
        ImportFormat::Csv
    }
}

/// `;` when the first line has semicolons and no commas outside quotes (CSV as much of Europe
/// writes it), `|` likewise, otherwise `,`.
fn csv_delimiter(sample: &str) -> char {
    let first = sample
        .trim_start_matches('\u{feff}')
        .lines()
        .next()
        .unwrap_or("");
    let mut counts = [0usize; 3];
    let mut quoted = false;
    for c in first.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => counts[0] += 1,
            ';' if !quoted => counts[1] += 1,
            '|' if !quoted => counts[2] += 1,
            _ => {}
        }
    }
    if counts[0] == 0 && counts[1] > 0 {
        ';'
    } else if counts[0] == 0 && counts[2] > 0 {
        '|'
    } else {
        ','
    }
}

fn is_markdown_separator(line: &str) -> bool {
    let line = line.trim();
    line.contains('-')
        && line.contains('|')
        && line
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// The encoding a byte order mark says, and its length; `Auto` when there is none.
fn sniff_encoding(head: &[u8]) -> (SourceEncoding, usize) {
    if head.starts_with(b"\xEF\xBB\xBF") {
        (SourceEncoding::Utf8, 3)
    } else if head.starts_with(b"\xFF\xFE") {
        (SourceEncoding::Utf16Le, 2)
    } else if head.starts_with(b"\xFE\xFF") {
        (SourceEncoding::Utf16Be, 2)
    } else {
        (SourceEncoding::Auto, 0)
    }
}

/// Whether `head` is UTF-8, a character cut at its end allowed.
fn looks_utf8(head: &[u8], complete: bool) -> bool {
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => !complete && e.error_len().is_none(),
    }
}

fn even(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.len() - bytes.len() % 2]
}

fn read_up_to(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

fn io_error(path: &Path, e: &io::Error) -> ImportError {
    ImportError::Io {
        path: path.display().to_string(),
        message: e.to_string(),
    }
}

/// The source as text a line at a time, and the encoding it was read in.
fn open_text(
    source: &ImportSource,
    encoding: SourceEncoding,
    whole: bool,
) -> Result<(Box<dyn BufRead + Send>, SourceEncoding), ImportError> {
    let path = match source {
        ImportSource::Text(text) => {
            let text = text.strip_prefix('\u{feff}').unwrap_or(text).to_string();
            return Ok((
                Box::new(Cursor::new(text.into_bytes())),
                SourceEncoding::Utf8,
            ));
        }
        ImportSource::File(path) => path,
    };
    let mut file = File::open(path).map_err(|e| io_error(path, &e))?;
    let size = file.metadata().map_err(|e| io_error(path, &e))?.len();
    let mut head = vec![0; SNIFF_BYTES];
    let read = read_up_to(&mut file, &mut head).map_err(|e| io_error(path, &e))?;
    head.truncate(read);
    let (bom, skip) = sniff_encoding(&head);
    let encoding = match encoding {
        SourceEncoding::Auto => match bom {
            SourceEncoding::Auto if looks_utf8(&head, read < SNIFF_BYTES) => SourceEncoding::Utf8,
            SourceEncoding::Auto => SourceEncoding::Windows1252,
            found => found,
        },
        chosen => chosen,
    };
    // A byte order mark is only skipped when it is the chosen encoding's.
    let skip = if bom == encoding { skip } else { 0 };
    if encoding == SourceEncoding::Utf8 && !whole {
        let rest = Cursor::new(head[skip..].to_vec()).chain(file);
        return Ok((
            Box::new(BufReader::with_capacity(256 * 1024, rest)),
            encoding,
        ));
    }
    if size > MAX_WHOLE_FILE {
        return Err(ImportError::Source(format!(
            "{} is too big to read whole ({} MiB at most for JSON, Markdown and text that is not \
             UTF-8): save it as UTF-8 CSV",
            path.display(),
            MAX_WHOLE_FILE / (1024 * 1024)
        )));
    }
    let mut bytes = head;
    file.read_to_end(&mut bytes)
        .map_err(|e| io_error(path, &e))?;
    let bytes = &bytes[skip..];
    let text = match encoding {
        SourceEncoding::Utf8 | SourceEncoding::Auto => {
            String::from_utf8(bytes.to_vec()).map_err(|_| ImportError::Source(not_utf8(path)))?
        }
        SourceEncoding::Utf16Le => decode_utf16(bytes, u16::from_le_bytes)
            .map_err(|_| ImportError::Source(format!("{} is not valid UTF-16", path.display())))?,
        SourceEncoding::Utf16Be => decode_utf16(bytes, u16::from_be_bytes)
            .map_err(|_| ImportError::Source(format!("{} is not valid UTF-16", path.display())))?,
        SourceEncoding::Windows1252 => bytes.iter().map(|b| windows_1252_char(*b)).collect(),
    };
    Ok((Box::new(Cursor::new(text.into_bytes())), encoding))
}

fn not_utf8(path: &Path) -> String {
    format!(
        "{} is not valid UTF-8: choose its encoding in the options",
        path.display()
    )
}

/// Rows of the source, one at a time, as text or NULL (after the options' trimming and NULL
/// rules), each as wide as the columns.
pub(crate) struct RowReader {
    pub columns: Vec<String>,
    pub encoding: SourceEncoding,
    inner: Inner,
    cells: CellRules,
    /// Data rows returned so far.
    pub rows: u64,
}

enum Inner {
    Delimited {
        parser: Delimited,
        /// The first record, when it is data (no header).
        pending: Option<Vec<String>>,
    },
    Whole(std::vec::IntoIter<Vec<Option<String>>>),
}

struct CellRules {
    trim: bool,
    null_text: Option<String>,
    empty_as_null: bool,
}

impl CellRules {
    fn apply(&self, raw: String) -> Option<String> {
        let value = if self.trim {
            let trimmed = raw.trim();
            if trimmed.len() == raw.len() {
                raw
            } else {
                trimmed.to_string()
            }
        } else {
            raw
        };
        if self.null_text.as_deref() == Some(value.as_str())
            || (self.empty_as_null && value.is_empty())
        {
            None
        } else {
            Some(value)
        }
    }
}

impl RowReader {
    pub fn open(source: &ImportSource, options: &ParseOptions) -> Result<Self, ImportError> {
        let cells = CellRules {
            trim: options.trim,
            null_text: options
                .null_text
                .as_ref()
                .filter(|t| !t.is_empty())
                .map(|t| {
                    if options.trim {
                        t.trim().to_string()
                    } else {
                        t.clone()
                    }
                }),
            empty_as_null: options.empty_as_null,
        };
        let whole = matches!(options.format, ImportFormat::Json | ImportFormat::Markdown);
        let (mut input, encoding) = open_text(source, options.encoding, whole)?;
        let (columns, inner) = match options.format {
            ImportFormat::Csv | ImportFormat::Tsv => {
                let delimiter = if options.format == ImportFormat::Tsv {
                    '\t'
                } else {
                    options.delimiter
                };
                check_delimited(delimiter, options.quote, options.escape)?;
                let mut parser = Delimited {
                    input,
                    delimiter,
                    quote: options.quote,
                    escape: options.escape,
                    line: 0,
                    buf: String::new(),
                    path: source_name(source),
                };
                let first = parser.next_record()?.unwrap_or_default();
                if first.len() > MAX_COLUMNS {
                    return Err(too_many_columns(first.len()));
                }
                if options.header {
                    (
                        column_names(first.into_iter().map(Some)),
                        Inner::Delimited {
                            parser,
                            pending: None,
                        },
                    )
                } else {
                    (
                        column_names((0..first.len()).map(|_| None)),
                        Inner::Delimited {
                            parser,
                            pending: (!first.is_empty()).then_some(first),
                        },
                    )
                }
            }
            ImportFormat::Json => {
                let mut text = String::new();
                input
                    .read_to_string(&mut text)
                    .map_err(|e| ImportError::Source(e.to_string()))?;
                let (columns, rows) = json_rows(&text, options.header, &cells)?;
                (columns, Inner::Whole(rows.into_iter()))
            }
            ImportFormat::Markdown => {
                let mut text = String::new();
                input
                    .read_to_string(&mut text)
                    .map_err(|e| ImportError::Source(e.to_string()))?;
                let (columns, rows) = markdown_rows(&text, &cells)?;
                (columns, Inner::Whole(rows.into_iter()))
            }
        };
        Ok(Self {
            columns,
            encoding,
            inner,
            cells,
            rows: 0,
        })
    }

    /// The next row, as wide as [`RowReader::columns`]: short rows end in NULLs; a row with
    /// more values than columns is an error, unless the extra ones are empty (a trailing
    /// separator).
    pub fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, ImportError> {
        let width = self.columns.len();
        let row = match &mut self.inner {
            Inner::Whole(rows) => rows.next(),
            Inner::Delimited { parser, pending } => {
                let record = match pending.take() {
                    Some(record) => Some(record),
                    None => parser.next_record()?,
                };
                match record {
                    None => None,
                    Some(mut record) => {
                        if record.len() > width {
                            if record[width..].iter().all(|v| v.trim().is_empty()) {
                                record.truncate(width);
                            } else {
                                return Err(ImportError::Source(format!(
                                    "line {}: {} values, but there are {width} columns",
                                    parser.line,
                                    record.len()
                                )));
                            }
                        }
                        Some(record.into_iter().map(|v| self.cells.apply(v)).collect())
                    }
                }
            }
        };
        Ok(row.map(|mut row: Vec<Option<String>>| {
            row.resize(width, None);
            self.rows += 1;
            row
        }))
    }
}

fn source_name(source: &ImportSource) -> String {
    match source {
        ImportSource::File(path) => path.display().to_string(),
        ImportSource::Text(_) => "the pasted text".into(),
    }
}

fn too_many_columns(found: usize) -> ImportError {
    ImportError::Source(format!(
        "{found} columns: at most {MAX_COLUMNS} can be imported"
    ))
}

fn check_delimited(
    delimiter: char,
    quote: Option<char>,
    escape: Option<char>,
) -> Result<(), ImportError> {
    let bad = |c: char| c == '\n' || c == '\r';
    if bad(delimiter) || quote.is_some_and(bad) || escape.is_some_and(bad) {
        return Err(ImportError::Invalid(
            "the separator, quote and escape cannot be line breaks".into(),
        ));
    }
    if quote == Some(delimiter) || escape == Some(delimiter) {
        return Err(ImportError::Invalid(
            "the quote and the escape must differ from the separator".into(),
        ));
    }
    Ok(())
}

/// Column names from a header (or none): blanks become `columnN`, control characters go, a
/// name keeps at most 63 bytes (PostgreSQL's limit), and repeated names get `_2`, `_3`…
fn column_names(header: impl Iterator<Item = Option<String>>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (i, name) in header.enumerate() {
        let cleaned: String = name
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .to_string();
        let base = if cleaned.is_empty() {
            format!("column{}", i + 1)
        } else {
            truncate_bytes(&cleaned, 63)
        };
        let mut name = base.clone();
        let mut n = 2;
        while names.iter().any(|other| other.eq_ignore_ascii_case(&name)) {
            let suffix = format!("_{n}");
            name = format!("{}{suffix}", truncate_bytes(&base, 63 - suffix.len()));
            n += 1;
        }
        names.push(name);
    }
    names
}

fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// A CSV/TSV reader that streams records: quoted values may span lines, a quote inside one is
/// doubled (or escaped with the escape character), blank lines are skipped.
struct Delimited {
    input: Box<dyn BufRead + Send>,
    delimiter: char,
    quote: Option<char>,
    escape: Option<char>,
    /// The line the last record ended on.
    line: u64,
    buf: String,
    path: String,
}

impl Delimited {
    /// Reads the next line, or at most `limit` bytes of it.
    fn read_line(&mut self, limit: usize) -> Result<bool, ImportError> {
        self.buf.clear();
        let limit = u64::try_from(limit).unwrap_or(u64::MAX);
        match (&mut self.input).take(limit).read_line(&mut self.buf) {
            Ok(0) => Ok(false),
            Ok(_) => {
                self.line += 1;
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(ImportError::Source(format!(
                "{} is not valid UTF-8 at line {}: choose its encoding in the options",
                self.path,
                self.line + 1
            ))),
            Err(e) => Err(ImportError::Source(format!("{}: {e}", self.path))),
        }
    }

    fn next_record(&mut self) -> Result<Option<Vec<String>>, ImportError> {
        // Blank lines between records are skipped.
        loop {
            if !self.read_line(MAX_RECORD_BYTES + 1)? {
                return Ok(None);
            }
            if !self.buf.trim_end_matches(['\r', '\n']).is_empty() {
                break;
            }
        }
        let start_line = self.line;
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut quoted = false;
        let mut size = 0usize;
        loop {
            let line = std::mem::take(&mut self.buf);
            size += line.len();
            if size > MAX_RECORD_BYTES {
                return Err(ImportError::Source(format!(
                    "line {start_line}: a record longer than {} MiB (a quote that never closes?)",
                    MAX_RECORD_BYTES / (1024 * 1024)
                )));
            }
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if Some(c) == self.escape {
                    match chars.next() {
                        Some(next) => field.push(next),
                        None => field.push(c),
                    }
                } else if quoted {
                    if Some(c) == self.quote {
                        if self.escape.is_none() && chars.peek() == Some(&c) {
                            chars.next();
                            field.push(c);
                        } else {
                            quoted = false;
                        }
                    } else {
                        field.push(c);
                    }
                } else if c == self.delimiter {
                    fields.push(std::mem::take(&mut field));
                    if fields.len() > MAX_COLUMNS {
                        return Err(too_many_columns(fields.len()));
                    }
                } else if Some(c) == self.quote && field.chars().all(|f| f == ' ') {
                    // Spaces before an opening quote (`a, "b"`) are not part of the value.
                    field.clear();
                    quoted = true;
                } else if c == '\n' || (c == '\r' && matches!(chars.peek(), None | Some('\n'))) {
                    // The end of the record.
                } else {
                    field.push(c);
                }
            }
            if !quoted {
                break;
            }
            // A quoted value goes on in the next line.
            if !self.read_line(MAX_RECORD_BYTES + 1 - size)? {
                return Err(ImportError::Source(format!(
                    "line {start_line}: a quoted value never ends"
                )));
            }
        }
        fields.push(field);
        Ok(Some(fields))
    }
}

/// A JSON array of objects (columns in the order their keys first appear) or of arrays (the
/// first one names the columns when `header`).
fn json_rows(text: &str, header: bool, cells: &CellRules) -> Result<Table, ImportError> {
    let items: Vec<JsonRow> = serde_json::from_str::<JsonRows>(text.trim_start_matches('\u{feff}'))
        .map_err(|e| ImportError::Source(format!("not a JSON array of rows: {e}")))?
        .0;
    let value = |v: serde_json::Value| -> Option<String> {
        match v {
            serde_json::Value::Null => None,
            serde_json::Value::String(s) => cells.apply(s),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            nested @ (serde_json::Value::Array(_) | serde_json::Value::Object(_)) => {
                Some(nested.to_string())
            }
        }
    };
    let objects = items.iter().any(|i| matches!(i, JsonRow::Object(_)));
    if objects {
        let mut columns: Vec<String> = Vec::new();
        for item in &items {
            match item {
                JsonRow::Object(fields) => {
                    for (key, _) in fields {
                        if !columns.contains(key) {
                            columns.push(key.clone());
                            if columns.len() > MAX_COLUMNS {
                                return Err(too_many_columns(columns.len()));
                            }
                        }
                    }
                }
                JsonRow::Array(_) => {
                    return Err(ImportError::Source(
                        "the JSON array mixes objects and arrays".into(),
                    ));
                }
            }
        }
        let rows = items
            .into_iter()
            .map(|item| {
                let JsonRow::Object(fields) = item else {
                    unreachable!("checked above")
                };
                let mut row = vec![None; columns.len()];
                for (key, v) in fields {
                    if let Some(i) = columns.iter().position(|c| *c == key) {
                        row[i] = value(v);
                    }
                }
                row
            })
            .collect();
        let names = column_names(columns.into_iter().map(Some));
        return Ok((names, rows));
    }
    let mut arrays = items.into_iter().map(|item| match item {
        JsonRow::Array(values) => values,
        JsonRow::Object(_) => unreachable!("no objects here"),
    });
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let names: Vec<Option<String>> = if header {
        arrays
            .next()
            .unwrap_or_default()
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Null => None,
                other => Some(other.to_string()),
            })
            .collect()
    } else {
        Vec::new()
    };
    for values in arrays {
        rows.push(values.into_iter().map(value).collect());
    }
    let width = rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .max(names.len());
    if width > MAX_COLUMNS {
        return Err(too_many_columns(width));
    }
    let mut names = names;
    names.resize(width, None);
    Ok((column_names(names.into_iter()), rows))
}

/// The top-level array, its objects with their keys in the file's order.
struct JsonRows(Vec<JsonRow>);

enum JsonRow {
    Object(Vec<(String, serde_json::Value)>),
    Array(Vec<serde_json::Value>),
}

impl<'de> Deserialize<'de> for JsonRows {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Rows;
        impl<'de> Visitor<'de> for Rows {
            type Value = JsonRows;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an array of objects or arrays")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JsonRows, A::Error> {
                let mut rows = Vec::new();
                while let Some(row) = seq.next_element::<JsonRow>()? {
                    rows.push(row);
                }
                Ok(JsonRows(rows))
            }
        }
        deserializer.deserialize_seq(Rows)
    }
}

impl<'de> Deserialize<'de> for JsonRow {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Row;
        impl<'de> Visitor<'de> for Row {
            type Value = JsonRow;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object or an array")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JsonRow, A::Error> {
                let mut fields = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    fields.push((key, value));
                }
                Ok(JsonRow::Object(fields))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<JsonRow, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<serde_json::Value>()? {
                    values.push(value);
                }
                Ok(JsonRow::Array(values))
            }
            fn visit_unit<E: de::Error>(self) -> Result<JsonRow, E> {
                Err(E::custom("a row is null"))
            }
        }
        deserializer.deserialize_any(Row)
    }
}

/// A Markdown pipe table: the first row names the columns, the `|---|` line after it is
/// skipped, and `\|` is a pipe inside a value. The table ends at the first line that is not a
/// row.
fn markdown_rows(text: &str, cells: &CellRules) -> Result<Table, ImportError> {
    let mut lines = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .skip_while(|l| l.is_empty());
    let header = lines
        .next()
        .filter(|l| l.contains('|'))
        .ok_or_else(|| ImportError::Source("not a Markdown table".into()))?;
    if !lines.next().is_some_and(is_markdown_separator) {
        return Err(ImportError::Source(
            "not a Markdown table: the second line must be its |---| line".into(),
        ));
    }
    let names = markdown_cells(header);
    if names.len() > MAX_COLUMNS {
        return Err(too_many_columns(names.len()));
    }
    let rows: Vec<Vec<Option<String>>> = lines
        .take_while(|l| l.contains('|'))
        .map(|line| {
            markdown_cells(line)
                .into_iter()
                .take(names.len())
                .map(|v| cells.apply(v))
                .collect()
        })
        .collect();
    Ok((column_names(names.into_iter().map(Some)), rows))
}

fn markdown_cells(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = if line.ends_with('|') && !line.ends_with("\\|") {
        &line[..line.len() - 1]
    } else {
        line
    };
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cell.push('|');
            }
            '|' => cells.push(std::mem::take(&mut cell).trim().to_string()),
            other => cell.push(other),
        }
    }
    cells.push(cell.trim().to_string());
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, options: &ParseOptions) -> Result<Table, ImportError> {
        let mut reader = RowReader::open(&ImportSource::Text(text.into()), options)?;
        let mut rows = Vec::new();
        while let Some(row) = reader.next_row()? {
            rows.push(row);
        }
        Ok((reader.columns, rows))
    }

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn csv_quotes_doubled_quotes_and_line_breaks_inside_values() {
        let text =
            "id,name,note\r\n1,\"O'Brien, \"\"Bob\"\"\",\"two\nlines\"\r\n2,plain,\r\n\r\n3,,x\n";
        let (columns, rows) = read(text, &ParseOptions::default()).unwrap();
        assert_eq!(columns, ["id", "name", "note"]);
        assert_eq!(
            rows,
            vec![
                vec![s("1"), s("O'Brien, \"Bob\""), s("two\nlines")],
                vec![s("2"), s("plain"), None],
                vec![s("3"), None, s("x")],
            ]
        );
    }

    #[test]
    fn escape_character_null_text_trim_and_empty_strings() {
        let options = ParseOptions {
            delimiter: ';',
            escape: Some('\\'),
            null_text: Some("\\N".into()),
            empty_as_null: false,
            trim: true,
            header: false,
            ..ParseOptions::default()
        };
        // `\\N` is an escaped backslash then N: the text `\N`, read as NULL.
        let (columns, rows) = read(" a ; \"x\\\"y\" ;\\\\N;\n;b;c;\n", &options).unwrap();
        assert_eq!(columns, ["column1", "column2", "column3", "column4"]);
        assert_eq!(
            rows,
            vec![
                vec![s("a"), s("x\"y"), None, s("")],
                vec![s(""), s("b"), s("c"), s("")],
            ]
        );
    }

    #[test]
    fn rows_too_long_and_quotes_that_never_close_are_errors() {
        let options = ParseOptions::default();
        assert!(read("a,b\n1,2\n", &options).is_ok());
        // A trailing separator is not an extra value.
        assert_eq!(
            read("a,b\n1,2,\n", &options).unwrap().1,
            vec![vec![s("1"), s("2")]]
        );
        let err = read("a,b\n1,2,3\n", &options).unwrap_err().to_string();
        assert!(err.contains("line 2") && err.contains("3 values"), "{err}");
        let err = read("a,b\n1,\"open\n2,3\n", &options)
            .unwrap_err()
            .to_string();
        assert!(err.contains("never ends"), "{err}");
        // Short rows end in NULLs.
        assert_eq!(
            read("a,b,c\n1\n", &options).unwrap().1,
            vec![vec![s("1"), None, None]]
        );
    }

    #[test]
    fn header_names_are_cleaned_and_made_unique() {
        let (columns, _) =
            read("id,,Name,name,\"a\u{7}b\",ID\n", &ParseOptions::default()).unwrap();
        assert_eq!(columns, ["id", "column2", "Name", "name_2", "ab", "ID_2"]);
        let long = "x".repeat(100);
        let (columns, _) = read(&format!("{long},{long}\n"), &ParseOptions::default()).unwrap();
        assert_eq!(columns[0].len(), 63);
        assert!(columns[1].ends_with("_2") && columns[1].len() == 63);
    }

    #[test]
    fn json_objects_keep_their_key_order_and_nested_values_become_json() {
        let text = r#"[{"b": 1, "a": "x", "n": null}, {"a": "y", "c": {"k": [1, 2]}, "b": true}]"#;
        let options = ParseOptions {
            format: ImportFormat::Json,
            ..ParseOptions::default()
        };
        let (columns, rows) = read(text, &options).unwrap();
        assert_eq!(columns, ["b", "a", "n", "c"]);
        assert_eq!(
            rows,
            vec![
                vec![s("1"), s("x"), None, None],
                vec![s("true"), s("y"), None, s(r#"{"k":[1,2]}"#)],
            ]
        );
        let (columns, rows) = read(r#"[["id","v"],[1,"a"],[2]]"#, &options).unwrap();
        assert_eq!(columns, ["id", "v"]);
        assert_eq!(rows, vec![vec![s("1"), s("a")], vec![s("2"), None]]);
        assert!(read(r#"{"a": 1}"#, &options).is_err());
        assert!(read(r#"[1, 2]"#, &options).is_err());
    }

    #[test]
    fn markdown_tables_with_escaped_pipes() {
        let text = "\n| id | name |\n|---:|:-----|\n| 1 | a \\| b |\n| 2 |  |\nafter the table\n";
        let options = ParseOptions {
            format: ImportFormat::Markdown,
            ..ParseOptions::default()
        };
        let (columns, rows) = read(text, &options).unwrap();
        assert_eq!(columns, ["id", "name"]);
        assert_eq!(rows, vec![vec![s("1"), s("a | b")], vec![s("2"), None]]);
        assert!(read("| a |\n| b |\n", &options).is_err());
    }

    #[test]
    fn pasted_text_formats_and_separators_are_guessed() {
        assert_eq!(text_format("a\tb\n1\t2"), ImportFormat::Tsv);
        assert_eq!(
            text_format("| a | b |\n|---|---|\n| 1 | 2 |"),
            ImportFormat::Markdown
        );
        assert_eq!(text_format("  [{\"a\": 1}]"), ImportFormat::Json);
        assert_eq!(text_format("a,b\n1,2"), ImportFormat::Csv);
        assert_eq!(csv_delimiter("a;b;\"c,d\"\n"), ';');
        assert_eq!(csv_delimiter("a,b;c\n"), ',');
        let guessed = guess_options(&ImportSource::Text("x;y\n1;2".into())).unwrap();
        assert_eq!(
            (guessed.format, guessed.delimiter),
            (ImportFormat::Csv, ';')
        );
    }

    #[test]
    fn files_in_other_encodings_are_decoded() {
        let dir = std::env::temp_dir().join(format!("spelaion-import-enc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("name\nJosé\n".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        let latin = b"name\nJos\xe9\n".to_vec();
        let bom = b"\xEF\xBB\xBFname\nJos\xc3\xa9\n".to_vec();
        for (file, bytes, encoding) in [
            ("utf16.csv", utf16, SourceEncoding::Utf16Le),
            ("latin.csv", latin, SourceEncoding::Windows1252),
            ("bom.csv", bom, SourceEncoding::Utf8),
        ] {
            let path = dir.join(file);
            std::fs::write(&path, bytes).unwrap();
            let mut reader =
                RowReader::open(&ImportSource::File(path), &ParseOptions::default()).unwrap();
            assert_eq!(reader.encoding, encoding, "{file}");
            assert_eq!(reader.columns, ["name"], "{file}");
            assert_eq!(reader.next_row().unwrap(), Some(vec![s("José")]), "{file}");
        }
        // UTF-8 chosen for a file that is not: an error that says so, not garbage.
        let path = dir.join("latin.csv");
        let options = ParseOptions {
            encoding: SourceEncoding::Utf8,
            ..ParseOptions::default()
        };
        let mut reader = RowReader::open(&ImportSource::File(path), &options).unwrap();
        let err = reader.next_row().unwrap_err().to_string();
        assert!(err.contains("not valid UTF-8"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_huge_record_stops_instead_of_filling_memory() {
        // One endless line, and a quote that never closes over many lines.
        let line = format!("a\n{}", "x".repeat(MAX_RECORD_BYTES + 10));
        let err = read(&line, &ParseOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("longer than"), "{err}");
        let lines = format!("a\n\"{}", "xxxxxxx\n".repeat(MAX_RECORD_BYTES / 8 + 10));
        let err = read(&lines, &ParseOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("longer than"), "{err}");
    }
}
