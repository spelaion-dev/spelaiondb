//! XLSX workbooks (SDB-390): a sheet per result or table, its first row the column names, and
//! numbers, booleans, dates, times and timestamps as real cells (dates with a date format).
//! The workbook streams like every file export (ADR-034): a sheet is a zip entry written as its
//! rows arrive and every batch is dropped once written, so the export holds one batch whatever
//! the number of rows; the file goes through the same [`PartFile`] (`<name>.part`, renamed only
//! when complete). Past Excel's 1,048,576 rows a sheet goes on in a new one (`orders (2)`).
//!
//! What a cell cannot hold exactly stays text, never rounded: an integer past 2^53, a decimal
//! a double does not hold digit for digit, a date before 1900-03-01 (Excel's calendar is wrong
//! before it) or after 9999. A text longer than an Excel cell holds (32,767 characters) stops
//! the export with the column's name, as Parquet does with a decimal it cannot hold.
//! Strings are inline (`inlineStr`): no shared-strings table grows with the rows.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use arrow::array::RecordBatch;
use arrow::datatypes::Field;
use spelaion_driver_api::{ColumnMeta, LogicalType, Value};
use zip::write::{SimpleFileOptions, StreamWriter};
use zip::{CompressionMethod, ZipWriter};

use crate::export::{Encoder, FileSummary, PartFile, Sink, TextEncoding, text};

/// Rows of an Excel sheet, the header row included.
pub(crate) const MAX_SHEET_ROWS: u64 = 1_048_576;
/// Columns of an Excel sheet.
pub(crate) const MAX_SHEET_COLUMNS: usize = 16_384;
/// Characters (UTF-16 code units, as Excel counts them) of a cell.
pub(crate) const MAX_CELL_CHARS: usize = 32_767;
/// Characters of a sheet's name.
const MAX_SHEET_NAME: usize = 31;
/// Bytes of a sheet's XML before it goes on in a new sheet: a zip entry past 4 GiB needs
/// ZIP64, which not every reader opens.
const MAX_SHEET_BYTES: u64 = 3_500_000_000;
/// The largest integer a double (an Excel number) holds exactly.
const MAX_EXACT_INTEGER: i64 = 1 << 53;
/// 1970-01-01 as an Excel serial date (1900 date system).
const UNIX_EPOCH_SERIAL: i64 = 25_569;
/// 1900-03-01: Excel counts a 29 February 1900 that never was, so earlier serials are wrong.
const FIRST_SERIAL: i64 = 61;
/// 9999-12-31, the last date Excel shows.
const LAST_SERIAL: i64 = 2_958_465;
const MICROS_PER_DAY: i64 = 86_400_000_000;

/// The styles of `styles.xml`, by index.
const STYLE_HEADER: u8 = 1;
const STYLE_DATE: u8 = 2;
const STYLE_TIMESTAMP: u8 = 3;
const STYLE_TIME: u8 = 4;

/// How a column's cells are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Float,
    Decimal,
    Bool,
    Date,
    Time,
    Timestamp,
    Text,
}

impl Kind {
    fn of(field: &Field) -> (Kind, LogicalType) {
        let logical = ColumnMeta::from_field(field)
            .map(|m| m.logical)
            .unwrap_or(LogicalType::Text);
        let kind = match logical {
            LogicalType::Int => Kind::Int,
            LogicalType::Float => Kind::Float,
            LogicalType::Decimal => Kind::Decimal,
            LogicalType::Bool => Kind::Bool,
            LogicalType::Date => Kind::Date,
            LogicalType::Time => Kind::Time,
            // Excel has no time zones: a timestamp with one is written in UTC.
            LogicalType::Timestamp | LogicalType::TimestampTz => Kind::Timestamp,
            LogicalType::Text
            | LogicalType::Bytes
            | LogicalType::Uuid
            | LogicalType::Json
            | LogicalType::Interval
            | LogicalType::Array
            | LogicalType::Native => Kind::Text,
        };
        (kind, logical)
    }
}

/// The columns of the sheet being written.
struct Sheet {
    /// The name asked for; its later parts are `name (2)`, `name (3)`…
    base: String,
    part: u32,
    columns: Vec<usize>,
    kinds: Vec<Kind>,
    logicals: Vec<LogicalType>,
    names: Vec<String>,
    /// `A`, `B`… for each column.
    letters: Vec<String>,
    /// Rows of the open part, its header included.
    rows: u64,
    /// Bytes of the open part's XML.
    bytes: u64,
}

