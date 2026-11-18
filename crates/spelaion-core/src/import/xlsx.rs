//! Reading a sheet of an XLSX workbook (SDB-390). The file is untrusted input (ADR-038), and a
//! workbook is a zip of XML parts, so every way it could take the memory or the time is bounded
//! before it is used:
//!
//! - the zip: at most [`MAX_ENTRIES`] entries, names that differ only in case refused, and each
//!   part read checked before anything of it is unpacked: stored or deflated (nothing else, never
//!   encrypted), its declared size under that part's limit and no more than deflate can unpack
//!   from its compressed size (a zip bomb declares what it cannot be). While a part is unpacked
//!   it may not give a byte past its declared size, and its CRC is checked at its end;
//! - the XML: no DOCTYPE (so no entities to expand), at most [`MAX_DEPTH`] elements deep, at
//!   most [`MAX_XML_ITEM`] bytes in one tag or text, at most [`MAX_ATTRIBUTES`] attributes;
//! - the workbook: at most [`MAX_SHEETS`] sheets and [`MAX_STYLES`] cell formats; the shared
//!   strings at most [`MAX_SHARED_STRINGS`] strings and [`MAX_SHARED_TEXT`] bytes of text, the
//!   only part held whole besides the small ones;
//! - the sheet: streamed a row at a time, at most Excel's 1,048,576 rows, the source's
//!   [`MAX_COLUMNS`] columns and [`MAX_RECORD_BYTES`] bytes per row.
//!
//! Formulas are never evaluated: a cell gives the value Excel saved with it, or its formula's
//! text (`=A1+1`) when none was saved. Numbers with a date format become ISO dates and
//! timestamps (`2024-03-04`, `2024-03-04 10:00:00.5`), times `10:00:00`; booleans `true` and
//! `false`. Everything else is the text the cell holds, and the import detects types from it
//! as it does for CSV.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use zip::{CompressionMethod, ZipArchive};

use super::ImportError;
use super::source::{MAX_COLUMNS, MAX_RECORD_BYTES};

/// Entries of the zip.
pub const MAX_ENTRIES: usize = 10_000;
/// Sheets of the workbook.
pub const MAX_SHEETS: usize = 4_096;
/// Cell formats (`cellXfs`) and number formats of the styles.
pub const MAX_STYLES: usize = 65_536;
/// Strings of the shared-strings table.
pub const MAX_SHARED_STRINGS: usize = 4_000_000;
/// Bytes of text of the shared-strings table, all strings together.
pub const MAX_SHARED_TEXT: usize = 256 * 1024 * 1024;
/// Rows of an Excel sheet.
pub const MAX_SHEET_ROWS: u64 = 1_048_576;
/// Elements inside one another.
pub const MAX_DEPTH: usize = 32;
/// Bytes of one tag or one text of the XML.
pub const MAX_XML_ITEM: u64 = MAX_RECORD_BYTES as u64;
/// Attributes of one element.
pub const MAX_ATTRIBUTES: usize = 64;
/// Unpacked bytes of the small parts (relationships, the workbook, the styles).
const MAX_SMALL_PART: u64 = 32 * 1024 * 1024;
/// Unpacked bytes of the shared-strings part.
const MAX_SHARED_PART: u64 = 1024 * 1024 * 1024;
/// Unpacked bytes of a sheet (streamed: this bounds the time, not the memory).
const MAX_SHEET_PART: u64 = 16 * 1024 * 1024 * 1024;
/// What deflate can unpack from a byte, at most (about 1,032): more is a lie.
const MAX_RATIO: u64 = 1_100;
/// The last Excel serial date, 9999-12-31.
const LAST_SERIAL: f64 = 2_958_466.0;
const MICROS_PER_DAY: f64 = 86_400_000_000.0;

fn bad(path: &Path, message: impl std::fmt::Display) -> ImportError {
    ImportError::Source(format!(
        "{} is not a workbook this can read: {message}",
        path.display()
    ))
}

/// A part of the zip, unpacked as it is read: never past its declared size, its CRC checked
/// at its end.
struct Checked {
    inner: Box<dyn Read + Send>,
    left: u64,
    crc: flate2::Crc,
    expected: u32,
    name: String,
}

impl Read for Checked {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // One byte more than declared is asked for, so a part that gives more is caught.
        let want = buf
            .len()
            .min(usize::try_from(self.left + 1).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buf[..want])?;
        if read as u64 > self.left {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} unpacks to more than it declares", self.name),
            ));
        }
        self.left -= read as u64;
        self.crc.update(&buf[..read]);
        if read == 0 && want > 0 {
            if self.left != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} ends before its declared size", self.name),
                ));
            }
            if self.crc.sum() != self.expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is damaged (its CRC does not match)", self.name),
                ));
            }
        }
        Ok(read)
    }
}

/// The bytes the XML reader takes, bounded between two of its items ([`Limited::mark`]): a tag
/// or a text that never ends stops at [`MAX_XML_ITEM`] instead of filling the memory.
struct Limited {
    inner: BufReader<Checked>,
    since_mark: u64,
}

impl Limited {
    fn mark(&mut self) {
        self.since_mark = 0;
    }
}

impl Read for Limited {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl BufRead for Limited {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.since_mark > MAX_XML_ITEM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "an XML tag or text longer than {} MiB",
                    MAX_XML_ITEM / (1024 * 1024)
                ),
            ));
        }
        self.inner.fill_buf()
    }

    fn consume(&mut self, amount: usize) {
        self.since_mark += amount as u64;
        self.inner.consume(amount);
    }
}

/// An XML item, owned.
enum Item {
    Start {
        /// Without its prefix (`x:row` → `row`).
        name: String,
        /// Names as written (`r:id`), values unescaped.
        attributes: Vec<(String, String)>,
        empty: bool,
    },
    /// The element that ended, without its prefix.
    End(String),
    Text(String),
    Eof,
}

/// An XML part, read an item at a time within the limits above.
struct Xml {
    reader: quick_xml::Reader<Limited>,
    buf: Vec<u8>,
    /// The open elements, without their prefixes.
    open: Vec<String>,
    path: PathBuf,
    part: String,
}

fn local(name: &str) -> String {
    match name.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => name.to_string(),
    }
}

impl Xml {
    fn error(&self, message: impl std::fmt::Display) -> ImportError {
        bad(&self.path, format!("{}: {message}", self.part))
    }

