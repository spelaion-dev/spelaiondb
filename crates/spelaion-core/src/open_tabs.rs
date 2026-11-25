//! Open editor tabs that survive a restart (ADR-036).
//!
//! [`TabKeeper`] decides how often the tabs reach the local store: at most [`TAB_SAVE_DELAY`]
//! after the first change it has not written, so a crash loses at most that window.
//! [`restore_tabs`] checks the kept tabs before the app shows them again: they come back
//! disconnected, with their text and nothing else, and with a note when their connection or
//! their file is gone. Nothing here runs anything.
//!
//! A tab belongs to the folder of its connection (ADR-075, amending ADR-036 point 3): a tab on
//! a folder's connection is kept under that folder ([`tab_scope`]), and one on a personal
//! connection, or with none, under the window's empty key, whatever folders open or close.
//! [`window_sets`] splits a window's tabs that way, with their place in the window;
//! [`restore_window_tabs`] puts the open folders' and the window's tabs back in that order,
//! each connection looked up in its own folder only.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use spelaion_config::{ConnectionId, Source};
use spelaion_store::{SavedTab, SavedTabs};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::sql_file::{FileError, content_hash, read_sql_file};
use crate::{HistoryError, QueryHistory};

/// Longest time between a change to the tabs and its write to the store.
pub const TAB_SAVE_DELAY: Duration = Duration::from_secs(1);

