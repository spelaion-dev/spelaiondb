//! The desktop app's log file: what went wrong and where, in a file a report can attach
//! (SDB-283), and what the log viewer in Settings shows (SDB-31). Only what the app chooses to
//! write goes here: paths, connection ids and error messages, never SQL, data or secrets. Every
//! entry is scrubbed as it is written and again as it is read ([`scrub`]), in case an error
//! message carried a password anyway.
//!
//! It lives in the personal folder (`logs/spelaion.log`), private like the rest of it; when there
//! is no personal folder, in the system's temporary folder. It is kept small: past
//! [`MAX_LOG_BYTES`] the file becomes `spelaion.log.1` (replacing the previous one) and a new one
//! starts.
//!
//! An entry is one line, `<time> <LEVEL> <message>`, with the message's later lines indented by
//! four spaces. Entries written before there were levels (before 0.18) have none and read as
//! errors: the log held only failures then.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use spelaion_config::atomic_file::{AtomicWriter, NewFileMode};

/// The size past which the log starts over, keeping the previous file as `.1`.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// The most entries [`AppLog::read`] returns.
pub const MAX_LOG_ENTRIES: usize = 5_000;

const FILE_NAME: &str = "spelaion.log";
const CONTINUATION: &str = "    ";

/// How much an entry matters, from the most to the least.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warning,
    Info,
}

impl LogLevel {
    /// The word written in the file.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warning => "WARN",
            LogLevel::Info => "INFO",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "ERROR" => Some(LogLevel::Error),
            "WARN" => Some(LogLevel::Warning),
            "INFO" => Some(LogLevel::Info),
            _ => None,
        }
    }
}

/// One entry of the log, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// When it was written (UTC, ISO 8601).
    pub time: String,
    pub level: LogLevel,
    /// The message, its lines joined with `\n`, scrubbed.
    pub message: String,
}

/// The entries a read keeps, and how many of each level the log has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogPage {
    /// Newest first, at most [`MAX_LOG_ENTRIES`].
    pub entries: Vec<LogEntry>,
    pub errors: usize,
    pub warnings: usize,
    pub infos: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLog {
    path: PathBuf,
}

impl AppLog {
    /// The log in the personal folder `user_dir`.
    pub fn in_user_dir(user_dir: &Path) -> Self {
        Self {
            path: user_dir.join("logs").join(FILE_NAME),
        }
    }

    /// The log when there is no personal folder to keep it in.
    pub fn without_user_dir() -> Self {
        Self {
            path: std::env::temp_dir().join(FILE_NAME),
        }
    }

