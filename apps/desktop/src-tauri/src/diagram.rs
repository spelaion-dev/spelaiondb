//! ER diagram commands (ADR-046). The core reads the model from the catalog, lays it out and
//! writes its text exports; the model stays here, by the id the UI gave it, between the layouts
//! of one diagram tab, so changing a display option never reads the catalog again.
//!
//! Saved diagrams (ADR-084) are local data the core checks when they are saved and read; the
//! page holds the arrangement it is editing and sends it with each layout.

use spelaion_desktop_macros::command;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::{
    Cardinality, DiagramScope, ErArrangement, ErColor, ErColumns, ErGroup, ErLayoutOptions,
    ErMetrics, ErModel, ErNote, ErPlacement, ErSide, SavedDiagram, SavedDiagramContent,
    SavedDiagramSummary, TableName,
};
use tauri::State;
use tauri::ipc::Channel;

use crate::dto::{ConnectionKeyItem, NameFilterItem};
use crate::{AppState, CmdResult};

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableNameItem {
    pub schema: String,
    pub name: String,
}

impl From<TableNameItem> for TableName {
    fn from(t: TableNameItem) -> Self {
        TableName {
            schema: t.schema,
            name: t.name,
        }
    }
}

impl From<TableName> for TableNameItem {
    fn from(t: TableName) -> Self {
        TableNameItem {
            schema: t.schema,
            name: t.name,
        }
    }
}

/// What a diagram shows: a schema (with the explorer's table filter), chosen tables, or one
/// table and the tables it is joined to.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
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
                tables: tables.into_iter().map(Into::into).collect(),
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

impl From<DiagramScope> for DiagramScopeItem {
    fn from(scope: DiagramScope) -> Self {
        match scope {
            DiagramScope::Schema { schema, filter } => DiagramScopeItem::Schema {
                schema,
                filter: filter.map(|f| NameFilterItem {
                    include: f.include,
                    exclude: f.exclude,
                }),
            },
            DiagramScope::Tables { tables } => DiagramScopeItem::Tables {
                tables: tables.into_iter().map(Into::into).collect(),
            },
            DiagramScope::Focus { table } => DiagramScopeItem::Focus {
                schema: table.schema,
                table: table.name,
            },
        }
    }
}

/// The palette a note or a group is colored from (ADR-084), drawn with `--er-color-*`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DiagramColorItem {
    Gray,
    Blue,
    Green,
    Amber,
    Red,
    Purple,
}

impl From<DiagramColorItem> for ErColor {
    fn from(c: DiagramColorItem) -> Self {
        match c {
            DiagramColorItem::Gray => ErColor::Gray,
            DiagramColorItem::Blue => ErColor::Blue,
            DiagramColorItem::Green => ErColor::Green,
            DiagramColorItem::Amber => ErColor::Amber,
            DiagramColorItem::Red => ErColor::Red,
            DiagramColorItem::Purple => ErColor::Purple,
        }
    }
}

impl From<ErColor> for DiagramColorItem {
    fn from(c: ErColor) -> Self {
        match c {
            ErColor::Gray => DiagramColorItem::Gray,
            ErColor::Blue => DiagramColorItem::Blue,
            ErColor::Green => DiagramColorItem::Green,
            ErColor::Amber => DiagramColorItem::Amber,
            ErColor::Red => DiagramColorItem::Red,
            ErColor::Purple => DiagramColorItem::Purple,
        }
    }
}

/// Where the user put a table: its box's top left corner.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramPlacementItem {
    pub schema: String,
    pub name: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramNoteItem {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub text: String,
    pub color: DiagramColorItem,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramGroupItem {
    pub name: String,
    pub color: DiagramColorItem,
    pub tables: Vec<TableNameItem>,
}

/// What the user arranged: with no positions the tables follow the automatic layout.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramArrangementItem {
    pub positions: Vec<DiagramPlacementItem>,
    pub notes: Vec<DiagramNoteItem>,
    pub groups: Vec<DiagramGroupItem>,
}

impl From<DiagramArrangementItem> for ErArrangement {
    fn from(a: DiagramArrangementItem) -> Self {
        ErArrangement {
            positions: a
                .positions
                .into_iter()
                .map(|p| ErPlacement {
                    table: TableName {
                        schema: p.schema,
                        name: p.name,
                    },
                    x: p.x,
                    y: p.y,
                })
                .collect(),
            notes: a
                .notes
                .into_iter()
                .map(|n| ErNote {
                    x: n.x,
                    y: n.y,
                    width: n.width,
                    text: n.text,
                    color: n.color.into(),
                })
                .collect(),
            groups: a
                .groups
                .into_iter()
                .map(|g| ErGroup {
                    name: g.name,
                    color: g.color.into(),
                    tables: g.tables.into_iter().map(Into::into).collect(),
                })
                .collect(),
        }
    }
}