    fn attributes(&self, tag: &BytesStart<'_>) -> Result<Vec<(String, String)>, ImportError> {
        let mut out = Vec::new();
        for (index, attribute) in tag.attributes().enumerate() {
            if index >= MAX_ATTRIBUTES {
                return Err(self.error("an element with too many attributes"));
            }
            let attribute = attribute.map_err(|e| self.error(e))?;
            let value = attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|e| self.error(e))?
                .into_owned();
            out.push((attribute.key.as_ref().to_string(), value));
        }
        Ok(out)
    }

    fn next(&mut self) -> Result<Item, ImportError> {
        self.buf.clear();
        self.reader.get_mut().mark();
        let event = match self.reader.read_event_into(&mut self.buf) {
            Ok(event) => event.into_owned(),
            Err(e) => return Err(self.error(e)),
        };
        Ok(match event {
            Event::Start(tag) => {
                if self.open.len() >= MAX_DEPTH {
                    return Err(self.error(format!("elements nested more than {MAX_DEPTH} deep")));
                }
                let name = local(tag.name().as_ref());
                self.open.push(name.clone());
                Item::Start {
                    name,
                    attributes: self.attributes(&tag)?,
                    empty: false,
                }
            }
            Event::Empty(tag) => Item::Start {
                name: local(tag.name().as_ref()),
                attributes: self.attributes(&tag)?,
                empty: true,
            },
            // `check_end_names` makes an end that does not match its start an error.
            Event::End(_) => Item::End(self.open.pop().unwrap_or_default()),
            Event::Text(text) => Item::Text(text.xml10_content().into_owned()),
            Event::CData(data) => Item::Text(data.as_ref().to_string()),
            Event::GeneralRef(reference) => {
                let name: &str = reference.as_ref();
                if let Some(text) = quick_xml::escape::resolve_predefined_entity(name) {
                    Item::Text(text.to_string())
                } else {
                    match reference.resolve_char_ref() {
                        Ok(Some(c)) => Item::Text(c.to_string()),
                        _ => return Err(self.error(format!("an unknown entity &{name};"))),
                    }
                }
            }
            Event::DocType(_) => return Err(self.error("a DOCTYPE (entities are not read)")),
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) => Item::Text(String::new()),
            Event::Eof => Item::Eof,
        })
    }
}

fn attribute<'a>(attributes: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// A relationship's id attribute: `r:id`, whatever its prefix.
fn relationship_id(attributes: &[(String, String)]) -> Option<&str> {
    attributes
        .iter()
        .find(|(key, _)| key.ends_with(":id"))
        .map(|(_, value)| value.as_str())
}

/// A part's name from a relationship's target: relative to `base`'s folder, or from the root
/// when it starts with `/`. `None` for one that climbs out of the package.
fn resolve(base: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        let mut folder: Vec<&str> = base.split('/').collect();
        folder.pop();
        folder
    };
    for segment in target.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The relationships part of `part`: `xl/workbook.xml` → `xl/_rels/workbook.xml.rels`.
fn rels_of(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((folder, file)) => format!("{folder}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// How a cell format shows a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Number,
    Date,
    Time,
    Timestamp,
}

/// What a number format's code shows: a date, a time, both, or a number. Quoted text,
/// `[colors]`, `\x` and `_x` / `*x` are not codes.
fn shown_by_code(code: &str) -> Shown {
    let mut date = false;
    let mut time = false;
    let mut chars = code.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for d in chars.by_ref() {
                    if d == '"' {
                        break;
                    }
                }
            }
            '[' => {
                let mut inside = String::new();
                for d in chars.by_ref() {
                    if d == ']' {
                        break;
                    }
                    inside.push(d);
                }
                // `[h]`, `[mm]`, `[ss]`: elapsed time.
                let lower = inside.to_ascii_lowercase();
                if !lower.is_empty() && lower.chars().all(|c| matches!(c, 'h' | 'm' | 's')) {
                    time = true;
                }
            }
            '\\' | '_' | '*' => {
                chars.next();
            }
            'y' | 'Y' | 'd' | 'D' => date = true,
            'h' | 'H' | 's' | 'S' => time = true,
            'm' | 'M' => {
                // `m` is months next to a date, minutes next to a time: either way a date code.
                date = true;
            }
            _ => {}
        }
    }
    match (date, time) {
        (true, true) => {
            // `mm:ss` and `h:mm` say minutes, not months.
            let lower = code.to_ascii_lowercase();
            if !lower.contains('y') && !lower.contains('d') {
                Shown::Time
            } else {
                Shown::Timestamp
            }
        }
        (true, false) => Shown::Date,
        (false, true) => Shown::Time,
        (false, false) => Shown::Number,
    }
}

/// Excel's built-in number formats that show dates and times.
fn shown_by_builtin(id: u32) -> Shown {
    match id {
        14..=17 | 27..=31 | 34..=36 | 50..=58 => Shown::Date,
        18..=21 | 32 | 33 | 45..=47 => Shown::Time,
        22 => Shown::Timestamp,
        _ => Shown::Number,
    }
}

/// A serial date as ISO text, as `shown`; `None` past the dates Excel holds.
fn serial_text(serial: f64, shown: Shown, date1904: bool) -> Option<String> {
    if !serial.is_finite() || serial < 0.0 || serial >= LAST_SERIAL {
        return None;
    }
    let mut days = serial.floor() as i64;
    let mut micros = ((serial - serial.floor()) * MICROS_PER_DAY).round() as i64;
    if micros >= MICROS_PER_DAY as i64 {
        days += 1;
        micros = 0;
    }
    let time = || {
        let seconds = micros / 1_000_000;
        let fraction = micros % 1_000_000;
        let mut text = format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
        if fraction != 0 {
            let digits = format!("{fraction:06}");
            text.push('.');
            text.push_str(digits.trim_end_matches('0'));
        }
        text
    };
    if shown == Shown::Time && days == 0 {
        return Some(time());
    }
    let epoch = if date1904 {
        chrono::NaiveDate::from_ymd_opt(1904, 1, 1)?
    } else if days >= 61 {
        chrono::NaiveDate::from_ymd_opt(1899, 12, 30)?
    } else if days == 60 || days == 0 {
        // 29 February 1900 never was, and day 0 is no date: the number as it is.
        return None;
    } else {
        chrono::NaiveDate::from_ymd_opt(1899, 12, 31)?
    };
    let date = epoch.checked_add_signed(chrono::TimeDelta::try_days(days)?)?;
    let day = date.format("%Y-%m-%d").to_string();
    Some(match shown {
        Shown::Date if micros == 0 => day,
        Shown::Date | Shown::Timestamp | Shown::Time => format!("{day} {}", time()),
        Shown::Number => return None,
    })
}

