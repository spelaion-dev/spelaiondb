//! The editor's open tabs per workspace and the recent files (ADR-036).
//!
//! What is kept is only what brings a tab back disconnected: its text, title, connection id,
//! order, pin, run options and, for a file tab, how to save the file again. Never results,
//! secrets or a session's state: there is no column for them.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::{ScriptErrorMode, Store, StoreError, now_ms};

/// How many recent files are kept.
pub const MAX_RECENT_FILES: usize = 20;

/// Most tabs one workspace keeps; more is refused rather than silently cut.
const MAX_SAVED_TABS: usize = 500;

/// The encoding a `.sql` file was read in, and is written back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileEncoding {
    Utf8,
    /// UTF-8 starting with a byte order mark.
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl FileEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            FileEncoding::Utf8 => "utf-8",
            FileEncoding::Utf8Bom => "utf-8-bom",
            FileEncoding::Utf16Le => "utf-16le",
            FileEncoding::Utf16Be => "utf-16be",
            FileEncoding::Windows1252 => "windows-1252",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "utf-8" => Some(FileEncoding::Utf8),
            "utf-8-bom" => Some(FileEncoding::Utf8Bom),
            "utf-16le" => Some(FileEncoding::Utf16Le),
            "utf-16be" => Some(FileEncoding::Utf16Be),
            "windows-1252" => Some(FileEncoding::Windows1252),
            _ => None,
        }
    }
}

/// The line break a `.sql` file is written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "lf",
            LineEnding::CrLf => "crlf",
            LineEnding::Cr => "cr",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "lf" => Some(LineEnding::Lf),
            "crlf" => Some(LineEnding::CrLf),
            "cr" => Some(LineEnding::Cr),
            _ => None,
        }
    }

    /// The characters written for each line break.
    pub fn chars(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::CrLf => "\r\n",
            LineEnding::Cr => "\r",
        }
    }
}

/// The file behind a file tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedFile {
    pub path: String,
    /// SHA-256 of the file's bytes when it was last read or saved; `None` before the first
    /// read (a file tab always has one in practice).
    pub hash: Option<String>,
    pub encoding: FileEncoding,
    pub line_ending: LineEnding,
    /// The tab's text differs from the file: it comes back as unsaved.
    pub modified: bool,
}

/// One editor tab as it is kept between runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SavedTab {
    /// A name the user gave it; `None` takes the default (the file's name, or "Query N").
    pub title: Option<String>,
    pub text: String,
    /// Connection id (`~/name` for a personal one); never anything secret.
    pub connection: Option<String>,
    pub pinned: bool,
    pub on_error: Option<ScriptErrorMode>,
    pub timeout_seconds: Option<u32>,
    /// Saved query it was opened from or saved as (offered again by Ctrl+S on a console).
    pub snippet_name: Option<String>,
    /// `Some` for a file tab, `None` for a console.
    pub file: Option<SavedFile>,
}

/// A workspace's open tabs, in the tab bar's order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SavedTabs {
    pub tabs: Vec<SavedTab>,
    /// Index of the tab that was shown.
    pub active: Option<usize>,
}

/// A file opened or saved recently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentFile {
    pub path: String,
    pub opened_ms: i64,
}

