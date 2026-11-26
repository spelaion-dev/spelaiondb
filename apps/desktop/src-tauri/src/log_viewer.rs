//! Thin adapters for Settings › Logs (SDB-31): the core reads, scrubs, filters and exports the
//! app log (`spelaion-core`'s `app_log.rs`); these only pass the level and the path the user
//! chose.
use std::path::PathBuf;

use crate::{
    AppState, CmdResult,
    dto::{LogLevelItem, LogPageItem},
};
use spelaion_desktop_macros::command;
use tauri::State;

/// The log's entries of `level` or more important, newest first, with where the file is.
#[command]
pub async fn read_log(level: LogLevelItem, state: State<'_, AppState>) -> CmdResult<LogPageItem> {
    let page = state
        .log
        .read(level.into())
        .map_err(|e| format!("could not read {}: {e}", state.log.path().display()))?;
    Ok(LogPageItem::new(state.log.path(), page))
}

/// Writes the entries of `level` or more important to `path`, which the user chose in the save
/// dialog; how many were written.
#[command]
pub async fn export_log(
    level: LogLevelItem,
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let path = PathBuf::from(path);
    state
        .log
        .export(level.into(), &path)
        .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}
