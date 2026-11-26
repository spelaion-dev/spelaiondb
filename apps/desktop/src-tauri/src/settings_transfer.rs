//! Thin adapters for exporting and importing the personal settings (SDB-31). The core reads,
//! checks and writes everything (`spelaion-core`'s `settings_transfer.rs`); these only pass the
//! path the user chose and apply what the window must show at once.
use std::path::PathBuf;

use crate::{AppState, CmdResult, dto::SettingsSummaryItem};
use spelaion_desktop_macros::command;
use tauri::{Manager, State};

/// Writes the personal settings to `path`, which the user chose in the save dialog.
#[command]
pub async fn export_settings(
    path: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<SettingsSummaryItem> {
    let version = app.package_info().version.to_string();
    spelaion_core::export_settings(
        &state.history,
        &state.user_dir,
        &PathBuf::from(path),
        &version,
    )
    .await
    .map(Into::into)
    .map_err(|e| e.to_string())
}

/// What importing the settings file at `path` would change; nothing is written.
#[command]
pub async fn preview_settings_import(
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<SettingsSummaryItem> {
    spelaion_core::preview_settings(&state.history, &state.user_dir, &PathBuf::from(path))
        .await
        .map(Into::into)
        .map_err(|e| e.to_string())
}

/// Imports the settings file at `path`, checked whole by the core first. The configuration is
/// read again and every window takes the new interface size; the page reloads its own
/// preferences.
#[command]
pub async fn import_settings(
    path: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<SettingsSummaryItem> {
    let summary =
        spelaion_core::import_settings(&state.history, &state.user_dir, &PathBuf::from(path))
            .await
            .map_err(|e| e.to_string())?;
    state.reload().await;
    if let Ok(size) = state.history.interface_size().await {
        for window in app.webview_windows().values() {
            let _ = window.set_zoom(crate::interface_scale(size));
        }
    }
    Ok(summary.into())
}
