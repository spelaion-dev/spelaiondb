//! Thin adapters for moving personal connections between computers in an encrypted file
//! (ADR-088). `spelaion-config`'s `connection_transfer.rs` gathers, seals, opens, checks and
//! writes everything, secrets included; these only take the user's choices and password from
//! the page, hold the password as a `Secret` from the first moment, and read the configuration
//! again after an import. Nothing here is logged, and no password goes back to the page.
use std::path::PathBuf;

use crate::{
    AppState, CmdResult,
    dto::{ConnectionImportItem, ConnectionImportResultItem, ConnectionsExportedItem},
};
use spelaion_config::{ConnectionId, Source, SystemBackend};
use spelaion_desktop_macros::command;
use spelaion_driver_api::Secret;
use tauri::State;

fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    spelaion_core::audit::iso_time(ms)
}

/// Seals the personal connections `ids` (as the explorer lists them, `~/orders`; empty: every
/// personal one) with the password the user typed twice, into `path`, chosen in the save
/// dialog. A repository connection is refused: it moves with its repository (ADR-022).
#[command]
pub async fn export_personal_connections(
    ids: Vec<String>,
    password: String,
    path: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ConnectionsExportedItem> {
    let password = Secret::new(password);
    let mut paths = Vec::with_capacity(ids.len());
    for id in ids {
        let parsed = ConnectionId::parse(&id);
        if parsed.source != Source::User {
            return Err(format!(
                "{id} is a repository connection: it moves with its repository, not in this file"
            ));
        }
        paths.push(parsed.path);
    }
    let config = state.config().await;
    spelaion_config::export_connections(
        &config,
        &SystemBackend,
        &paths,
        &password,
        &app.package_info().version.to_string(),
        &now_iso(),
        &PathBuf::from(path),
    )
    .map(Into::into)
    .map_err(|e| e.to_string())
}

/// Opens the connections file at `path` with `password` and says what importing each item
/// would do; nothing is written.
#[command]
pub async fn preview_connections_file(
    path: String,
    password: String,
    state: State<'_, AppState>,
) -> CmdResult<Vec<ConnectionImportItem>> {
    let password = Secret::new(password);
    let bytes =
        spelaion_config::read_connections_file(&PathBuf::from(path)).map_err(|e| e.to_string())?;
    let config = state.config().await;
    spelaion_config::preview_connections_import(&config, &SystemBackend, &bytes, &password)
        .map(|items| items.into_iter().map(Into::into).collect())
        .map_err(|e| e.to_string())
}

/// Imports the connections file at `path`: each item whole or not at all, its passwords into
/// the keychain. Items that exist here are replaced only when named in `replace_connections`
/// or `replace_profiles`. The configuration is read again afterwards.
#[command]
pub async fn import_connections_file(
    path: String,
    password: String,
    replace_connections: Vec<String>,
    replace_profiles: Vec<String>,
    state: State<'_, AppState>,
) -> CmdResult<Vec<ConnectionImportResultItem>> {
    let password = Secret::new(password);
    let bytes =
        spelaion_config::read_connections_file(&PathBuf::from(path)).map_err(|e| e.to_string())?;
    let config = state.config().await;
    let results = spelaion_config::import_connections(
        &config,
        &SystemBackend,
        &bytes,
        &password,
        &replace_connections,
        &replace_profiles,
    )
    .map_err(|e| e.to_string())?;
    drop(config);
    state.reload().await;
    Ok(results.into_iter().map(Into::into).collect())
}
