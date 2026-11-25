//! Copying a table to another connection (ADR-069): a thin adapter. The core reads the source,
//! maps the columns, writes every statement, checks the target's barriers and loads the rows;
//! these commands open the sessions the copy runs on and carry the user's choices to the core
//! and its answers back.

use spelaion_desktop_macros::command;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::table_copy::{Blocked, CopyError, CopyHow, CopyRequest, Loss, NotCopied};
use spelaion_core::{IfExists, OpenConnection, OpenOptions, open_connection};
use spelaion_ssh::KnownHosts;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::import::{
    IfExistsItem, ImportDoneItem, ImportOutcome, ImportPlanItem, ImportProgressItem, plan_item,
};
use crate::interaction::AppInteraction;
use crate::jobs::{self, JobKind, JobTicket};
use crate::{AppState, CmdResult};

/// What the copy does with the target.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CopyHowItem {
    /// A new table with the source's rows.
    StructureAndData,
    /// A new table, empty.
    StructureOnly,
    /// The rows into a table that is there.
    DataOnly,
}

/// One table to copy, from a connection's explorer session to another's.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CopyRequestItem {
    pub source_schema: String,
    pub source_table: String,
    pub target_schema: String,
    pub target_table: String,
    pub how: CopyHowItem,
    pub if_exists: IfExistsItem,
    /// Source columns left out.
    pub skip: Vec<String>,
    /// Target types typed instead of the mapped ones: `[column, type]`.
    pub types: Vec<(String, String)>,
    pub commit_every: f64,
}

impl From<CopyRequestItem> for CopyRequest {
    fn from(item: CopyRequestItem) -> Self {
        let if_exists = match item.if_exists {
            IfExistsItem::Fail => IfExists::Fail,
            IfExistsItem::Append => IfExists::Append,
            IfExistsItem::Replace => IfExists::Replace,
        };
        CopyRequest {
            source_schema: item.source_schema,
            source_table: item.source_table,
            target_schema: item.target_schema,
            target_table: item.target_table,
            how: match item.how {
                CopyHowItem::StructureAndData => CopyHow::NewTable {
                    if_exists,
                    rows: true,
                },
                CopyHowItem::StructureOnly => CopyHow::NewTable {
                    if_exists,
                    rows: false,
                },
                CopyHowItem::DataOnly => CopyHow::IntoExisting,
            },
            skip: item.skip,
            types: item.types,
            commit_every: item.commit_every.max(0.0) as u64,
        }
    }
}

/// What a column loses on its way, for the page to word.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LossItem {
    ZoneToUtc,
    UuidAsText,
    JsonAsText,
    TimeAsText,
    AsText,
    DecimalNarrowed { precision: u32, scale: u32 },
    DecimalBounded { precision: u32, scale: u32 },
    DecimalApproximate,
    LengthDropped,
    BinaryNotYet,
}

impl From<&Loss> for LossItem {
    fn from(loss: &Loss) -> Self {
        match loss {
            Loss::ZoneToUtc => LossItem::ZoneToUtc,
            Loss::UuidAsText => LossItem::UuidAsText,
            Loss::JsonAsText => LossItem::JsonAsText,
            Loss::TimeAsText => LossItem::TimeAsText,
            Loss::AsText { .. } => LossItem::AsText,
            Loss::DecimalNarrowed { precision, scale } => LossItem::DecimalNarrowed {
                precision: *precision,
                scale: *scale,
            },
            Loss::DecimalBounded { precision, scale } => LossItem::DecimalBounded {
                precision: *precision,
                scale: *scale,
            },
            Loss::DecimalApproximate => LossItem::DecimalApproximate,
            Loss::LengthDropped => LossItem::LengthDropped,
            Loss::BinaryNotYet => LossItem::BinaryNotYet,
        }
    }
}

/// Why a column cannot be copied as it is.
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum BlockedItem {
    Binary,
    Masked,
}

/// One source column and what it becomes.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CopyColumnItem {
    pub name: String,
    pub source_type: String,
    pub target_type: String,
    pub losses: Vec<LossItem>,
    /// Left out by the user.
    pub skipped: bool,
    /// Why it cannot be copied as it is, if it cannot (and is not left out).
    pub blocked: Option<BlockedItem>,
}

/// What of the source table the copy does not bring, counted.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NotCopiedItem {
    pub indexes: u32,
    pub foreign_keys: u32,
    pub constraints: u32,
    pub defaults: u32,
    pub primary_key: bool,
}

impl From<&NotCopied> for NotCopiedItem {
    fn from(n: &NotCopied) -> Self {
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        NotCopiedItem {
            indexes: count(n.indexes),
            foreign_keys: count(n.foreign_keys),
            constraints: count(n.constraints),
            defaults: count(n.defaults),
            primary_key: n.primary_key,
        }
    }
}

/// What a copy will do, written by the core, to show before it runs.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CopyPlanItem {
    pub columns: Vec<CopyColumnItem>,
    pub primary_key: Vec<String>,
    pub not_copied: NotCopiedItem,
    /// What runs on the target, as an import shows it.
    pub target: ImportPlanItem,
}

