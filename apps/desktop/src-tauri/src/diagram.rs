//! ER diagram commands (ADR-046). The core reads the model from the catalog, lays it out and
//! writes its text exports; the model stays here, by the id the UI gave it, between the layouts
//! of one diagram tab, so changing a display option never reads the catalog again.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::{
    Cardinality, DiagramScope, ErColumns, ErLayoutOptions, ErMetrics, ErModel, ErSide, TableName,
};
use tauri::State;
use tauri::ipc::Channel;

use crate::dto::NameFilterItem;
use crate::{AppState, CmdResult};

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableNameItem {
    pub schema: String,
    pub name: String,
}

/// What a diagram shows: a schema (with the explorer's table filter), chosen tables, or one
/// table and the tables it is joined to.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DiagramScopeItem {
    Schema {
        schema: String,
        filter: Option<NameFilterItem>,
    },
    Tables {
        tables: Vec<TableNameItem>,
    },
    Focus {
        schema: String,
        table: String,
    },
}

impl From<DiagramScopeItem> for DiagramScope {
    fn from(scope: DiagramScopeItem) -> Self {
        match scope {
            DiagramScopeItem::Schema { schema, filter } => DiagramScope::Schema {
                schema,
                filter: filter.map(Into::into),
            },
            DiagramScopeItem::Tables { tables } => DiagramScope::Tables {
                tables: tables
                    .into_iter()
                    .map(|t| TableName {
                        schema: t.schema,
                        name: t.name,
                    })
                    .collect(),
            },
            DiagramScopeItem::Focus { schema, table } => DiagramScope::Focus {
                table: TableName {
                    schema,
                    name: table,
                },
            },
        }
    }
}

/// `read` of `total` tables described so far.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramProgressItem {
    pub read: u32,
    pub total: u32,
}

/// What was read: counts, and what the diagram leaves out.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramSummaryItem {
    pub tables: u32,
    pub relations: u32,
    /// Keys to tables outside the diagram.
    pub outside_keys: u32,
    /// Tables past the most a diagram reads.
    pub left_out: u32,
    pub max_tables: u32,
    /// Tables that could not be described, and the first error.
    pub unreadable: u32,
    pub first_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DiagramColumnsItem {
    All,
    Keys,
    None,
}

#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramOptionsItem {
    pub columns: DiagramColumnsItem,
    pub types: bool,
    pub comments: bool,
}

/// The diagram font's measures, in pixels, as the UI measured them.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramMetricsItem {
    pub char_width: f64,
    pub line_height: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CardinalityItem {
    One,
    ZeroOrOne,
    ZeroOrMany,
}

