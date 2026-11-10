//! The cell viewer's value: read from the stored result when the viewer opens, never kept in
//! the UI beyond the cell shown (ADR-010), with a size limit and "load anyway" for bigger ones.
//! Images are recognised by their first bytes (PNG, JPEG, GIF, WebP), never by the column's
//! name, and only those four: nothing active (SVG, HTML) is ever handed to the UI to render.
//! XML is pretty-printed here as text. A value can be saved to a file byte for byte, and a
//! file loaded into a cell becomes an ordinary edit of `edit.rs` (ADR-014): one UPDATE by
//! primary key. A geometry (ADR-045) is read into GeoJSON here too, for the map view.

use std::io::Read;
use std::path::Path;

use base64::Engine as _;
use quick_xml::events::{BytesText, Event};
use quick_xml::{Reader, Writer};
use spelaion_driver_api::{LogicalType, Value};

use crate::edit::{CellInput, RowChange};
use crate::geometry::{GeoValue, read_geometry};
use crate::results::StoredResult;
use crate::sql_file::replace_atomically;

/// Values up to this size (bytes of the value, or of its text in UTF-8) are shown when the
/// viewer opens; bigger ones wait for "load anyway".
pub const VIEW_LIMIT: usize = 2 * 1024 * 1024;
/// The biggest value the viewer shows even when asked: past it, it can only be saved to a file.
pub const VIEW_MAX: usize = 64 * 1024 * 1024;
/// The biggest file loaded into a cell. The statement carries it as a literal (twice its size
/// in hex), and servers refuse packets past their own limit (16 MB by default on MariaDB).
pub const LOAD_FILE_MAX: u64 = 16 * 1024 * 1024;
/// Longest statement shown whole when a loaded file needs confirming; longer ones show their
/// start and their end (the `WHERE` with the key).
pub const DISPLAY_STATEMENT_CHARS: usize = 2_000;

/// The image formats the viewer previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    /// The format whose signature `bytes` start with, if any.
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }

    /// A `data:` URL of `bytes` with this format's type: the only way an image reaches the UI.
    pub fn data_url(self, bytes: &[u8]) -> String {
        format!(
            "data:{};base64,{}",
            self.mime(),
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }
}

/// What the viewer shows of a cell.
#[derive(Debug, Clone, PartialEq)]
pub enum CellContent {
    Null,
    /// Any value that is not bytes, as the grid writes it; `xml` is its pretty-printed form
    /// when it is well-formed XML.
    Text {
        text: String,
        xml: Option<String>,
    },
    /// Bytes, with their text (`\x…`, as the grid writes them) and, when they are an
    /// image, a `data:` URL of it with the detected type.
    Bytes {
        text: String,
        image_url: Option<String>,
    },
    /// Bigger than [`VIEW_LIMIT`]: shown only when asked.
    Withheld,
    /// Bigger than [`VIEW_MAX`]: never shown, only saved to a file.
    TooBig,
}