type Zip = ZipWriter<StreamWriter<Encoder<Sink>>>;

/// A workbook being written, a sheet and a batch at a time.
pub(crate) struct XlsxBook {
    zip: Zip,
    /// Every sheet written, in order.
    sheets: Vec<String>,
    current: Option<Sheet>,
    /// Rows of a sheet, its header included ([`MAX_SHEET_ROWS`]; smaller in tests).
    sheet_rows: u64,
    rows: u64,
    line: String,
}

fn entry_options() -> SimpleFileOptions {
    // The fastest deflate level: a large export spends its time compressing otherwise, and
    // the gain of a higher level on XML is small.
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .compression_level(Some(1))
}

fn zip_error(e: zip::result::ZipError) -> io::Error {
    match e {
        zip::result::ZipError::Io(e) => e,
        other => io::Error::other(other),
    }
}

impl XlsxBook {
    pub(crate) fn create(path: &Path, progress: Option<Arc<AtomicU64>>) -> io::Result<Self> {
        let mut file = PartFile::create(path, TextEncoding::Utf8, false)?;
        if let Some(counter) = progress {
            file.report_to(counter);
        }
        Ok(Self {
            zip: ZipWriter::new_stream(file.into_writer()),
            sheets: Vec::new(),
            current: None,
            sheet_rows: MAX_SHEET_ROWS,
            rows: 0,
            line: String::new(),
        })
    }

    /// Sheets of `rows` rows, the header included (tests: a split without a million rows).
    #[cfg(test)]
    pub(crate) fn with_sheet_rows(mut self, rows: u64) -> Self {
        self.sheet_rows = rows.max(2);
        self
    }

