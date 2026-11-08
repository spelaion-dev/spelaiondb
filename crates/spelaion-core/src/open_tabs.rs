//! Open editor tabs that survive a restart (ADR-036).
//!
//! [`TabKeeper`] decides how often the tabs reach the local store: at most [`TAB_SAVE_DELAY`]
//! after the first change it has not written, so a crash loses at most that window.
//! [`restore_tabs`] checks the kept tabs before the app shows them again: they come back
//! disconnected, with their text and nothing else, and with a note when their connection or
//! their file is gone. Nothing here runs anything.

use std::path::Path;
use std::time::Duration;

use spelaion_store::{SavedTab, SavedTabs};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::sql_file::{FileError, content_hash, read_sql_file};
use crate::{HistoryError, QueryHistory};

/// Longest time between a change to the tabs and its write to the store.
pub const TAB_SAVE_DELAY: Duration = Duration::from_secs(1);

enum KeeperRequest {
    Update {
        workspace: String,
        tabs: SavedTabs,
    },
    Flush {
        reply: oneshot::Sender<Result<(), HistoryError>>,
    },
}

/// Writes the open tabs to the local store on a deadline. Cloning shares the same writer.
#[derive(Clone)]
pub struct TabKeeper {
    requests: mpsc::UnboundedSender<KeeperRequest>,
    #[cfg(test)]
    task: tokio::task::AbortHandle,
}

impl TabKeeper {
    /// Starts the writer on the current Tokio runtime.
    pub fn spawn(history: QueryHistory) -> Self {
        Self::with_delay(history, TAB_SAVE_DELAY)
    }

    fn with_delay(history: QueryHistory, delay: Duration) -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        let _task = tokio::spawn(keep(history, delay, receiver));
        Self {
            requests,
            #[cfg(test)]
            task: _task.abort_handle(),
        }
    }

    /// The tabs of `workspace` as they are now. Returns at once; the write follows within
    /// [`TAB_SAVE_DELAY`]. Tabs of another workspace still waiting are written first, so
    /// switching folders never drops the last state of the previous one.
    pub fn update(&self, workspace: String, tabs: SavedTabs) {
        let _ = self
            .requests
            .send(KeeperRequest::Update { workspace, tabs });
    }

    /// Writes what is waiting now (quitting, switching folders) and reports the last error.
    pub async fn flush(&self) -> Result<(), HistoryError> {
        let (reply, answer) = oneshot::channel();
        self.requests
            .send(KeeperRequest::Flush { reply })
            .map_err(|_| HistoryError::Closed)?;
        answer.await.map_err(|_| HistoryError::Closed)?
    }

    /// Stops the writer without writing what is waiting, as a crash would.
    #[cfg(test)]
    fn crash(&self) {
        self.task.abort();
    }
}

async fn keep(
    history: QueryHistory,
    delay: Duration,
    mut requests: mpsc::UnboundedReceiver<KeeperRequest>,
) {
    let mut pending: Option<(String, SavedTabs)> = None;
    let mut deadline: Option<Instant> = None;
    let mut last_error: Option<HistoryError> = None;
    loop {
        let request = match deadline {
            Some(at) => tokio::select! {
                request = requests.recv() => request,
                () = tokio::time::sleep_until(at) => {
                    deadline = None;
                    write(&history, &mut pending, &mut last_error).await;
                    continue;
                }
            },
            None => requests.recv().await,
        };
        match request {
            // Every handle dropped (the app is closing): write what is left.
            None => {
                write(&history, &mut pending, &mut last_error).await;
                return;
            }
            Some(KeeperRequest::Update { workspace, tabs }) => {
                if pending.as_ref().is_some_and(|(w, _)| *w != workspace) {
                    write(&history, &mut pending, &mut last_error).await;
                }
                pending = Some((workspace, tabs));
                // A deadline, not a debounce: steady typing still saves every `delay`.
                deadline.get_or_insert_with(|| Instant::now() + delay);
            }
            Some(KeeperRequest::Flush { reply }) => {
                deadline = None;
                write(&history, &mut pending, &mut last_error).await;
                let _ = reply.send(last_error.take().map_or(Ok(()), Err));
            }
        }
    }
}