/// A cell for the viewer.
#[derive(Debug, Clone, PartialEq)]
pub struct CellView {
    /// Bytes of the value, or of its text in UTF-8 (0 for NULL).
    pub size: usize,
    pub binary: bool,
    /// Recognised from the first bytes, even when the value is withheld.
    pub image: Option<ImageFormat>,
    /// The extension a file of this value would have (`png`, `bin`, `xml`, `json`, `txt`).
    pub extension: &'static str,
    pub content: CellContent,
    /// The value read as a geometry, when it is one and it is shown (not withheld).
    pub geometry: Option<GeoValue>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValueError {
    #[error("there is no row {0} in this result")]
    NoSuchRow(usize),
    #[error("there is no column {0} in this result")]
    NoSuchColumn(usize),
    #[error("the value is NULL: there is nothing to save")]
    Null,
    #[error("{path}: {message}")]
    File { path: String, message: String },
    #[error("{path} is {size} bytes, more than the {max} bytes a value can be loaded from")]
    FileTooBig { path: String, size: u64, max: u64 },
    #[error("{path} is not UTF-8 text: only binary columns take any file")]
    NotText { path: String },
}

/// The cell at `row` (a position of the current view) and `column` of `result`, for the
/// viewer: whole up to [`VIEW_LIMIT`], or up to [`VIEW_MAX`] with `load_anyway`.
pub fn view_cell(
    result: &StoredResult,
    row: usize,
    column: usize,
    load_anyway: bool,
) -> Result<CellView, ValueError> {
    let meta = result
        .schema()
        .columns
        .get(column)
        .ok_or(ValueError::NoSuchColumn(column))?;
    let value = result
        .value_at(row, column)
        .ok_or(ValueError::NoSuchRow(row))?;
    let xml_type = meta.native_type.to_ascii_lowercase().contains("xml");
    let limit = if load_anyway { VIEW_MAX } else { VIEW_LIMIT };
    let fits = |size: usize| {
        if size > VIEW_MAX {
            None
        } else if size > limit {
            Some(false)
        } else {
            Some(true)
        }
    };
    Ok(match value {
        Value::Null => CellView {
            size: 0,
            binary: meta.logical == LogicalType::Bytes,
            image: None,
            extension: text_extension(meta.logical, xml_type),
            content: CellContent::Null,
            geometry: None,
        },
        Value::Bytes(bytes) => {
            let image = ImageFormat::detect(&bytes);
            let size = bytes.len();
            let mut geometry = None;
            let content = match fits(size) {
                None => CellContent::TooBig,
                Some(false) => CellContent::Withheld,
                Some(true) => {
                    let value = Value::Bytes(bytes);
                    geometry = image
                        .is_none()
                        .then(|| read_geometry(&value, &meta.native_type))
                        .flatten();
                    let Value::Bytes(bytes) = &value else {
                        unreachable!("built just above")
                    };
                    CellContent::Bytes {
                        image_url: image.map(|format| format.data_url(bytes)),
                        text: value.to_string(),
                    }
                }
            };
            CellView {
                size,
                binary: true,
                image,
                extension: image.map_or("bin", ImageFormat::extension),
                content,
                geometry,
            }
        }
        other => {
            let text = other.to_string();
            let size = text.len();
            let mut geometry = None;
            let content = match fits(size) {
                None => CellContent::TooBig,
                Some(false) => CellContent::Withheld,
                Some(true) => {
                    geometry = read_geometry(&other, &meta.native_type);
                    let xml = (xml_type || text.trim_start().starts_with('<'))
                        .then(|| pretty_xml(&text))
                        .flatten();
                    CellContent::Text { text, xml }
                }
            };
            CellView {
                size,
                binary: false,
                image: None,
                extension: text_extension(meta.logical, xml_type),
                content,
                geometry,
            }
        }
    })
}

fn text_extension(logical: LogicalType, xml_type: bool) -> &'static str {
    match logical {
        LogicalType::Bytes => "bin",
        LogicalType::Json => "json",
        _ if xml_type => "xml",
        _ => "txt",
    }
}