    /// Starts a sheet named after `name` (made a valid, unused sheet name) with the columns
    /// `columns` of the batches to come; `fields` are their fields, in that order.
    pub(crate) fn begin_sheet(
        &mut self,
        name: &str,
        fields: &[Field],
        columns: Vec<usize>,
    ) -> io::Result<()> {
        self.end_sheet()?;
        if columns.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a sheet needs at least one column",
            ));
        }
        if columns.len() > MAX_SHEET_COLUMNS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} columns: an Excel sheet holds at most {MAX_SHEET_COLUMNS}",
                    columns.len()
                ),
            ));
        }
        let (kinds, logicals) = fields.iter().map(Kind::of).unzip();
        self.current = Some(Sheet {
            base: sheet_name(name),
            part: 0,
            letters: (0..columns.len()).map(column_letters).collect(),
            columns,
            kinds,
            logicals,
            names: fields.iter().map(|f| f.name().clone()).collect(),
            rows: 0,
            bytes: 0,
        });
        self.open_part()
    }

    /// Opens the current sheet's next part: its entry, its frozen header row.
    fn open_part(&mut self) -> io::Result<()> {
        let Some(sheet) = self.current.as_mut() else {
            return Ok(());
        };
        sheet.part += 1;
        let wanted = if sheet.part == 1 {
            sheet.base.clone()
        } else {
            with_suffix(&sheet.base, &format!(" ({})", sheet.part))
        };
        let name = unused_name(&wanted, &self.sheets);
        self.sheets.push(name);
        let entry = format!("xl/worksheets/sheet{}.xml", self.sheets.len());
        self.zip
            .start_file(entry, entry_options())
            .map_err(zip_error)?;
        let mut head = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
             <worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
             xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
             <sheetViews><sheetView workbookViewId=\"0\">\
             <pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/>\
             </sheetView></sheetViews><sheetData><row r=\"1\">",
        );
        for (name, letter) in sheet.names.iter().zip(&sheet.letters) {
            let _ = write!(
                head,
                "<c r=\"{letter}1\" s=\"{STYLE_HEADER}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">"
            );
            escape_into(&mut head, name);
            head.push_str("</t></is></c>");
        }
        head.push_str("</row>");
        self.zip.write_all(head.as_bytes())?;
        sheet.rows = 1;
        sheet.bytes = head.len() as u64;
        Ok(())
    }

    fn close_part(&mut self) -> io::Result<()> {
        self.zip.write_all(b"</sheetData></worksheet>")
    }

    /// Writes the chosen columns of every row of `batch`, starting new sheets as they fill.
    pub(crate) fn write(&mut self, batch: &RecordBatch) -> io::Result<()> {
        for row in 0..batch.num_rows() {
            let full = self
                .current
                .as_ref()
                .is_some_and(|s| s.rows >= self.sheet_rows || s.bytes >= MAX_SHEET_BYTES);
            if full {
                self.close_part()?;
                self.open_part()?;
            }
            let Some(sheet) = self.current.as_mut() else {
                return Err(io::Error::other("no sheet was started"));
            };
            self.line.clear();
            let number = sheet.rows + 1;
            let _ = write!(self.line, "<row r=\"{number}\">");
            for (i, &column) in sheet.columns.iter().enumerate() {
                let value =
                    Value::from_array(batch.column(column).as_ref(), row, sheet.logicals[i])
                        .unwrap_or(Value::Null);
                cell(
                    &mut self.line,
                    &sheet.letters[i],
                    number,
                    sheet.kinds[i],
                    &value,
                )
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{}: {e}", sheet.names[i]),
                    )
                })?;
            }
            self.line.push_str("</row>");
            self.zip.write_all(self.line.as_bytes())?;
            sheet.rows += 1;
            sheet.bytes += self.line.len() as u64;
            self.rows += 1;
        }
        Ok(())
    }

    /// Ends the current sheet, if any.
    pub(crate) fn end_sheet(&mut self) -> io::Result<()> {
        if self.current.take().is_some() {
            self.close_part()?;
        }
        Ok(())
    }

    /// Writes the workbook's parts and renames the file to its final name.
    pub(crate) fn finish(mut self) -> io::Result<FileSummary> {
        self.end_sheet()?;
        if self.sheets.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a workbook needs at least one sheet",
            ));
        }
        let parts = [
            ("[Content_Types].xml", content_types(self.sheets.len())),
            ("_rels/.rels", ROOT_RELS.to_string()),
            ("xl/workbook.xml", workbook(&self.sheets)),
            (
                "xl/_rels/workbook.xml.rels",
                workbook_rels(self.sheets.len()),
            ),
            ("xl/styles.xml", STYLES.to_string()),
        ];
        for (name, xml) in parts {
            self.zip
                .start_file(name, entry_options())
                .map_err(zip_error)?;
            self.zip.write_all(xml.as_bytes())?;
        }
        let out = self.zip.finish().map_err(zip_error)?.into_inner();
        PartFile::from_writer(out).commit(self.rows)
    }
}

/// One cell, or nothing for NULL. An error says why the value cannot be written.
fn cell(out: &mut String, letter: &str, row: u64, kind: Kind, value: &Value) -> Result<(), String> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    let number = |out: &mut String, style: Option<u8>, v: &str| {
        let _ = write!(out, "<c r=\"{letter}{row}\"");
        if let Some(style) = style {
            let _ = write!(out, " s=\"{style}\"");
        }
        let _ = write!(out, "><v>{v}</v></c>");
    };
    match (kind, value) {
        (Kind::Int, Value::Int(i)) if i.unsigned_abs() <= MAX_EXACT_INTEGER as u64 => {
            number(out, None, &i.to_string());
            return Ok(());
        }
        (Kind::Float, Value::Float(x)) if x.is_finite() => {
            number(out, None, &format!("{x:?}"));
            return Ok(());
        }
        (Kind::Decimal, Value::Decimal(text)) => {
            if let Some(exact) = exact_double(text) {
                number(out, None, &exact);
                return Ok(());
            }
        }
        (Kind::Bool, Value::Bool(b)) => {
            let _ = write!(
                out,
                "<c r=\"{letter}{row}\" t=\"b\"><v>{}</v></c>",
                u8::from(*b)
            );
            return Ok(());
        }
        (Kind::Date, Value::Date(days)) => {
            if let Some(serial) = date_serial(*days) {
                number(out, Some(STYLE_DATE), &serial.to_string());
                return Ok(());
            }
        }
        (Kind::Timestamp, Value::Timestamp(us) | Value::TimestampTz(us)) => {
            if let Some(serial) = timestamp_serial(*us) {
                number(out, Some(STYLE_TIMESTAMP), &format!("{serial:?}"));
                return Ok(());
            }
        }
        (Kind::Time, Value::Time(us)) if (0..MICROS_PER_DAY).contains(us) => {
            let fraction = *us as f64 / MICROS_PER_DAY as f64;
            number(out, Some(STYLE_TIME), &format!("{fraction:?}"));
            return Ok(());
        }
        // Anything else is written as the text CSV would hold.
        _ => {}
    }
    let Some(text) = text(value) else {
        return Ok(());
    };
    let units = text.encode_utf16().count();
    if units > MAX_CELL_CHARS {
        return Err(format!(
            "a value of {units} characters, more than the {MAX_CELL_CHARS} an Excel cell holds: export it as CSV"
        ));
    }
    let _ = write!(
        out,
        "<c r=\"{letter}{row}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">"
    );
    escape_into(out, &text);
    out.push_str("</t></is></c>");
    Ok(())
}

