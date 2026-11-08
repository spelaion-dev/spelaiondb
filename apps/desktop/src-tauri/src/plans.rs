//! Execution-plan commands: typed projections over the core, never engine parsing in Tauri.
use std::path::Path;
use std::sync::Arc;

use crate::{
    AppState, CmdResult,
    dto::{RiskyItem, ScriptEvent},
};
use spelaion_core::plan::{
    self, ExplainOptions, ExplainPolicy, PlanPage, PlanStatKind, PlanStatsPage, PlanSummary,
    PlanView, RawWindow,
};
use tauri::{State, ipc::Channel};

#[derive(serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ExplainCheck {
    sql: String,
    risks: Vec<RiskyItem>,
    executes_query: bool,
}

#[tauri::command]
#[specta::specta]
pub async fn plan_capabilities(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<plan::PlanCapabilities> {
    let open = state.session(&session).await?;
    Ok(plan::plan_capabilities(
        &open.session,
        &ExplainPolicy::for_connection(&open),
    ))
}

#[tauri::command]
#[specta::specta]
pub async fn explain_preflight(
    session: String,
    sql: String,
    options: ExplainOptions,
    state: State<'_, AppState>,
) -> CmdResult<ExplainCheck> {
    let open = state.session(&session).await?;
    let check = plan::explain_preflight(
        &open.session,
        &sql,
        options,
        &ExplainPolicy::for_connection(&open),
    )
    .map_err(|e| e.to_string())?;
    Ok(ExplainCheck {
        sql: check.sql,
        risks: check.risks.into_iter().map(Into::into).collect(),
        executes_query: check.executes_query,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn explain_query(
    session: String,
    sql: String,
    options: ExplainOptions,
    on_event: Channel<ScriptEvent>,
    state: State<'_, AppState>,
) -> CmdResult<PlanSummary> {
    let (open, workspace) = {
        let sessions = state.sessions.lock().await;
        let entry = sessions
            .get(&session)
            .ok_or("there is no open connection for this tab")?;
        (entry.open.clone(), entry.history_workspace.clone())
    };
    let scope = spelaion_core::HistoryScope {
        workspace,
        connection: open.resolved.id.to_string(),
        database: (!open.resolved.params.database.is_empty())
            .then(|| open.resolved.params.database.clone()),
        switchable_database: false,
    };
    let document = plan::explain(
        &open.session,
        &sql,
        options,
        &ExplainPolicy::for_connection(&open),
        Some((state.history.clone(), scope)),
        |event| {
            let _ = on_event.send(event.into());
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    state.plans.insert(document).map_err(|e| {
        if options.analyze {
            format!("ANALYZE ran, but its plan could not be retained: {e}. Check the current transaction before running the statement again.")
        } else { e.to_string() }
    })
}

#[tauri::command]
#[specta::specta]
pub async fn open_plan_file(path: String, state: State<'_, AppState>) -> CmdResult<PlanSummary> {
    let document = tokio::task::spawn_blocking(move || plan::read_plan(Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    state.plans.insert(document).map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn startup_plan_path(state: State<'_, AppState>) -> Option<String> {
    state
        .startup_plan
        .lock()
        .expect("startup plan lock poisoned")
        .take()
}

#[tauri::command]
#[specta::specta]
pub fn plan_page(
    plan: u32,
    view: PlanView,
    offset: u32,
    limit: u32,
    state: State<'_, AppState>,
) -> CmdResult<PlanPage> {
    state
        .plans
        .page(plan, &view, offset as usize, limit as usize)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn plan_statistics(
    plan: u32,
    kind: PlanStatKind,
    offset: u32,
    limit: u32,
    state: State<'_, AppState>,
) -> CmdResult<PlanStatsPage> {
    state
        .plans
        .statistics(plan, kind, offset as usize, limit as usize)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn plan_raw_window(plan: u32, offset: u32, state: State<'_, AppState>) -> CmdResult<RawWindow> {
    state
        .plans
        .raw_window(plan, offset as usize)
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub fn release_plan(plan: u32, state: State<'_, AppState>) {
    state.plans.release(plan);
}

#[tauri::command]
#[specta::specta]
pub async fn export_plan(
    export_id: u32,
    plan: u32,
    view: PlanView,
    path: String,
    csv: bool,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let cancel = Arc::new(spelaion_core::ExportCancel::new());
    {
        let mut exports = state.exports.lock().expect("exports lock poisoned");
        if exports.contains_key(&export_id) {
            return Err("an export with this identifier is already running".into());
        }
        exports.insert(export_id, cancel.clone());
    }
    let plans = state.plans.clone();
    let result = tokio::task::spawn_blocking(move || {
        if csv {
            plans.export_csv(plan, &view, Path::new(&path), &cancel)
        } else {
            plans.save_raw(plan, Path::new(&path), &cancel)
        }
    })
    .await;
    state
        .exports
        .lock()
        .expect("exports lock poisoned")
        .remove(&export_id);
    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}
