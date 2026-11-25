//! `.sql` files and the open tabs that survive a restart (ADR-036): a thin adapter. The core
//! reads and writes files (encodings, atomic writes, the changed-on-disk check), decides how
//! often the tabs are written and checks them before they come back; these commands carry it.

use spelaion_desktop_macros::command;
use std::collections::HashMap;
use std::path::Path;

use spelaion_config::{ConnectionFile, Source};
use spelaion_core::{
    FileError, FileFormat, SaveCheck, read_sql_file, restore_window_tabs, tab_scope, window_sets,
    write_sql_file,
};
use tauri::State;

use crate::dto::*;
use crate::{AppState, CmdResult};

/// The last part of a path, for a tab's title.
fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

/// Reads a `.sql` file the user picked (or one from the recent files or a kept tab) and puts
/// it first in the recent files.
#[command]
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
#[command]
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
#[command]
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

#[command]
pub(crate) async fn forget_recent_file(path: String, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .history
        .remove_recent_file(path)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// What the window keeps tabs under (ADR-075): each open folder's key, in the order they were
/// opened, then the window's empty key (personal connections and none). Each key once, with
/// the identifiers of the connections a tab kept under it may come back on: that folder's own
/// for a folder's key, the personal ones for the window's.
async fn tab_scopes(state: &AppState) -> Vec<(String, HashMap<String, ConnectionFile>)> {
    let mut scopes: Vec<(String, HashMap<String, ConnectionFile>)> = Vec::new();
    let connections_of = |config: &spelaion_config::Config, source| {
        config
            .connections
            .iter()
            .filter(|c| c.id.source == source)
            .map(|c| (c.id.to_string(), c.file.clone()))
            .collect::<HashMap<_, _>>()
    };
    for config in state.open_configs().await {
        let scope = spelaion_config::repository_scope(&config);
        if !scope.is_empty() && !scopes.iter().any(|(s, _)| *s == scope) {
            scopes.push((scope, connections_of(&config, Source::Workspace)));
        }
    }
    let shown = state.config().await;
    scopes.push((String::new(), connections_of(&shown, Source::User)));
    scopes
}

/// The file of connection `id` as a tab kept under `scope` finds it: in that folder, or among
/// the personal connections, since tabs kept before ADR-075 under the folder that was open may
/// be on a personal one (they are kept in the window from now). Never in another folder.
fn connection_file<'a>(
    scopes: &'a [(String, HashMap<String, ConnectionFile>)],
    personal: &'a HashMap<String, ConnectionFile>,
    scope: &str,
    id: &str,
) -> Option<&'a ConnectionFile> {
    personal.get(id).or_else(|| {
        scopes
            .iter()
            .find(|(s, _)| s == scope)
            .and_then(|(_, connections)| connections.get(id))
    })
}

/// The kept tabs of the window and of every open folder (ADR-075), checked by the core: every
/// one comes back disconnected, in the window's order, with its text and a note when its
/// connection or file is gone. A connection is looked up in the folder the tab was kept under
/// (and personal ones among the personal connections), never in another folder. Nothing is run.
#[command]
pub(crate) async fn restore_tabs_for_workspace(
    state: State<'_, AppState>,
) -> CmdResult<RestoredTabsItem> {
    // What the tabs were written under before anything else is (a reload may run).
    state.tabs.flush().await.map_err(|e| e.to_string())?;
    let scopes = tab_scopes(&state).await;
    let personal: HashMap<String, ConnectionFile> = scopes
        .last()
        .map(|(_, connections)| connections.clone())
        .unwrap_or_default();
    let mut sets = Vec::with_capacity(scopes.len());
    for (scope, _) in &scopes {
        let saved = state
            .history
            .open_tabs(scope.clone())
            .await
            .map_err(|e| e.to_string())?;
        sets.push((scope.clone(), saved));
    }
    let reconnect = state
        .history
        .reconnect_on_start()
        .await
        .map_err(|e| e.to_string())?;
    let (tabs, active) = tokio::task::spawn_blocking(move || {
        restore_window_tabs(
            sets,
            |scope, id| connection_file(&scopes, &personal, scope, id).is_some(),
            // Which database a connection still opens is the core's rule (ADR-074).
            |scope, id, database| {
                connection_file(&scopes, &personal, scope, id)
                    .is_some_and(|file| spelaion_core::file_opens_database(file, database))
            },
        )
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(RestoredTabsItem {
        tabs: tabs
            .into_iter()
            .map(|(scope, t)| {
                let mut tab: OpenTabItem = t.tab.into();
                tab.folder = (!scope.is_empty() && tab.connection.is_some()).then_some(scope);
                RestoredTabItem {
                    tab,
                    notes: t.notes.into_iter().map(Into::into).collect(),
                }
            })
            .collect(),
        active: active.and_then(|i| u32::try_from(i).ok()),
        reconnect,
    })
}

/// The window's tabs as they are now, in the tab bar's order (ADR-075): the core keeps each
/// under its connection's folder, or under the window's key, and writes them within a second
/// (ADR-036). Closed folders keep the tabs they had.
#[command]
pub(crate) async fn save_tabs(
    tabs: Vec<OpenTabItem>,
    active: Option<u32>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let open: Vec<String> = tab_scopes(&state)
        .await
        .into_iter()
        .map(|(scope, _)| scope)
        .collect();
    let tabs = tabs
        .into_iter()
        .map(|tab| {
            let scope = tab_scope(tab.connection.as_deref(), tab.folder.as_deref());
            (scope, tab.into())
        })
        .collect();
    state
        .tabs
        .update_window(window_sets(&open, tabs, active.map(|i| i as usize)));
    Ok(())
}

/// Writes the tabs waiting now (before switching folders).
#[command]
pub(crate) async fn flush_tabs(state: State<'_, AppState>) -> CmdResult<()> {
    state.tabs.flush().await.map_err(|e| e.to_string())
}

#[command]
pub(crate) async fn get_reconnect_on_start(state: State<'_, AppState>) -> CmdResult<bool> {
    state
        .history
        .reconnect_on_start()
        .await
        .map_err(|e| e.to_string())
}

#[command]
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