/// A decimal's text as the double that holds it exactly, written as Excel reads it; `None`
/// when a double would change a digit (`0.1` is exact enough: its shortest form is `0.1`).
fn exact_double(text: &str) -> Option<String> {
    let x: f64 = text.trim().parse().ok()?;
    if !x.is_finite() {
        return None;
    }
    let written = format!("{x:?}");
    (canonical_decimal(&written)? == canonical_decimal(text.trim())?).then_some(written)
}

/// A decimal number's digits without what says nothing: `-012.500` → `-12.5`, `1e3` →
/// `1000`. `None` for what is not a plain decimal.
fn canonical_decimal(text: &str) -> Option<String> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (mantissa, exponent) = match digits.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (digits, 0),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // All the digits, and where the point goes among them.
    let all: String = format!("{whole}{fraction}");
    let point = i64::from(exponent) + whole.len() as i64;
    let leading = all.len() - all.trim_start_matches('0').len();
    let all = all.trim_start_matches('0');
    let point = point - leading as i64;
    let all = all.trim_end_matches('0');
    if all.is_empty() {
        return Some("0".into());
    }
    if point.abs() > 400 {
        return None;
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', point.unsigned_abs() as usize));
        out.push_str(all);
    } else if point as usize >= all.len() {
        out.push_str(all);
        out.extend(std::iter::repeat_n('0', point as usize - all.len()));
    } else {
        out.push_str(&all[..point as usize]);
        out.push('.');
        out.push_str(&all[point as usize..]);
    }
    Some(out)
}

/// The Excel serial of a date (days since 1970-01-01), within the dates Excel holds rightly
/// (PostgreSQL's infinities, `i32::MAX` and `i32::MIN`, are far outside them).
fn date_serial(days: i32) -> Option<i64> {
    let serial = i64::from(days) + UNIX_EPOCH_SERIAL;
    (FIRST_SERIAL..=LAST_SERIAL)
        .contains(&serial)
        .then_some(serial)
}

/// The Excel serial of a timestamp (microseconds since 1970-01-01): days and a day's fraction
/// (the infinities, `i64::MAX` and `i64::MIN`, are far outside the dates Excel holds).
fn timestamp_serial(us: i64) -> Option<f64> {
    let days = us.div_euclid(MICROS_PER_DAY) + UNIX_EPOCH_SERIAL;
    if !(FIRST_SERIAL..=LAST_SERIAL).contains(&days) {
        return None;
    }
    let fraction = us.rem_euclid(MICROS_PER_DAY) as f64 / MICROS_PER_DAY as f64;
    Some(days as f64 + fraction)
}

/// `A`…`Z`, `AA`… for a column index.
fn column_letters(mut index: usize) -> String {
    let mut letters = Vec::new();
    loop {
        letters.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap_or_default()
}

/// XML text, with what XML cannot hold written as Excel writes it (`_x0001_`): control
/// characters, a carriage return (XML would turn `\r\n` into `\n`) and a literal `_xHHHH_`,
/// whose `_` is escaped so a reader does not take it for an escape.
pub(crate) fn escape_into(out: &mut String, text: &str) {
    let bytes = text.as_bytes();
    for (i, c) in text.char_indices() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' => out.push(c),
            '_' if looks_escaped(&bytes[i..]) => out.push_str("_x005F_"),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {
                let _ = write!(out, "_x{:04X}_", c as u32);
            }
            c => out.push(c),
        }
    }
}

