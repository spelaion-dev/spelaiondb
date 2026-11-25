//! The app's windows (SDB-30, ADR-083). Every window is another view over the one core: the
//! sessions, the results and their tabs stay in `AppState`, never copied per window.
//!
//! This registry only says which windows are open, what each one shows, and where its closing
//! stands, as one state per window with its transitions here (ADR-060). Which window opened a
//! session is kept with the session itself (`DesktopSession::window`), so closing a window
//! closes the sessions it opened and none of another window's.
//!
//! The main window is the one the app starts with and quits with. A connection window shows
//! one connection, with its own tabs and explorer; a results window shows the results of one
//! editor tab of another window, read from the core by result id like the tab's own grid.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_desktop_macros::command;
use tauri::{Manager, State};
use tauri_specta::Event as _;
use tokio::sync::oneshot;

use crate::dto::ConnectionKeyItem;
use crate::{AppState, CmdResult};

/// The main window's label (`tauri.conf.json`).
pub const MAIN: &str = "main";

/// What a window shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowKind {
    Main,
    /// One connection, by its identity (ADR-075): the window's explorer and pickers offer
    /// only it.
    Connection {
        connection: ConnectionKeyItem,
    },
    /// The results of the editor tab `tab` (its session key) of the window `owner`.
    Results {
        owner: String,
        tab: String,
    },
}

/// Where a window's closing stands.
#[derive(Debug)]
enum Closing {
    /// Open; nobody asked it to close.
    Open,
    /// Its page was asked to close (it asks its own questions first). `quit` is the main
    /// window's quit waiting for the answer; none when the user closed the window itself.
    Asked { quit: Option<oneshot::Sender<bool>> },
}

#[derive(Debug)]
struct Entry {
    kind: WindowKind,
    closing: Closing,
}

#[derive(Debug, Default)]
struct Registry {
    /// The number in the next window's label.
    next: u32,
    open: HashMap<String, Entry>,
}

/// What to do with a close request from a window's title bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// Nothing in it can be lost: it closes now.
    Close,
    /// Its page decides, as the main window's does before quitting.
    AskThePage,
}

/// The open windows. Its lock is never held across an `.await`.
#[derive(Debug)]
pub struct Windows {
    inner: Mutex<Registry>,
}

impl Default for Windows {
    fn default() -> Self {
        Self::new()
    }
}

impl Windows {
    /// The registry with the main window in it.
    pub fn new() -> Self {
        let mut open = HashMap::new();
        open.insert(
            MAIN.to_owned(),
            Entry {
                kind: WindowKind::Main,
                closing: Closing::Open,
            },
        );
        Self {
            inner: Mutex::new(Registry { next: 1, open }),
        }
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.inner.lock().expect("lock poisoned")
    }

    /// Adds a window that is about to be built, and gives it its label.
    pub fn add(&self, kind: WindowKind) -> String {
        let mut registry = self.registry();
        let prefix = match &kind {
            WindowKind::Main => return MAIN.to_owned(),
            WindowKind::Connection { .. } => "connection",
            WindowKind::Results { .. } => "results",
        };
        let label = format!("{prefix}-{}", registry.next);
        registry.next += 1;
        registry.open.insert(
            label.clone(),
            Entry {
                kind,
                closing: Closing::Open,
            },
        );
        label
    }

    /// Takes out a window that could not be built.
    pub fn forget(&self, label: &str) {
        self.registry().open.remove(label);
    }

    pub fn kind(&self, label: &str) -> Option<WindowKind> {
        self.registry()
            .open
            .get(label)
            .map(|entry| entry.kind.clone())
    }

    /// The results window already open for `owner`'s tab `tab`, if any.
    pub fn results_window(&self, owner: &str, tab: &str) -> Option<String> {
        self.registry()
            .open
            .iter()
            .find(|(_, entry)| {
                matches!(&entry.kind, WindowKind::Results { owner: o, tab: t } if o == owner && t == tab)
            })
            .map(|(label, _)| label.clone())
    }

