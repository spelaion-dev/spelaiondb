//! Opening from outside the app (SDB-397): the command line at start, a second launch's
//! (handed over by `spelaion_core::instance`), files opened from macOS's Finder, and the
//! recent folders. What the words mean is the core's (`spelaion_core::launch`); the page opens
//! the folder through repository trust and the files as tabs, never run, never connected.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::launch::{LaunchRequest, parse};
use spelaion_desktop_macros::command;
use tauri::{Manager as _, State};
use tauri_specta::Event as _;

use crate::{AppState, CmdResult};

/// What a launch asks the page to open, already checked by the core.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequestItem {
    /// A folder to open (a second launch's: the first launch's is open already).
    pub folder: Option<String>,
    /// A connection to select in the explorer, by id: never connected.
    pub connection: Option<String>,
    /// `.sql` files to open in tabs, in order.
    pub files: Vec<String>,
    /// An execution plan file to open.
    pub plan: Option<String>,
    /// What was not accepted and why, to say so.
    pub refused: Vec<LaunchRefusedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRefusedItem {
    pub argument: String,
    pub reason: String,
}

impl From<&LaunchRequest> for LaunchRequestItem {
    fn from(request: &LaunchRequest) -> Self {
        let text = |path: &PathBuf| path.display().to_string();
        Self {
            folder: request.folder.as_ref().map(text),
            connection: request.connection.clone(),
            files: request.files.iter().map(text).collect(),
            plan: request.plan.as_ref().map(text),
            refused: request
                .refused
                .iter()
                .map(|refused| LaunchRefusedItem {
                    argument: refused.argument.clone(),
                    reason: refused.reason.to_string(),
                })
                .collect(),
        }
    }
}

/// A second launch (or Finder) asked the running window to open something.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequested {
    pub request: LaunchRequestItem,
}

/// What this app's own command line asked for, once: the folder is already open and the plan
/// goes through `startup_plan_path`, so only the files, the connection and the refusals.
#[command]
pub fn take_launch_request(state: State<'_, AppState>) -> Option<LaunchRequestItem> {
    let mut launch = state.launch.lock().expect("launch lock poisoned");
    launch.taken = true;
    launch.request.take()
}

/// This app's own request until the page takes it; after that, later ones are events.
#[derive(Debug, Default)]
pub struct LaunchSlot {
    pub request: Option<LaunchRequestItem>,
    pub taken: bool,
}

/// One recent folder, as the folder menu and the palette list it.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RecentFolderItem {
    pub path: String,
    /// Its last part, to show first.
    pub name: String,
    /// Whether it is still there.
    pub exists: bool,
}

/// The folders opened most recently, newest first.
#[command]
pub async fn recent_folders(state: State<'_, AppState>) -> CmdResult<Vec<RecentFolderItem>> {
    let folders = state
        .history
        .recent_folders()
        .await
        .map_err(|e| e.to_string())?;
    Ok(folders
        .into_iter()
        .map(|path| {
            let folder = Path::new(&path);
            RecentFolderItem {
                name: folder
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone()),
                exists: folder.is_dir(),
                path,
            }
        })
        .collect())
}

/// Takes a folder out of the recent folders; the folder itself is not touched.
#[command]
pub async fn forget_recent_folder(path: String, state: State<'_, AppState>) -> CmdResult<bool> {
    state
        .history
        .forget_recent_folder(path)
        .await
        .map_err(|e| e.to_string())
}

/// Hands each later launch's request to the page, and brings the window forward.
pub fn serve(server: spelaion_core::instance::InstanceServer, app: tauri::AppHandle) {
    server.serve(move |args, cwd| deliver(&app, &parse(&args, &cwd)));
}

/// Sends a request to the page (and shows the window), unless there is nothing in it.
pub fn deliver(app: &tauri::AppHandle, request: &LaunchRequest) {
    if request.is_empty() {
        return;
    }
    // To the main window only (SDB-30): another window never opens what a launch asked for.
    let _ = LaunchRequested {
        request: request.into(),
    }
    .emit_to(app, crate::windows::MAIN);
    // The smoke test's window never takes the foreground (SDB-274).
    if std::env::var_os(crate::OFFSCREEN_SWITCH).is_none()
        && let Some(window) = app.get_webview_window("main")
    {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Files opened from Finder on macOS arrive as an event, not as arguments: before the page
/// has taken this app's request they join it, after that they are delivered like a second
/// launch's.
#[cfg(target_os = "macos")]
pub fn opened(app: &tauri::AppHandle, urls: &[tauri::Url]) {
    let args: Vec<String> = urls
        .iter()
        .filter_map(|url| url.to_file_path().ok())
        .map(|path| path.display().to_string())
        .collect();
    let request = parse(&args, Path::new("/"));
    {
        let state = app.state::<AppState>();
        let mut launch = state.launch.lock().expect("launch lock poisoned");
        if !launch.taken {
            let item = LaunchRequestItem::from(&request);
            match &mut launch.request {
                Some(pending) => {
                    pending.files.extend(item.files);
                    pending.refused.extend(item.refused);
                }
                None => launch.request = Some(item),
            }
            return;
        }
    }
    deliver(app, &request);
}
