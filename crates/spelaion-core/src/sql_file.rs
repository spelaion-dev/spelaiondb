//! `.sql` files in editor tabs (ADR-036): read and written by the core, never by the webview.
//!
//! The editor works with `\n` line breaks and Unicode text; this module turns a file's bytes
//! into that and back, keeping the file's encoding and line ending, writes atomically
//! (temporary file + rename) and refuses to overwrite a file that changed on disk since it was
//! last read or saved, unless asked to.
//!
//! Every function here blocks on the file system: async callers run them on a blocking thread.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
pub use spelaion_store::{FileEncoding, LineEnding};

/// Largest file the editor opens.
pub const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// How a file's text is turned into bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFormat {
    pub encoding: FileEncoding,
    pub line_ending: LineEnding,
}

impl FileFormat {
    /// What a new file is written as: UTF-8 without a BOM, the platform's line break.
    pub fn new_file() -> Self {
        Self {
            encoding: FileEncoding::Utf8,
            line_ending: if cfg!(windows) {
                LineEnding::CrLf
            } else {
                LineEnding::Lf
            },
        }
    }
}

/// A file as the editor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlFile {
    /// The text with `\n` line breaks.
    pub text: String,
    pub format: FileFormat,
    /// SHA-256 of the file's bytes, to notice a change on disk.
    pub hash: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FileError {
    #[error("the file does not exist")]
    NotFound,
    #[error("the file is too large to open ({bytes} bytes; at most {MAX_FILE_BYTES})")]
    TooLarge { bytes: u64 },
    #[error("the file is not text")]
    Binary,
    #[error("the file changed on disk since it was opened")]
    ChangedOnDisk,
    #[error("the file was deleted from disk since it was opened")]
    DeletedOnDisk,
    #[error("{encoding} cannot hold the character {character:?}")]
    Unrepresentable {
        character: char,
        encoding: &'static str,
    },
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for FileError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            FileError::NotFound
        } else {
            FileError::Io(e.to_string())
        }
    }
}

/// What a save checks before replacing the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveCheck {
    /// The file must still have these bytes (the hash last read or saved): refused otherwise
    /// with [`FileError::ChangedOnDisk`] or [`FileError::DeletedOnDisk`].
    Unchanged(String),
    /// Write whatever is there: a "save as" whose dialog already asked about replacing, or
    /// the user chose to overwrite after a conflict.
    Overwrite,
}

/// SHA-256 of `bytes`, in lowercase hex.
pub fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0xf), 16).unwrap_or('0'));
    }
    hex
}

/// Reads a `.sql` file for an editor tab.
pub fn read_sql_file(path: &Path) -> Result<SqlFile, FileError> {
    let metadata = fs::metadata(path)?;
    if metadata.is_dir() {
        return Err(FileError::Io("it is a folder".into()));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(FileError::TooLarge {
            bytes: metadata.len(),
        });
    }
    let bytes = fs::read(path)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(FileError::TooLarge {
            bytes: bytes.len() as u64,
        });
    }
    let (text, format) = decode(&bytes)?;
    Ok(SqlFile {
        text,
        format,
        hash: content_hash(&bytes),
    })
}

/// Writes `text` (any line breaks) to `path` in `format`, atomically, after `check`.
/// Returns the hash of what was written.
pub fn write_sql_file(
    path: &Path,
    text: &str,
    format: FileFormat,
    check: &SaveCheck,
) -> Result<String, FileError> {
    let bytes = encode(text, format)?;
    let target = follow_link(path)?;
    if let SaveCheck::Unchanged(expected) = check {
        match fs::read(&target) {
            Ok(current) if content_hash(&current) == *expected => {}
            Ok(_) => return Err(FileError::ChangedOnDisk),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FileError::DeletedOnDisk);
            }
            Err(e) => return Err(e.into()),
        }
    }
    replace_atomically(&target, &bytes)?;
    Ok(content_hash(&bytes))
}

/// The file a symbolic link points to (so saving keeps the link), or `path` itself.
fn follow_link(path: &Path) -> Result<PathBuf, FileError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Ok(fs::canonicalize(path)?),
        _ => Ok(path.to_path_buf()),
    }
}

