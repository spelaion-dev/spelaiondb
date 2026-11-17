//! When the desktop app cannot start (SDB-283): what it tells the user, the choices it offers and
//! what each one does. The app shows it in a native dialog, since its own window may not exist
//! yet, and writes it to the app log ([`crate::AppLog`]). Nothing here is a secret: only paths and
//! the errors of SQLite, the file system or the window system.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::HistoryError;

/// Why the app could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupFailure {
    /// There is no personal folder: the variable the system keeps its place in is missing or
    /// not an absolute path.
    NoUserDir,
    /// The local store (history, audit, saved queries, open tabs, personal settings) could not
    /// be opened or updated.
    Store {
        path: PathBuf,
        error: String,
        /// The store's schema when a newer SpelaionDB wrote it: never started over then.
        newer_schema: Option<i64>,
    },
    /// The app's window could not be opened.
    Window { error: String },
}

/// What the user can do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupAction {
    /// Close the app. The default, and what closing the dialog does.
    Quit,
    /// Open the folder of the file involved (or of the log), then ask again.
    OpenFolder,
    /// Move the local store aside and start with a new one ([`start_over`]).
    StartOver,
}

impl StartupAction {
    pub fn label(self) -> &'static str {
        match self {
            StartupAction::Quit => "Quit",
            StartupAction::OpenFolder => "Open the folder",
            StartupAction::StartOver => "Start with a new local store",
        }
    }
}

/// The dialog the app shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupDialog {
    pub title: String,
    pub body: String,
    /// In the order shown, [`StartupAction::Quit`] first.
    pub actions: Vec<StartupAction>,
    /// The folder [`StartupAction::OpenFolder`] opens.
    pub folder: Option<PathBuf>,
}

impl StartupFailure {
    /// The local store at `path` could not be opened.
    pub fn store(path: PathBuf, error: &HistoryError) -> Self {
        let newer_schema = match error {
            HistoryError::Store(e) => e.newer_schema(),
            HistoryError::Closed
            | HistoryError::Start(_)
            | HistoryError::UnsupportedLanguage
            | HistoryError::UnsupportedTheme
            | HistoryError::InvalidSnippetName
            | HistoryError::NamePattern(_)
            | HistoryError::EditorSnippet(_) => None,
        };
        let text = error.to_string();
        // "local store: …" would repeat what the dialog's first words say.
        let error = text
            .strip_prefix("local store: ")
            .unwrap_or(&text)
            .to_string();
        StartupFailure::Store {
            path,
            error,
            newer_schema,
        }
    }

    /// The window could not be opened.
    pub fn window(error: impl std::fmt::Display) -> Self {
        StartupFailure::Window {
            error: error.to_string(),
        }
    }

    /// One entry for the app log.
    pub fn log_entry(&self) -> String {
        match self {
            StartupFailure::NoUserDir => {
                format!("could not start: no personal folder ({})", user_dir_hint())
            }
            StartupFailure::Store { path, error, .. } => format!(
                "could not start: could not open the local store {}: {error}",
                path.display()
            ),
            StartupFailure::Window { error } => {
                format!("could not start: could not open the window: {error}")
            }
        }
    }