    /// The connection windows open, in label order: what quitting asks to close first.
    pub fn connection_windows(&self) -> Vec<String> {
        let mut labels: Vec<String> = self
            .registry()
            .open
            .iter()
            .filter(|(_, entry)| matches!(entry.kind, WindowKind::Connection { .. }))
            .map(|(label, _)| label.clone())
            .collect();
        labels.sort();
        labels
    }

    /// The user asked to close a window other than the main one (its title bar). A results
    /// window holds nothing of its own and closes. A connection window's page asks first (its
    /// tabs' SQL is kept nowhere else, its sessions may hold transactions or edits) and closes
    /// at once when nothing would be lost; a second request while its page has not answered (it
    /// is frozen) closes it, as a second request quits the main window.
    pub fn close_requested(&self, label: &str) -> CloseDecision {
        let mut registry = self.registry();
        let Some(entry) = registry.open.get_mut(label) else {
            return CloseDecision::Close;
        };
        match (&entry.kind, &entry.closing) {
            (WindowKind::Main, _) | (WindowKind::Results { .. }, _) => CloseDecision::Close,
            (WindowKind::Connection { .. }, Closing::Asked { .. }) => CloseDecision::Close,
            (WindowKind::Connection { .. }, Closing::Open) => {
                entry.closing = Closing::Asked { quit: None };
                CloseDecision::AskThePage
            }
        }
    }

    /// Quitting asks this window's page to close: the answer arrives when the window is gone
    /// (`true`) or its page stays open (`false`). `None`: the window is not open.
    pub fn ask_to_close(&self, label: &str) -> Option<oneshot::Receiver<bool>> {
        let mut registry = self.registry();
        let entry = registry.open.get_mut(label)?;
        let (tx, rx) = oneshot::channel();
        entry.closing = Closing::Asked { quit: Some(tx) };
        Some(rx)
    }

    /// The page answered that it stays open (the user cancelled its question).
    pub fn stay_open(&self, label: &str) {
        let mut registry = self.registry();
        if let Some(entry) = registry.open.get_mut(label) {
            let closing = std::mem::replace(&mut entry.closing, Closing::Open);
            match closing {
                Closing::Asked { quit: Some(quit) } => {
                    let _ = quit.send(false);
                }
                Closing::Asked { quit: None } | Closing::Open => {}
            }
        }
    }

    /// The window is gone. Answers a quit that waited for it, and returns the results windows
    /// it owned, which go with it.
    pub fn closed(&self, label: &str) -> Vec<String> {
        let mut registry = self.registry();
        if let Some(entry) = registry.open.remove(label) {
            match entry.closing {
                Closing::Asked { quit: Some(quit) } => {
                    let _ = quit.send(true);
                }
                Closing::Asked { quit: None } | Closing::Open => {}
            }
        }
        let mut owned: Vec<String> = registry
            .open
            .iter()
            .filter(|(_, entry)| matches!(&entry.kind, WindowKind::Results { owner, .. } if owner == label))
            .map(|(label, _)| label.clone())
            .collect();
        owned.sort();
        owned
    }
}

// ------------------------------------------------------------------ the adapter's commands

/// What the page of a window shows, as the registry says it (ADR-060: read, never guessed).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WindowContextItem {
    Main,
    /// `label` prefixes the window's session keys (`connection-2/tab-1`), so no two windows
    /// ever name the same session.
    #[serde(rename_all = "camelCase")]
    Connection {
        label: String,
        connection: ConnectionKeyItem,
    },
    /// The results of `owner`'s editor tab `tab` (its session key).
    #[serde(rename_all = "camelCase")]
    Results {
        label: String,
        owner: String,
        tab: String,
    },
}

/// A window other than the main one was asked to close while its sessions are open (its title
/// bar, or quitting): its page looks for what closing would lose, asks, and calls
/// `close_window` or `window_stays_open`. Sent only to that window.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
pub struct WindowCloseRequested {}

/// The results window of the editor tab `tab` closed: the tab shows its results again. Sent
/// only to the window that owns the tab.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct ResultsWindowClosed {
    pub tab: String,
}