/// Writes the cell's value to `path`: bytes exactly as stored, any other value as its text in
/// UTF-8. Returns the bytes written. The file is replaced whole (a temporary file renamed over
/// it), so a failure never leaves half a value behind.
pub fn save_cell(
    result: &StoredResult,
    row: usize,
    column: usize,
    path: &Path,
) -> Result<u64, ValueError> {
    if column >= result.schema().columns.len() {
        return Err(ValueError::NoSuchColumn(column));
    }
    let bytes = match result
        .value_at(row, column)
        .ok_or(ValueError::NoSuchRow(row))?
    {
        Value::Null => return Err(ValueError::Null),
        Value::Bytes(bytes) => bytes,
        other => other.to_string().into_bytes(),
    };
    replace_atomically(path, &bytes).map_err(|e| ValueError::File {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(bytes.len() as u64)
}

/// The value of a file for a column of this type: its bytes for a binary column, its UTF-8
/// text (without a byte order mark) for any other, checked like a typed value when the
/// statement is written. Refused past [`LOAD_FILE_MAX`].
pub fn read_value_file(path: &Path, logical: LogicalType) -> Result<CellInput, ValueError> {
    let shown = || path.display().to_string();
    let file_error = |e: std::io::Error| ValueError::File {
        path: shown(),
        message: e.to_string(),
    };
    let file = std::fs::File::open(path).map_err(file_error)?;
    let meta = file.metadata().map_err(file_error)?;
    if !meta.is_file() {
        return Err(ValueError::File {
            path: shown(),
            message: "not a file".into(),
        });
    }
    let too_big = |size| ValueError::FileTooBig {
        path: shown(),
        size,
        max: LOAD_FILE_MAX,
    };
    if meta.len() > LOAD_FILE_MAX {
        return Err(too_big(meta.len()));
    }
    // Bounded even if the file grows while it is read.
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(LOAD_FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(file_error)?;
    if bytes.len() as u64 > LOAD_FILE_MAX {
        return Err(too_big(bytes.len() as u64));
    }
    match logical {
        LogicalType::Bytes => Ok(CellInput::Bytes(bytes)),
        LogicalType::Int
        | LogicalType::Float
        | LogicalType::Decimal
        | LogicalType::Bool
        | LogicalType::Text
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => {
            let body = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
            String::from_utf8(body.to_vec())
                .map(CellInput::Text)
                .map_err(|_| ValueError::NotText { path: shown() })
        }
    }
}

/// The change that loads the file at `path` into the cell at `row` and `column` of a table
/// data page: one UPDATE of that row by its primary key once `edit_statements` writes it.
pub fn file_change(
    result: &StoredResult,
    row: usize,
    column: usize,
    path: &Path,
) -> Result<RowChange, ValueError> {
    let meta = result
        .schema()
        .columns
        .get(column)
        .ok_or(ValueError::NoSuchColumn(column))?;
    if row >= result.rows() {
        return Err(ValueError::NoSuchRow(row));
    }
    let input = read_value_file(path, meta.logical)?;
    Ok(RowChange::Update {
        row,
        values: vec![(column, input)],
    })
}

/// A statement as the confirmation dialog shows it: whole up to
/// [`DISPLAY_STATEMENT_CHARS`], otherwise its start and its end around the number of
/// characters left out, so a file's literal does not flood the dialog and the key stays visible.
pub fn statement_for_display(sql: &str) -> String {
    let chars = sql.chars().count();
    if chars <= DISPLAY_STATEMENT_CHARS {
        return sql.to_string();
    }
    let head = DISPLAY_STATEMENT_CHARS * 3 / 4;
    let tail = DISPLAY_STATEMENT_CHARS / 4;
    let start: String = sql.chars().take(head).collect();
    let end: String = sql.chars().skip(chars - tail).collect();
    format!(
        "{start}… ({} characters not shown) …{end}",
        chars - head - tail
    )
}

/// `text` indented two spaces per level when it is one well-formed XML document; `None`
/// otherwise. Text inside elements is kept as it is (mixed content stays on its line) and
/// entities are never expanded: the result is shown as text, never parsed as markup again.
pub fn pretty_xml(text: &str) -> Option<String> {
    if !text.trim_start().starts_with('<') {
        return None;
    }
    let mut reader = Reader::from_str(text);
    reader.config_mut().check_end_names = true;
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    let mut depth = 0usize;
    let mut roots = 0usize;
    loop {
        let event = reader.read_event().ok()?;
        let event = match event {
            Event::Eof => break,
            Event::Start(start) => {
                if depth == 0 {
                    roots += 1;
                }
                depth += 1;
                Event::Start(start)
            }
            Event::End(end) => {
                depth = depth.checked_sub(1)?;
                Event::End(end)
            }
            Event::Empty(empty) => {
                if depth == 0 {
                    roots += 1;
                }
                Event::Empty(empty)
            }
            Event::Text(text) => {
                let raw: &str = &text;
                if raw.trim().is_empty() {
                    continue;
                }
                if depth == 0 {
                    // Text outside the root element: not a document.
                    return None;
                }
                Event::Text(text)
            }
            // Written back as it came, inline: it is part of the text around it.
            Event::GeneralRef(reference) => {
                if depth == 0 {
                    return None;
                }
                let name: &str = &reference;
                Event::Text(BytesText::from_escaped(format!("&{name};")))
            }
            event @ (Event::CData(_)
            | Event::Comment(_)
            | Event::Decl(_)
            | Event::PI(_)
            | Event::DocType(_)) => event,
        };
        writer.write_event(event).ok()?;
    }
    if depth != 0 || roots != 1 {
        return None;
    }
    String::from_utf8(writer.into_inner()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_recognised_by_their_first_bytes_only() {
        assert_eq!(
            ImageFormat::detect(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            ImageFormat::detect(&[0xff, 0xd8, 0xff, 0xe0, 0, 0x10]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(ImageFormat::detect(b"GIF89a\x01\0"), Some(ImageFormat::Gif));
        assert_eq!(ImageFormat::detect(b"GIF87a"), Some(ImageFormat::Gif));
        assert_eq!(
            ImageFormat::detect(b"RIFF\x24\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        // Not images: SVG (active content), HTML, a RIFF that is a WAV, a cut signature.
        for bytes in [
            &b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>"[..],
            b"<html><body><img src=x onerror=alert(1)></body></html>",
            b"RIFF\x24\0\0\0WAVEfmt ",
            b"RIFF\x24\0\0\0WEB",
            b"\x89PNG\r\n",
            b"GIF8",
            b"",
        ] {
            assert_eq!(ImageFormat::detect(bytes), None, "{bytes:?}");
        }
    }

    #[test]
    fn data_urls_carry_the_detected_type() {
        assert_eq!(
            ImageFormat::Gif.data_url(b"GIF89a"),
            "data:image/gif;base64,R0lGODlh"
        );
        assert_eq!(ImageFormat::Png.extension(), "png");
        assert_eq!(ImageFormat::Jpeg.mime(), "image/jpeg");
    }

    #[test]
    fn xml_is_indented_and_text_is_kept() {
        assert_eq!(
            pretty_xml("<?xml version=\"1.0\"?><a x=\"1\"><b>t &amp; u</b><c/><!-- n --></a>")
                .as_deref(),
            Some(
                "<?xml version=\"1.0\"?>\n<a x=\"1\">\n  <b>t &amp; u</b>\n  <c/>\n  <!-- n -->\n</a>"
            )
        );
        // Mixed content stays on its line; whitespace between elements is replaced.
        assert_eq!(
            pretty_xml("<p>\n   Hello <b>World</b>!</p>").as_deref(),
            Some("<p>\n   Hello <b>World</b>!</p>")
        );
        assert_eq!(
            pretty_xml("<a>&amp;x</a>").as_deref(),
            Some("<a>&amp;x</a>")
        );
        // Not one well-formed document.
        for text in [
            "plain text",
            "<a><b></a>",
            "<a>",
            "<a/><b/>",
            "text <a/>",
            "<a/> text",
            "",
            "{\"a\": 1}",
        ] {
            assert_eq!(pretty_xml(text), None, "{text}");
        }
    }

    #[test]
    fn long_statements_are_shortened_for_display_only() {
        let short = "UPDATE t SET b = X'00' WHERE id = 1";
        assert_eq!(statement_for_display(short), short);
        let long = format!("UPDATE t SET b = X'{}' WHERE id = 1", "ab".repeat(5_000));
        let shown = statement_for_display(&long);
        assert!(shown.starts_with("UPDATE t SET b = X'abab"));
        assert!(shown.ends_with("abab' WHERE id = 1"));
        assert!(shown.contains("characters not shown"));
        assert!(shown.chars().count() < DISPLAY_STATEMENT_CHARS + 60);
    }

    #[test]
    fn files_are_read_by_column_type_and_bounded() {
        let dir = std::env::temp_dir().join(format!("spelaion-value-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("v.bin");
        std::fs::write(&binary, b"\x89PNG\r\n\x1a\n\xff\x00").unwrap();
        assert_eq!(
            read_value_file(&binary, LogicalType::Bytes),
            Ok(CellInput::Bytes(b"\x89PNG\r\n\x1a\n\xff\x00".to_vec()))
        );
        // Not UTF-8: only a binary column takes it.
        assert!(matches!(
            read_value_file(&binary, LogicalType::Text),
            Err(ValueError::NotText { .. })
        ));
        let text = dir.join("v.xml");
        std::fs::write(&text, "\u{feff}<a>ñ</a>").unwrap();
        assert_eq!(
            read_value_file(&text, LogicalType::Native),
            Ok(CellInput::Text("<a>ñ</a>".into()))
        );
        let big = dir.join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(LOAD_FILE_MAX + 1).unwrap();
        drop(file);
        assert!(matches!(
            read_value_file(&big, LogicalType::Bytes),
            Err(ValueError::FileTooBig { .. })
        ));
        assert!(matches!(
            read_value_file(&dir, LogicalType::Bytes),
            Err(ValueError::File { .. })
        ));
        assert!(matches!(
            read_value_file(&dir.join("missing"), LogicalType::Bytes),
            Err(ValueError::File { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
