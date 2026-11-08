//! `.sql` files and the open tabs that survive a restart (ADR-036): a thin adapter. The core
//! reads and writes files (encodings, atomic writes, the changed-on-disk check), decides how
//! often the tabs are written and checks them before they come back; these commands carry it.

use std::path::Path;

use spelaion_config::Source;
use spelaion_core::{
    FileError, FileFormat, SaveCheck, SavedTabs, read_sql_file, restore_tabs, write_sql_file,
};
use tauri::State;

use crate::dto::*;
use crate::{AppState, CmdResult, history_workspace};

/// The last part of a path, for a tab's title.
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Reads a `.sql` file the user picked (or one from the recent files or a kept tab) and puts
/// it first in the recent files.
#[tauri::command]
#[specta::specta]
pub(crate) async fn open_sql_file(
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<SqlFileItem> {
    let read = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || read_sql_file(Path::new(&path)))
            .await
            .map_err(|e| e.to_string())?
    }
    .map_err(|e| format!("{}: {e}", file_name(&path)))?;
    // A file that opened is worth listing even if the list cannot be written now.
    let _ = state.history.add_recent_file(path.clone()).await;
    Ok(SqlFileItem {
        name: file_name(&path),
        path,
        text: read.text,
        format: read.format.into(),
        hash: read.hash,
    })
}

/// Saves a tab's text to `path`. `format` is the file's (`None`: a new file, UTF-8 and the
/// platform's line break); `expected_hash` is what the tab last read or saved (`None`: write
/// whatever is there, after a "save as" dialog or the user's "overwrite").
#[tauri::command]
#[specta::specta]
pub(crate) async fn save_sql_file(
    path: String,
    text: String,
    format: Option<FileFormatItem>,
    expected_hash: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<SaveFileOutcome> {
    let format = format.map_or_else(FileFormat::new_file, Into::into);
    let check = expected_hash.map_or(SaveCheck::Overwrite, SaveCheck::Unchanged);
    let written = {
        let path = path.clone();
        tokio::task::spawn_blocking(move || write_sql_file(Path::new(&path), &text, format, &check))
            .await
            .map_err(|e| e.to_string())?
    };
    match written {
        Ok(hash) => {
            let _ = state.history.add_recent_file(path.clone()).await;
            Ok(SaveFileOutcome::Saved {
                name: file_name(&path),
                path,
                hash,
                format: format.into(),
            })
        }
        Err(FileError::ChangedOnDisk) => Ok(SaveFileOutcome::ChangedOnDisk),
        Err(FileError::DeletedOnDisk) => Ok(SaveFileOutcome::DeletedOnDisk),
        Err(FileError::Unrepresentable {
            character,
            encoding,
        }) => Ok(SaveFileOutcome::Unrepresentable {
            character: character.to_string(),
            encoding: encoding.into(),
        }),
        Err(
            e @ (FileError::NotFound
            | FileError::TooLarge { .. }
            | FileError::Binary
            | FileError::Io(_)),
        ) => Err(format!("{}: {e}", file_name(&path))),
    }
}

/// The recent files, newest first, each saying whether it is still there.
#[tauri::command]
#[specta::specta]
pub(crate) async fn recent_files(state: State<'_, AppState>) -> CmdResult<Vec<RecentFileItem>> {
    let recent = state
        .history
        .recent_files()
        .await
        .map_err(|e| e.to_string())?;
    Ok(recent
        .into_iter()
        .map(|r| RecentFileItem {
            name: file_name(&r.path),
            exists: Path::new(&r.path).is_file(),
            path: r.path,
        })
        .collect())
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn forget_recent_file(path: String, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .history
        .remove_recent_file(path)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The open folder's kept tabs, checked by the core: every one comes back disconnected,
/// with its text and a note when its connection or file is gone. Nothing is run.
#[tauri::command]
#[specta::specta]
pub(crate) async fn restore_tabs_for_workspace(
    state: State<'_, AppState>,
) -> CmdResult<RestoredTabsItem> {
    // What the folder's tabs were written under before anything else is (a reload may run).
    state.tabs.flush().await.map_err(|e| e.to_string())?;
    let (scope, connections) = {
        let config = state.config.read().await;
        (
            history_workspace(&config, Source::Workspace),
            config
                .connections
                .iter()
                .map(|c| c.id.to_string())
                .collect::<std::collections::HashSet<_>>(),
        )
    };
    let saved = state
        .history
        .open_tabs(scope.clone())
        .await
        .map_err(|e| e.to_string())?;
    let reconnect = state
        .history
        .reconnect_on_start()
        .await
        .map_err(|e| e.to_string())?;
    let (tabs, active) =
        tokio::task::spawn_blocking(move || restore_tabs(saved, |id| connections.contains(id)))
            .await
            .map_err(|e| e.to_string())?;
    Ok(RestoredTabsItem {
        scope,
        tabs: tabs
            .into_iter()
            .map(|t| RestoredTabItem {
                tab: t.tab.into(),
                notes: t.notes.into_iter().map(Into::into).collect(),
            })
            .collect(),
        active: active.and_then(|i| u32::try_from(i).ok()),
        reconnect,
    })
}

/// The open tabs as they are now, for the folder `scope` (from `restore_tabs_for_workspace`).
/// The core writes them within a second (ADR-036).
#[tauri::command]
#[specta::specta]
pub(crate) async fn save_tabs(
    scope: String,
    tabs: Vec<OpenTabItem>,
    active: Option<u32>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let tabs = SavedTabs {
        tabs: tabs.into_iter().map(Into::into).collect(),
        active: active.map(|i| i as usize),
    };
    state.tabs.update(scope, tabs);
    Ok(())
}

/// Writes the tabs waiting now (before switching folders).
#[tauri::command]
#[specta::specta]
pub(crate) async fn flush_tabs(state: State<'_, AppState>) -> CmdResult<()> {
    state.tabs.flush().await.map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_reconnect_on_start(state: State<'_, AppState>) -> CmdResult<bool> {
    state
        .history
        .reconnect_on_start()
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn set_reconnect_on_start(
    enabled: bool,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_reconnect_on_start(enabled)
        .await
        .map_err(|e| e.to_string())
}