    /// The log file's full path, to tell the user where it is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn previous_path(&self) -> PathBuf {
        let mut previous = self.path.as_os_str().to_owned();
        previous.push(".1");
        PathBuf::from(previous)
    }

    /// Adds `message` as one entry of `level`, with the time (UTC), scrubbed. Its lines after
    /// the first are indented, so an entry never reads as two.
    pub fn append(&self, level: LogLevel, message: &str) -> std::io::Result<()> {
        if let Some(folder) = self.path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
            std::fs::rename(&self.path, self.previous_path())?;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let entry = format!(
            "{} {} {}\n",
            crate::audit::iso_time(now),
            level.as_str(),
            scrub(message.trim_end()).replace('\n', &format!("\n{CONTINUATION}"))
        );
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(entry.as_bytes())
    }

    /// Every entry of the log and of the previous file, oldest first, scrubbed. A file that
    /// does not exist reads as none.
    fn entries(&self) -> std::io::Result<Vec<LogEntry>> {
        let mut entries = Vec::new();
        for path in [self.previous_path(), self.path.clone()] {
            match std::fs::read(&path) {
                Ok(bytes) => parse_entries(&String::from_utf8_lossy(&bytes), &mut entries),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(entries)
    }

    /// The entries of `level` or more important, newest first, and how many of each level
    /// there are.
    pub fn read(&self, level: LogLevel) -> std::io::Result<LogPage> {
        let all = self.entries()?;
        let mut page = LogPage::default();
        for entry in &all {
            match entry.level {
                LogLevel::Error => page.errors += 1,
                LogLevel::Warning => page.warnings += 1,
                LogLevel::Info => page.infos += 1,
            }
        }
        page.entries = all
            .into_iter()
            .rev()
            .filter(|entry| entry.level <= level)
            .take(MAX_LOG_ENTRIES)
            .collect();
        Ok(page)
    }

    /// Writes the entries of `level` or more important to `path`, oldest first, in the log's
    /// own format (replacing it whole, or not at all). How many were written.
    pub fn export(&self, level: LogLevel, path: &Path) -> std::io::Result<usize> {
        let entries: Vec<LogEntry> = self
            .entries()?
            .into_iter()
            .filter(|entry| entry.level <= level)
            .collect();
        let mut writer = AtomicWriter::create(path, NewFileMode::OwnerOnly)?;
        for entry in &entries {
            writeln!(
                writer,
                "{} {} {}",
                entry.time,
                entry.level.as_str(),
                entry.message.replace('\n', &format!("\n{CONTINUATION}"))
            )?;
        }
        writer.commit()?;
        Ok(entries.len())
    }
}

/// Reads the entries of one file's text into `entries`.
fn parse_entries(text: &str, entries: &mut Vec<LogEntry>) {
    let mut current: Option<LogEntry> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(CONTINUATION)
            && let Some(entry) = current.as_mut()
        {
            entry.message.push('\n');
            entry.message.push_str(&scrub(rest));
            continue;
        }
        if let Some(entry) = current.take() {
            entries.push(entry);
        }
        let (time, rest) = line.split_once(' ').unwrap_or((line, ""));
        if !looks_like_time(time) {
            // Not an entry this log wrote (a damaged line): kept as an error, so nothing in the
            // file is hidden from the viewer.
            current = Some(LogEntry {
                time: String::new(),
                level: LogLevel::Error,
                message: scrub(line),
            });
            continue;
        }
        let (level, message) = match rest.split_once(' ') {
            Some((word, message)) if LogLevel::parse(word).is_some() => {
                (LogLevel::parse(word).unwrap_or(LogLevel::Error), message)
            }
            _ if LogLevel::parse(rest).is_some() => {
                (LogLevel::parse(rest).unwrap_or(LogLevel::Error), "")
            }
            _ => (LogLevel::Error, rest),
        };
        current = Some(LogEntry {
            time: time.to_string(),
            level,
            message: scrub(message),
        });
    }
    if let Some(entry) = current {
        entries.push(entry);
    }
}

fn looks_like_time(word: &str) -> bool {
    word.len() >= 20
        && word.ends_with('Z')
        && word.as_bytes().get(10) == Some(&b'T')
        && word.as_bytes()[..4].iter().all(u8::is_ascii_digit)
}

/// Words after which a value is a secret, as `password=…`, `pwd: …` or `token …`.
const SECRET_KEYS: [&str; 6] = ["password", "passwd", "pwd", "secret", "token", "passphrase"];