/// Undoes Excel's escapes of what XML cannot hold: `_x000D_` is a carriage return, `_x005F_`
/// a `_`.
fn unescape(text: String) -> String {
    if !text.contains("_x") {
        return text;
    }
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if crate::export_xlsx::looks_escaped(&bytes[i..]) {
            let code = u32::from_str_radix(&text[i + 2..i + 6], 16).ok();
            if let Some(c) = code.and_then(char::from_u32) {
                out.push(c);
                i += 7;
                continue;
            }
        }
        let c = text[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A cell reference's column: `B7` → 1. `None` for one that is not a reference.
fn column_of(reference: &str) -> Option<usize> {
    let letters: &str = &reference[..reference
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(reference.len())];
    if letters.is_empty() || letters.len() > 3 {
        return None;
    }
    let mut column = 0usize;
    for b in letters.bytes() {
        column = column * 26 + usize::from(b.to_ascii_uppercase() - b'A' + 1);
    }
    Some(column - 1)
}

/// The zip and the names of its entries.
struct Package {
    archive: ZipArchive<File>,
    /// Lower-case name → entry: part names ignore case.
    names: HashMap<String, usize>,
    path: PathBuf,
}

impl Package {
    fn open(path: &Path) -> Result<Self, ImportError> {
        let file = File::open(path).map_err(|e| ImportError::Io {
            path: path.display().to_string(),
            message: e.to_string(),
        })?;
        let archive = ZipArchive::new(file).map_err(|e| bad(path, e))?;
        if archive.len() > MAX_ENTRIES {
            return Err(bad(
                path,
                format!("{} entries, more than {MAX_ENTRIES}", archive.len()),
            ));
        }
        let mut names = HashMap::new();
        for index in 0..archive.len() {
            let Some(Ok(name)) = archive.name_for_index(index) else {
                continue;
            };
            let key = name.trim_start_matches('/').to_lowercase();
            if names.insert(key, index).is_some() {
                return Err(bad(path, format!("two entries named {name}")));
            }
        }
        Ok(Self {
            archive,
            names,
            path: path.to_path_buf(),
        })
    }

    fn has(&self, part: &str) -> bool {
        self.names.contains_key(&part.to_lowercase())
    }

    /// A part as XML, checked as the module says before anything of it is unpacked.
    fn xml(&mut self, part: &str, max: u64) -> Result<Xml, ImportError> {
        let index = *self
            .names
            .get(&part.to_lowercase())
            .ok_or_else(|| bad(&self.path, format!("{part} is missing")))?;
        let entry = self
            .archive
            .by_index_raw(index)
            .map_err(|e| bad(&self.path, e))?;
        let size = entry.size();
        let compressed = entry.compressed_size();
        let method = entry.compression();
        let encrypted = entry.encrypted();
        let crc = entry.crc32();
        let start = entry.data_start();
        drop(entry);
        if encrypted {
            return Err(bad(&self.path, format!("{part} is encrypted")));
        }
        if size > max {
            return Err(bad(
                &self.path,
                format!("{part} unpacks to {size} bytes, more than the {max} it may"),
            ));
        }
        if size > compressed.saturating_mul(MAX_RATIO).max(1024) {
            return Err(bad(
                &self.path,
                format!(
                    "{part} declares {size} bytes from {compressed}: more than deflate can unpack"
                ),
            ));
        }
        let start = start.ok_or_else(|| bad(&self.path, format!("{part} has no data")))?;
        let mut file = File::open(&self.path).map_err(|e| ImportError::Io {
            path: self.path.display().to_string(),
            message: e.to_string(),
        })?;
        file.seek(SeekFrom::Start(start))
            .map_err(|e| bad(&self.path, e))?;
        let raw = BufReader::new(file.take(compressed));
        let inner: Box<dyn Read + Send> = match method {
            CompressionMethod::Stored if size == compressed => Box::new(raw),
            CompressionMethod::Deflated => Box::new(flate2::bufread::DeflateDecoder::new(raw)),
            other => {
                return Err(bad(
                    &self.path,
                    format!("{part} is compressed with {other}, not deflate"),
                ));
            }
        };
        let checked = Checked {
            inner,
            left: size,
            crc: flate2::Crc::new(),
            expected: crc,
            name: part.to_string(),
        };
        let mut reader = quick_xml::Reader::from_reader(Limited {
            inner: BufReader::with_capacity(64 * 1024, checked),
            since_mark: 0,
        });
        let config = reader.config_mut();
        config.trim_text(false);
        config.check_end_names = true;
        config.expand_empty_elements = false;
        Ok(Xml {
            reader,
            buf: Vec::new(),
            open: Vec::new(),
            path: self.path.clone(),
            part: part.to_string(),
        })
    }

    /// The relationships of `part`: id → (type, target part).
    fn relationships(
        &mut self,
        part: &str,
    ) -> Result<HashMap<String, (String, String)>, ImportError> {
        let rels = rels_of(part);
        let mut out = HashMap::new();
        if !self.has(&rels) {
            return Ok(out);
        }
        let mut xml = self.xml(&rels, MAX_SMALL_PART)?;
        loop {
            match xml.next()? {
                Item::Start {
                    name, attributes, ..
                } if name == "Relationship" => {
                    if out.len() >= MAX_ENTRIES {
                        return Err(xml.error("too many relationships"));
                    }
                    let (Some(id), Some(target)) = (
                        attribute(&attributes, "Id"),
                        attribute(&attributes, "Target"),
                    ) else {
                        continue;
                    };
                    if attribute(&attributes, "TargetMode") == Some("External") {
                        continue;
                    }
                    let kind = attribute(&attributes, "Type").unwrap_or("").to_string();
                    if let Some(target) = resolve(part, target) {
                        out.insert(id.to_string(), (kind, target));
                    }
                }
                Item::Eof => return Ok(out),
                Item::Start { .. } | Item::End(_) | Item::Text(_) => {}
            }
        }
    }
}

/// The sheets of a workbook, by name, and what reading any of them needs.
struct Workbook {
    package: Package,
    /// Name and part of each worksheet, in the workbook's order.
    sheets: Vec<(String, String)>,
    date1904: bool,
    /// Each cell format's number format, by the cell's `s` attribute.
    formats: Vec<Shown>,
    shared: Vec<String>,
}

impl Workbook {
    fn open(path: &Path) -> Result<Self, ImportError> {
        let mut package = Package::open(path)?;
        let root = package.relationships("")?;
        let document = root
            .values()
            .find(|(kind, _)| kind.ends_with("/officeDocument"))
            .map(|(_, target)| target.clone())
            .unwrap_or_else(|| "xl/workbook.xml".into());
        let rels = package.relationships(&document)?;
        let mut xml = package.xml(&document, MAX_SMALL_PART)?;
        let mut sheets = Vec::new();
        let mut date1904 = false;
        loop {
            match xml.next()? {
                Item::Start {
                    name, attributes, ..
                } => match name.as_str() {
                    "workbookPr" => {
                        date1904 = matches!(attribute(&attributes, "date1904"), Some("1" | "true"));
                    }
                    "sheet" => {
                        if sheets.len() >= MAX_SHEETS {
                            return Err(xml.error(format!("more than {MAX_SHEETS} sheets")));
                        }
                        let (Some(sheet), Some(id)) =
                            (attribute(&attributes, "name"), relationship_id(&attributes))
                        else {
                            continue;
                        };
                        // Chart sheets and macro sheets hold no rows.
                        if let Some((kind, target)) = rels.get(id)
                            && kind.ends_with("/worksheet")
                        {
                            sheets.push((sheet.to_string(), target.clone()));
                        }
                    }
                    _ => {}
                },
                Item::Eof => break,
                Item::End(_) | Item::Text(_) => {}
            }
        }
        drop(xml);
        if sheets.is_empty() {
            return Err(bad(path, "it has no worksheet"));
        }
        let part_of = |suffix: &str| {
            rels.values()
                .find(|(kind, _)| kind.ends_with(suffix))
                .map(|(_, target)| target.clone())
        };
        let formats = match part_of("/styles") {
            Some(styles) if package.has(&styles) => read_styles(&mut package, &styles)?,
            _ => Vec::new(),
        };
        let shared = match part_of("/sharedStrings") {
            Some(strings) if package.has(&strings) => {
                read_shared(&mut package, &strings, MAX_SHARED_STRINGS, MAX_SHARED_TEXT)?
            }
            _ => Vec::new(),
        };
        Ok(Self {
            package,
            sheets,
            date1904,
            formats,
            shared,
        })
    }
}

fn read_styles(package: &mut Package, part: &str) -> Result<Vec<Shown>, ImportError> {
    let mut xml = package.xml(part, MAX_SMALL_PART)?;
    let mut codes: HashMap<u32, Shown> = HashMap::new();
    let mut formats = Vec::new();
    let mut in_cell_formats = false;
    loop {
        match xml.next()? {
            Item::Start {
                name,
                attributes,
                empty,
            } => match name.as_str() {
                "numFmt" => {
                    if codes.len() >= MAX_STYLES {
                        return Err(xml.error("too many number formats"));
                    }
                    if let (Some(id), Some(code)) = (
                        attribute(&attributes, "numFmtId").and_then(|v| v.parse().ok()),
                        attribute(&attributes, "formatCode"),
                    ) {
                        codes.insert(id, shown_by_code(code));
                    }
                }
                "cellXfs" if !empty => in_cell_formats = true,
                "xf" if in_cell_formats => {
                    if formats.len() >= MAX_STYLES {
                        return Err(xml.error("too many cell formats"));
                    }
                    let id: u32 = attribute(&attributes, "numFmtId")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    formats.push(
                        codes
                            .get(&id)
                            .copied()
                            .unwrap_or_else(|| shown_by_builtin(id)),
                    );
                }
                _ => {}
            },
            Item::End(name) if name == "cellXfs" => in_cell_formats = false,
            Item::End(_) => {}
            Item::Eof => return Ok(formats),
            Item::Text(_) => {}
        }
    }
}

/// The shared strings, at most `max_strings` of them and `max_text` bytes of text.
fn read_shared(
    package: &mut Package,
    part: &str,
    max_strings: usize,
    max_text: usize,
) -> Result<Vec<String>, ImportError> {
    let mut xml = package.xml(part, MAX_SHARED_PART)?;
    let mut strings: Vec<String> = Vec::new();
    let mut total = 0usize;
    // Inside `<si>`; inside a `<t>` that is part of its text (not of `<rPh>`, the phonetic
    // reading).
    let mut current: Option<String> = None;
    let mut in_text = false;
    let mut phonetic = 0usize;
    loop {
        match xml.next()? {
            Item::Start { name, empty, .. } => match name.as_str() {
                "si" => {
                    if strings.len() >= max_strings {
                        return Err(xml.error(format!("more than {max_strings} shared strings")));
                    }
                    if empty {
                        strings.push(String::new());
                    } else {
                        current = Some(String::new());
                    }
                }
                "rPh" if !empty => phonetic += 1,
                "t" if !empty && current.is_some() && phonetic == 0 => in_text = true,
                _ => {}
            },
            Item::Text(text) => {
                if in_text && let Some(current) = current.as_mut() {
                    total += text.len();
                    if total > max_text {
                        return Err(
                            xml.error(format!("shared strings of more than {max_text} bytes"))
                        );
                    }
                    current.push_str(&text);
                }
            }
            Item::End(name) => match name.as_str() {
                "t" => in_text = false,
                "rPh" => phonetic = phonetic.saturating_sub(1),
                "si" => {
                    if let Some(text) = current.take() {
                        strings.push(unescape(text));
                    }
                }
                _ => {}
            },
            Item::Eof => return Ok(strings),
        }
    }
}

/// The cell being read.
#[derive(Default)]
struct Cell {
    column: usize,
    kind: Option<String>,
    style: usize,
    value: Option<String>,
    formula: Option<String>,
    inline: Option<String>,
}

/// Where the text being read goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextOf {
    Nowhere,
    Value,
    Formula,
    Inline,
}

/// The rows of one sheet, a row at a time, each cell as text or NULL (absent or empty).
pub(crate) struct SheetRows {
    xml: Xml,
    date1904: bool,
    formats: Vec<Shown>,
    shared: Vec<String>,
    /// The workbook's worksheets, in order.
    pub sheets: Vec<String>,
    in_data: bool,
    ended: bool,
    rows: u64,
}

impl SheetRows {
    /// The sheet named `sheet` (any case), or the first one.
    pub(crate) fn open(path: &Path, sheet: Option<&str>) -> Result<Self, ImportError> {
        let mut workbook = Workbook::open(path)?;
        let names: Vec<String> = workbook.sheets.iter().map(|(n, _)| n.clone()).collect();
        let part = match sheet.filter(|s| !s.is_empty()) {
            Some(wanted) => workbook
                .sheets
                .iter()
                .find(|(name, _)| name.to_lowercase() == wanted.to_lowercase())
                .map(|(_, part)| part.clone())
                .ok_or_else(|| {
                    ImportError::Invalid(format!(
                        "the workbook has no sheet named {wanted}: it has {}",
                        names.join(", ")
                    ))
                })?,
            None => workbook.sheets[0].1.clone(),
        };
        let xml = workbook.package.xml(&part, MAX_SHEET_PART)?;
        Ok(Self {
            xml,
            date1904: workbook.date1904,
            formats: std::mem::take(&mut workbook.formats),
            shared: std::mem::take(&mut workbook.shared),
            sheets: names,
            in_data: false,
            ended: false,
            rows: 0,
        })
    }

    /// The next row that has a `<row>` element: its cells by column, `None` where there is
    /// none. Rows missing from the sheet are not returned (as blank lines of a CSV).
    pub(crate) fn next_row(&mut self) -> Result<Option<Vec<Option<String>>>, ImportError> {
        if self.ended {
            return Ok(None);
        }
        let mut row: Option<Vec<Option<String>>> = None;
        let mut size = 0usize;
        let mut cell: Option<Cell> = None;
        let mut into = TextOf::Nowhere;
        let mut phonetic = false;
        let mut next_column = 0usize;
        loop {
            match self.xml.next()? {
                Item::Start {
                    name,
                    attributes,
                    empty,
                } => match name.as_str() {
                    "sheetData" if !empty => self.in_data = true,
                    "row" if self.in_data => {
                        self.rows += 1;
                        if self.rows > MAX_SHEET_ROWS {
                            return Err(self.xml.error(format!(
                                "more than the {MAX_SHEET_ROWS} rows a sheet holds"
                            )));
                        }
                        next_column = 0;
                        if empty {
                            return Ok(Some(Vec::new()));
                        }
                        row = Some(Vec::new());
                    }
                    "c" if row.is_some() => {
                        let column = match attribute(&attributes, "r") {
                            Some(reference) => column_of(reference).ok_or_else(|| {
                                self.xml
                                    .error(format!("{reference} is not a cell reference"))
                            })?,
                            None => next_column,
                        };
                        if column >= MAX_COLUMNS {
                            return Err(ImportError::Source(format!(
                                "a cell in column {}: at most {MAX_COLUMNS} columns can be imported",
                                column + 1
                            )));
                        }
                        next_column = column + 1;
                        let new = Cell {
                            column,
                            kind: attribute(&attributes, "t").map(str::to_string),
                            style: attribute(&attributes, "s")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0),
                            ..Cell::default()
                        };
                        if empty {
                            // A formatted cell with nothing in it.
                            continue;
                        }
                        cell = Some(new);
                    }
                    // A `<v>` or `<t>` that is there holds a value, even an empty one.
                    "v" if !empty => {
                        if let Some(cell) = cell.as_mut() {
                            cell.value.get_or_insert_with(String::new);
                            into = TextOf::Value;
                        }
                    }
                    "f" if cell.is_some() && !empty => into = TextOf::Formula,
                    "rPh" if cell.is_some() && !empty => phonetic = true,
                    "t" if !phonetic => {
                        if let Some(cell) = cell.as_mut() {
                            cell.inline.get_or_insert_with(String::new);
                            if !empty {
                                into = TextOf::Inline;
                            }
                        }
                    }
                    _ => {}
                },
                Item::Text(text) => {
                    let Some(cell) = cell.as_mut() else {
                        continue;
                    };
                    let target = match into {
                        TextOf::Nowhere => continue,
                        TextOf::Value => &mut cell.value,
                        TextOf::Formula => &mut cell.formula,
                        TextOf::Inline => &mut cell.inline,
                    };
                    size += text.len();
                    if size > MAX_RECORD_BYTES {
                        return Err(ImportError::Source(format!(
                            "row {}: more than {} MiB of values",
                            self.rows,
                            MAX_RECORD_BYTES / (1024 * 1024)
                        )));
                    }
                    target.get_or_insert_with(String::new).push_str(&text);
                }
                Item::End(name) => match name.as_str() {
                    "v" | "f" | "t" => into = TextOf::Nowhere,
                    "rPh" => phonetic = false,
                    "c" => {
                        if let Some(done) = cell.take() {
                            let column = done.column;
                            let value = self.value(done)?;
                            if let Some(row) = row.as_mut() {
                                if row.len() <= column {
                                    row.resize(column + 1, None);
                                }
                                row[column] = value;
                            }
                        }
                    }
                    "row" => {
                        if let Some(done) = row.take() {
                            return Ok(Some(done));
                        }
                    }
                    "sheetData" => {
                        self.in_data = false;
                        self.ended = true;
                        return Ok(None);
                    }
                    _ => {}
                },
                Item::Eof => {
                    self.ended = true;
                    return Ok(row);
                }
            }
        }
    }

    /// A cell's text: its saved value as its type and format say, else its formula's text.
    fn value(&self, cell: Cell) -> Result<Option<String>, ImportError> {
        let Cell {
            kind,
            style,
            value,
            formula,
            inline,
            ..
        } = cell;
        let text = match kind.as_deref() {
            Some("s") => match value {
                Some(index) => {
                    let index: usize = index
                        .trim()
                        .parse()
                        .map_err(|_| self.xml.error(format!("{index} is not a string's index")))?;
                    Some(self.shared.get(index).cloned().ok_or_else(|| {
                        self.xml
                            .error(format!("shared string {index} is not there"))
                    })?)
                }
                None => None,
            },
            Some("inlineStr") => inline.or(value).map(unescape),
            Some("str") => value.map(unescape),
            Some("b") => value.map(|v| match v.trim() {
                "1" | "true" | "TRUE" => "true".to_string(),
                "0" | "false" | "FALSE" => "false".to_string(),
                other => other.to_string(),
            }),
            // An error (`#DIV/0!`) and an ISO date (`t="d"`) are their text.
            Some("e" | "d") => value,
            None | Some("n") => value.map(|v| {
                let shown = self.formats.get(style).copied().unwrap_or(Shown::Number);
                if shown == Shown::Number {
                    return v;
                }
                match v.trim().parse::<f64>() {
                    Ok(serial) => serial_text(serial, shown, self.date1904).unwrap_or(v),
                    Err(_) => v,
                }
            }),
            Some(_) => value,
        };
        Ok(match (text, formula) {
            (Some(text), _) => Some(text),
            (None, Some(formula)) if !formula.trim().is_empty() => Some(format!("={formula}")),
            (None, _) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::sync::Arc;

    use arrow::array::{
        ArrayRef, BinaryArray, BooleanArray, Date32Array, Float64Array, Int64Array, RecordBatch,
        StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
    };
    use spelaion_driver_api::{ColumnMeta, LogicalType, ResultSchema};
    use zip::write::SimpleFileOptions;

    use crate::export_xlsx::XlsxBook;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spelaion-xlsx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    fn column(name: &str, logical: LogicalType, native: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            logical,
            native_type: native.into(),
            native_id: None,
            origin: None,
        }
    }

    fn all_rows(path: &Path, sheet: Option<&str>) -> Result<Vec<Vec<Option<String>>>, ImportError> {
        let mut rows = SheetRows::open(path, sheet)?;
        let mut out = Vec::new();
        while let Some(row) = rows.next_row()? {
            out.push(row);
        }
        Ok(out)
    }

    fn some(values: &[&str]) -> Vec<Option<String>> {
        values.iter().map(|v| Some((*v).to_string())).collect()
    }

    /// A workbook of the given parts (name, XML), deflated, as Excel writes them.
    fn workbook(path: &Path, parts: &[(&str, String)]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, xml) in parts {
            zip.start_file(*name, options).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    const ROOT: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
    const BOOK: &str = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    const BOOK_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>"#;

    fn sheet(rows: &str) -> String {
        format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
        )
    }

    fn one_sheet(path: &Path, rows: &str, shared: &str) {
        workbook(
            path,
            &[
                ("_rels/.rels", ROOT.into()),
                ("xl/workbook.xml", BOOK.into()),
                ("xl/_rels/workbook.xml.rels", BOOK_RELS.into()),
                ("xl/worksheets/sheet1.xml", sheet(rows)),
                (
                    "xl/sharedStrings.xml",
                    format!(
                        r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{shared}</sst>"#
                    ),
                ),
            ],
        );
    }

    /// Sets the size `name` declares, in its local header and in the central directory.
    fn declare_size(path: &Path, name: &str, size: u32) {
        let mut bytes = std::fs::read(path).unwrap();
        let mut i = 0;
        while i + 46 < bytes.len() {
            let signature = u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
            let (name_at, size_at) = match signature {
                0x0403_4b50 => (30, 22),
                0x0201_4b50 => (46, 24),
                _ => {
                    i += 1;
                    continue;
                }
            };
            let length_at = if signature == 0x0403_4b50 { 26 } else { 28 };
            let length =
                u16::from_le_bytes(bytes[i + length_at..i + length_at + 2].try_into().unwrap())
                    as usize;
            if &bytes[i + name_at..i + name_at + length] == name.as_bytes() {
                bytes[i + size_at..i + size_at + 4].copy_from_slice(&size.to_le_bytes());
            }
            i += 4;
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn a_workbook_written_by_the_export_reads_back() {
        let schema = Arc::new(ResultSchema::new(vec![
            column("id", LogicalType::Int, "integer"),
            column("name", LogicalType::Text, "text"),
            column("amount", LogicalType::Decimal, "numeric(10,2)"),
            column("ratio", LogicalType::Float, "double"),
            column("flag", LogicalType::Bool, "boolean"),
            column("day", LogicalType::Date, "date"),
            column("at", LogicalType::Timestamp, "timestamp"),
            column("clock", LogicalType::Time, "time"),
            column("raw", LogicalType::Bytes, "bytea"),
            column("big", LogicalType::Int, "bigint"),
        ]));
        let batch = RecordBatch::try_new(
            schema.arrow.clone(),
            vec![
                Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3)])) as ArrayRef,
                Arc::new(StringArray::from(vec![
                    Some("O'Brien \\ <b>&amp; _x0041_ \u{1}"),
                    Some("line one\r\nline two: ñ €"),
                    None,
                ])),
                Arc::new(StringArray::from(vec![
                    Some("12.50"),
                    Some("-0.25"),
                    Some("123456789012345678.5"),
                ])),
                Arc::new(Float64Array::from(vec![
                    Some(1.5),
                    Some(-22_500_000_000.25),
                    None,
                ])),
                Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
                // 2024-03-04, 1999-12-31 and 1800-01-01 (before Excel's calendar).
                Arc::new(Date32Array::from(vec![
                    Some(19_786),
                    Some(10_956),
                    Some(-62_091),
                ])),
                Arc::new(TimestampMicrosecondArray::from(vec![
                    Some(1_709_546_400_500_000),
                    Some(946_684_799_000_000),
                    None,
                ])),
                Arc::new(Time64MicrosecondArray::from(vec![
                    Some(36_000_000_000),
                    Some(0),
                    None,
                ])),
                Arc::new(BinaryArray::from(vec![
                    Some(&[0xde, 0xad, 0x00][..]),
                    None,
                    None,
                ])),
                Arc::new(Int64Array::from(vec![Some(i64::MAX), Some(-7), None])),
            ],
        )
        .unwrap();
        let path = scratch("round-trip.xlsx");
        let fields: Vec<_> = schema
            .arrow
            .fields()
            .iter()
            .map(|f| f.as_ref().clone())
            .collect();
        // Sheets of a header and two rows: the third row goes on in a second sheet.
        let mut book = XlsxBook::create(&path, None).unwrap().with_sheet_rows(3);
        book.begin_sheet("orders", &fields, (0..fields.len()).collect())
            .unwrap();
        book.write(&batch).unwrap();
        let summary = book.finish().unwrap();
        assert_eq!(summary.rows, 3);
        assert_eq!(summary.bytes, std::fs::metadata(&path).unwrap().len());

        let first = SheetRows::open(&path, None).unwrap();
        assert_eq!(first.sheets, vec!["orders", "orders (2)"]);
        let rows = all_rows(&path, None).unwrap();
        let names: Vec<_> = fields.iter().map(|f| Some(f.name().clone())).collect();
        assert_eq!(rows[0], names);
        assert_eq!(
            rows[1],
            vec![
                Some("1".into()),
                Some("O'Brien \\ <b>&amp; _x0041_ \u{1}".into()),
                Some("12.5".into()),
                Some("1.5".into()),
                Some("true".into()),
                Some("2024-03-04".into()),
                Some("2024-03-04 10:00:00.5".into()),
                Some("10:00:00".into()),
                Some("\\xdead00".into()),
                Some("9223372036854775807".into()),
            ]
        );
        assert_eq!(rows[2][1].as_deref(), Some("line one\r\nline two: ñ €"));
        assert_eq!(rows[2][2].as_deref(), Some("-0.25"));
        assert_eq!(rows[2][3].as_deref(), Some("-22500000000.25"));
        assert_eq!(rows[2][4].as_deref(), Some("false"));
        assert_eq!(rows[2][6].as_deref(), Some("1999-12-31 23:59:59"));
        assert_eq!(rows[2][7].as_deref(), Some("00:00:00"));
        assert_eq!(rows.len(), 3);
        let second = all_rows(&path, Some("ORDERS (2)")).unwrap();
        assert_eq!(second[0], names);
        // A row ends at its last cell; the import fills the rest with NULLs.
        let mut padded = second[1].clone();
        padded.resize(10, None);
        assert_eq!(
            padded,
            vec![
                Some("3".into()),
                None,
                // Not a double digit for digit: text, never rounded.
                Some("123456789012345678.5".into()),
                None,
                None,
                Some("1800-01-01".into()),
                None,
                None,
                None,
                None,
            ]
        );
        let missing = all_rows(&path, Some("nope")).unwrap_err().to_string();
        assert!(missing.contains("orders (2)"), "{missing}");
    }

    #[test]
    fn formulas_give_their_saved_value_or_their_text() {
        let path = scratch("formulas.xlsx");
        one_sheet(
            &path,
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><r><t>to</t></r><r><t>tal</t></r><rPh><t>ignored</t></rPh></is></c></row>
               <row r="2"><c r="A2"><f>1+1</f><v>2</v></c><c r="B2"><f>SUM(A1:A3)</f></c><c r="C2" t="e"><f>1/0</f><v>#DIV/0!</v></c><c r="D2" t="str"><f>"a"&amp;"b"</f><v>ab</v></c></row>
               <row r="5"><c r="C5" t="b"><v>1</v></c></row>"#,
            r#"<si><t>name</t></si>"#,
        );
        let rows = all_rows(&path, None).unwrap();
        assert_eq!(rows[0], some(&["name", "total"]));
        assert_eq!(rows[1], some(&["2", "=SUM(A1:A3)", "#DIV/0!", "ab"]));
        // Missing rows are not rows; missing cells are NULL.
        assert_eq!(rows[2], vec![None, None, Some("true".into())]);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn a_huge_declared_size_is_refused_before_anything_is_unpacked() {
        let path = scratch("declared.xlsx");
        one_sheet(
            &path,
            r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#,
            "<si><t>a</t></si>",
        );
        declare_size(&path, "xl/sharedStrings.xml", 0xFFFF_FFF0);
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("sharedStrings.xml unpacks to"), "{error}");

        // A size smaller than what it unpacks to is caught while unpacking.
        let path = scratch("lies.xlsx");
        let rows: String = (1..=2000)
            .map(|r| format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>"#))
            .collect();
        one_sheet(&path, &rows, "");
        declare_size(&path, "xl/worksheets/sheet1.xml", 4096);
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("more than it declares"), "{error}");
    }

    #[test]
    fn a_zip_bomb_is_refused_by_its_ratio() {
        // 64 MiB of one byte deflates to about 64 KiB: within deflate's ratio. Declaring it
        // as 1 GiB from those bytes is not.
        let path = scratch("bomb.xlsx");
        let filler = " ".repeat(8 * 1024 * 1024);
        one_sheet(
            &path,
            &format!("<row r=\"1\"><c r=\"A1\"><v>1</v></c></row>{filler}"),
            "",
        );
        declare_size(&path, "xl/worksheets/sheet1.xml", 0xC000_0000);
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("more than deflate can unpack"), "{error}");
    }

    #[test]
    fn deep_nesting_long_items_and_doctypes_fail_cleanly() {
        let path = scratch("deep.xlsx");
        let open = "<x>".repeat(100);
        let close = "</x>".repeat(100);
        one_sheet(
            &path,
            &format!("<row r=\"1\"><c r=\"A1\">{open}{close}</c></row>"),
            "",
        );
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("nested more than 32 deep"), "{error}");

        let path = scratch("long.xlsx");
        let long = "a".repeat(2 * MAX_XML_ITEM as usize);
        one_sheet(
            &path,
            &format!("<row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>{long}</t></is></c></row>"),
            "",
        );
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("longer than"), "{error}");

        let path = scratch("doctype.xlsx");
        workbook(
            &path,
            &[
                ("_rels/.rels", ROOT.into()),
                (
                    "xl/workbook.xml",
                    format!("<!DOCTYPE w [<!ENTITY a \"aaaaaaaaaa\">]>{BOOK}"),
                ),
                ("xl/_rels/workbook.xml.rels", BOOK_RELS.into()),
                ("xl/worksheets/sheet1.xml", sheet("")),
            ],
        );
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("DOCTYPE"), "{error}");
    }

    #[test]
    fn shared_strings_are_bounded() {
        let path = scratch("shared.xlsx");
        let strings = "<si><t>abcdef</t></si>".repeat(50);
        one_sheet(&path, "", &strings);
        let mut package = Package::open(&path).unwrap();
        let error = read_shared(&mut package, "xl/sharedStrings.xml", 10, MAX_SHARED_TEXT)
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 10 shared strings"), "{error}");
        let error = read_shared(&mut package, "xl/sharedStrings.xml", 1_000, 100)
            .unwrap_err()
            .to_string();
        assert!(error.contains("more than 100 bytes"), "{error}");
        let all = read_shared(&mut package, "xl/sharedStrings.xml", 1_000, 1_000).unwrap();
        assert_eq!(all.len(), 50);
    }

    #[test]
    fn columns_rows_entries_and_paths_are_bounded() {
        let path = scratch("wide.xlsx");
        one_sheet(&path, r#"<row r="1"><c r="ZZZ1"><v>1</v></c></row>"#, "");
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("columns can be imported"), "{error}");

        let path = scratch("rows.xlsx");
        let rows = "<row/>".repeat(MAX_SHEET_ROWS as usize + 1);
        one_sheet(&path, &rows, "");
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("rows a sheet holds"), "{error}");

        let path = scratch("entries.xlsx");
        let mut parts: Vec<(String, String)> = (0..=MAX_ENTRIES)
            .map(|n| (format!("junk/{n}"), String::new()))
            .collect();
        parts.push(("_rels/.rels".into(), ROOT.into()));
        let parts: Vec<(&str, String)> =
            parts.iter().map(|(n, x)| (n.as_str(), x.clone())).collect();
        workbook(&path, &parts);
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("entries"), "{error}");

        // A relationship that climbs out of the package is not followed.
        let path = scratch("climb.xlsx");
        workbook(
            &path,
            &[
                ("_rels/.rels", ROOT.into()),
                ("xl/workbook.xml", BOOK.into()),
                (
                    "xl/_rels/workbook.xml.rels",
                    BOOK_RELS.replace("worksheets/sheet1.xml", "../../../../sheet1.xml"),
                ),
                ("xl/worksheets/sheet1.xml", sheet("")),
            ],
        );
        let error = all_rows(&path, None).unwrap_err().to_string();
        assert!(error.contains("no worksheet"), "{error}");
    }

    #[test]
    fn number_format_codes_say_what_they_show() {
        assert_eq!(shown_by_code("yyyy-mm-dd"), Shown::Date);
        assert_eq!(shown_by_code("yyyy\\-mm\\-dd\\ hh:mm:ss"), Shown::Timestamp);
        assert_eq!(shown_by_code("hh:mm:ss"), Shown::Time);
        assert_eq!(shown_by_code("mm:ss"), Shown::Time);
        assert_eq!(shown_by_code("[h]:mm"), Shown::Time);
        assert_eq!(shown_by_code("d/m/yy h:mm"), Shown::Timestamp);
        assert_eq!(shown_by_code("0.00"), Shown::Number);
        assert_eq!(shown_by_code("#,##0 \"days\""), Shown::Number);
        assert_eq!(shown_by_code("[Red]0.00"), Shown::Number);
        assert_eq!(shown_by_code("0.00_);(0.00)"), Shown::Number);
        assert_eq!(shown_by_builtin(14), Shown::Date);
        assert_eq!(shown_by_builtin(22), Shown::Timestamp);
        assert_eq!(shown_by_builtin(20), Shown::Time);
        assert_eq!(shown_by_builtin(2), Shown::Number);
    }

    #[test]
    fn serials_become_iso_dates() {
        assert_eq!(
            serial_text(45_355.0, Shown::Date, false).as_deref(),
            Some("2024-03-04")
        );
        assert_eq!(
            serial_text(45_355.5, Shown::Timestamp, false).as_deref(),
            Some("2024-03-04 12:00:00")
        );
        assert_eq!(
            serial_text(45_355.0 + 0.5 / 86_400.0, Shown::Timestamp, false).as_deref(),
            Some("2024-03-04 00:00:00.5")
        );
        assert_eq!(
            serial_text(0.75, Shown::Time, false).as_deref(),
            Some("18:00:00")
        );
        assert_eq!(
            serial_text(1.0, Shown::Date, false).as_deref(),
            Some("1900-01-01")
        );
        assert_eq!(
            serial_text(59.0, Shown::Date, false).as_deref(),
            Some("1900-02-28")
        );
        assert_eq!(serial_text(60.0, Shown::Date, false), None);
        assert_eq!(
            serial_text(61.0, Shown::Date, false).as_deref(),
            Some("1900-03-01")
        );
        assert_eq!(
            serial_text(0.0, Shown::Date, true).as_deref(),
            Some("1904-01-01")
        );
        assert_eq!(serial_text(-1.0, Shown::Date, false), None);
        assert_eq!(serial_text(3e6, Shown::Date, false), None);
        assert_eq!(serial_text(f64::NAN, Shown::Date, false), None);
        // A value rounded up to the next day's midnight is that day.
        assert_eq!(
            serial_text(61.0 - 1e-13, Shown::Timestamp, false).as_deref(),
            Some("1900-03-01 00:00:00")
        );
    }

    #[test]
    fn references_and_paths_are_read_strictly() {
        assert_eq!(column_of("A1"), Some(0));
        assert_eq!(column_of("Z9"), Some(25));
        assert_eq!(column_of("AA10"), Some(26));
        assert_eq!(column_of("XFD1048576"), Some(16_383));
        assert_eq!(column_of("1A"), None);
        assert_eq!(column_of("ABCD1"), None);
        assert_eq!(
            resolve("xl/workbook.xml", "worksheets/sheet1.xml").as_deref(),
            Some("xl/worksheets/sheet1.xml")
        );
        assert_eq!(
            resolve("xl/workbook.xml", "/xl/worksheets/s.xml").as_deref(),
            Some("xl/worksheets/s.xml")
        );
        assert_eq!(
            resolve("xl/workbook.xml", "../docProps/app.xml").as_deref(),
            Some("docProps/app.xml")
        );
        assert_eq!(resolve("xl/workbook.xml", "../../../etc/passwd"), None);
        assert_eq!(
            resolve("", "xl/workbook.xml").as_deref(),
            Some("xl/workbook.xml")
        );
        assert_eq!(rels_of("xl/workbook.xml"), "xl/_rels/workbook.xml.rels");
        assert_eq!(rels_of(""), "_rels/.rels");
    }

    #[test]
    fn excel_escapes_are_undone() {
        assert_eq!(unescape("a_x000D_\nb".into()), "a\r\nb");
        assert_eq!(unescape("_x005F_x0041_".into()), "_x0041_");
        assert_eq!(unescape("_x0041_".into()), "A");
        assert_eq!(unescape("plain_text".into()), "plain_text");
        assert_eq!(unescape("_xZZZZ_ é".into()), "_xZZZZ_ é");
    }
}
