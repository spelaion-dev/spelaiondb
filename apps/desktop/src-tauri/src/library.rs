//! Thin adapters for the paged local query library.
use crate::{AppState, CmdResult, dto::*};
use spelaion_desktop_macros::command;
use tauri::State;

#[command]
pub async fn history_page(
    filter: HistoryFilterItem,
    limit: u32,
    offset: u32,
    state: State<'_, AppState>,
) -> CmdResult<HistoryPageItem> {
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
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
#[command]
pub async fn history_databases(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
    state
        .history
        .history_databases(workspace)
        .await
        .map_err(|e| e.to_string())
}
#[command]
pub async fn pin_history(id: u32, pinned: bool, state: State<'_, AppState>) -> CmdResult<bool> {
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
    state
        .history
        .pin_history(workspace, id.into(), pinned)
        .await
        .map_err(|e| e.to_string())
}
#[command]
pub async fn export_history(
    job: tauri::ipc::Channel<crate::jobs::JobTicket>,
    filter: HistoryFilterItem,
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let running = crate::jobs::start(&state, crate::jobs::JobKind::Export, &job);
    let cancel = running.cancel().clone();
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
    let result = state
        .history
        .export_history(workspace, filter.into(), path.into(), cancel)
        .await;
    drop(running);
    result
        .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
        .map_err(|e| e.to_string())
}
#[command]
pub async fn snippet_page(
    search: String,
    folder: Option<u32>,
    unfiled: bool,
    limit: u32,
    offset: u32,
    state: State<'_, AppState>,
) -> CmdResult<Vec<SnippetItem>> {
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
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
#[command]
pub async fn snippet_folders(state: State<'_, AppState>) -> CmdResult<Vec<SnippetFolderItem>> {
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
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
#[command]
pub async fn create_snippet_folder(name: String, state: State<'_, AppState>) -> CmdResult<u32> {
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
    state
        .history
        .create_snippet_folder(workspace, name)
        .await
        .map_err(|e| e.to_string())
        .and_then(|id| u32::try_from(id).map_err(|_| "folder identifier out of range".into()))
}
#[command]
pub async fn move_snippet(
    id: u32,
    folder: Option<u32>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let workspace = spelaion_config::repository_scope(&*state.config.read().await);
    state
        .history
        .move_snippet(workspace, id.into(), folder.map(i64::from))
        .await
        .map_err(|e| e.to_string())
}