#[command]
pub fn window_context(window: tauri::Window, state: State<'_, AppState>) -> WindowContextItem {
    let label = window.label().to_owned();
    match state.windows.kind(&label) {
        Some(WindowKind::Connection { connection }) => {
            WindowContextItem::Connection { label, connection }
        }
        Some(WindowKind::Results { owner, tab }) => {
            WindowContextItem::Results { label, owner, tab }
        }
        Some(WindowKind::Main) | None => WindowContextItem::Main,
    }
}

/// Opens `connection` in a window of its own, titled `title`: its own tabs and explorer, its
/// own sessions, over the same core. Nothing is connected or run until its page asks.
#[command]
pub async fn open_connection_window(
    connection: ConnectionKeyItem,
    title: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    // Only a connection of the folder open gets a window (ADR-075).
    crate::connection_in(&*state.config().await, &connection)?;
    let label = state.windows.add(WindowKind::Connection { connection });
    let size = interface_size(&state).await;
    if let Err(e) = crate::open_window(&app, &label, &title, size) {
        state.windows.forget(&label);
        return Err(format!("could not open the window: {e}"));
    }
    Ok(label)
}

/// Shows the results of the calling window's editor tab `tab` in a window of their own, or
/// brings forward the one already showing them. The rows stay in the core's store: the new
/// window asks for windows of the same results by id, and nothing is run again.
#[command]
pub async fn open_results_window(
    tab: String,
    title: String,
    window: tauri::Window,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let owner = window.label().to_owned();
    let app = window.app_handle().clone();
    if let Some(label) = state.windows.results_window(&owner, &tab)
        && let Some(open) = app.get_webview_window(&label)
    {
        let _ = open.unminimize();
        let _ = open.set_focus();
        return Ok(label);
    }
    let label = state.windows.add(WindowKind::Results { owner, tab });
    let size = interface_size(&state).await;
    if let Err(e) = crate::open_window(&app, &label, &title, size) {
        state.windows.forget(&label);
        return Err(format!("could not open the window: {e}"));
    }
    Ok(label)
}

/// Closes the results window of the calling window's tab `tab` ("Re-attach results"): the
/// tab hears `ResultsWindowClosed` and shows them again. Nothing if there is none.
#[command]
pub async fn close_results_window(
    tab: String,
    window: tauri::Window,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let app = window.app_handle().clone();
    match state.windows.results_window(window.label(), &tab) {
        Some(label) => match app.get_webview_window(&label) {
            Some(results) => results.destroy().map_err(|e| e.to_string()),
            // Already gone without its event: the tab shows its results again now.
            None => {
                state.windows.closed(&label);
                let _ = ResultsWindowClosed { tab }.emit_to(&app, window.label());
                Ok(())
            }
        },
        None => {
            let _ = ResultsWindowClosed { tab }.emit_to(&app, window.label());
            Ok(())
        }
    }
}

/// Closes the calling window (not the main one, which quits): the sessions it opened close
/// first, through the same path as closing their tabs. Another window's sessions are not
/// touched. Its page has asked whatever closing would lose.
#[command]
pub async fn close_window(window: tauri::Window, state: State<'_, AppState>) -> CmdResult<()> {
    if window.label() == MAIN {
        return Err("the main window quits instead".into());
    }
    crate::close_window_sessions(&state, window.label()).await;
    window.destroy().map_err(|e| e.to_string())
}

/// The calling window's page went back from its closing question.
#[command]
pub async fn window_stays_open(window: tauri::Window, state: State<'_, AppState>) -> CmdResult<()> {
    state.windows.stay_open(window.label());
    Ok(())
}

