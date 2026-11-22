//! Execution-plan commands: typed projections over the core, never engine parsing in Tauri.
use spelaion_desktop_macros::command;
use std::path::Path;

use crate::{
    AppState, CmdResult,
    dto::{RiskyItem, RunEvents, ScriptEvent},
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

#[command]
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

#[command]
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

#[command]
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
    // The same checks the run makes, before anything is replaced: a refused explain leaves the
    // results as they were. Then, like a run, it takes the place of the results that are not
    // pinned (SDB-378).
    plan::explain_preflight(
        &open.session,
        &sql,
        options,
        &ExplainPolicy::for_connection(&open),
    )
    .map_err(|e| e.to_string())?;
    // The plan's own rows are the core's and it frees them itself: they are not result tabs.
    let replaced = RunEvents::new(
        state
            .begin_results(
                &session,
                spelaion_core::Placement::Replace,
                &open,
                &sql,
                None,
            )
            .await?,
    );
    let _ = on_event.send(replaced.started());
    drop(replaced);
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

#[command]
pub async fn open_plan_file(path: String, state: State<'_, AppState>) -> CmdResult<PlanSummary> {
    let document = tokio::task::spawn_blocking(move || plan::read_plan(Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    state.plans.insert(document).map_err(|e| e.to_string())
}

#[command]
pub fn startup_plan_path(state: State<'_, AppState>) -> Option<String> {
    state
        .startup_plan
        .lock()
        .expect("startup plan lock poisoned")
        .take()
}

#[command]
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

#[command]
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

#[command]
pub fn plan_raw_window(plan: u32, offset: u32, state: State<'_, AppState>) -> CmdResult<RawWindow> {
    state
        .plans
        .raw_window(plan, offset as usize)
        .map_err(|e| e.to_string())
}

#[command]
pub fn release_plan(plan: u32, state: State<'_, AppState>) {
    state.plans.release(plan);
}

#[command]
pub async fn export_plan(
    job: tauri::ipc::Channel<crate::jobs::JobTicket>,
    plan: u32,
    view: PlanView,
    path: String,
    csv: bool,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let running = crate::jobs::start(&state, crate::jobs::JobKind::Export, &job);
    let cancel = running.cancel().clone();
    let plans = state.plans.clone();
    let result = tokio::task::spawn_blocking(move || {
        if csv {
            plans.export_csv(plan, &view, Path::new(&path), &cancel)
        } else {
            plans.save_raw(plan, Path::new(&path), &cancel)
        }
    })
    .await;
    drop(running);
    result
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}