impl From<ErArrangement> for DiagramArrangementItem {
    fn from(a: ErArrangement) -> Self {
        DiagramArrangementItem {
            positions: a
                .positions
                .into_iter()
                .map(|p| DiagramPlacementItem {
                    schema: p.table.schema,
                    name: p.table.name,
                    x: p.x,
                    y: p.y,
                })
                .collect(),
            notes: a
                .notes
                .into_iter()
                .map(|n| DiagramNoteItem {
                    x: n.x,
                    y: n.y,
                    width: n.width,
                    text: n.text,
                    color: n.color.into(),
                })
                .collect(),
            groups: a
                .groups
                .into_iter()
                .map(|g| DiagramGroupItem {
                    name: g.name,
                    color: g.color.into(),
                    tables: g.tables.into_iter().map(Into::into).collect(),
                })
                .collect(),
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum DiagramColumnsItem {
    All,
    Keys,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramOptionsItem {
    pub columns: DiagramColumnsItem,
    pub types: bool,
    pub comments: bool,
}

impl From<DiagramOptionsItem> for ErLayoutOptions {
    fn from(options: DiagramOptionsItem) -> Self {
        ErLayoutOptions {
            columns: match options.columns {
                DiagramColumnsItem::All => ErColumns::All,
                DiagramColumnsItem::Keys => ErColumns::Keys,
                DiagramColumnsItem::None => ErColumns::None,
            },
            types: options.types,
            comments: options.comments,
        }
    }
}

impl From<ErLayoutOptions> for DiagramOptionsItem {
    fn from(options: ErLayoutOptions) -> Self {
        DiagramOptionsItem {
            columns: match options.columns {
                ErColumns::All => DiagramColumnsItem::All,
                ErColumns::Keys => DiagramColumnsItem::Keys,
                ErColumns::None => DiagramColumnsItem::None,
            },
            types: options.types,
            comments: options.comments,
        }
    }
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
    /// Not placed by the saved diagram: laid out by the core below the placed tables.
    pub new: bool,
}

/// A table the saved diagram placed that is no longer read in its scope, where it was.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramGoneItem {
    pub schema: String,
    pub name: String,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// A note, its text wrapped by the core.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramNoteBoxItem {
    /// Its position in the arrangement's notes.
    pub note: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub lines: Vec<String>,
    pub color: DiagramColorItem,
}

/// A group's frame around its tables.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DiagramGroupFrameItem {
    /// Its position in the arrangement's groups.
    pub group: u32,
    pub label: String,
    pub color: DiagramColorItem,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
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
    /// The top left corner of what is drawn (a table placed by hand may be left of or above
    /// the origin).
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub line_height: i32,
    pub nodes: Vec<DiagramNodeItem>,
    pub edges: Vec<DiagramEdgeItem>,
    pub gone: Vec<DiagramGoneItem>,
    pub notes: Vec<DiagramNoteBoxItem>,
    pub groups: Vec<DiagramGroupFrameItem>,
}

/// A saved diagram as listed (ADR-084).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SavedDiagramItem {
    #[specta(type = specta_typescript::Number)]
    pub id: i64,
    /// Its connection, by its folder and its id (ADR-075).
    pub connection: ConnectionKeyItem,
    pub name: String,
    #[specta(type = specta_typescript::Number)]
    pub updated_ms: i64,
    /// Why it cannot be opened, when it cannot.
    pub unreadable: Option<String>,
}

impl SavedDiagramItem {
    fn listed(s: SavedDiagramSummary, connection: ConnectionKeyItem) -> Self {
        SavedDiagramItem {
            id: s.id,
            connection,
            name: s.name,
            updated_ms: s.updated_ms,
            unreadable: s.unreadable,
        }
    }
}

/// A saved diagram with what it holds, checked by the core just now.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SavedDiagramDetailItem {
    pub saved: SavedDiagramItem,
    pub scope: DiagramScopeItem,
    pub options: DiagramOptionsItem,
    pub arrangement: DiagramArrangementItem,
}