/// A new session from an open one (the explorer's): the same resolved connection, tunnel and
/// in-memory password. `read_only` makes it a session that only reads (the source), refused
/// where the server would not keep it from writing.
async fn open_from(
    state: &AppState,
    app: &AppHandle,
    session: &str,
    read_only: bool,
) -> CmdResult<(Arc<OpenConnection>, OpenConnection)> {
    let (existing, password, window) = {
        let sessions = state.sessions.lock().await;
        let open = sessions
            .get(session)
            .ok_or("a connection of this copy is closed")?;
        (
            open.open.clone(),
            open.password.clone(),
            open.window.clone(),
        )
    };
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        // Its SSH questions go to the source session's window (SDB-522, ADR-083).
        interaction: Arc::new(AppInteraction::for_window(
            app.clone(),
            state.pending.clone(),
            &window,
        )),
        password,
        read_only_session: read_only,
    };
    let opened = open_connection(
        existing.resolved.clone(),
        &state.engines(),
        options,
        state.results.clone(),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok((existing, opened))
}

/// The engines a copy can write to, by the configuration's name (ADR-069).
#[command]
pub fn copy_targets() -> Vec<String> {
    [
        "postgres",
        "mysql",
        "mariadb",
        "sqlserver",
        "sqlite",
        "duckdb",
        "clickhouse",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// What copying `request` from `source`'s connection to `target`'s will do. The source is read
/// on a session of the plan's own, opened and closed here (never the explorer's: its read
/// barrier may be a setting that stays); the target's plan only reads its catalog, on its
/// explorer session, as an import's does.
#[command]
pub async fn copy_plan(
    source: String,
    target: String,
    request: CopyRequestItem,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<CopyPlanItem> {
    let request: CopyRequest = request.into();
    let target_open = state.session(&target).await?;
    let (_, source_open) = open_from(&state, &app, &source, true).await?;
    let planned = spelaion_core::table_copy::plan_copy(
        &source_open.session,
        &target_open.session,
        &request,
        &target_open.resolved.safeguards(),
        target_open.session.is_read_only(),
    )
    .await;
    source_open.close().await;
    let plan = planned.map_err(|e| e.to_string())?;
    let columns = plan
        .columns
        .iter()
        .map(|column| CopyColumnItem {
            name: column.name.clone(),
            source_type: column.source_type.clone(),
            target_type: column.target.sql_type.clone(),
            losses: column.target.losses.iter().map(LossItem::from).collect(),
            skipped: plan.skipped.contains(&column.name),
            blocked: plan
                .blocked
                .iter()
                .find(|(name, _)| name == &column.name)
                .map(|(_, why)| match why {
                    Blocked::Binary => BlockedItem::Binary,
                    Blocked::Masked => BlockedItem::Masked,
                }),
        })
        .collect();
    Ok(CopyPlanItem {
        columns,
        primary_key: plan.primary_key,
        not_copied: (&plan.not_copied).into(),
        target: plan_item(plan.target),
    })
}

/// Runs a copy on sessions of its own (a reading one from the source's connection, one from the
/// target's), for a job whose ticket goes to `job` at once so `cancel_job` can stop it while
/// they open. The core writes the plan again, refuses blocked columns, a read-only target and an
/// unconfirmed plan that needs a confirmation, and reports progress on `on_progress`.
#[command]
#[allow(clippy::too_many_arguments)]
pub async fn run_copy(
    source: String,
    target: String,
    job: Channel<JobTicket>,
    request: CopyRequestItem,
    confirmed: bool,
    on_progress: Channel<ImportProgressItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ImportOutcome> {
    let request: CopyRequest = request.into();
    let running = jobs::start(&state, JobKind::Copy, &job);
    let cancel = running.cancel().clone();
    let opened = async {
        let (_, source_open) = open_from(&state, &app, &source, true).await?;
        let (target_existing, target_open) = open_from(&state, &app, &target, false).await?;
        CmdResult::Ok((source_open, target_existing, target_open))
    }
    .await;
    let (source_open, target_existing, target_open) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            drop(running);
            return Err(e);
        }
    };
    // The new target session finds names where the explorer's does.
    let context =
        spelaion_core::follow_session_context(&target_existing.session, &target_open.session).await;
    let safeguards = target_existing.resolved.safeguards();
    let read_only = target_existing.session.is_read_only();
    drop(target_existing);
    let progress = move |p: spelaion_core::ImportProgress| {
        let _ = on_progress.send(ImportProgressItem {
            read: p.read as f64,
            committed: p.committed as f64,
        });
    };
    let outcome = match context {
        Err(message) => Err(CopyError::Source(message)),
        Ok(()) => {
            spelaion_core::table_copy::run_copy(
                &source_open.session,
                &target_open.session,
                &request,
                &safeguards,
                read_only,
                confirmed,
                &cancel,
                &progress,
            )
            .await
        }
    };
    drop(running);
    source_open.close().await;
    target_open.close().await;
    // A copy stopped early keeps what the target committed, as an import does.
    let outcome = match outcome {
        Ok(summary) => Ok(summary),
        Err(CopyError::Target(e)) => Err(e),
        Err(other) => Err(spelaion_core::ImportError::Invalid(other.to_string())),
    };
    Ok(ImportOutcome::from_end(
        outcome.into(),
        |summary| ImportDoneItem {
            schema: summary.schema,
            table: summary.table,
            rows: summary.rows as f64,
            created_table: summary.created_table,
            millis: summary.elapsed.as_millis() as f64,
        },
        |committed| committed as f64,
    ))
}