/// Before the main window quits, every connection window is asked to close, one at a time,
/// each asking its own questions (its page closes at once when nothing would be lost).
/// `false`: one stayed open, and quitting stops. A window whose page never answers is closed
/// by a second click on its own close button, which answers `true` here.
#[command]
pub async fn close_other_windows(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<bool> {
    for label in state.windows.connection_windows() {
        let Some(window) = app.get_webview_window(&label) else {
            state.windows.closed(&label);
            continue;
        };
        let Some(answer) = state.windows.ask_to_close(&label) else {
            continue;
        };
        let _ = window.unminimize();
        let _ = window.set_focus();
        let asked = WindowCloseRequested {}.emit_to(&app, label.as_str());
        if asked.is_err() {
            state.windows.stay_open(&label);
            return Ok(false);
        }
        // Gone (true), or its page stayed open (false). A dropped sender is a window that
        // went away without saying it: it is gone.
        if !answer.await.unwrap_or(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The interface size every window opens at (SDB-389); the normal size if it cannot be read.
async fn interface_size(state: &AppState) -> u16 {
    state
        .history
        .interface_size()
        .await
        .unwrap_or(spelaion_core::DEFAULT_INTERFACE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(id: &str) -> WindowKind {
        WindowKind::Connection {
            connection: ConnectionKeyItem {
                folder: None,
                id: id.to_owned(),
            },
        }
    }

    fn results(owner: &str, tab: &str) -> WindowKind {
        WindowKind::Results {
            owner: owner.to_owned(),
            tab: tab.to_owned(),
        }
    }

    #[test]
    fn windows_get_labels_of_their_kind_and_the_main_one_is_there_from_the_start() {
        let windows = Windows::new();
        assert_eq!(windows.kind(MAIN), Some(WindowKind::Main));
        assert_eq!(windows.add(connection("shop")), "connection-1");
        assert_eq!(windows.add(results(MAIN, "tab-3")), "results-2");
        assert_eq!(windows.add(WindowKind::Main), MAIN);
        assert_eq!(windows.kind("connection-1"), Some(connection("shop")));
        assert_eq!(
            windows.results_window(MAIN, "tab-3").as_deref(),
            Some("results-2")
        );
        assert_eq!(windows.results_window(MAIN, "tab-4"), None);
        assert_eq!(
            windows.connection_windows(),
            vec!["connection-1".to_owned()]
        );
        windows.forget("connection-1");
        assert_eq!(windows.kind("connection-1"), None);
        // A label is never given twice, even after its window is gone.
        assert_eq!(windows.add(connection("shop")), "connection-3");
    }

    #[test]
    fn a_connection_window_asks_its_page_and_a_second_request_closes_it() {
        let windows = Windows::new();
        let label = windows.add(connection("shop"));
        assert_eq!(windows.close_requested(&label), CloseDecision::AskThePage);
        assert_eq!(windows.close_requested(&label), CloseDecision::Close);
        // Cancelled: the next request asks again.
        windows.stay_open(&label);
        assert_eq!(windows.close_requested(&label), CloseDecision::AskThePage);
        // A results window and an unknown one close at once.
        let shown = windows.add(results(&label, "tab-1"));
        assert_eq!(windows.close_requested(&shown), CloseDecision::Close);
        assert_eq!(windows.close_requested("nowhere"), CloseDecision::Close);
    }

    #[test]
    fn quitting_hears_whether_each_window_closed_or_stayed() {
        let windows = Windows::new();
        let first = windows.add(connection("shop"));
        let second = windows.add(connection("crm"));
        let mut stayed = windows.ask_to_close(&first).expect("open");
        let mut gone = windows.ask_to_close(&second).expect("open");
        windows.stay_open(&first);
        assert_eq!(stayed.try_recv(), Ok(false));
        assert!(windows.closed(&second).is_empty());
        assert_eq!(gone.try_recv(), Ok(true));
        assert!(windows.ask_to_close(&second).is_none());
        assert_eq!(windows.connection_windows(), vec![first]);
    }

    #[test]
    fn a_window_takes_its_results_windows_with_it_and_no_one_elses() {
        let windows = Windows::new();
        let owner = windows.add(connection("shop"));
        let mine = windows.add(results(&owner, "connection-1/tab-1"));
        let main_results = windows.add(results(MAIN, "tab-2"));
        assert_eq!(windows.closed(&owner), vec![mine.clone()]);
        assert_eq!(windows.kind(&main_results), Some(results(MAIN, "tab-2")));
        // Closing a results window answers nobody and owns nothing.
        assert!(windows.closed(&mine).is_empty());
    }
}