impl Store {
    /// Replaces the open tabs kept for `workspace`, in one transaction: a crash leaves the
    /// previous set or this one, never a mix.
    pub fn save_open_tabs(&mut self, workspace: &str, saved: &SavedTabs) -> Result<(), StoreError> {
        if saved.tabs.len() > MAX_SAVED_TABS {
            return Err(StoreError(format!(
                "too many open tabs to keep ({}; at most {MAX_SAVED_TABS})",
                saved.tabs.len()
            )));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("delete from open_tabs where workspace = ?1", [workspace])?;
        {
            let mut insert = tx.prepare(
                "insert into open_tabs (workspace, position, active, title, text, connection,
                     pinned, on_error, timeout_seconds, snippet_name, file_path, file_hash,
                     file_encoding, file_line_ending, file_modified)
                 values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )?;
            for (position, tab) in saved.tabs.iter().enumerate() {
                let file = tab.file.as_ref();
                insert.execute(params![
                    workspace,
                    position as i64,
                    saved.active == Some(position),
                    tab.title,
                    tab.text,
                    tab.connection,
                    tab.pinned,
                    tab.on_error.map(ScriptErrorMode::as_str),
                    tab.timeout_seconds.filter(|s| *s > 0),
                    tab.snippet_name,
                    file.map(|f| f.path.as_str()),
                    file.and_then(|f| f.hash.as_deref()),
                    file.map(|f| f.encoding.as_str()),
                    file.map(|f| f.line_ending.as_str()),
                    file.map(|f| f.modified),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The open tabs kept for `workspace`, in order; none if it never kept any.
    pub fn open_tabs(&self, workspace: &str) -> Result<SavedTabs, StoreError> {
        let mut statement = self.conn.prepare(
            "select active, title, text, connection, pinned, on_error, timeout_seconds,
                    snippet_name, file_path, file_hash, file_encoding, file_line_ending,
                    file_modified
             from open_tabs where workspace = ?1 order by position",
        )?;
        let rows = statement.query_map([workspace], |r| {
            let file = match r.get::<_, Option<String>>(8)? {
                None => None,
                Some(path) => {
                    let encoding: String = r.get(10)?;
                    let line_ending: String = r.get(11)?;
                    Some((
                        path,
                        r.get::<_, Option<String>>(9)?,
                        encoding,
                        line_ending,
                        r.get::<_, bool>(12)?,
                    ))
                }
            };
            Ok((
                r.get::<_, bool>(0)?,
                SavedTab {
                    title: r.get(1)?,
                    text: r.get(2)?,
                    connection: r.get(3)?,
                    pinned: r.get(4)?,
                    on_error: r
                        .get::<_, Option<String>>(5)?
                        .as_deref()
                        .and_then(ScriptErrorMode::parse),
                    timeout_seconds: r
                        .get::<_, Option<i64>>(6)?
                        .and_then(|s| u32::try_from(s).ok()),
                    snippet_name: r.get(7)?,
                    file: None,
                },
                file,
            ))
        })?;
        let mut saved = SavedTabs::default();
        for row in rows {
            let (active, mut tab, file) = row?;
            if let Some((path, hash, encoding, line_ending, modified)) = file {
                // The check constraints keep these valid; a value from a newer version that
                // this one does not know is read as the default rather than losing the tab.
                tab.file = Some(SavedFile {
                    path,
                    hash,
                    encoding: FileEncoding::parse(&encoding).unwrap_or(FileEncoding::Utf8),
                    line_ending: LineEnding::parse(&line_ending).unwrap_or(LineEnding::Lf),
                    modified,
                });
            }
            if active {
                saved.active = Some(saved.tabs.len());
            }
            saved.tabs.push(tab);
        }
        Ok(saved)
    }

    /// Puts `path` first in the recent files, keeping the newest [`MAX_RECENT_FILES`].
    pub fn add_recent_file(&mut self, path: &str) -> Result<(), StoreError> {
        if path.is_empty() {
            return Err(StoreError("a recent file needs a path".into()));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Strictly newer than the newest one, so two files opened in the same millisecond keep
        // their order.
        let newest: Option<i64> = tx
            .query_row("select max(opened_ms) from recent_files", [], |r| r.get(0))
            .optional()?
            .flatten();
        let at = now_ms().max(newest.map_or(i64::MIN, |n| n.saturating_add(1)));
        tx.execute(
            "insert into recent_files (path, opened_ms) values (?1, ?2)
             on conflict (path) do update set opened_ms = excluded.opened_ms",
            params![path, at],
        )?;
        tx.execute(
            "delete from recent_files where path not in
                 (select path from recent_files order by opened_ms desc limit ?1)",
            [MAX_RECENT_FILES as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The recent files, newest first.
    pub fn recent_files(&self) -> Result<Vec<RecentFile>, StoreError> {
        let mut statement = self.conn.prepare(
            "select path, opened_ms from recent_files order by opened_ms desc, path limit ?1",
        )?;
        let rows = statement.query_map([MAX_RECENT_FILES as i64], |r| {
            Ok(RecentFile {
                path: r.get(0)?,
                opened_ms: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Whether `path` was in the recent files.
    pub fn remove_recent_file(&self, path: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("delete from recent_files where path = ?1", [path])?
            > 0)
    }

    /// Whether the app connects the restored tabs at startup (off by default). It only
    /// connects: nothing is run (ADR-036).
    pub fn reconnect_on_start(&self) -> Result<bool, StoreError> {
        self.get_flag("editor.reconnect_on_start", false)
    }

    pub fn set_reconnect_on_start(&self, enabled: bool) -> Result<(), StoreError> {
        self.set_flag("editor.reconnect_on_start", enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIGRATIONS;
    use rusqlite::Connection;

    fn temp(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "spelaion-store-tabs-{name}-{}.db",
            std::process::id()
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        path
    }

    fn three_tabs() -> SavedTabs {
        SavedTabs {
            tabs: vec![
                SavedTab {
                    title: Some("Pinned report".into()),
                    text: "select 1;\nselect 'two';".into(),
                    connection: Some("~/shop".into()),
                    pinned: true,
                    on_error: Some(ScriptErrorMode::Continue),
                    timeout_seconds: Some(30),
                    snippet_name: Some("report".into()),
                    file: None,
                },
                SavedTab {
                    text: "update t set x = 1 where id = 2;\n".into(),
                    connection: Some("shop".into()),
                    file: Some(SavedFile {
                        path: "/repo/queries/fix.sql".into(),
                        hash: Some("ab12".into()),
                        encoding: FileEncoding::Utf8Bom,
                        line_ending: LineEnding::CrLf,
                        modified: true,
                    }),
                    ..SavedTab::default()
                },
                SavedTab {
                    text: "-- scratch".into(),
                    ..SavedTab::default()
                },
            ],
            active: Some(1),
        }
    }

    #[test]
    fn open_tabs_round_trip_per_workspace_and_replace_the_previous_set() {
        let path = temp("round-trip");
        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.open_tabs("/repo").unwrap(), SavedTabs::default());
        let saved = three_tabs();
        store.save_open_tabs("/repo", &saved).unwrap();
        store
            .save_open_tabs(
                "",
                &SavedTabs {
                    tabs: vec![SavedTab {
                        text: "select 'personal';".into(),
                        ..SavedTab::default()
                    }],
                    active: None,
                },
            )
            .unwrap();
        drop(store);

        let mut store = Store::open(&path).unwrap();
        assert_eq!(store.open_tabs("/repo").unwrap(), saved);
        assert_eq!(store.open_tabs("").unwrap().tabs.len(), 1);
        assert_eq!(store.open_tabs("/other").unwrap(), SavedTabs::default());

        // A later save replaces the whole set; an empty one forgets it.
        let mut fewer = saved.clone();
        fewer.tabs.remove(0);
        fewer.active = Some(0);
        store.save_open_tabs("/repo", &fewer).unwrap();
        assert_eq!(store.open_tabs("/repo").unwrap(), fewer);
        store
            .save_open_tabs("/repo", &SavedTabs::default())
            .unwrap();
        assert_eq!(store.open_tabs("/repo").unwrap(), SavedTabs::default());
        assert_eq!(store.open_tabs("").unwrap().tabs.len(), 1);
    }

    #[test]
    fn a_failed_save_keeps_the_previous_tabs() {
        let path = temp("failed-save");
        let mut store = Store::open(&path).unwrap();
        let saved = three_tabs();
        store.save_open_tabs("/repo", &saved).unwrap();
        // An empty path breaks a check constraint on the second row: nothing changes.
        let mut broken = saved.clone();
        broken.tabs[1].file.as_mut().unwrap().path = String::new();
        broken.tabs[0].text = "lost".into();
        assert!(store.save_open_tabs("/repo", &broken).is_err());
        assert_eq!(store.open_tabs("/repo").unwrap(), saved);

        let too_many = SavedTabs {
            tabs: vec![SavedTab::default(); MAX_SAVED_TABS + 1],
            active: None,
        };
        assert!(store.save_open_tabs("/repo", &too_many).is_err());
        assert_eq!(store.open_tabs("/repo").unwrap(), saved);
    }

    #[test]
    fn the_table_refuses_what_a_tab_cannot_be() {
        let path = temp("constraints");
        let store = Store::open(&path).unwrap();
        let insert = |sql: &str| store.conn.execute(sql, []);
        // A file path without its encoding, an unknown encoding or line ending, a run option
        // that does not exist: refused by SQLite itself.
        assert!(
            insert(
                "insert into open_tabs (workspace, position, active, text, pinned, file_path)
             values ('', 0, 0, '', 0, '/a.sql')"
            )
            .is_err()
        );
        assert!(
            insert(
                "insert into open_tabs (workspace, position, active, text, pinned, file_path,
                 file_encoding, file_line_ending, file_modified)
             values ('', 1, 0, '', 0, '/a.sql', 'latin-7', 'lf', 0)"
            )
            .is_err()
        );
        assert!(
            insert(
                "insert into open_tabs (workspace, position, active, text, pinned, file_path,
                 file_encoding, file_line_ending, file_modified)
             values ('', 2, 0, '', 0, '/a.sql', 'utf-8', 'nl', 0)"
            )
            .is_err()
        );
        assert!(
            insert(
                "insert into open_tabs (workspace, position, active, text, pinned, on_error)
             values ('', 3, 0, '', 0, 'retry')"
            )
            .is_err()
        );
        assert!(insert(
            "insert into personal_settings (key, value) values ('editor.reconnect_on_start', 'yes')"
        )
        .is_err());
    }

    #[test]
    fn the_open_tabs_migration_keeps_existing_data() {
        let path = temp("upgrade");
        {
            let mut conn = Connection::open(&path).unwrap();
            let tx = conn.transaction().unwrap();
            // Every migration before this one (the tenth).
            let before = 9;
            for migration in &MIGRATIONS[..before] {
                tx.execute_batch(migration).unwrap();
            }
            tx.pragma_update(None, "user_version", before as i64)
                .unwrap();
            tx.execute(
                "insert into snippets (workspace, name, statement, connection, updated_ms)
                 values ('/repo', 'kept', 'select 42', 'shop', 1)",
                [],
            )
            .unwrap();
            tx.execute(
                "insert into personal_settings (key, value) values ('theme', 'dark')",
                [],
            )
            .unwrap();
            tx.commit().unwrap();
        }
        let mut store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        assert_eq!(
            store.snippets("/repo", "").unwrap()[0].statement,
            "select 42"
        );
        assert_eq!(store.get_theme().unwrap(), "dark");
        // Nothing was open before: no tabs, no recent files, reconnecting off.
        assert_eq!(store.open_tabs("/repo").unwrap(), SavedTabs::default());
        assert!(store.recent_files().unwrap().is_empty());
        assert!(!store.reconnect_on_start().unwrap());
        store.save_open_tabs("/repo", &three_tabs()).unwrap();
        assert_eq!(store.open_tabs("/repo").unwrap(), three_tabs());
    }

    #[test]
    fn recent_files_are_newest_first_without_duplicates_and_bounded() {
        let path = temp("recent");
        let mut store = Store::open(&path).unwrap();
        for i in 0..(MAX_RECENT_FILES + 5) {
            store.add_recent_file(&format!("/q/{i}.sql")).unwrap();
        }
        store.add_recent_file("/q/7.sql").unwrap();
        let recent = store.recent_files().unwrap();
        assert_eq!(recent.len(), MAX_RECENT_FILES);
        assert_eq!(recent[0].path, "/q/7.sql");
        assert_eq!(recent[1].path, format!("/q/{}.sql", MAX_RECENT_FILES + 4));
        assert_eq!(recent.iter().filter(|r| r.path == "/q/7.sql").count(), 1);
        // The oldest ones went (7 came back to the front).
        assert!(!recent.iter().any(|r| r.path == "/q/0.sql"));
        assert!(store.remove_recent_file("/q/7.sql").unwrap());
        assert!(!store.remove_recent_file("/q/7.sql").unwrap());
        assert!(store.add_recent_file("").is_err());
        drop(store);
        assert_eq!(
            Store::open(&path).unwrap().recent_files().unwrap().len(),
            MAX_RECENT_FILES - 1
        );
    }

    #[test]
    fn reconnect_on_start_is_off_until_turned_on() {
        let path = temp("reconnect");
        let store = Store::open(&path).unwrap();
        assert!(!store.reconnect_on_start().unwrap());
        store.set_reconnect_on_start(true).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.reconnect_on_start().unwrap());
        store.set_reconnect_on_start(false).unwrap();
        assert!(!store.reconnect_on_start().unwrap());
    }
}