/// `_xHHHH_` at the start of `bytes`.
pub(crate) fn looks_escaped(bytes: &[u8]) -> bool {
    bytes.len() >= 7
        && bytes[0] == b'_'
        && bytes[1] == b'x'
        && bytes[2..6].iter().all(u8::is_ascii_hexdigit)
        && bytes[6] == b'_'
}

/// A valid sheet name: no `[]:*?/\`, no control characters, no `'` at either end, at most
/// 31 characters, never empty, never `History` (Excel keeps it).
fn sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('\'').trim();
    let mut out: String = cleaned.chars().take(MAX_SHEET_NAME).collect();
    if out.is_empty() {
        out = "Sheet".into();
    }
    if out.eq_ignore_ascii_case("history") {
        out.push('_');
    }
    out
}

/// `base` with `suffix`, cut so both fit a sheet name.
fn with_suffix(base: &str, suffix: &str) -> String {
    let room = MAX_SHEET_NAME.saturating_sub(suffix.chars().count());
    let mut out: String = base.chars().take(room).collect();
    out.push_str(suffix);
    out
}

/// `wanted`, or `wanted (2)`… when a sheet already has that name (Excel ignores case).
fn unused_name(wanted: &str, taken: &[String]) -> String {
    let used = |name: &str| {
        taken
            .iter()
            .any(|t| t.to_lowercase() == name.to_lowercase())
    };
    if !used(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|n| with_suffix(wanted, &format!(" ({n})")))
        .find(|name| !used(name))
        .unwrap_or_else(|| wanted.to_string())
}

fn content_types(sheets: usize) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
         <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
         <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
         <Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
         <Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>",
    );
    for n in 1..=sheets {
        let _ = write!(
            xml,
            "<Override PartName=\"/xl/worksheets/sheet{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>"
        );
    }
    xml.push_str("</Types>");
    xml
}

const ROOT_RELS: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
    <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
    <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/>\
    </Relationships>";

fn workbook(sheets: &[String]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" \
         xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">\
         <bookViews><workbookView/></bookViews><sheets>",
    );
    for (i, name) in sheets.iter().enumerate() {
        let n = i + 1;
        xml.push_str("<sheet name=\"");
        escape_into(&mut xml, name);
        let _ = write!(xml, "\" sheetId=\"{n}\" r:id=\"rId{n}\"/>");
    }
    xml.push_str("</sheets></workbook>");
    xml
}

fn workbook_rels(sheets: usize) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
         <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">",
    );
    for n in 1..=sheets {
        let _ = write!(
            xml,
            "<Relationship Id=\"rId{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>"
        );
    }
    let _ = write!(
        xml,
        "<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/>",
        sheets + 1
    );
    xml.push_str("</Relationships>");
    xml
}