/// Writes the waiting tabs. On failure they stay waiting for the next update or flush, and
/// the error is kept for the next flush.
async fn write(
    history: &QueryHistory,
    pending: &mut Option<(String, SavedTabs)>,
    last_error: &mut Option<HistoryError>,
) {
    let Some((workspace, tabs)) = pending.take() else {
        return;
    };
    match history
        .save_open_tabs(workspace.clone(), tabs.clone())
        .await
    {
        Ok(()) => *last_error = None,
        Err(e) => {
            *last_error = Some(e);
            *pending = Some((workspace, tabs));
        }
    }
}

/// Why a restored tab is not exactly as it was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreNote {
    /// Its connection was deleted or renamed: the tab has no connection now.
    ConnectionMissing { connection: String },
    /// Its file is gone: the kept text came back as unsaved.
    FileMissing,
    /// Its file could not be read: the kept text came back as unsaved.
    FileUnreadable { message: String },
    /// It had unsaved changes and its file changed on disk since: saving will ask.
    FileChangedOnDisk,
}

/// A kept tab, checked, ready to show disconnected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredTab {
    pub tab: SavedTab,
    pub notes: Vec<RestoreNote>,
}

/// Checks the kept tabs before they are shown again (blocking: it reads their files).
///
/// - A connection for which `connection_exists` says no is taken off the tab, with a note.
/// - A file tab without unsaved changes is read again from disk, so it shows the file as it is
///   now; if that fails, its kept text comes back as unsaved, with a note.
/// - A file tab with unsaved changes keeps its text; a note says if the file changed since.
///
/// The tabs carry their text only: there is nothing in them to run, and no result, session or
/// transaction to bring back.
pub fn restore_tabs(
    saved: SavedTabs,
    connection_exists: impl Fn(&str) -> bool,
) -> (Vec<RestoredTab>, Option<usize>) {
    let active = saved.active.filter(|i| *i < saved.tabs.len());
    let tabs = saved
        .tabs
        .into_iter()
        .map(|mut tab| {
            let mut notes = Vec::new();
            if let Some(connection) = tab.connection.take() {
                if connection_exists(&connection) {
                    tab.connection = Some(connection);
                } else {
                    notes.push(RestoreNote::ConnectionMissing { connection });
                }
            }
            if let Some(file) = tab.file.as_mut() {
                let path = Path::new(&file.path);
                if file.modified {
                    match std::fs::read(path) {
                        Ok(bytes) => {
                            if file.hash.as_deref() != Some(content_hash(&bytes).as_str()) {
                                notes.push(RestoreNote::FileChangedOnDisk);
                            }
                        }
                        Err(e) => notes.push(file_note(e.into())),
                    }
                } else {
                    match read_sql_file(path) {
                        Ok(read) => {
                            tab.text = read.text;
                            file.hash = Some(read.hash);
                            file.encoding = read.format.encoding;
                            file.line_ending = read.format.line_ending;
                        }
                        Err(e) => {
                            file.modified = true;
                            notes.push(file_note(e));
                        }
                    }
                }
            }
            RestoredTab { tab, notes }
        })
        .collect();
    (tabs, active)
}