    /// What to show, telling where the log is (`log`, when it could be written) and, for the
    /// local store, where [`start_over`] would move it on the day of `now`.
    pub fn dialog(&self, log: Option<&Path>, now: SystemTime) -> StartupDialog {
        let log_note = match log {
            Some(log) => format!("\n\nThis error was also written to {}.", log.display()),
            None => String::new(),
        };
        let title = "SpelaionDB could not start".to_string();
        match self {
            StartupFailure::NoUserDir => StartupDialog {
                title,
                body: format!(
                    "Could not find your personal folder, where SpelaionDB keeps its settings \
                     and local store.\n\n{}.{log_note}",
                    user_dir_hint()
                ),
                actions: vec![StartupAction::Quit],
                folder: log.and_then(Path::parent).map(Path::to_path_buf),
            },
            StartupFailure::Store {
                path,
                error,
                newer_schema: Some(_),
            } => StartupDialog {
                title,
                body: format!(
                    "Could not open the local store: {error}\n\nFile: {}\n\nA newer version of \
                     SpelaionDB uses this file. Update SpelaionDB to open it; this version \
                     leaves the file as it is.{log_note}",
                    path.display()
                ),
                actions: vec![StartupAction::Quit, StartupAction::OpenFolder],
                folder: path.parent().map(Path::to_path_buf),
            },
            StartupFailure::Store {
                path,
                error,
                newer_schema: None,
            } => StartupDialog {
                title,
                body: format!(
                    "Could not open the local store: {error}\n\nFile: {}\n\n\"{}\" moves this \
                     file to {} and starts with an empty one. The new store has no query \
                     history, agent activity log, saved queries, open tabs or personal \
                     settings: they stay in the moved file, which is never deleted. Your \
                     connections are kept: they are in their own files.{log_note}",
                    path.display(),
                    StartupAction::StartOver.label(),
                    spelaion_store::aside_path(path, now)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
                actions: vec![
                    StartupAction::Quit,
                    StartupAction::OpenFolder,
                    StartupAction::StartOver,
                ],
                folder: path.parent().map(Path::to_path_buf),
            },
            StartupFailure::Window { error } => StartupDialog {
                title,
                body: format!(
                    "Could not open the window: {error}{}{log_note}",
                    window_hint()
                ),
                actions: vec![StartupAction::Quit, StartupAction::OpenFolder],
                folder: log.and_then(Path::parent).map(Path::to_path_buf),
            },
        }
    }
}

/// Why [`start_over`] did nothing.
#[derive(Debug, thiserror::Error)]
pub enum StartOverError {
    #[error("only a local store that cannot be opened can be started over")]
    NotAStore,
    #[error(
        "a newer version of SpelaionDB uses this local store: update SpelaionDB instead of starting over"
    )]
    Newer,
    #[error("{0}")]
    Move(#[from] spelaion_store::StoreError),
}

/// "Start with a new local store": moves the store and the files SQLite keeps beside it to
/// `state.db.broken-<date>` (never deleting anything) and returns where it went. Refused for a
/// store a newer version wrote: that version still uses it.
pub fn start_over(failure: &StartupFailure, now: SystemTime) -> Result<PathBuf, StartOverError> {
    match failure {
        StartupFailure::Store {
            path,
            newer_schema: None,
            ..
        } => Ok(spelaion_store::set_aside(path, now)?),
        StartupFailure::Store {
            newer_schema: Some(_),
            ..
        } => Err(StartOverError::Newer),
        StartupFailure::NoUserDir | StartupFailure::Window { .. } => Err(StartOverError::NotAStore),
    }
}

/// Where the personal folder comes from on this system.
fn user_dir_hint() -> &'static str {
    if cfg!(windows) {
        "It is found from the APPDATA variable, which must be an absolute path; or start \
         SpelaionDB with SPELAION_USER_DIR set to the folder to use"
    } else if cfg!(target_os = "macos") {
        "It is found from your home folder (HOME); or start SpelaionDB with SPELAION_USER_DIR \
         set to the folder to use"
    } else {
        "It is found from XDG_CONFIG_HOME, which must be an absolute path, or your home folder \
         (HOME); or start SpelaionDB with SPELAION_USER_DIR set to the folder to use"
    }
}

