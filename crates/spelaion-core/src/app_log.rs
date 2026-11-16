//! The desktop app's log file: what went wrong and where, in a file a report can attach
//! (SDB-283). Only what the app chooses to write goes here: paths and error messages, never
//! SQL, data or secrets.
//!
//! It lives in the personal folder (`logs/spelaion.log`), private like the rest of it; when there
//! is no personal folder, in the system's temporary folder. It is kept small: past
//! [`MAX_LOG_BYTES`] the file becomes `spelaion.log.1` (replacing the previous one) and a new one
//! starts.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The size past which the log starts over, keeping the previous file as `.1`.
pub const MAX_LOG_BYTES: u64 = 1024 * 1024;

const FILE_NAME: &str = "spelaion.log";

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

    /// Adds `message` as one entry, with the time (UTC). Its lines after the first are indented,
    /// so an entry never reads as two.
    pub fn append(&self, message: &str) -> std::io::Result<()> {
        if let Some(folder) = self.path.parent() {
            std::fs::create_dir_all(folder)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
            let mut previous = self.path.as_os_str().to_owned();
            previous.push(".1");
            std::fs::rename(&self.path, PathBuf::from(previous))?;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let entry = format!(
            "{} {}\n",
            crate::audit::iso_time(now),
            message.trim_end().replace('\n', "\n    ")
        );
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(entry.as_bytes())
    }
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
    fn entries_are_appended_with_the_time_and_their_lines_indented() {
        let dir = scratch("append");
        let log = AppLog::in_user_dir(&dir);
        assert_eq!(log.path(), dir.join("logs").join("spelaion.log"));
        log.append("first").unwrap();
        log.append("second\nmore of it\n").unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with("Z first"), "{text}");
        assert!(lines[1].ends_with("Z second"), "{text}");
        assert_eq!(lines[2], "    more of it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_log_starts_over_keeping_the_previous_one() {
        let dir = scratch("rotate");
        let log = AppLog::in_user_dir(&dir);
        std::fs::create_dir_all(log.path().parent().unwrap()).unwrap();
        std::fs::write(log.path(), vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        log.append("after").unwrap();
        let text = std::fs::read_to_string(log.path()).unwrap();
        assert!(text.ends_with("Z after\n"), "{text}");
        let previous = dir.join("logs").join("spelaion.log.1");
        assert_eq!(
            std::fs::metadata(previous).unwrap().len(),
            MAX_LOG_BYTES + 1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