impl SavedDiagramDetailItem {
    fn listed(d: SavedDiagram, connection: ConnectionKeyItem) -> Self {
        SavedDiagramDetailItem {
            saved: SavedDiagramItem::listed(d.summary, connection),
            scope: d.content.scope.into(),
            options: d.content.options.into(),
            arrangement: d.content.arrangement.into(),
        }
    }
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
#[command]
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
/// options and font measures, and as `arrangement` places tables, notes and groups (checked by
/// the core first; with no positions the layout is automatic). Panning and zooming never ask
/// for it again.
#[command]
pub async fn diagram_layout(
    diagram: u32,
    options: DiagramOptionsItem,
    metrics: DiagramMetricsItem,
    arrangement: DiagramArrangementItem,
    state: State<'_, AppState>,
) -> CmdResult<DiagramLayoutItem> {
    let model = model(&state, diagram)?;
    let options = ErLayoutOptions::from(options);
    let metrics = ErMetrics {
        char_width: metrics.char_width,
        line_height: metrics.line_height,
    };
    let arrangement = ErArrangement::from(arrangement);
    spelaion_core::check_diagram_arrangement(&arrangement)?;
    tokio::task::spawn_blocking(move || {
        let layout = spelaion_core::er_arrange(&model, &options, &metrics, &arrangement);
        DiagramLayoutItem {
            x: px(layout.x),
            y: px(layout.y),
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
                        new: node.new,
                    }
                })
                .collect(),
            gone: layout
                .gone
                .into_iter()
                .map(|g| DiagramGoneItem {
                    schema: g.schema,
                    name: g.name,
                    title: g.title,
                    x: px(g.x),
                    y: px(g.y),
                    width: px(g.width),
                    height: px(g.height),
                })
                .collect(),
            notes: layout
                .notes
                .into_iter()
                .map(|n| DiagramNoteBoxItem {
                    note: count(n.note),
                    x: px(n.x),
                    y: px(n.y),
                    width: px(n.width),
                    height: px(n.height),
                    lines: n.lines,
                    color: n.color.into(),
                })
                .collect(),
            groups: layout
                .groups
                .into_iter()
                .map(|g| DiagramGroupFrameItem {
                    group: count(g.group),
                    label: g.label,
                    color: g.color.into(),
                    x: px(g.x),
                    y: px(g.y),
                    width: px(g.width),
                    height: px(g.height),
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
#[command]
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

/// A saved diagram's connection by its folder and its id, when it is one of the open folder's
/// connections and still the one of the scope it was saved under (ADR-075): only those are
/// listed and opened, as bookmarks are.
fn connection_of(
    config: &spelaion_config::Config,
    summary: &SavedDiagramSummary,
) -> Option<ConnectionKeyItem> {
    let id = spelaion_config::ConnectionId::parse(&summary.connection);
    config.connection(&id)?;
    let key = spelaion_config::ConnectionKey::of(config, &id);
    (key.store_scope() == summary.workspace).then(|| ConnectionKeyItem::from(&key))
}

const NOT_IN_FOLDER: &str = "the saved diagram's connection is not in the open folder";

/// The saved diagrams of the open folder's connections, each checked by the core: one that
/// cannot be read says why.
#[command]
pub async fn saved_diagrams(state: State<'_, AppState>) -> CmdResult<Vec<SavedDiagramItem>> {
    let saved = state
        .history
        .saved_diagrams()
        .await
        .map_err(|e| e.to_string())?;
    let config = state.config().await;
    Ok(saved
        .into_iter()
        .filter_map(|s| {
            let connection = connection_of(&config, &s)?;
            Some(SavedDiagramItem::listed(s, connection))
        })
        .collect())
}

/// A saved diagram with what it holds, read back and checked by the core.
#[command]
pub async fn open_saved_diagram(
    id: u32,
    state: State<'_, AppState>,
) -> CmdResult<SavedDiagramDetailItem> {
    let saved = state
        .history
        .saved_diagram(i64::from(id))
        .await
        .map_err(|e| e.to_string())?;
    let connection = connection_of(&*state.config().await, &saved.summary).ok_or(NOT_IN_FOLDER)?;
    Ok(SavedDiagramDetailItem::listed(saved, connection))
}

/// Saves a diagram of `connection` under `name`: a new one, or over `id`. The core checks the
/// name, the scope and the arrangement; another saved diagram of the connection with the same
/// name is refused.
#[command]
pub async fn save_diagram(
    connection: ConnectionKeyItem,
    id: Option<u32>,
    name: String,
    scope: DiagramScopeItem,
    options: DiagramOptionsItem,
    arrangement: DiagramArrangementItem,
    state: State<'_, AppState>,
) -> CmdResult<SavedDiagramDetailItem> {
    let (workspace, id_of_connection) = crate::connection_scope(&state, &connection).await?;
    let content = SavedDiagramContent {
        scope: scope.into(),
        options: options.into(),
        arrangement: arrangement.into(),
    };
    state
        .history
        .save_diagram(
            id.map(i64::from),
            workspace,
            id_of_connection,
            &name,
            &content,
        )
        .await
        .map(|saved| SavedDiagramDetailItem::listed(saved, connection))
        .map_err(|e| e.to_string())
}

/// Renames a saved diagram, keeping what it holds.
#[command]
pub async fn rename_saved_diagram(
    id: u32,
    name: String,
    state: State<'_, AppState>,
) -> CmdResult<SavedDiagramItem> {
    // Only a diagram of the open folder's connections, as it is listed and opened.
    let saved = state
        .history
        .saved_diagram(i64::from(id))
        .await
        .map_err(|e| e.to_string())?;
    let connection = connection_of(&*state.config().await, &saved.summary).ok_or(NOT_IN_FOLDER)?;
    state
        .history
        .rename_saved_diagram(i64::from(id), &name)
        .await
        .map(|d| SavedDiagramItem::listed(d.summary, connection))
        .map_err(|e| e.to_string())
}

/// Deletes a saved diagram; whether it was there.
#[command]
pub async fn delete_saved_diagram(id: u32, state: State<'_, AppState>) -> CmdResult<bool> {
    state
        .history
        .delete_saved_diagram(i64::from(id))
        .await
        .map_err(|e| e.to_string())
}

/// Forgets a diagram's model (its tab closed or read it again under another id).
#[command]
pub async fn close_diagram(diagram: u32, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .diagrams
        .lock()
        .map_err(|_| "diagram lock poisoned")?
        .remove(&diagram);
    Ok(())
}
