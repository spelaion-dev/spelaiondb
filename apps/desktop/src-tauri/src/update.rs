//! Automatic updates (ADR-021): Tauri's updater plugin behind two commands. Nothing is
//! downloaded or installed until the user asks; the signature is checked by the plugin
//! against the public key in `tauri.conf.json`.

use serde::Serialize;
use specta::Type;
use tauri::{AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;

use crate::CmdResult;

/// Where the app looks for `latest.json`. Empty until the releases have a public home: the
/// repository is private and no credential may ship in the app (ADR-021). With no endpoint
/// the app says updates are not available in this build and never makes a request.
const ENDPOINTS: &[&str] = &[];

/// The update found by the last check, kept for `update_install`.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateStatus {
    /// This build has nowhere to look for updates.
    Unavailable,
    UpToDate,
    #[serde(rename_all = "camelCase")]
    Available {
        version: String,
        current_version: String,
        notes: Option<String>,
    },
}

#[tauri::command]
#[specta::specta]
pub async fn update_check(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> CmdResult<UpdateStatus> {
    if ENDPOINTS.is_empty() {
        return Ok(UpdateStatus::Unavailable);
    }
    let endpoints = ENDPOINTS
        .iter()
        .map(|url| url.parse().map_err(|e| format!("{url}: {e}")))
        .collect::<Result<Vec<_>, _>>()?;
    let update = app
        .updater_builder()
        .endpoints(endpoints)
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let status = match &update {
        Some(update) => UpdateStatus::Available {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        },
        None => UpdateStatus::UpToDate,
    };
    *pending.0.lock().await = update;
    Ok(status)
}

/// Downloads and installs the update the last check found, then restarts the app. Open tabs
/// come back with their SQL, not executed (AGENTS.md).
#[tauri::command]
#[specta::specta]
pub async fn update_install(app: AppHandle, pending: State<'_, PendingUpdate>) -> CmdResult<()> {
    let Some(update) = pending.0.lock().await.take() else {
        return Err("there is no update to install: check for updates first".into());
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart()
}
