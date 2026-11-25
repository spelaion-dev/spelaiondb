//! The process monitor's live dashboard (SDB-192). The sampler is the core's
//! (`spelaion_core::start_dashboard`): it decides when to read and keeps the samples; this file
//! keeps one sampler per monitor session, forwards its state to the page as it changes, and
//! passes on whether the dashboard is shown. Nothing is written to disk.

use std::collections::HashMap;

use serde::Serialize;
use specta::Type;
use spelaion_desktop_macros::command;
use tauri::State;
use tauri::async_runtime::JoinHandle;
use tauri::ipc::Channel;

use crate::{AppState, CmdResult};

/// A monitor session's sampler, and the task that sends its state to the page's channel.
struct Running {
    sampler: spelaion_core::DashboardSampler,
    forward: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.forward.abort();
    }
}

/// The samplers running, by the monitor session they read.
#[derive(Default)]
pub(crate) struct Dashboards(std::sync::Mutex<HashMap<String, Running>>);

impl Dashboards {
    /// Stops the sampler of `session`, if it has one, and forgets its samples.
    pub(crate) fn stop(&self, session: &str) {
        let removed = self.0.lock().expect("lock poisoned").remove(session);
        drop(removed);
    }
}

/// Where a sampler stands (`spelaion_core::DashboardPhase`).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DashboardPhaseItem {
    /// Shown: it reads every interval.
    Sampling,
    /// Hidden: it reads nothing.
    Paused,
    /// A read failed: it stopped, with the server's words.
    Failed { message: String },
    /// It stopped.
    Stopped,
}

/// One sample (`spelaion_core::DashboardSample`): gauges at that moment, rates since the one
/// before; `null` where the server gives nothing or there is nothing to compare with.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSampleItem {
    /// Milliseconds since the sampler started (49 days fit).
    pub at_ms: u32,
    /// The first sample after a start or a pause: the line breaks before it.
    pub gap: bool,
    pub sessions: Option<u32>,
    pub active: Option<u32>,
    pub waiting: Option<u32>,
    pub transactions_per_second: Option<f64>,
    pub reads_per_second: Option<f64>,
    pub log_bytes_per_second: Option<f64>,
}

/// The sampler's state as the page draws it, sent whole each time it changes.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DashboardStateItem {
    pub phase: DashboardPhaseItem,
    pub interval_ms: u32,
    pub samples: Vec<DashboardSampleItem>,
}

/// A count of sessions, never negative, as the page's number.
fn count(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

/// Milliseconds as the page's number: past 49 days of sampling they stay at the largest.
fn millis(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

impl From<&spelaion_core::DashboardState> for DashboardStateItem {
    fn from(state: &spelaion_core::DashboardState) -> Self {
        use spelaion_core::DashboardPhase as P;
        Self {
            phase: match &state.phase {
                P::Sampling => DashboardPhaseItem::Sampling,
                P::Paused => DashboardPhaseItem::Paused,
                P::Failed { message } => DashboardPhaseItem::Failed {
                    message: message.clone(),
                },
                P::Stopped => DashboardPhaseItem::Stopped,
            },
            interval_ms: millis(state.interval_ms),
            samples: state
                .samples
                .iter()
                .map(|s| DashboardSampleItem {
                    at_ms: millis(s.at_ms),
                    gap: s.gap,
                    sessions: s.sessions.map(count),
                    active: s.active.map(count),
                    waiting: s.waiting.map(count),
                    transactions_per_second: s.transactions_per_second,
                    reads_per_second: s.reads_per_second,
                    log_bytes_per_second: s.log_bytes_per_second,
                })
                .collect(),
        }
    }
}

/// Sends the sampler's state to `on_state` now and each time it changes, until it ends or the
/// page's channel is gone.
fn forward(
    mut changes: tokio::sync::watch::Receiver<spelaion_core::DashboardState>,
    on_state: Channel<DashboardStateItem>,
) -> JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        loop {
            let (item, ended) = {
                let state = changes.borrow_and_update();
                (DashboardStateItem::from(&*state), state.phase.has_ended())
            };
            if on_state.send(item).is_err() || ended || changes.changed().await.is_err() {
                break;
            }
        }
    })
}

/// Shows the dashboard of the monitor session `session`: its sampler, shown, with the samples
/// it kept while hidden, or a new one sampling every few seconds
/// (`spelaion_core::DASHBOARD_INTERVAL`) when it has none or the one it had stopped. Its state
/// goes to `on_state` (and no longer to an earlier channel) each time it changes.
#[command]
pub async fn start_dashboard(
    session: String,
    on_state: Channel<DashboardStateItem>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let open = state.session(&session).await?;
    let mut dashboards = state.dashboards.0.lock().expect("lock poisoned");
    if let Some(running) = dashboards.get_mut(&session)
        && !running.sampler.has_ended()
    {
        running.forward.abort();
        running.forward = forward(running.sampler.state(), on_state);
        running.sampler.set_visible(true);
        return Ok(());
    }
    let sampler = spelaion_core::start_dashboard(&open, spelaion_core::DASHBOARD_INTERVAL)?;
    let forward = forward(sampler.state(), on_state);
    dashboards.insert(session, Running { sampler, forward });
    Ok(())
}

/// Whether the dashboard of `session` is shown: hidden, its sampler reads nothing.
#[command]
pub async fn set_dashboard_visible(
    session: String,
    visible: bool,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    if let Some(running) = state
        .dashboards
        .0
        .lock()
        .expect("lock poisoned")
        .get(&session)
    {
        running.sampler.set_visible(visible);
    }
    Ok(())
}

/// Stops the sampler of `session` and forgets its samples.
#[command]
pub async fn stop_dashboard(session: String, state: State<'_, AppState>) -> CmdResult<()> {
    state.dashboards.stop(&session);
    Ok(())
}