/// What the window needs on this system.
fn window_hint() -> &'static str {
    if cfg!(windows) {
        "\n\nSpelaionDB needs the Microsoft Edge WebView2 Runtime: install or repair it, then \
         start SpelaionDB again."
    } else if cfg!(target_os = "linux") {
        "\n\nSpelaionDB needs WebKitGTK 4.1 and a graphical session: check that both are \
         available, then start SpelaionDB again."
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    /// 2026-11-16 12:00 UTC.
    fn day() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_794_830_400)
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-startup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("db")).unwrap();
        dir
    }

    /// The error the store gives for a file in a state a real one was found in: a newer
    /// version's, or the one 0.8.0 could not open on 2026-11-13 (schema 10 by its version, with a
    /// development build's history table, without the `cancelled` column).
    fn store_error(path: &Path, newer: bool) -> HistoryError {
        if newer {
            drop(crate::QueryHistory::open(path.to_path_buf()).unwrap());
            rusqlite::Connection::open(path)
                .unwrap()
                .pragma_update(None, "user_version", 1000)
                .unwrap();
        } else {
            rusqlite::Connection::open(path)
                .unwrap()
                .execute_batch(
                    "create table history (id integer primary key, at_ms integer not null,
                         workspace text not null, connection text not null,
                         statement text not null, duration_ms integer not null,
                         rows integer not null, error text);
                     create table personal_settings (key text primary key, value text not null);
                     pragma user_version = 10;",
                )
                .unwrap();
        }
        crate::QueryHistory::open(path.to_path_buf())
            .err()
            .expect("the store must not open")
    }

    #[test]
    fn a_broken_store_offers_to_start_over_and_says_what_is_kept() {
        let dir = scratch("broken");
        let path = dir.join("db").join("state.db");
        let error = store_error(&path, false);
        let failure = StartupFailure::store(path.clone(), &error);
        let StartupFailure::Store {
            error: text,
            newer_schema,
            ..
        } = &failure
        else {
            panic!("{failure:?}");
        };
        assert_eq!(*newer_schema, None);
        assert!(!text.starts_with("local store:"), "{text}");

        let log = dir.join("logs").join("spelaion.log");
        let dialog = failure.dialog(Some(&log), day());
        assert_eq!(
            dialog.actions,
            vec![
                StartupAction::Quit,
                StartupAction::OpenFolder,
                StartupAction::StartOver
            ]
        );
        assert_eq!(dialog.folder.as_deref(), path.parent());
        for needed in [
            &path.display().to_string(),
            "state.db.broken-2026-11-16",
            "never deleted",
            "connections are kept",
            &log.display().to_string(),
        ] {
            assert!(dialog.body.contains(needed), "{needed}: {}", dialog.body);
        }

        let moved = start_over(&failure, day()).unwrap();
        assert_eq!(moved, dir.join("db").join("state.db.broken-2026-11-16"));
        assert!(moved.is_file());
        assert!(!path.exists());
        // The app opens again with a new store.
        assert!(crate::QueryHistory::open(path.clone()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_store_says_to_update_and_is_never_started_over() {
        let dir = scratch("newer");
        let path = dir.join("db").join("state.db");
        let error = store_error(&path, true);
        let failure = StartupFailure::store(path.clone(), &error);
        let dialog = failure.dialog(None, day());
        assert_eq!(
            dialog.actions,
            vec![StartupAction::Quit, StartupAction::OpenFolder]
        );
        assert!(dialog.body.contains("Update SpelaionDB"), "{}", dialog.body);
        assert!(!dialog.body.contains("broken"), "{}", dialog.body);
        assert!(matches!(
            start_over(&failure, day()),
            Err(StartOverError::Newer)
        ));
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_failures_only_quit_or_show_the_log() {
        let log = Path::new("/somewhere/logs/spelaion.log");
        let dialog = StartupFailure::NoUserDir.dialog(Some(log), day());
        assert_eq!(dialog.actions, vec![StartupAction::Quit]);
        assert!(dialog.body.contains("SPELAION_USER_DIR"), "{}", dialog.body);

        let failure = StartupFailure::window("no webview");
        let dialog = failure.dialog(Some(log), day());
        assert_eq!(
            dialog.actions,
            vec![StartupAction::Quit, StartupAction::OpenFolder]
        );
        assert_eq!(dialog.folder.as_deref(), log.parent());
        assert!(dialog.body.contains("no webview"), "{}", dialog.body);
        assert!(matches!(
            start_over(&failure, day()),
            Err(StartOverError::NotAStore)
        ));
        assert!(failure.log_entry().contains("no webview"));
    }
}