impl From<Cardinality> for CardinalityItem {
    fn from(c: Cardinality) -> Self {
        match c {
            Cardinality::One => Self::One,
            Cardinality::ZeroOrOne => Self::ZeroOrOne,
            Cardinality::ZeroOrMany => Self::ZeroOrMany,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DiagramSideItem {
    Left,
    Right,
}

impl From<ErSide> for DiagramSideItem {
    fn from(side: ErSide) -> Self {
        match side {
            ErSide::Left => Self::Left,
            ErSide::Right => Self::Right,
        }
    }
}

/// A listed column: its texts as the box shows them (shortened), and whole for the tooltip.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramRowItem {
    /// "PK", "FK", "UK" or a combination; empty for no key.
    pub key: String,
    pub primary: bool,
    pub name: String,
    pub native_type: String,
    pub comment: Option<String>,
    pub full_name: String,
    pub full_type: String,
    pub full_comment: Option<String>,
    pub nullable: bool,
}

/// A table's box, in pixels; the row and text positions are relative to its corner.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramNodeItem {
    pub schema: String,
    pub name: String,
    pub focus: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub title: String,
    pub comment: Option<String>,
    pub full_comment: Option<String>,
    pub rows_top: i32,
    pub key_x: i32,
    pub name_x: i32,
    pub type_x: i32,
    pub comment_x: i32,
    pub rows: Vec<DiagramRowItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramPointItem {
    pub x: i32,
    pub y: i32,
}

/// A relation's line: six points, drawn `M p0 L p1 C p2 p3 p4 L p5`; crow's foot marks go on
/// the stubs (p0–p1 at the referencing table, p4–p5 at the referenced one).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramEdgeItem {
    pub from: u32,
    pub to: u32,
    pub name: String,
    pub from_columns: Vec<String>,
    pub to_columns: Vec<String>,
    pub from_cardinality: CardinalityItem,
    pub to_cardinality: CardinalityItem,
    pub from_side: DiagramSideItem,
    pub to_side: DiagramSideItem,
    pub points: Vec<DiagramPointItem>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramLayoutItem {
    pub width: i32,
    pub height: i32,
    pub line_height: i32,
    pub nodes: Vec<DiagramNodeItem>,
    pub edges: Vec<DiagramEdgeItem>,
}

#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DiagramTextFormatItem {
    Mermaid,
    Dbml,
}

fn px(value: f64) -> i32 {
    value.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn model(state: &AppState, diagram: u32) -> CmdResult<Arc<ErModel>> {
    state
        .diagrams
        .lock()
        .map_err(|_| "diagram lock poisoned")?
        .get(&diagram)
        .cloned()
        .ok_or_else(|| "the diagram is no longer open".to_string())
}

/// Reads a diagram's tables from the catalog on the tab's session, one table at a time
/// (`on_progress` hears each one), and keeps the model as `diagram` for its layouts and
/// exports.
#[tauri::command]
#[specta::specta]
pub async fn read_diagram(
    session: String,
    diagram: u32,
    scope: DiagramScopeItem,
    on_progress: Channel<DiagramProgressItem>,
    state: State<'_, AppState>,
) -> CmdResult<DiagramSummaryItem> {
    let open = state.session(&session).await?;
    let scope = DiagramScope::from(scope);
    let model = spelaion_core::read_er_model(open.session.connection().as_ref(), &scope, |p| {
        let _ = on_progress.send(DiagramProgressItem {
            read: count(p.read),
            total: count(p.total),
        });
    })
    .await
    .map_err(|e| e.to_string())?;
    let summary = DiagramSummaryItem {
        tables: count(model.tables.len()),
        relations: count(model.relations.len()),
        outside_keys: count(model.outside_keys),
        left_out: count(model.left_out),
        max_tables: count(spelaion_core::MAX_DIAGRAM_TABLES),
        unreadable: count(model.unreadable),
        first_error: model.first_error.clone(),
    };
    state
        .diagrams
        .lock()
        .map_err(|_| "diagram lock poisoned")?
        .insert(diagram, Arc::new(model));
    Ok(summary)
}

/// Where each table of the diagram goes and how its relations are drawn, for these display
/// options and font measures. Panning and zooming never ask for it again.
#[tauri::command]
#[specta::specta]
pub async fn diagram_layout(
    diagram: u32,
    options: DiagramOptionsItem,
    metrics: DiagramMetricsItem,
    state: State<'_, AppState>,
) -> CmdResult<DiagramLayoutItem> {
    let model = model(&state, diagram)?;
    let options = ErLayoutOptions {
        columns: match options.columns {
            DiagramColumnsItem::All => ErColumns::All,
            DiagramColumnsItem::Keys => ErColumns::Keys,
            DiagramColumnsItem::None => ErColumns::None,
        },
        types: options.types,
        comments: options.comments,
    };
    let metrics = ErMetrics {
        char_width: metrics.char_width,
        line_height: metrics.line_height,
    };
    tokio::task::spawn_blocking(move || {
        let layout = spelaion_core::er_layout(&model, &options, &metrics);
        DiagramLayoutItem {
            width: px(layout.width),
            height: px(layout.height),
            line_height: px(layout.line_height),
            nodes: layout
                .nodes
                .into_iter()
                .map(|node| {
                    let table = &model.tables[node.table];
                    DiagramNodeItem {
                        schema: table.schema.clone(),
                        name: table.name.clone(),
                        focus: model.focus == Some(node.table),
                        x: px(node.x),
                        y: px(node.y),
                        width: px(node.width),
                        height: px(node.height),
                        title: node.title,
                        comment: node.comment,
                        full_comment: table.comment.clone(),
                        rows_top: px(node.rows_top),
                        key_x: px(node.key_x),
                        name_x: px(node.name_x),
                        type_x: px(node.type_x),
                        comment_x: px(node.comment_x),
                        rows: node
                            .rows
                            .into_iter()
                            .map(|row| {
                                let column = &table.columns[row.column];
                                DiagramRowItem {
                                    key: row.key.text(),
                                    primary: row.key.primary,
                                    name: row.name,
                                    native_type: row.native_type,
                                    comment: row.comment,
                                    full_name: column.name.clone(),
                                    full_type: column.native_type.clone(),
                                    full_comment: column.comment.clone(),
                                    nullable: row.nullable,
                                }
                            })
                            .collect(),
                    }
                })
                .collect(),
            edges: layout
                .edges
                .into_iter()
                .map(|edge| {
                    let relation = &model.relations[edge.relation];
                    DiagramEdgeItem {
                        from: count(edge.from),
                        to: count(edge.to),
                        name: edge.name,
                        from_columns: relation.from_columns.clone(),
                        to_columns: relation.to_columns.clone(),
                        from_cardinality: edge.from_cardinality.into(),
                        to_cardinality: edge.to_cardinality.into(),
                        from_side: edge.from_side.into(),
                        to_side: edge.to_side.into(),
                        points: edge
                            .points
                            .iter()
                            .map(|p| DiagramPointItem {
                                x: px(p.x),
                                y: px(p.y),
                            })
                            .collect(),
                    }
                })
                .collect(),
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// Writes the diagram's whole model as Mermaid or DBML (written by the core) to `path`.
#[tauri::command]
#[specta::specta]
pub async fn save_diagram_text(
    diagram: u32,
    format: DiagramTextFormatItem,
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let model = model(&state, diagram)?;
    tokio::task::spawn_blocking(move || {
        let text = match format {
            DiagramTextFormatItem::Mermaid => spelaion_core::mermaid(&model),
            DiagramTextFormatItem::Dbml => spelaion_core::dbml(&model),
        };
        spelaion_core::save_diagram_text(Path::new(&path), &text).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Forgets a diagram's model (its tab closed or read it again under another id).
#[tauri::command]
#[specta::specta]
pub async fn close_diagram(diagram: u32, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .diagrams
        .lock()
        .map_err(|_| "diagram lock poisoned")?
        .remove(&diagram);
    Ok(())
}