enum KeeperRequest {
    /// Each workspace's tabs as they are now; workspaces not named keep what they have.
    Update { sets: Vec<(String, SavedTabs)> },
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
        self.update_window(vec![(workspace, tabs)]);
    }

    /// A window's tabs as they are now, each workspace's set ([`window_sets`]): written within
    /// [`TAB_SAVE_DELAY`], together. Workspaces not named (closed folders) keep their tabs.
    pub fn update_window(&self, sets: Vec<(String, SavedTabs)>) {
        let _ = self.requests.send(KeeperRequest::Update { sets });
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
    let mut pending: BTreeMap<String, SavedTabs> = BTreeMap::new();
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
            Some(KeeperRequest::Update { sets }) => {
                // The newest set of each workspace; others still waiting are written with them.
                pending.extend(sets);
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

/// Writes the waiting tabs, every workspace's in one transaction. On failure they stay waiting
/// for the next update or flush (newer sets replace them), and the error is kept for the next
/// flush.
async fn write(
    history: &QueryHistory,
    pending: &mut BTreeMap<String, SavedTabs>,
    last_error: &mut Option<HistoryError>,
) {
    if pending.is_empty() {
        return;
    }
    let sets: Vec<(String, SavedTabs)> = std::mem::take(pending).into_iter().collect();
    match history.save_window_tabs(sets.clone()).await {
        Ok(()) => *last_error = None,
        Err(e) => {
            *last_error = Some(e);
            for (workspace, tabs) in sets {
                pending.entry(workspace).or_insert(tabs);
            }
        }
    }
}

/// The workspace a tab is kept under (ADR-075): the folder of its connection (`folder`, its
/// folder's text as the store keys it) for a folder's connection; the window's empty key for a
/// personal connection or none.
pub fn tab_scope(connection: Option<&str>, folder: Option<&str>) -> String {
    match connection.map(ConnectionId::parse) {
        Some(id) if id.source == Source::Workspace => folder.unwrap_or_default().to_string(),
        Some(_) | None => String::new(),
    }
}

/// A window's tabs (in the tab bar's order, each with the workspace [`tab_scope`] gives it)
/// split into each workspace's set, every tab with its place in the window. `open` names the
/// workspaces the window has open (the empty key and each open folder's): each gets a set, an
/// empty one if none of the tabs is its, so a folder whose tabs were all closed keeps none.
/// `active` is the index of the tab shown, which stays the active one of its set.
pub fn window_sets(
    open: &[String],
    tabs: Vec<(String, SavedTab)>,
    active: Option<usize>,
) -> Vec<(String, SavedTabs)> {
    let mut sets: BTreeMap<String, SavedTabs> = open
        .iter()
        .map(|scope| (scope.clone(), SavedTabs::default()))
        .collect();
    for (index, (scope, mut tab)) in tabs.into_iter().enumerate() {
        tab.window_position = u32::try_from(index).ok();
        let set = sets.entry(scope).or_default();
        if active == Some(index) {
            set.active = Some(set.tabs.len());
        }
        set.tabs.push(tab);
    }
    sets.into_iter().collect()
}

/// The tabs kept for each workspace the window opens (`sets`: the open folders', in the order
/// they were opened, then the window's empty key), checked as [`restore_tabs`] does and put
/// back in the window's order: by their place in the window, then the tabs kept before that
/// existed, folder by folder in their own order. A connection is looked up in the workspace
/// it was kept under (`connection_exists(workspace, id)`), so a folder's tab never comes back
/// on another folder's connection of the same id; so is the other database it was on
/// (`database_opened(workspace, id, database)`). Each tab comes with its workspace; the active
/// one is the first of them that was shown.
pub fn restore_window_tabs(
    sets: Vec<(String, SavedTabs)>,
    connection_exists: impl Fn(&str, &str) -> bool,
    database_opened: impl Fn(&str, &str, &str) -> bool,
) -> (Vec<(String, RestoredTab)>, Option<usize>) {
    /// A kept tab with where it goes back.
    struct Kept {
        /// Placed tabs first, by their place in the window; then the ones kept before places
        /// existed, workspace by workspace, in their own order.
        order: (u8, usize, usize, usize),
        scope: String,
        tab: SavedTab,
        shown: bool,
    }
    let mut kept: Vec<Kept> = Vec::new();
    for (set_index, (scope, saved)) in sets.into_iter().enumerate() {
        let active = saved.active.filter(|i| *i < saved.tabs.len());
        for (position, tab) in saved.tabs.into_iter().enumerate() {
            let order = match tab.window_position {
                Some(place) => (0, place as usize, set_index, position),
                None => (1, set_index, position, 0),
            };
            kept.push(Kept {
                order,
                scope: scope.clone(),
                tab,
                shown: active == Some(position),
            });
        }
    }
    kept.sort_by_key(|k| k.order);
    let active = kept.iter().position(|k| k.shown);
    let tabs = kept
        .into_iter()
        .map(|k| {
            let restored = restore_tab(
                k.tab,
                |id| connection_exists(&k.scope, id),
                |id, database| database_opened(&k.scope, id, database),
            );
            (k.scope, restored)
        })
        .collect();
    (tabs, active)
}

/// Why a restored tab is not exactly as it was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreNote {
    /// Its connection was deleted or renamed: the tab has no connection now.
    ConnectionMissing { connection: String },
    /// Its connection no longer opens the other database it was on (ADR-074, SDB-505): the tab
    /// is on its connection's own database now.
    DatabaseMissing { database: String },
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
/// - The other database it was on, if `database_opened` says its connection no longer opens
///   it, is taken off the tab, with a note: it comes back on its connection's own database.
/// - A file tab without unsaved changes is read again from disk, so it shows the file as it is
///   now; if that fails, its kept text comes back as unsaved, with a note.
/// - A file tab with unsaved changes keeps its text; a note says if the file changed since.
///
/// The tabs carry their text only: there is nothing in them to run, and no result, session or
/// transaction to bring back.
pub fn restore_tabs(
    saved: SavedTabs,
    connection_exists: impl Fn(&str) -> bool,
    database_opened: impl Fn(&str, &str) -> bool,
) -> (Vec<RestoredTab>, Option<usize>) {
    let active = saved.active.filter(|i| *i < saved.tabs.len());
    let tabs = saved
        .tabs
        .into_iter()
        .map(|tab| restore_tab(tab, &connection_exists, &database_opened))
        .collect();
    (tabs, active)
}

/// One kept tab, checked ([`restore_tabs`]).
fn restore_tab(
    mut tab: SavedTab,
    connection_exists: impl Fn(&str) -> bool,
    database_opened: impl Fn(&str, &str) -> bool,
) -> RestoredTab {
    let mut notes = Vec::new();
    if let Some(connection) = tab.connection.take() {
        if connection_exists(&connection) {
            tab.connection = Some(connection);
        } else {
            notes.push(RestoreNote::ConnectionMissing { connection });
        }
    }
    // A database is only ever of a connection: without one it goes with it.
    match (tab.connection.as_deref(), tab.database.take()) {
        (Some(connection), Some(database)) if database_opened(connection, &database) => {
            tab.database = Some(database);
        }
        (Some(_), Some(database)) => notes.push(RestoreNote::DatabaseMissing { database }),
        (None, _) | (_, None) => {}
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
        // The next folder's tabs arrive before A's deadline: both wait, neither is dropped, and
        // they are written together (ADR-075: a window's folders in one transaction).
        keeper.update("/b".into(), tabs(&["in b", "also b"]));
        settle().await;
        assert!(stored(&db, "/a").is_empty());
        keeper.flush().await.unwrap();
        assert_eq!(stored(&db, "/a"), ["in a"]);
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

    /// ADR-074, SDB-505: a tab keeps the other database it was on while its connection still
    /// opens it; otherwise it comes back on the connection's own database, with a note.
    #[test]
    fn restore_keeps_a_tabs_database_or_says_it_is_gone() {
        let on = |database: &str| SavedTab {
            connection: Some("tenants".into()),
            database: Some(database.into()),
            ..console("select 1;")
        };
        let saved = SavedTabs {
            tabs: vec![
                on("billing"),
                on("closed"),
                SavedTab {
                    connection: Some("~/deleted".into()),
                    ..on("billing")
                },
            ],
            active: Some(0),
        };
        let (restored, _) = restore_tabs(
            saved,
            |id| id == "tenants",
            |id, database| id == "tenants" && database == "billing",
        );
        assert_eq!(restored[0].tab.database.as_deref(), Some("billing"));
        assert!(restored[0].notes.is_empty());
        assert_eq!(restored[1].tab.connection.as_deref(), Some("tenants"));
        assert_eq!(restored[1].tab.database, None);
        assert_eq!(
            restored[1].notes,
            [RestoreNote::DatabaseMissing {
                database: "closed".into()
            }]
        );
        // Without its connection, its database goes too, under the connection's note.
        assert_eq!(restored[2].tab.database, None);
        assert!(matches!(
            restored[2].notes[..],
            [RestoreNote::ConnectionMissing { .. }]
        ));
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
        let (restored, active) = restore_tabs(saved, |id| id == "shop", |_, _| true);
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
            |_, _| true,
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
        let (restored, _) = restore_tabs(saved, |_| true, |_, _| true);

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

    // ------------------------------------------------ tabs belong to their folder (ADR-075)

    /// A console on `connection` (none: no connection).
    fn on(text: &str, connection: Option<&str>) -> SavedTab {
        SavedTab {
            text: text.into(),
            connection: connection.map(Into::into),
            ..SavedTab::default()
        }
    }

    /// A window's tabs, each with the folder its connection is from, as the page sends them.
    fn window(tabs: &[(&str, Option<&str>, Option<&str>)]) -> Vec<(String, SavedTab)> {
        tabs.iter()
            .map(|(text, connection, folder)| {
                (tab_scope(*connection, *folder), on(text, *connection))
            })
            .collect()
    }

    fn texts(restored: &[(String, RestoredTab)]) -> Vec<String> {
        restored.iter().map(|(_, t)| t.tab.text.clone()).collect()
    }

    /// What the store keeps for each of `scopes`, as the next start reads it.
    fn kept(store: &Store, scopes: &[&str]) -> Vec<(String, SavedTabs)> {
        scopes
            .iter()
            .map(|scope| (scope.to_string(), store.open_tabs(scope).unwrap()))
            .collect()
    }

    #[test]
    fn a_tab_is_kept_under_its_connections_folder_and_a_personal_one_under_the_window() {
        assert_eq!(tab_scope(Some("prod/orders"), Some("/work/a")), "/work/a");
        assert_eq!(
            tab_scope(Some("~/local"), Some("/work/a")),
            "",
            "a personal connection"
        );
        assert_eq!(tab_scope(None, Some("/work/a")), "", "no connection");
        assert_eq!(tab_scope(None, None), "");
    }

    #[test]
    fn two_folders_and_the_window_are_saved_each_under_its_key_and_come_back_in_order() {
        let dir = temp("window-sets");
        let mut store = Store::open(&dir.join("state.db")).unwrap();
        let tabs = window(&[
            ("a1", Some("prod/orders"), Some("/a")),
            ("mine", Some("~/local"), None),
            ("b1", Some("prod/orders"), Some("/b")),
            ("scratch", None, None),
            ("a2", Some("crm"), Some("/a")),
        ]);
        let open = ["/a".to_string(), "/b".to_string(), String::new()];
        store
            .save_window_tabs(&window_sets(&open, tabs, Some(2)))
            .unwrap();

        // Each under its key.
        let only = |scope: &str| -> Vec<String> {
            store
                .open_tabs(scope)
                .unwrap()
                .tabs
                .into_iter()
                .map(|t| t.text)
                .collect()
        };
        assert_eq!(only("/a"), ["a1", "a2"]);
        assert_eq!(only("/b"), ["b1"]);
        assert_eq!(only(""), ["mine", "scratch"]);

        // The next start: the window's order across folders, and the tab that was shown.
        let (restored, active) =
            restore_window_tabs(kept(&store, &["/a", "/b", ""]), |_, _| true, |_, _, _| true);
        assert_eq!(texts(&restored), ["a1", "mine", "b1", "scratch", "a2"]);
        assert_eq!(active, Some(2));
        let scopes: Vec<&str> = restored.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(scopes, ["/a", "", "/b", "", "/a"]);
    }

    #[test]
    fn closing_a_folder_keeps_its_tabs_for_next_time_and_leaves_the_others_open() {
        let dir = temp("close-folder");
        let mut store = Store::open(&dir.join("state.db")).unwrap();
        let both = ["/a".to_string(), "/b".to_string(), String::new()];
        let tabs = window(&[
            ("a1", Some("orders"), Some("/a")),
            ("b1", Some("orders"), Some("/b")),
            ("mine", Some("~/local"), None),
        ]);
        store
            .save_window_tabs(&window_sets(&both, tabs, Some(0)))
            .unwrap();

        // A is closed: the window keeps B's tab and the personal one, and only those are
        // written, so A's set stays as it was.
        let without_a = ["/b".to_string(), String::new()];
        let tabs = window(&[
            ("b1", Some("orders"), Some("/b")),
            ("mine", Some("~/local"), None),
        ]);
        store
            .save_window_tabs(&window_sets(&without_a, tabs, Some(1)))
            .unwrap();
        let (restored, _) =
            restore_window_tabs(kept(&store, &["/b", ""]), |_, _| true, |_, _, _| true);
        assert_eq!(texts(&restored), ["b1", "mine"]);

        // A opens again: its tab is back.
        let (restored, _) =
            restore_window_tabs(kept(&store, &["/a", "/b", ""]), |_, _| true, |_, _, _| true);
        assert!(texts(&restored).contains(&"a1".to_string()));

        // A folder whose tabs were all closed keeps none.
        store
            .save_window_tabs(&window_sets(&both, Vec::new(), None))
            .unwrap();
        assert!(store.open_tabs("/a").unwrap().tabs.is_empty());
    }

    #[test]
    fn two_folders_with_a_connection_of_the_same_id_never_exchange_tabs() {
        let sets = vec![
            (
                "/a".to_string(),
                SavedTabs {
                    tabs: vec![on("in a", Some("prod/orders"))],
                    active: None,
                },
            ),
            (
                "/b".to_string(),
                SavedTabs {
                    tabs: vec![on("in b", Some("prod/orders"))],
                    active: None,
                },
            ),
        ];
        // `prod/orders` was deleted from A only: A's tab loses its connection, B's keeps its own.
        let exists = |scope: &str, id: &str| scope == "/b" && id == "prod/orders";
        let (restored, _) = restore_window_tabs(sets, exists, |_, _, _| true);
        let (scope_a, a) = &restored[0];
        let (scope_b, b) = &restored[1];
        assert_eq!(
            (scope_a.as_str(), a.tab.connection.as_deref()),
            ("/a", None)
        );
        assert_eq!(
            a.notes,
            [RestoreNote::ConnectionMissing {
                connection: "prod/orders".into()
            }]
        );
        assert_eq!(
            (scope_b.as_str(), b.tab.connection.as_deref()),
            ("/b", Some("prod/orders"))
        );
    }

    #[test]
    fn tabs_kept_before_come_back_as_they_were_and_are_saved_again_under_the_new_rule() {
        let dir = temp("legacy-tabs");
        let mut store = Store::open(&dir.join("state.db")).unwrap();
        // As the previous version kept them: every tab under the folder that was open, a
        // personal one included, without a place in the window.
        store
            .save_open_tabs(
                "/a",
                &SavedTabs {
                    tabs: vec![
                        on("mine", Some("~/local")),
                        on("a1", Some("orders")),
                        on("scratch", None),
                    ],
                    active: Some(1),
                },
            )
            .unwrap();
        let (restored, active) =
            restore_window_tabs(kept(&store, &["/a", ""]), |_, _| true, |_, _, _| true);
        assert_eq!(texts(&restored), ["mine", "a1", "scratch"]);
        assert_eq!(active, Some(1));

        // Saved again: A keeps only its own tab; the personal one and the one without a
        // connection belong to the window, and the order is the same after the next start.
        let tabs: Vec<(String, SavedTab)> = restored
            .into_iter()
            .map(|(scope, t)| {
                let folder = (!scope.is_empty()).then_some(scope);
                (
                    tab_scope(t.tab.connection.as_deref(), folder.as_deref()),
                    t.tab,
                )
            })
            .collect();
        let open = ["/a".to_string(), String::new()];
        store
            .save_window_tabs(&window_sets(&open, tabs, Some(1)))
            .unwrap();
        let only = |scope: &str| -> Vec<String> {
            store
                .open_tabs(scope)
                .unwrap()
                .tabs
                .into_iter()
                .map(|t| t.text)
                .collect()
        };
        assert_eq!(only("/a"), ["a1"]);
        assert_eq!(only(""), ["mine", "scratch"]);
        let (restored, active) =
            restore_window_tabs(kept(&store, &["/a", ""]), |_, _| true, |_, _, _| true);
        assert_eq!(texts(&restored), ["mine", "a1", "scratch"]);
        assert_eq!(active, Some(1));
        // The personal tab now comes back with another folder open, or none.
        let (restored, _) =
            restore_window_tabs(kept(&store, &["/b", ""]), |_, _| true, |_, _, _| true);
        assert_eq!(texts(&restored), ["mine", "scratch"]);
    }
}