fn file_note(error: FileError) -> RestoreNote {
    match error {
        FileError::NotFound => RestoreNote::FileMissing,
        other => RestoreNote::FileUnreadable {
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_file::{FileFormat, SaveCheck, write_sql_file};
    use spelaion_store::{FileEncoding, LineEnding, SavedFile, Store};
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-open-tabs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn console(text: &str) -> SavedTab {
        SavedTab {
            text: text.into(),
            ..SavedTab::default()
        }
    }

    fn tabs(texts: &[&str]) -> SavedTabs {
        SavedTabs {
            tabs: texts.iter().map(|t| console(t)).collect(),
            active: Some(0),
        }
    }

    /// What another process (the next start of the app) reads from the store file.
    fn stored(db: &Path, workspace: &str) -> Vec<String> {
        Store::open(db)
            .unwrap()
            .open_tabs(workspace)
            .unwrap()
            .tabs
            .into_iter()
            .map(|t| t.text)
            .collect()
    }

    /// Lets the keeper and the store's thread finish what is due now, such as taking an update
    /// in (time is paused: only real work happens here, the clock does not move).
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    async fn advance(by: Duration) {
        tokio::time::advance(by).await;
        settle().await;
    }

    #[tokio::test(start_paused = true)]
    async fn tabs_are_written_within_the_delay_and_a_crash_loses_at_most_that_window() {
        let dir = temp("window");
        let db = dir.join("state.db");
        let history = QueryHistory::open(db.clone()).unwrap();
        let keeper = TabKeeper::spawn(history);

        keeper.update("/repo".into(), tabs(&["select 1;"]));

        settle().await;
        advance(TAB_SAVE_DELAY - Duration::from_millis(100)).await;
        assert!(stored(&db, "/repo").is_empty(), "written before the delay");
        advance(Duration::from_millis(200)).await;
        assert_eq!(stored(&db, "/repo"), ["select 1;"]);

        // Typing without a pause: a deadline, not a debounce, so it is still written every
        // delay, and what is on disk is never older than the delay.
        let mut last_written_at = 0u64;
        for step in 1..=20u64 {
            keeper.update("/repo".into(), tabs(&[&format!("select {step};")]));
            settle().await;
            advance(Duration::from_millis(150)).await;
            let now = step * 150;
            if stored(&db, "/repo") == [format!("select {step};")] {
                last_written_at = now;
            }
            assert!(
                now - last_written_at <= 1_150,
                "nothing written for {} ms",
                now - last_written_at
            );
        }

        // A crash: what was sent less than the delay ago is lost, and only that.
        advance(TAB_SAVE_DELAY).await;
        assert_eq!(stored(&db, "/repo"), ["select 20;"]);
        keeper.update("/repo".into(), tabs(&["select 'lost';"]));
        settle().await;
        advance(Duration::from_millis(500)).await;
        keeper.crash();
        advance(TAB_SAVE_DELAY * 2).await;
        assert_eq!(stored(&db, "/repo"), ["select 20;"]);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_writes_at_once_and_switching_workspaces_keeps_both() {
        let dir = temp("flush");
        let db = dir.join("state.db");
        let keeper = TabKeeper::spawn(QueryHistory::open(db.clone()).unwrap());

        keeper.update("/a".into(), tabs(&["in a"]));
        // The next folder's tabs arrive before A's deadline: A is written first, not dropped.
        keeper.update("/b".into(), tabs(&["in b", "also b"]));
        settle().await;
        assert_eq!(stored(&db, "/a"), ["in a"]);
        assert!(stored(&db, "/b").is_empty());
        keeper.flush().await.unwrap();
        assert_eq!(stored(&db, "/b"), ["in b", "also b"]);
        // Nothing waiting: a flush is a no-op.
        keeper.flush().await.unwrap();

        // Dropping every handle (the app closing) writes what is left.
        keeper.update("/b".into(), tabs(&["closing"]));
        drop(keeper);
        settle().await;
        assert_eq!(stored(&db, "/b"), ["closing"]);
    }

    fn file_tab(path: &Path, text: &str, hash: Option<String>, modified: bool) -> SavedTab {
        SavedTab {
            text: text.into(),
            connection: Some("shop".into()),
            file: Some(SavedFile {
                path: path.display().to_string(),
                hash,
                encoding: FileEncoding::Utf8,
                line_ending: LineEnding::Lf,
                modified,
            }),
            ..SavedTab::default()
        }
    }

    #[test]
    fn restore_keeps_the_text_of_a_tab_whose_connection_is_gone() {
        let saved = SavedTabs {
            tabs: vec![
                SavedTab {
                    connection: Some("~/deleted".into()),
                    title: Some("Monthly".into()),
                    pinned: true,
                    ..console("select * from orders;")
                },
                SavedTab {
                    connection: Some("shop".into()),
                    ..console("select 1;")
                },
                console("-- no connection"),
            ],
            active: Some(2),
        };
        let (restored, active) = restore_tabs(saved, |id| id == "shop");
        assert_eq!(active, Some(2));
        assert_eq!(restored.len(), 3);
        assert_eq!(restored[0].tab.text, "select * from orders;");
        assert_eq!(restored[0].tab.connection, None);
        assert_eq!(restored[0].tab.title.as_deref(), Some("Monthly"));
        assert!(restored[0].tab.pinned);
        assert_eq!(
            restored[0].notes,
            [RestoreNote::ConnectionMissing {
                connection: "~/deleted".into()
            }]
        );
        assert_eq!(restored[1].tab.connection.as_deref(), Some("shop"));
        assert!(restored[1].notes.is_empty());
        assert!(restored[2].notes.is_empty());
        // An active index past the end (a store from elsewhere) shows no tab rather than panic.
        let (_, active) = restore_tabs(
            SavedTabs {
                tabs: vec![console("x")],
                active: Some(5),
            },
            |_| true,
        );
        assert_eq!(active, None);
    }

    #[test]
    fn restore_reads_clean_files_again_and_keeps_unsaved_text() {
        let dir = temp("files");
        let clean = dir.join("clean.sql");
        let dirty = dir.join("dirty.sql");
        let gone = dir.join("gone.sql");
        let format = FileFormat {
            encoding: FileEncoding::Utf8,
            line_ending: LineEnding::Lf,
        };
        let clean_hash =
            write_sql_file(&clean, "select 'old';", format, &SaveCheck::Overwrite).unwrap();
        let dirty_hash =
            write_sql_file(&dirty, "select 'base';", format, &SaveCheck::Overwrite).unwrap();
        // While the app was closed: a pull changes both files, and one is deleted.
        std::fs::write(&clean, b"\xEF\xBB\xBFselect 'pulled';\r\n").unwrap();
        std::fs::write(&dirty, "select 'pulled';").unwrap();

        let saved = SavedTabs {
            tabs: vec![
                file_tab(&clean, "select 'old';", Some(clean_hash), false),
                file_tab(&dirty, "select 'mine, unsaved';", Some(dirty_hash), true),
                file_tab(&gone, "select 'kept';", Some("feed".into()), false),
            ],
            active: Some(1),
        };
        let (restored, _) = restore_tabs(saved, |_| true);

        // Clean: the file as it is now, in its new format, still clean.
        let tab = &restored[0];
        assert_eq!(tab.tab.text, "select 'pulled';\n");
        let file = tab.tab.file.as_ref().unwrap();
        assert!(!file.modified);
        assert_eq!(file.encoding, FileEncoding::Utf8Bom);
        assert_eq!(file.line_ending, LineEnding::CrLf);
        assert!(tab.notes.is_empty());

        // Unsaved: the text stays, still unsaved, and the change on disk is noted.
        let tab = &restored[1];
        assert_eq!(tab.tab.text, "select 'mine, unsaved';");
        assert!(tab.tab.file.as_ref().unwrap().modified);
        assert_eq!(tab.notes, [RestoreNote::FileChangedOnDisk]);

        // Gone: the kept text comes back as unsaved, with a note.
        let tab = &restored[2];
        assert_eq!(tab.tab.text, "select 'kept';");
        assert!(tab.tab.file.as_ref().unwrap().modified);
        assert_eq!(tab.notes, [RestoreNote::FileMissing]);
        assert_eq!(tab.tab.connection.as_deref(), Some("shop"));
    }
}