/// Writes `bytes` to a temporary file next to `target`, flushes it to disk and renames it
/// over `target`: a crash leaves the old file or the new one, never part of one.
fn replace_atomically(target: &Path, bytes: &[u8]) -> Result<(), FileError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = target
        .file_name()
        .ok_or_else(|| FileError::Io("the path has no file name".into()))?;
    let dir = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let temporary = dir.join(format!(
        ".{}.{}-{}.spelaion-tmp",
        name.to_string_lossy(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // The new file keeps the old one's permissions (its executable or group bits). Not on
        // Windows, where the only one is "read-only" and the rename must fail on such a file.
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(target) {
            fs::set_permissions(&temporary, meta.permissions())?;
        }
        fs::rename(&temporary, target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temporary);
        return Err(match e.kind() {
            std::io::ErrorKind::NotFound => {
                FileError::Io(format!("the folder {} does not exist", dir.display()))
            }
            _ => FileError::Io(e.to_string()),
        });
    }
    // The rename itself is durable once the folder is (Unix; Windows has no such call).
    #[cfg(unix)]
    if let Ok(folder) = fs::File::open(&dir) {
        let _ = folder.sync_all();
    }
    Ok(())
}

/// A file's bytes as editor text and the format to write them back in.
pub fn decode(bytes: &[u8]) -> Result<(String, FileFormat), FileError> {
    let (raw, encoding) = if let Some(rest) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        (
            String::from_utf8(rest.to_vec()).map_err(|_| FileError::Binary)?,
            FileEncoding::Utf8Bom,
        )
    } else if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        (
            decode_utf16(rest, u16::from_le_bytes)?,
            FileEncoding::Utf16Le,
        )
    } else if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        (
            decode_utf16(rest, u16::from_be_bytes)?,
            FileEncoding::Utf16Be,
        )
    } else if bytes.contains(&0) {
        // UTF-16 without a byte order mark, or a binary file: not something to edit as SQL.
        return Err(FileError::Binary);
    } else {
        match std::str::from_utf8(bytes) {
            Ok(text) => (text.to_string(), FileEncoding::Utf8),
            Err(_) => (
                bytes.iter().map(|b| windows_1252_char(*b)).collect(),
                FileEncoding::Windows1252,
            ),
        }
    };
    let line_ending = dominant_line_ending(&raw);
    Ok((
        normalize_line_breaks(&raw),
        FileFormat {
            encoding,
            line_ending,
        },
    ))
}

/// Editor text as a file's bytes: every line break (`\r\n`, `\r` or `\n`) becomes the
/// format's, and the text is encoded with a byte order mark where the encoding has one.
pub fn encode(text: &str, format: FileFormat) -> Result<Vec<u8>, FileError> {
    let text = normalize_line_breaks(text);
    let text = match format.line_ending {
        LineEnding::Lf => text,
        other => text.replace('\n', other.chars()),
    };
    Ok(match format.encoding {
        FileEncoding::Utf8 => text.into_bytes(),
        FileEncoding::Utf8Bom => {
            let mut bytes = b"\xEF\xBB\xBF".to_vec();
            bytes.extend_from_slice(text.as_bytes());
            bytes
        }
        FileEncoding::Utf16Le => {
            let mut bytes = vec![0xFF, 0xFE];
            bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
            bytes
        }
        FileEncoding::Utf16Be => {
            let mut bytes = vec![0xFE, 0xFF];
            bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
            bytes
        }
        FileEncoding::Windows1252 => text
            .chars()
            .map(|c| {
                windows_1252_byte(c).ok_or(FileError::Unrepresentable {
                    character: c,
                    encoding: "Windows-1252",
                })
            })
            .collect::<Result<_, _>>()?,
    })
}

pub(crate) fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Result<String, FileError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(FileError::Binary);
    }
    char::decode_utf16(bytes.as_chunks::<2>().0.iter().map(|pair| unit(*pair)))
        .collect::<Result<String, _>>()
        .map_err(|_| FileError::Binary)
}

/// `\r\n` and lone `\r` as `\n`, as the editor splits lines.
fn normalize_line_breaks(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// The most common line break of `text`; `\n` on a tie or without any.
fn dominant_line_ending(text: &str) -> LineEnding {
    let (mut lf, mut crlf, mut cr) = (0usize, 0usize, 0usize);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                crlf += 1;
            }
            '\r' => cr += 1,
            '\n' => lf += 1,
            _ => {}
        }
    }
    if crlf > lf && crlf >= cr {
        LineEnding::CrLf
    } else if cr > lf && cr > crlf {
        LineEnding::Cr
    } else {
        LineEnding::Lf
    }
}

/// Windows-1252's characters for 0x80..=0x9F; the five unassigned bytes are the C1 controls
/// with the same number (as WHATWG decodes them), so every byte round-trips.
const WINDOWS_1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

pub(crate) fn windows_1252_char(byte: u8) -> char {
    match byte {
        0x80..=0x9F => WINDOWS_1252_HIGH[usize::from(byte - 0x80)],
        other => char::from(other),
    }
}