/// `text` with what looks like a secret masked: the quoted literals and URL passwords
/// [`crate::audit::redact_secrets`] masks, and an unquoted value after `password=`, `pwd:`,
/// `token=`… up to a space, `;`, `&` or `,`. A heuristic that catches what error messages and
/// connection strings usually carry; the app never writes a secret on purpose.
pub fn scrub(text: &str) -> String {
    let text = crate::audit::redact_secrets(text);
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let key = SECRET_KEYS.iter().find(|key| {
            lower.as_bytes()[i..].starts_with(key.as_bytes())
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        });
        let Some(key) = key else {
            i += 1;
            continue;
        };
        let mut j = i + key.len();
        // `password_hash` or `tokens` are other words.
        if bytes
            .get(j)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            i = j;
            continue;
        }
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        if !matches!(bytes.get(j), Some(b'=' | b':')) {
            i = j.max(i + 1);
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        let start = j;
        while j < bytes.len() && !matches!(bytes[j], b' ' | b';' | b'&' | b',' | b'\n' | b'\r') {
            j += 1;
        }
        let value = &text[start..j];
        if value.is_empty() || value == "***" || value.starts_with('\'') || value.starts_with('"') {
            i = j.max(i + 1);
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str("***");
        copied = j;
        i = j;
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-app-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn entries_are_appended_with_the_time_the_level_and_their_lines_indented() {
        let dir = scratch("append");
        let log = AppLog::in_user_dir(&dir);
        assert_eq!(log.path(), dir.join("logs").join("spelaion.log"));
        log.append(LogLevel::Info, "first").unwrap();
        log.append(LogLevel::Error, "second\nmore of it\n").unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("Z INFO first"), "{text}");
        assert!(lines[1].ends_with("Z ERROR second"), "{text}");
        assert_eq!(lines[2], "    more of it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_log_starts_over_keeping_the_previous_one() {
        let dir = scratch("rotate");
        let log = AppLog::in_user_dir(&dir);
        std::fs::create_dir_all(log.path().parent().unwrap()).unwrap();
        std::fs::write(log.path(), vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        log.append(LogLevel::Warning, "after").unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(text.ends_with("Z WARN after\n"), "{text}");
        let previous = dir.join("logs").join("spelaion.log.1");
        assert_eq!(
            std::fs::metadata(previous).unwrap().len(),
            MAX_LOG_BYTES + 1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_viewer_reads_both_files_newest_first_and_filters_by_level() {
        let dir = scratch("read");
        let log = AppLog::in_user_dir(&dir);
        std::fs::create_dir_all(log.path().parent().unwrap()).unwrap();
        // The previous file, written before there were levels: its entries read as errors.
        std::fs::write(
            log.previous_path(),
            "2026-11-01T10:00:00.000Z the app could not start\n    because of this\n",
        )
        .unwrap();
        log.append(LogLevel::Info, "started").unwrap();
        log.append(LogLevel::Warning, "connection failed").unwrap();
        log.append(LogLevel::Error, "panicked").unwrap();

        let all = log.read(LogLevel::Info).unwrap();
        assert_eq!((all.errors, all.warnings, all.infos), (2, 1, 1));
        let messages: Vec<&str> = all.entries.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "panicked",
                "connection failed",
                "started",
                "the app could not start\nbecause of this"
            ]
        );
        let warnings = log.read(LogLevel::Warning).unwrap();
        assert_eq!(warnings.entries.len(), 3);
        let errors = log.read(LogLevel::Error).unwrap();
        assert!(errors.entries.iter().all(|e| e.level == LogLevel::Error));
        assert_eq!(errors.entries.len(), 2);

        let exported = dir.join("exported.log");
        assert_eq!(log.export(LogLevel::Warning, &exported).unwrap(), 3);
        let text = std::fs::read_to_string(&exported).unwrap();
        assert!(
            text.contains("ERROR the app could not start\n    because of this\n"),
            "{text}"
        );
        assert!(!text.contains("INFO started"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_that_does_not_exist_reads_as_empty() {
        let dir = scratch("missing");
        let page = AppLog::in_user_dir(&dir).read(LogLevel::Info).unwrap();
        assert_eq!(page, LogPage::default());
    }

    #[test]
    fn secrets_never_reach_the_file_nor_the_viewer() {
        let dir = scratch("secrets");
        let log = AppLog::in_user_dir(&dir);
        log.append(
            LogLevel::Warning,
            "could not connect: Server=db;User Id=sa;Password=hunter2;Encrypt=true\nurl postgres://app:s3cret@db/orders, token: abc123 pwd = p4ss",
        )
        .unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        for secret in ["hunter2", "s3cret", "abc123", "p4ss"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(text.contains("Password=***;Encrypt=true"), "{text}");
        // A line someone else wrote into the file is scrubbed when read.
        std::fs::write(
            log.path(),
            "2026-11-26T10:00:00.000Z ERROR connect failed: password=topsecret\n",
        )
        .unwrap();
        let page = log.read(LogLevel::Info).unwrap();
        assert_eq!(page.entries[0].message, "connect failed: password=***");
        // Words that only start like a secret's name are left alone.
        assert_eq!(
            scrub("password_hash: x, tokens=3"),
            "password_hash: x, tokens=3"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
