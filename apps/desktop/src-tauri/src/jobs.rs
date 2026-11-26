//! The desktop's one registry of running work (SDB-369, ADR-060): exports, data searches,
//! column statistics, imports, backups and restores, file runs, connection tests, row loads and
//! plugin installs.
//!
//! - **The id is the registry's, never the page's.** A job command takes a `job` channel and
//!   sends the job's ticket on it as soon as the job starts; the page keeps no counter and only
//!   gives the ticket back to cancel it.
//! - **An entry goes when its job ends.** The command holds the job's handle; whatever way it
//!   ends (a panic too), dropping the handle removes the entry.
//! - **A cancel names the kind of job it stops.** `cancel_job` with another kind's id is refused:
//!   a Cancel always reaches the job it was pressed on.
//!
//! The registry and the cancel it holds are the core's (`spelaion_core::JobRegistry`,
//! `JobCancel`); this file only names the desktop's kinds.

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_desktop_macros::command;
use tauri::State;
use tauri::ipc::Channel;

use crate::{AppState, CmdResult};

/// A kind of work the desktop runs and can stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum JobKind {
    /// A streaming export: of a query, of tables, of the history, of a plan, of a library.
    Export,
    DataSearch,
    /// A column's statistics or its top values.
    ColumnStats,
    Import,
    /// A copy of a table to another connection (ADR-069).
    Copy,
    /// A backup, with its vendor's tool or the engine's own.
    Backup,
    /// A restore, or an import of a DuckDB export.
    Restore,
    /// "Run SQL file…".
    FileRun,
    ConnectionTest,
    /// "Load every row", a page or a row jump of a query's result.
    RowLoad,
    PluginInstall,
    /// A schema comparison's reads (ADR-070).
    SchemaCompare,
    /// A saved task's run (ADR-089).
    Task,
}

/// What the page holds of a running job: its id and kind, to give back to `cancel_job`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct JobTicket {
    pub id: u32,
    pub kind: JobKind,
}

pub(crate) type Jobs = spelaion_core::JobRegistry<JobKind>;
pub(crate) type Job = spelaion_core::JobHandle<JobKind>;

/// Starts a job of `kind` and sends its ticket on `job` at once, so the page can cancel it
/// while the command still runs. The job is listed until the returned handle is dropped.
pub(crate) fn start(state: &AppState, kind: JobKind, job: &Channel<JobTicket>) -> Job {
    let handle = state.jobs.start(kind);
    let _ = job.send(JobTicket {
        id: handle.id(),
        kind,
    });
    handle
}

/// Stops the running job `id`, which must be a `kind`: the ticket its command sent. A job that
/// has ended is not an error; an id of another kind is refused and stops nothing.
#[command]
pub async fn cancel_job(id: u32, kind: JobKind, state: State<'_, AppState>) -> CmdResult<()> {
    state.jobs.cancel(id, kind).await.map_err(|e| e.to_string())
}