fn windows_1252_byte(c: char) -> Option<u8> {
    let code = u32::from(c);
    if code < 0x80 || (0xA0..=0xFF).contains(&code) {
        return u8::try_from(code).ok();
    }
    WINDOWS_1252_HIGH
        .iter()
        .position(|high| *high == c)
        .and_then(|i| u8::try_from(0x80 + i).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-sql-file-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".spelaion-tmp"))
            .collect()
    }

    const TEXT: &str = "-- Año fiscal: €\nselect 'naïve', '日本' from t;\n\nselect 2;";

    #[test]
    fn every_encoding_and_line_ending_round_trips_byte_for_byte() {
        let dir = folder("round-trip");
        let encodings = [
            FileEncoding::Utf8,
            FileEncoding::Utf8Bom,
            FileEncoding::Utf16Le,
            FileEncoding::Utf16Be,
        ];
        for encoding in encodings {
            for line_ending in [LineEnding::Lf, LineEnding::CrLf, LineEnding::Cr] {
                let format = FileFormat {
                    encoding,
                    line_ending,
                };
                let bytes = encode(TEXT, format).unwrap();
                let path = dir.join(format!(
                    "{}-{}.sql",
                    encoding.as_str(),
                    line_ending.as_str()
                ));
                fs::write(&path, &bytes).unwrap();

                let file = read_sql_file(&path).unwrap();
                assert_eq!(file.text, TEXT, "{format:?}");
                assert_eq!(file.format, format);
                assert_eq!(file.hash, content_hash(&bytes));

                // Saving the text unchanged writes the same bytes, in place.
                let hash = write_sql_file(
                    &path,
                    &file.text,
                    file.format,
                    &SaveCheck::Unchanged(file.hash.clone()),
                )
                .unwrap();
                assert_eq!(fs::read(&path).unwrap(), bytes, "{format:?}");
                assert_eq!(hash, file.hash);
            }
        }
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn the_byte_order_marks_and_line_breaks_are_what_the_file_had() {
        let crlf_bom = b"\xEF\xBB\xBFselect 1;\r\nselect 2;\r\n";
        let (text, format) = decode(crlf_bom).unwrap();
        assert_eq!(text, "select 1;\nselect 2;\n");
        assert_eq!(format.encoding, FileEncoding::Utf8Bom);
        assert_eq!(format.line_ending, LineEnding::CrLf);
        assert_eq!(encode(&text, format).unwrap(), crlf_bom);

        // UTF-16 starts with its mark.
        let utf16 = encode(
            "a\n",
            FileFormat {
                encoding: FileEncoding::Utf16Le,
                line_ending: LineEnding::CrLf,
            },
        )
        .unwrap();
        assert_eq!(utf16, b"\xFF\xFEa\0\r\0\n\0");
        // No line break at all: \n; a tie: \n.
        assert_eq!(decode(b"select 1").unwrap().1.line_ending, LineEnding::Lf);
        assert_eq!(decode(b"a\nb\r\nc").unwrap().1.line_ending, LineEnding::Lf);
        // Mixed: the most common one, and every break is written with it.
        let (text, format) = decode(b"a\r\nb\r\nc\nd").unwrap();
        assert_eq!(format.line_ending, LineEnding::CrLf);
        assert_eq!(encode(&text, format).unwrap(), b"a\r\nb\r\nc\r\nd");
        // Text from the editor with stray \r\n is still written with the file's line break.
        let lf = FileFormat {
            encoding: FileEncoding::Utf8,
            line_ending: LineEnding::Lf,
        };
        assert_eq!(encode("a\r\nb\rc", lf).unwrap(), b"a\nb\nc");
    }

    #[test]
    fn a_file_that_is_not_utf8_reads_and_writes_as_windows_1252() {
        // "Año €" in Windows-1252, with every byte from 0x80 to 0xFF after it.
        let mut bytes = b"select 'A\xF1o \x80';\r\n".to_vec();
        bytes.extend(0x80..=0xFFu8);
        let (text, format) = decode(&bytes).unwrap();
        assert_eq!(format.encoding, FileEncoding::Windows1252);
        assert!(text.starts_with("select 'Año €';\n"));
        assert_eq!(encode(&text, format).unwrap(), bytes);

        // A character it cannot hold is named, and nothing is written.
        let dir = folder("cp1252");
        let path = dir.join("ansi.sql");
        fs::write(&path, &bytes).unwrap();
        let file = read_sql_file(&path).unwrap();
        let error = write_sql_file(
            &path,
            &format!("{}-- 日本", file.text),
            file.format,
            &SaveCheck::Unchanged(file.hash.clone()),
        )
        .unwrap_err();
        assert_eq!(
            error,
            FileError::Unrepresentable {
                character: '日',
                encoding: "Windows-1252"
            }
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        // Saved as UTF-8 instead, it works.
        write_sql_file(
            &path,
            "-- 日本",
            FileFormat {
                encoding: FileEncoding::Utf8,
                line_ending: LineEnding::CrLf,
            },
            &SaveCheck::Unchanged(file.hash),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "-- 日本");
    }

    #[test]
    fn binary_and_oversized_files_are_refused() {
        assert_eq!(decode(b"PK\x03\x04\0\0").unwrap_err(), FileError::Binary);
        // UTF-16 without a mark has NUL bytes: refused rather than shown as garbage.
        assert_eq!(decode(b"s\0e\0l\0").unwrap_err(), FileError::Binary);
        // A broken UTF-16 file (odd length, lone surrogate).
        assert_eq!(decode(b"\xFF\xFEa").unwrap_err(), FileError::Binary);
        assert_eq!(decode(b"\xFF\xFE\x00\xD8").unwrap_err(), FileError::Binary);
        let dir = folder("limits");
        let big = dir.join("big.sql");
        let file = fs::File::create(&big).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(matches!(
            read_sql_file(&big),
            Err(FileError::TooLarge { .. })
        ));
        assert_eq!(
            read_sql_file(&dir.join("missing.sql")).unwrap_err(),
            FileError::NotFound
        );
        assert!(matches!(read_sql_file(&dir), Err(FileError::Io(_))));
    }

    #[test]
    fn a_file_changed_or_deleted_on_disk_is_not_overwritten_unless_asked() {
        let dir = folder("conflict");
        let path = dir.join("q.sql");
        fs::write(&path, "select 1;\n").unwrap();
        let opened = read_sql_file(&path).unwrap();

        // Someone else changes it (a git pull, another editor).
        fs::write(&path, "select 'theirs';\n").unwrap();
        let check = SaveCheck::Unchanged(opened.hash.clone());
        assert_eq!(
            write_sql_file(&path, "select 'mine';", opened.format, &check).unwrap_err(),
            FileError::ChangedOnDisk
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "select 'theirs';\n");

        // The same bytes put back (a checkout) are not a conflict: the content is compared.
        fs::write(&path, "select 1;\n").unwrap();
        let hash = write_sql_file(&path, "select 'mine';\n", opened.format, &check).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "select 'mine';\n");

        // Deleted: refused too, until the user says to write it anyway.
        fs::remove_file(&path).unwrap();
        let check = SaveCheck::Unchanged(hash);
        assert_eq!(
            write_sql_file(&path, "select 2;", opened.format, &check).unwrap_err(),
            FileError::DeletedOnDisk
        );
        assert!(!path.exists());
        write_sql_file(&path, "select 2;", opened.format, &SaveCheck::Overwrite).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "select 2;");
        assert!(leftovers(&dir).is_empty());
    }

    #[test]
    fn a_failed_write_leaves_the_old_file_and_no_temporary_file() {
        let dir = folder("failed");
        // A folder where the file should be: the rename fails, nothing is left behind.
        let blocked = dir.join("blocked.sql");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("inside.txt"), "x").unwrap();
        assert!(matches!(
            write_sql_file(
                &blocked,
                "select 1;",
                FileFormat::new_file(),
                &SaveCheck::Overwrite
            ),
            Err(FileError::Io(_))
        ));
        assert!(blocked.is_dir());
        assert!(leftovers(&dir).is_empty());
        // A missing folder is named.
        let error = write_sql_file(
            &dir.join("nowhere").join("q.sql"),
            "select 1;",
            FileFormat::new_file(),
            &SaveCheck::Overwrite,
        )
        .unwrap_err();
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn a_new_file_is_utf8_with_the_platform_line_break() {
        let dir = folder("new");
        let path = dir.join("new.sql");
        let hash =
            write_sql_file(&path, "a\nb", FileFormat::new_file(), &SaveCheck::Overwrite).unwrap();
        let expected: &[u8] = if cfg!(windows) { b"a\r\nb" } else { b"a\nb" };
        assert_eq!(fs::read(&path).unwrap(), expected);
        assert_eq!(hash, content_hash(expected));
        assert_eq!(read_sql_file(&path).unwrap().text, "a\nb");
    }

    #[cfg(unix)]
    #[test]
    fn saving_keeps_permissions_and_symbolic_links() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = folder("unix");
        let real = dir.join("real.sql");
        fs::write(&real, "select 1;\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.join("link.sql");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let opened = read_sql_file(&link).unwrap();
        write_sql_file(
            &link,
            "select 2;\n",
            opened.format,
            &SaveCheck::Unchanged(opened.hash),
        )
        .unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "select 2;\n");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