/// Fonts: normal and bold (the header). Cell formats, by the `STYLE_*` indexes: none, the
/// header, a date, a timestamp and a time.
const STYLES: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
    <styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">\
    <numFmts count=\"3\">\
    <numFmt numFmtId=\"164\" formatCode=\"yyyy\\-mm\\-dd\"/>\
    <numFmt numFmtId=\"165\" formatCode=\"yyyy\\-mm\\-dd\\ hh:mm:ss\"/>\
    <numFmt numFmtId=\"166\" formatCode=\"hh:mm:ss\"/>\
    </numFmts>\
    <fonts count=\"2\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font>\
    <font><b/><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts>\
    <fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill>\
    <fill><patternFill patternType=\"gray125\"/></fill></fills>\
    <borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\
    <cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
    <cellXfs count=\"5\">\
    <xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>\
    <xf numFmtId=\"0\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\"/>\
    <xf numFmtId=\"164\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\
    <xf numFmtId=\"165\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\
    <xf numFmtId=\"166\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\
    </cellXfs>\
    <cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles>\
    </styleSheet>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_letters_follow_excel() {
        assert_eq!(column_letters(0), "A");
        assert_eq!(column_letters(25), "Z");
        assert_eq!(column_letters(26), "AA");
        assert_eq!(column_letters(51), "AZ");
        assert_eq!(column_letters(52), "BA");
        assert_eq!(column_letters(701), "ZZ");
        assert_eq!(column_letters(702), "AAA");
        assert_eq!(column_letters(MAX_SHEET_COLUMNS - 1), "XFD");
    }

    #[test]
    fn sheet_names_are_valid_and_unique() {
        assert_eq!(sheet_name("orders"), "orders");
        assert_eq!(sheet_name("a/b\\c[d]:e*f?g"), "a_b_c_d__e_f_g");
        assert_eq!(sheet_name("'quoted'"), "quoted");
        assert_eq!(sheet_name(""), "Sheet");
        assert_eq!(sheet_name("History"), "History_");
        assert_eq!(sheet_name(&"x".repeat(40)).chars().count(), 31);
        let taken = vec!["Orders".to_string(), "orders (2)".to_string()];
        assert_eq!(unused_name("orders", &taken), "orders (3)");
        assert_eq!(
            unused_name(&"y".repeat(31), &["y".repeat(31)])
                .chars()
                .count(),
            31
        );
    }

    #[test]
    fn text_is_escaped_as_excel_reads_it() {
        let mut out = String::new();
        escape_into(&mut out, "a<b>&\"c\"\r\n\u{1}_x0041_ _y");
        assert_eq!(
            out,
            "a&lt;b&gt;&amp;&quot;c&quot;_x000D_\n_x0001__x005F_x0041_ _y"
        );
    }

    #[test]
    fn decimals_are_numbers_only_when_a_double_holds_them() {
        assert_eq!(exact_double("12.50").as_deref(), Some("12.5"));
        assert_eq!(exact_double("-0.25").as_deref(), Some("-0.25"));
        assert_eq!(exact_double("0.1").as_deref(), Some("0.1"));
        assert_eq!(exact_double("100").as_deref(), Some("100.0"));
        assert_eq!(exact_double("123456789012345678.5"), None);
        assert_eq!(exact_double("0.12345678901234567890"), None);
        assert_eq!(exact_double("NaN"), None);
        assert_eq!(canonical_decimal("1e3").as_deref(), Some("1000"));
        assert_eq!(canonical_decimal("-012.500").as_deref(), Some("-12.5"));
        assert_eq!(canonical_decimal("1.5e-3").as_deref(), Some("0.0015"));
        assert_eq!(canonical_decimal("0.000").as_deref(), Some("0"));
    }

    #[test]
    fn dates_are_serials_only_where_excel_counts_them_rightly() {
        // 2024-03-04 is day 19786 since 1970.
        assert_eq!(date_serial(19_786), Some(45_355));
        assert_eq!(date_serial(0), Some(25_569));
        // 1900-03-01 is the first right one; 1900-02-28 is not.
        assert_eq!(date_serial(-25_508), Some(61));
        assert_eq!(date_serial(-25_509), None);
        assert_eq!(date_serial(i32::MAX), None);
        assert_eq!(date_serial(i32::MIN), None);
        assert_eq!(timestamp_serial(MICROS_PER_DAY / 2), Some(25_569.5));
        assert_eq!(timestamp_serial(-MICROS_PER_DAY / 4), Some(25_568.75));
        assert_eq!(timestamp_serial(i64::MAX), None);
        assert_eq!(timestamp_serial(i64::MIN), None);
    }

    #[test]
    fn cells_that_cannot_hold_a_value_exactly_are_text() {
        let mut out = String::new();
        cell(&mut out, "A", 2, Kind::Int, &Value::Int(i64::MAX)).unwrap();
        assert!(
            out.contains("inlineStr") && out.contains("9223372036854775807"),
            "{out}"
        );
        out.clear();
        cell(&mut out, "A", 2, Kind::Int, &Value::Int(42)).unwrap();
        assert_eq!(out, "<c r=\"A2\"><v>42</v></c>");
        out.clear();
        cell(&mut out, "B", 3, Kind::Float, &Value::Float(f64::NAN)).unwrap();
        assert!(out.contains("inlineStr"), "{out}");
        out.clear();
        cell(&mut out, "C", 4, Kind::Bool, &Value::Bool(true)).unwrap();
        assert_eq!(out, "<c r=\"C4\" t=\"b\"><v>1</v></c>");
        out.clear();
        cell(&mut out, "D", 5, Kind::Text, &Value::Null).unwrap();
        assert_eq!(out, "");
        let long = Value::Text("é".repeat(MAX_CELL_CHARS + 1));
        let error = cell(&mut out, "E", 6, Kind::Text, &long).unwrap_err();
        assert!(error.contains("32767"), "{error}");
    }
}
