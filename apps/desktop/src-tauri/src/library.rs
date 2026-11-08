//! Thin adapters for the paged local query library.
use crate::{AppState, CmdResult, dto::*, history_workspace};
use spelaion_config::Source;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub async fn history_page(
    filter: HistoryFilterItem,
    limit: u32,
    offset: u32,
    state: State<'_, AppState>,
) -> CmdResult<HistoryPageItem> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    let page = state
        .history
        .history_page(workspace, filter.into(), limit as usize, offset as usize)
        .await
        .map_err(|e| e.to_string())?;
    Ok(HistoryPageItem {
        entries: page.entries.into_iter().map(HistoryItem::from).collect(),
        has_more: page.has_more,
        through_id: page.through_id,
    })
}
#[tauri::command]
#[specta::specta]
pub async fn history_databases(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .history_databases(workspace)
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
pub async fn pin_history(id: u32, pinned: bool, state: State<'_, AppState>) -> CmdResult<bool> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .pin_history(workspace, id.into(), pinned)
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
pub async fn export_history(
    export_id: u32,
    filter: HistoryFilterItem,
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let cancel = std::sync::Arc::new(spelaion_core::ExportCancel::new());
    {
        let mut exports = state.exports.lock().expect("lock poisoned");
        if exports.contains_key(&export_id) {
            return Err("an export with this identifier is already running".into());
        }
        exports.insert(export_id, cancel.clone());
    }
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    let result = state
        .history
        .export_history(workspace, filter.into(), path.into(), cancel)
        .await;
    state
        .exports
        .lock()
        .expect("lock poisoned")
        .remove(&export_id);
    result
        .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
        .map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
pub async fn snippet_page(
    search: String,
    folder: Option<u32>,
    unfiled: bool,
    limit: u32,
    offset: u32,
    state: State<'_, AppState>,
) -> CmdResult<Vec<SnippetItem>> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .snippet_page(
            workspace,
            spelaion_core::SnippetFilter {
                search,
                folder: folder.map(i64::from),
                unfiled,
            },
            limit as usize,
            offset as usize,
        )
        .await
        .map(|items| items.into_iter().map(SnippetItem::from).collect())
        .map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
pub async fn snippet_folders(state: State<'_, AppState>) -> CmdResult<Vec<SnippetFolderItem>> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .snippet_folders(workspace)
        .await
        .map(|items| {
            items
                .into_iter()
                .map(|folder| SnippetFolderItem {
                    id: folder.id,
                    name: folder.name,
                    personal: folder.workspace.is_empty(),
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}
#[tauri::command]
#[specta::specta]
pub async fn create_snippet_folder(name: String, state: State<'_, AppState>) -> CmdResult<u32> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .create_snippet_folder(workspace, name)
        .await
        .map_err(|e| e.to_string())
        .and_then(|id| u32::try_from(id).map_err(|_| "folder identifier out of range".into()))
}
#[tauri::command]
#[specta::specta]
pub async fn move_snippet(
    id: u32,
    folder: Option<u32>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let workspace = history_workspace(&*state.config.read().await, Source::Workspace);
    state
        .history
        .move_snippet(workspace, id.into(), folder.map(i64::from))
        .await
        .map_err(|e| e.to_string())
}
