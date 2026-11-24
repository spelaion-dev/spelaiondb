//! Schema compare commands (ADR-070). The core reads both sides, each in a new session opened
//! for reads only from the explorer's connection, and compares them; the comparison stays here,
//! by its job's id, until the page forgets it, so the script is written from what was read and
//! the page only ever holds the report.

use spelaion_desktop_macros::command;

use serde::Serialize;
use specta::Type;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::dto::{JobOutcome, McpDiffLineItem};
use crate::{AppState, CmdResult, jobs};

/// A comparison kept between its report and its script.
pub(crate) struct StoredComparison {
    compared: spelaion_core::ComparedSchemas,
    /// The explorer session of the target's connection: the script is written against its
    /// catalog, as a structure form's is.
    target_session: String,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CompareObjectKindItem {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
}

impl From<spelaion_core::CompareObjectKind> for CompareObjectKindItem {
    fn from(kind: spelaion_core::CompareObjectKind) -> Self {
        use spelaion_core::CompareObjectKind as K;
        match kind {
            K::Table => Self::Table,
            K::View => Self::View,
            K::MaterializedView => Self::MaterializedView,
            K::Function => Self::Function,
            K::Procedure => Self::Procedure,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ComparePropertyItem {
    Type,
    Nullable,
    Default,
    Comment,
    Generated,
    Columns,
    Definition,
    ReferencedTable,
    ReferencedColumns,
    Unique,
    Source,
}

impl From<spelaion_core::Property> for ComparePropertyItem {
    fn from(property: spelaion_core::Property) -> Self {
        use spelaion_core::Property as P;
        match property {
            P::Type => Self::Type,
            P::Nullable => Self::Nullable,
            P::Default => Self::Default,
            P::Comment => Self::Comment,
            P::Generated => Self::Generated,
            P::Columns => Self::Columns,
            P::Definition => Self::Definition,
            P::ReferencedTable => Self::ReferencedTable,
            P::ReferencedColumns => Self::ReferencedColumns,
            P::Unique => Self::Unique,
            P::Source => Self::Source,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PropertyChangeItem {
    pub property: ComparePropertyItem,
    pub source: Option<String>,
    pub target: Option<String>,
}

/// A part of an object, by its name where it has one.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ComparedPartItem {
    Object,
    Column { name: String },
    PrimaryKey,
    Constraint { name: String },
    Index { name: String },
    ForeignKey { name: String },
}

impl From<spelaion_core::ComparedPart> for ComparedPartItem {
    fn from(part: spelaion_core::ComparedPart) -> Self {
        use spelaion_core::ComparedPart as P;
        match part {
            P::Object => Self::Object,
            P::Column(name) => Self::Column { name },
            P::PrimaryKey => Self::PrimaryKey,
            P::Constraint(name) => Self::Constraint { name },
            P::Index(name) => Self::Index { name },
            P::ForeignKey(name) => Self::ForeignKey { name },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PartStatusItem {
    OnlyInSource { definition: String },
    OnlyInTarget { definition: String },
    Changed { changes: Vec<PropertyChangeItem> },
}

impl From<spelaion_core::PartStatus> for PartStatusItem {
    fn from(status: spelaion_core::PartStatus) -> Self {
        use spelaion_core::PartStatus as S;
        match status {
            S::OnlyInSource(definition) => Self::OnlyInSource { definition },
            S::OnlyInTarget(definition) => Self::OnlyInTarget { definition },
            S::Changed(changes) => Self::Changed {
                changes: changes
                    .into_iter()
                    .map(|c| PropertyChangeItem {
                        property: c.property.into(),
                        source: c.source,
                        target: c.target,
                    })
                    .collect(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PartComparisonItem {
    pub part: ComparedPartItem,
    pub status: PartStatusItem,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ObjectStatusItem {
    OnlyInSource,
    OnlyInTarget,
    Changed {
        parts: Vec<PartComparisonItem>,
        source_diff: Vec<McpDiffLineItem>,
        names_source_schema: bool,
    },
}

impl From<spelaion_core::ObjectStatus> for ObjectStatusItem {
    fn from(status: spelaion_core::ObjectStatus) -> Self {
        use spelaion_core::ObjectStatus as S;
        match status {
            S::OnlyInSource => Self::OnlyInSource,
            S::OnlyInTarget => Self::OnlyInTarget,
            S::Changed(changes) => Self::Changed {
                parts: changes
                    .parts
                    .into_iter()
                    .map(|p| PartComparisonItem {
                        part: p.part.into(),
                        status: p.status.into(),
                    })
                    .collect(),
                source_diff: changes.source_diff.into_iter().map(Into::into).collect(),
                names_source_schema: changes.names_source_schema,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ObjectComparisonItem {
    pub kind: CompareObjectKindItem,
    pub name: String,
    pub detail: Option<String>,
    pub status: ObjectStatusItem,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum NotComparedItem {
    ForeignKeyActions,
    ColumnAttributes,
    ColumnOrder,
    OtherObjects,
}

impl From<spelaion_core::NotCompared> for NotComparedItem {
    fn from(item: spelaion_core::NotCompared) -> Self {
        use spelaion_core::NotCompared as N;
        match item {
            N::ForeignKeyActions => Self::ForeignKeyActions,
            N::ColumnAttributes => Self::ColumnAttributes,
            N::ColumnOrder => Self::ColumnOrder,
            N::OtherObjects => Self::OtherObjects,
        }
    }
}

/// A comparison's report, under the id its script is asked for with.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SchemaComparisonItem {
    pub comparison: u32,
    pub source_schema: String,
    pub target_schema: String,
    pub objects: Vec<ObjectComparisonItem>,
    pub same: u32,
    pub not_compared: Vec<NotComparedItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CompareSideItem {
    Source,
    Target,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CompareProgressItem {
    pub side: CompareSideItem,
    pub done: u32,
    pub total: u32,
}

pub type CompareOutcome = JobOutcome<SchemaComparisonItem, ()>;

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NotScriptedItem {
    pub object: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MigrationScriptItem {
    pub sql: String,
    pub irreversible: Vec<String>,
    pub left_in_place: Vec<String>,
    pub not_scripted: Vec<NotScriptedItem>,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Whether `session` can take part in a comparison: `None`, or the core's reason why not.
#[command]
pub async fn schema_compare_support(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<Option<String>> {
    let open = state.session(&session).await?;
    Ok(spelaion_core::compare_support(&open.session).err())
}

/// Compares `source_schema` of `source`'s connection with `target_schema` of `target`'s (both
/// explorer sessions), each read in a new session opened for reads only. Progress goes to
/// `on_progress`, the ticket to `job`; `cancel_job` stops it. The comparison is kept until
/// `forget_schema_comparison`.
#[command]
#[allow(clippy::too_many_arguments)]
pub async fn compare_schemas(
    source: String,
    source_schema: String,
    target: String,
    target_schema: String,
    job: Channel<jobs::JobTicket>,
    on_progress: Channel<CompareProgressItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<CompareOutcome> {
    for side in [&source, &target] {
        let open = state.session(side).await?;
        spelaion_core::compare_support(&open.session)?;
    }
    let running = jobs::start(&state, jobs::JobKind::SchemaCompare, &job);
    let cancel = running.cancel().clone();
    let (source_tab, source_open) = crate::open_read_session(&state, &app, &source).await?;
    let (target_tab, target_open) = match crate::open_read_session(&state, &app, &target).await {
        Ok(opened) => opened,
        Err(e) => {
            crate::close_export_session(running, source_open).await;
            return Err(e);
        }
    };
    let context = match spelaion_core::follow_session_context(
        &source_tab.session,
        &source_open.session,
    )
    .await
    {
        Ok(()) => {
            spelaion_core::follow_session_context(&target_tab.session, &target_open.session).await
        }
        Err(message) => Err(message),
    };
    drop((source_tab, target_tab));
    let progress = move |event: spelaion_core::CompareProgress| {
        let _ = on_progress.send(CompareProgressItem {
            side: match event.side {
                spelaion_core::CompareSide::Source => CompareSideItem::Source,
                spelaion_core::CompareSide::Target => CompareSideItem::Target,
            },
            done: count(event.done),
            total: count(event.total),
        });
    };
    let id = running.id();
    let outcome = match context {
        Err(message) => CompareOutcome::Failed { message, kept: () },
        Ok(()) => match spelaion_core::compare_schemas(
            &source_open.session,
            &source_schema,
            &target_open.session,
            &target_schema,
            &cancel,
            &progress,
        )
        .await
        {
            Ok(compared) => {
                let report = compared.report.clone();
                state
                    .comparisons
                    .lock()
                    .map_err(|_| "comparison lock poisoned")?
                    .insert(
                        id,
                        StoredComparison {
                            compared,
                            target_session: target.clone(),
                        },
                    );
                CompareOutcome::Done {
                    result: SchemaComparisonItem {
                        comparison: id,
                        source_schema: report.source_schema,
                        target_schema: report.target_schema,
                        same: count(report.same),
                        not_compared: report.not_compared.into_iter().map(Into::into).collect(),
                        objects: report
                            .objects
                            .into_iter()
                            .map(|o| ObjectComparisonItem {
                                kind: o.kind.into(),
                                name: o.name,
                                detail: o.detail,
                                status: o.status.into(),
                            })
                            .collect(),
                    },
                }
            }
            Err(spelaion_core::CompareError::Cancelled) => CompareOutcome::Cancelled { kept: () },
            Err(e) => CompareOutcome::Failed {
                message: e.to_string(),
                kept: (),
            },
        },
    };
    crate::close_export_session(running, source_open).await;
    if let Ok(open) = std::sync::Arc::try_unwrap(target_open) {
        open.close().await;
    }
    Ok(outcome)
}

/// The script that makes the comparison's target like its source, written against the
/// target's catalog now. Nothing runs: the page opens it in an editor tab (ADR-070, point 7).
#[command]
pub async fn schema_compare_script(
    comparison: u32,
    drops: bool,
    state: State<'_, AppState>,
) -> CmdResult<MigrationScriptItem> {
    let (compared, target_session) = {
        let stored = state
            .comparisons
            .lock()
            .map_err(|_| "comparison lock poisoned")?;
        let stored = stored
            .get(&comparison)
            .ok_or("this comparison is no longer kept: compare again")?;
        (stored.compared.clone(), stored.target_session.clone())
    };
    let open = state.session(&target_session).await?;
    let script = spelaion_core::migration_script(
        &open.session,
        &compared,
        &spelaion_core::MigrationOptions { drops },
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(MigrationScriptItem {
        sql: script.sql,
        irreversible: script.irreversible,
        left_in_place: script.left_in_place,
        not_scripted: script
            .not_scripted
            .into_iter()
            .map(|n| NotScriptedItem {
                object: n.object,
                reason: n.reason,
            })
            .collect(),
    })
}

/// Forgets a comparison (its dialog closed, or it compared again).
#[command]
pub async fn forget_schema_comparison(
    comparison: u32,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .comparisons
        .lock()
        .map_err(|_| "comparison lock poisoned")?
        .remove(&comparison);
    Ok(())
}
