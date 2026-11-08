//! Types that cross to the UI. Exported to TypeScript with `tauri-specta`; no `u64`
//! (it does not fit in JS): counters and ids as `u32`, large counts as `f64`.

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::CoreEvent;
use spelaion_driver_api::{
    ConstraintKind, ObjectInfo, ObjectKind, SchemaObject, SchemaObjectKind, ServerObject,
    ServerObjectKind, TableInfo,
};

/// Include and exclude patterns for one kind of name (ADR-025).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NameFilterItem {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl From<NameFilterItem> for spelaion_driver_api::NameFilter {
    fn from(f: NameFilterItem) -> Self {
        Self {
            include: f.include,
            exclude: f.exclude,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum FilterKindItem {
    Schemas,
    Objects,
    Columns,
}

impl From<FilterKindItem> for spelaion_core::FilterKind {
    fn from(kind: FilterKindItem) -> Self {
        match kind {
            FilterKindItem::Schemas => Self::Schemas,
            FilterKindItem::Objects => Self::Objects,
            FilterKindItem::Columns => Self::Columns,
        }
    }
}

/// A table chosen in the explorer, by schema and name.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableRefItem {
    pub schema: String,
    pub name: String,
}

/// An object bookmarked in the explorer (ADR-026).
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkItem {
    pub connection: String,
    /// The schema the object is in, or the schema itself (with an empty `name`).
    pub schema: String,
    pub name: String,
    /// `table`, `view`… for a relation, `function`, `sequence`… for another object, `schema`.
    pub kind: String,
    /// A function's arguments…; empty if nothing tells it apart from others of its name.
    pub detail: String,
}

impl From<spelaion_core::Bookmark> for BookmarkItem {
    fn from(b: spelaion_core::Bookmark) -> Self {
        let spelaion_core::Bookmark {
            connection,
            schema,
            name,
            kind,
            detail,
        } = b;
        Self {
            connection,
            schema,
            name,
            kind,
            detail,
        }
    }
}

impl From<BookmarkItem> for spelaion_core::Bookmark {
    fn from(b: BookmarkItem) -> Self {
        let BookmarkItem {
            connection,
            schema,
            name,
            kind,
            detail,
        } = b;
        Self {
            connection,
            schema,
            name,
            kind,
            detail,
        }
    }
}

/// An object of the explorer by its path: its schema (empty for a schema itself) and its
/// names under it (a table, or a table and one of its columns).
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ObjectPathItem {
    pub schema: String,
    pub names: Vec<String>,
}

/// An object the explorer can change (ADR-026). The UI sends what the tree shows; the core
/// writes the statements.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ObjectTargetItem {
    Schema {
        name: String,
    },
    /// `relation` is the `kind` of an `ObjectItem`: `table`, `view`…
    Relation {
        schema: String,
        name: String,
        relation: String,
    },
    Column {
        schema: String,
        table: String,
        name: String,
    },
    Index {
        schema: String,
        table: String,
        name: String,
        primary: bool,
    },
    Constraint {
        schema: String,
        table: String,
        name: String,
        constraint: ConstraintKindItem,
    },
    ForeignKey {
        schema: String,
        table: String,
        name: String,
    },
    SchemaObject {
        object: SchemaObjectItem,
    },
}

impl TryFrom<ObjectTargetItem> for spelaion_core::ObjectTarget {
    type Error = String;
    fn try_from(item: ObjectTargetItem) -> Result<Self, String> {
        Ok(match item {
            ObjectTargetItem::Schema { name } => Self::Schema { name },
            ObjectTargetItem::Index {
                schema,
                table,
                name,
                primary,
            } => Self::Index {
                schema,
                table,
                name,
                primary,
            },
            ObjectTargetItem::Constraint {
                schema,
                table,
                name,
                constraint,
            } => Self::Constraint {
                schema,
                table,
                name,
                kind: constraint.into(),
            },
            ObjectTargetItem::ForeignKey {
                schema,
                table,
                name,
            } => Self::ForeignKey {
                schema,
                table,
                name,
            },
            ObjectTargetItem::SchemaObject { object } => Self::SchemaObject(object.into()),
            ObjectTargetItem::Relation {
                schema,
                name,
                relation,
            } => Self::Relation {
                schema,
                name,
                kind: match relation.as_str() {
                    "table" => ObjectKind::Table,
                    "partitioned_table" => ObjectKind::PartitionedTable,
                    "view" => ObjectKind::View,
                    "materialized_view" => ObjectKind::MaterializedView,
                    "foreign_table" => ObjectKind::ForeignTable,
                    other => return Err(format!("unknown kind of relation: {other}")),
                },
            },
            ObjectTargetItem::Column {
                schema,
                table,
                name,
            } => Self::Column {
                schema,
                table,
                name,
            },
        })
    }
}

/// What to do with an object of the explorer.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ObjectChangeItem {
    Rename {
        target: ObjectTargetItem,
        new_name: String,
    },
    Drop {
        target: ObjectTargetItem,
        cascade: bool,
    },
}

impl TryFrom<ObjectChangeItem> for spelaion_core::ObjectChange {
    type Error = String;
    fn try_from(item: ObjectChangeItem) -> Result<Self, String> {
        Ok(match item {
            ObjectChangeItem::Rename { target, new_name } => Self::Rename {
                target: target.try_into()?,
                new_name,
            },
            ObjectChangeItem::Drop { target, cascade } => Self::Drop {
                target: target.try_into()?,
                cascade,
            },
        })
    }
}

/// What "Open object by name" found: a relation or another object of a schema.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FoundObjectItem {
    Relation { object: ObjectItem },
    SchemaObject { object: SchemaObjectItem },
}

impl From<spelaion_core::FoundObject> for FoundObjectItem {
    fn from(found: spelaion_core::FoundObject) -> Self {
        match found {
            spelaion_core::FoundObject::Relation(o) => Self::Relation { object: o.into() },
            spelaion_core::FoundObject::Other(o) => Self::SchemaObject { object: o.into() },
        }
    }
}

/// What the explorer may offer on a session's engine (ADR-026).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ObjectActionsItem {
    pub rename_views: bool,
    pub drop_schemas: bool,
    pub drop_constraints: bool,
    pub create: Vec<CreateKindItem>,
    pub new_index: Vec<NewIndexKindItem>,
    pub active_schema: bool,
    pub add_constraints: bool,
    pub add_triggers: bool,
    pub before_triggers: bool,
    pub trigger_function: bool,
    pub add_check_constraints: bool,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NewTableElementItem {
    Column {
        name: String,
        data_type: String,
        nullable: bool,
        default: String,
    },
    Constraint {
        name: String,
        constraint_kind: String,
        columns: Vec<String>,
        expression: String,
    },
    ForeignKey {
        name: String,
        columns: Vec<String>,
        referenced_schema: String,
        referenced_table: String,
        referenced_columns: Vec<String>,
    },
    Index {
        name: String,
        columns: Vec<String>,
        unique: bool,
    },
    Trigger {
        name: String,
        timing: String,
        event: String,
        body: String,
    },
}

impl From<NewTableElementItem> for spelaion_core::NewTableElement {
    fn from(item: NewTableElementItem) -> Self {
        use spelaion_core::NewTableElement as Element;
        match item {
            NewTableElementItem::Column {
                name,
                data_type,
                nullable,
                default,
            } => Element::Column {
                name,
                data_type,
                nullable,
                default,
            },
            NewTableElementItem::Constraint {
                name,
                constraint_kind,
                columns,
                expression,
            } => Element::Constraint {
                name,
                kind: constraint_kind,
                columns,
                expression,
            },
            NewTableElementItem::ForeignKey {
                name,
                columns,
                referenced_schema,
                referenced_table,
                referenced_columns,
            } => Element::ForeignKey {
                name,
                columns,
                referenced_schema,
                referenced_table,
                referenced_columns,
            },
            NewTableElementItem::Index {
                name,
                columns,
                unique,
            } => Element::Index {
                name,
                columns,
                unique,
            },
            NewTableElementItem::Trigger {
                name,
                timing,
                event,
                body,
            } => Element::Trigger {
                name,
                timing,
                event,
                body,
            },
        }
    }
}

/// What can be added on the columns chosen in the explorer (see `spelaion_core::NewIndexKind`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum NewIndexKindItem {
    Index,
    UniqueIndex,
    UniqueConstraint,
    PrimaryKey,
}

impl From<spelaion_core::NewIndexKind> for NewIndexKindItem {
    fn from(kind: spelaion_core::NewIndexKind) -> Self {
        use spelaion_core::NewIndexKind;
        match kind {
            NewIndexKind::Index => Self::Index,
            NewIndexKind::UniqueIndex => Self::UniqueIndex,
            NewIndexKind::UniqueConstraint => Self::UniqueConstraint,
            NewIndexKind::PrimaryKey => Self::PrimaryKey,
        }
    }
}

impl From<NewIndexKindItem> for spelaion_core::NewIndexKind {
    fn from(kind: NewIndexKindItem) -> Self {
        match kind {
            NewIndexKindItem::Index => Self::Index,
            NewIndexKindItem::UniqueIndex => Self::UniqueIndex,
            NewIndexKindItem::UniqueConstraint => Self::UniqueConstraint,
            NewIndexKindItem::PrimaryKey => Self::PrimaryKey,
        }
    }
}

/// What "Create" in the explorer can start (see `spelaion_core::CreateKind`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum CreateKindItem {
    Table,
    View,
    MaterializedView,
    Sequence,
    Schema,
}

impl From<spelaion_core::CreateKind> for CreateKindItem {
    fn from(kind: spelaion_core::CreateKind) -> Self {
        use spelaion_core::CreateKind;
        match kind {
            CreateKind::Table => Self::Table,
            CreateKind::View => Self::View,
            CreateKind::MaterializedView => Self::MaterializedView,
            CreateKind::Sequence => Self::Sequence,
            CreateKind::Schema => Self::Schema,
        }
    }
}

impl From<CreateKindItem> for spelaion_core::CreateKind {
    fn from(kind: CreateKindItem) -> Self {
        match kind {
            CreateKindItem::Table => Self::Table,
            CreateKindItem::View => Self::View,
            CreateKindItem::MaterializedView => Self::MaterializedView,
            CreateKindItem::Sequence => Self::Sequence,
            CreateKindItem::Schema => Self::Schema,
        }
    }
}

impl From<spelaion_core::ObjectActions> for ObjectActionsItem {
    fn from(actions: spelaion_core::ObjectActions) -> Self {
        let spelaion_core::ObjectActions {
            rename_views,
            drop_schemas,
            drop_constraints,
            create,
            new_index,
            active_schema,
            add_constraints,
            add_triggers,
            before_triggers,
            trigger_function,
            add_check_constraints,
        } = actions;
        Self {
            rename_views,
            drop_schemas,
            drop_constraints,
            create: create.into_iter().map(CreateKindItem::from).collect(),
            new_index: new_index.into_iter().map(NewIndexKindItem::from).collect(),
            active_schema,
            add_constraints,
            add_triggers,
            before_triggers,
            trigger_function,
            add_check_constraints,
        }
    }
}

/// The SQL of a change to show, and whether its object can be dropped in cascade.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ObjectChangePreview {
    pub sql: String,
    pub cascade: bool,
}

/// What "Hide" and "Show only" in the explorer do with one name.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum FilterActionItem {
    Hide,
    ShowOnly,
}

/// A connection's explorer filter: personal, per workspace and connection (ADR-025).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerFilterItem {
    pub schemas: NameFilterItem,
    pub objects: NameFilterItem,
    pub columns: NameFilterItem,
}

impl From<spelaion_core::ExplorerFilter> for ExplorerFilterItem {
    fn from(f: spelaion_core::ExplorerFilter) -> Self {
        Self {
            schemas: NameFilterItem {
                include: f.schema_include,
                exclude: f.schema_exclude,
            },
            objects: NameFilterItem {
                include: f.object_include,
                exclude: f.object_exclude,
            },
            columns: NameFilterItem {
                include: f.column_include,
                exclude: f.column_exclude,
            },
        }
    }
}

impl From<ExplorerFilterItem> for spelaion_core::ExplorerFilter {
    fn from(f: ExplorerFilterItem) -> Self {
        Self {
            schema_include: f.schemas.include,
            schema_exclude: f.schemas.exclude,
            object_include: f.objects.include,
            object_exclude: f.objects.exclude,
            column_include: f.columns.include,
            column_exclude: f.columns.exclude,
        }
    }
}

/// How the explorer shows its tree; personal, like the theme.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerPreferencesItem {
    pub show_system: bool,
    pub sort_alphabetically: bool,
    pub sort_ignore_case: bool,
    pub folders_first: bool,
    pub connected_only: bool,
    pub server_side_filters: bool,
    pub link_with_editor: bool,
}

impl From<spelaion_core::ExplorerPreferences> for ExplorerPreferencesItem {
    fn from(p: spelaion_core::ExplorerPreferences) -> Self {
        let spelaion_core::ExplorerPreferences {
            show_system,
            sort_alphabetically,
            sort_ignore_case,
            folders_first,
            connected_only,
            server_side_filters,
            link_with_editor,
        } = p;
        Self {
            show_system,
            sort_alphabetically,
            sort_ignore_case,
            folders_first,
            connected_only,
            server_side_filters,
            link_with_editor,
        }
    }
}

impl From<ExplorerPreferencesItem> for spelaion_core::ExplorerPreferences {
    fn from(p: ExplorerPreferencesItem) -> Self {
        let ExplorerPreferencesItem {
            show_system,
            sort_alphabetically,
            sort_ignore_case,
            folders_first,
            connected_only,
            server_side_filters,
            link_with_editor,
        } = p;
        Self {
            show_system,
            sort_alphabetically,
            sort_ignore_case,
            folders_first,
            connected_only,
            server_side_filters,
            link_with_editor,
        }
    }
}

/// Personal editor preferences; all fields checked by the core store.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EditorPreferencesItem {
    pub font: String,
    pub font_size: u8,
    pub line_height: u8,
    pub tab_size: u8,
    pub line_numbers: bool,
    pub word_wrap: bool,
    pub keyword_case: String,
    pub indent: u8,
    pub comma_position: String,
}
impl From<spelaion_core::EditorPreferences> for EditorPreferencesItem {
    fn from(p: spelaion_core::EditorPreferences) -> Self {
        let spelaion_core::EditorPreferences {
            font,
            font_size,
            line_height,
            tab_size,
            line_numbers,
            word_wrap,
            keyword_case,
            indent,
            comma_position,
        } = p;
        Self {
            font,
            font_size,
            line_height,
            tab_size,
            line_numbers,
            word_wrap,
            keyword_case,
            indent,
            comma_position,
        }
    }
}
impl From<EditorPreferencesItem> for spelaion_core::EditorPreferences {
    fn from(p: EditorPreferencesItem) -> Self {
        let EditorPreferencesItem {
            font,
            font_size,
            line_height,
            tab_size,
            line_numbers,
            word_wrap,
            keyword_case,
            indent,
            comma_position,
        } = p;
        Self {
            font,
            font_size,
            line_height,
            tab_size,
            line_numbers,
            word_wrap,
            keyword_case,
            indent,
            comma_position,
        }
    }
}

/// How the result grid draws its cells; personal, like the theme.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct GridPreferencesItem {
    pub zebra: bool,
    pub null_text: bool,
    pub bool_checkboxes: bool,
}

impl From<spelaion_core::GridPreferences> for GridPreferencesItem {
    fn from(p: spelaion_core::GridPreferences) -> Self {
        let spelaion_core::GridPreferences {
            zebra,
            null_text,
            bool_checkboxes,
        } = p;
        Self {
            zebra,
            null_text,
            bool_checkboxes,
        }
    }
}

impl From<GridPreferencesItem> for spelaion_core::GridPreferences {
    fn from(p: GridPreferencesItem) -> Self {
        let GridPreferencesItem {
            zebra,
            null_text,
            bool_checkboxes,
        } = p;
        Self {
            zebra,
            null_text,
            bool_checkboxes,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub workspace: Option<String>,
    pub trusted: bool,
    pub has_workspace_connections: bool,
    pub user_dir: String,
    /// Configuration files with errors, with file:line:column.
    pub problems: Vec<String>,
}

/// What a connection type asks for (ADR-029).
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SafeguardsItem {
    pub confirm_execution: bool,
    pub confirm_data_changes: bool,
    /// Seconds.
    pub close_idle_after: Option<u32>,
    /// Editor tabs start with auto-commit off (ADR-031).
    pub manual_commit: bool,
    /// The first write switches a tab to manual commit (ADR-031).
    pub smart_commit: bool,
}

impl From<spelaion_config::Safeguards> for SafeguardsItem {
    fn from(s: spelaion_config::Safeguards) -> Self {
        Self {
            confirm_execution: s.confirm_execution,
            confirm_data_changes: s.confirm_data_changes,
            close_idle_after: s.close_idle_after,
            manual_commit: s.manual_commit,
            smart_commit: s.smart_commit,
        }
    }
}

/// The tone of a connection type's badge, from the app's fixed palette (ADR-029).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ToneItem {
    Neutral,
    Green,
    Blue,
    Amber,
    Red,
}

impl From<spelaion_config::Tone> for ToneItem {
    fn from(tone: spelaion_config::Tone) -> Self {
        use spelaion_config::Tone;
        match tone {
            Tone::Neutral => Self::Neutral,
            Tone::Green => Self::Green,
            Tone::Blue => Self::Blue,
            Tone::Amber => Self::Amber,
            Tone::Red => Self::Red,
        }
    }
}

impl From<ToneItem> for spelaion_config::Tone {
    fn from(tone: ToneItem) -> Self {
        match tone {
            ToneItem::Neutral => Self::Neutral,
            ToneItem::Green => Self::Green,
            ToneItem::Blue => Self::Blue,
            ToneItem::Amber => Self::Amber,
            ToneItem::Red => Self::Red,
        }
    }
}

/// One type as the Settings page edits it: what the personal layer says (empty texts and
/// `null` for what it leaves to the team or the built-in definition).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTypeDraft {
    pub name: String,
    pub tone: Option<ToneItem>,
    pub description: String,
    pub confirm_execution: bool,
    pub confirm_data_changes: bool,
    pub close_idle_after: Option<u32>,
    #[serde(default)]
    pub manual_commit: bool,
    #[serde(default)]
    pub smart_commit: bool,
}

impl From<&spelaion_config::ConnectionTypeConfig> for ConnectionTypeDraft {
    fn from(c: &spelaion_config::ConnectionTypeConfig) -> Self {
        let text = |v: &Option<String>| v.clone().unwrap_or_default();
        Self {
            name: text(&c.name),
            tone: c.tone.map(ToneItem::from),
            description: text(&c.description),
            confirm_execution: c.confirm_execution,
            confirm_data_changes: c.confirm_data_changes,
            close_idle_after: c.close_idle_after,
            manual_commit: c.manual_commit,
            smart_commit: c.smart_commit,
        }
    }
}

impl From<ConnectionTypeDraft> for spelaion_config::ConnectionTypeConfig {
    fn from(d: ConnectionTypeDraft) -> Self {
        let text = |v: String| Some(v).filter(|v| !v.trim().is_empty());
        Self {
            name: text(d.name),
            tone: d.tone.map(spelaion_config::Tone::from),
            description: text(d.description),
            confirm_execution: d.confirm_execution,
            confirm_data_changes: d.confirm_data_changes,
            close_idle_after: d.close_idle_after,
            manual_commit: d.manual_commit,
            smart_commit: d.smart_commit,
        }
    }
}

/// A connection type with every layer applied (ADR-029). The UI shows it; the core decides
/// what it does.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTypeItem {
    pub id: String,
    pub name: String,
    /// The tone of its badge.
    pub tone: ToneItem,
    pub description: Option<String>,
    pub built_in: bool,
    pub team: bool,
    /// Production: every change is confirmed.
    pub production: bool,
    pub safeguards: SafeguardsItem,
    /// What the built-in and team definitions turn on: the user cannot switch it off.
    pub inherited: SafeguardsItem,
    /// The user's own definition or changes, if any.
    pub personal: Option<ConnectionTypeDraft>,
}

impl From<&spelaion_config::ConnectionType> for ConnectionTypeItem {
    fn from(t: &spelaion_config::ConnectionType) -> Self {
        Self {
            id: t.id.clone(),
            name: t.name.clone(),
            tone: t.tone.into(),
            description: t.description.clone(),
            built_in: t.built_in,
            team: t.team,
            production: t.is_production(),
            safeguards: t.safeguards.into(),
            inherited: t.inherited.into(),
            personal: t.personal.as_ref().map(ConnectionTypeDraft::from),
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionItem {
    /// `prod/orders`, or `~/local` if it is personal.
    pub id: String,
    pub name: String,
    /// What the database is for, in the user's words; may span lines.
    pub description: Option<String>,
    /// Short labels to find the connection; filtered on in the explorer.
    pub tags: Vec<String>,
    pub engine: String,
    pub env: Option<String>,
    /// The type `env` names (ADR-029); `None` for no label or a plain one.
    pub kind: Option<ConnectionTypeItem>,
    pub folders: Vec<String>,
    pub personal: bool,
    /// The server's host name, for the explorer's search; `None` for a file database.
    pub host: Option<String>,
    pub tunnel: Option<String>,
    pub mcp_exposed: bool,
    pub history_enabled: bool,
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProcessItem {
    pub id: String,
    pub user: String,
    pub host: String,
    pub database: Option<String>,
    pub command: String,
    pub seconds: Option<String>,
    pub state: Option<String>,
    pub sql: Option<String>,
    pub identity: Option<String>,
    pub activity: String,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTargetItem {
    pub id: String,
    pub user: String,
    pub host: String,
    pub command: String,
    pub sql: Option<String>,
    pub identity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshotItem {
    pub own_id: String,
    pub processes: Vec<ProcessItem>,
    pub limited: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    // Local row IDs, epoch milliseconds and counts are transported as JS numbers.
    #[specta(type = specta_typescript::Number)]
    pub id: i64,
    #[specta(type = specta_typescript::Number)]
    pub at_ms: i64,
    pub connection: String,
    pub statement: String,
    #[specta(type = specta_typescript::Number)]
    pub duration_ms: i64,
    #[specta(type = specta_typescript::Number)]
    pub rows: i64,
    pub error: Option<String>,
    pub cancelled: bool,
    pub database: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum HistoryStatusItem {
    Success,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryFilterItem {
    pub search: String,
    pub status: Option<HistoryStatusItem>,
    pub database: Option<String>,
    #[specta(type=Option<specta_typescript::Number>)]
    pub from_ms: Option<i64>,
    #[specta(type=Option<specta_typescript::Number>)]
    pub until_ms: Option<i64>,
    pub pinned: bool,
    #[specta(type=Option<specta_typescript::Number>)]
    pub through_id: Option<i64>,
}
impl From<HistoryFilterItem> for spelaion_core::HistoryFilter {
    fn from(filter: HistoryFilterItem) -> Self {
        Self {
            search: filter.search,
            status: filter.status.map(|status| match status {
                HistoryStatusItem::Success => spelaion_core::HistoryStatus::Success,
                HistoryStatusItem::Error => spelaion_core::HistoryStatus::Error,
                HistoryStatusItem::Cancelled => spelaion_core::HistoryStatus::Cancelled,
            }),
            database: filter.database,
            from_ms: filter.from_ms,
            until_ms: filter.until_ms,
            pinned: filter.pinned,
            through_id: filter.through_id,
        }
    }
}
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryPageItem {
    pub entries: Vec<HistoryItem>,
    pub has_more: bool,
    #[specta(type=specta_typescript::Number)]
    pub through_id: i64,
}
#[derive(Debug, Clone, Serialize, Type)]
pub struct SnippetFolderItem {
    #[specta(type=specta_typescript::Number)]
    pub id: i64,
    pub name: String,
    pub personal: bool,
}

/// A saved query; `connection` is empty when it was saved without one.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SnippetItem {
    #[specta(type = specta_typescript::Number)]
    pub id: i64,
    pub name: String,
    pub statement: String,
    pub connection: String,
    #[specta(type = specta_typescript::Number)]
    pub updated_ms: i64,
    /// Saved outside any repository: visible from every workspace.
    pub personal: bool,
    #[specta(type=Option<specta_typescript::Number>)]
    pub folder_id: Option<i64>,
}

impl From<spelaion_core::Snippet> for SnippetItem {
    fn from(s: spelaion_core::Snippet) -> Self {
        Self {
            id: s.id,
            personal: s.workspace.is_empty(),
            name: s.name,
            statement: s.statement,
            connection: s.connection,
            updated_ms: s.updated_ms,
            folder_id: s.folder_id,
        }
    }
}

impl From<spelaion_core::HistoryEntry> for HistoryItem {
    fn from(entry: spelaion_core::HistoryEntry) -> Self {
        Self {
            id: entry.id,
            at_ms: entry.at_ms,
            connection: entry.connection,
            statement: entry.statement,
            duration_ms: entry.duration_ms,
            rows: entry.rows,
            error: entry.error,
            cancelled: entry.cancelled,
            database: entry.database,
            pinned: entry.pinned,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectOutcome {
    Connected {
        server_version: String,
    },
    /// The password must be asked for. `can_remember`: it can be stored in the keychain.
    PasswordRequired {
        can_remember: bool,
    },
    /// The server rejected the password (or the user): it must be asked for again.
    AuthFailed {
        message: String,
        can_remember: bool,
    },
    /// The repository is not trusted: ask before resolving anything.
    Untrusted,
    /// A repository connection or tunnel is new or changed since the repository was trusted
    /// (ADR-022): show the change and ask before resolving anything.
    Changed {
        /// `connection:<id>` or `tunnel:<name>`, for `accept_change`.
        item: String,
        new: bool,
        fields: Vec<FieldChangeItem>,
        root: String,
    },
    /// The connection uses `${user.…}` variables the user has not defined (ADR-022): ask for
    /// their values, save them with `set_user_vars` and try again.
    MissingVariables {
        names: Vec<String>,
    },
}

/// Trust status of an existing connection before opening its editor.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum EditorAccessOutcome {
    Ready,
    Untrusted,
    Changed {
        /// `connection:<id>`, for `accept_change`.
        item: String,
        new: bool,
        fields: Vec<FieldChangeItem>,
        root: String,
    },
}

/// One field of a changed repository item. Repository files hold secret references, never
/// secrets.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FieldChangeItem {
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

impl ConnectOutcome {
    pub fn changed(root: std::path::PathBuf, change: spelaion_config::ItemChange) -> Self {
        ConnectOutcome::Changed {
            item: change.item,
            new: change.new,
            fields: change
                .fields
                .into_iter()
                .map(|f| FieldChangeItem {
                    field: f.field,
                    before: f.before,
                    after: f.after,
                })
                .collect(),
            root: root.to_string_lossy().into_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnItem {
    pub name: String,
    /// Common logical type (`int`, `text`, `timestamptz`…).
    pub logical: String,
    pub native_type: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ScriptEvent {
    /// Byte range of the statement within the script.
    StatementStarted {
        index: u32,
        start: u32,
        end: u32,
    },
    ResultStarted {
        index: u32,
        result: u32,
        columns: Vec<ColumnItem>,
    },
    Rows {
        index: u32,
        result: u32,
        total_rows: u32,
    },
    Notice {
        index: u32,
        severity: String,
        message: String,
    },
    StatementFinished {
        index: u32,
        rows_affected: Option<f64>,
        elapsed_ms: f64,
        /// Until the engine's first answer.
        execution_ms: f64,
        /// Reading the rows after it.
        fetch_ms: f64,
    },
    StatementFailed {
        index: u32,
        message: String,
        code: Option<String>,
        detail: Option<String>,
        hint: Option<String>,
        /// Error position in bytes within the script, to underline it.
        script_offset: Option<u32>,
        cancelled: bool,
        /// It ran longer than the run's statement timeout.
        timed_out: bool,
    },
    /// The run waits for `answer_script_error` with this ticket (ADR-035).
    ErrorPaused {
        index: u32,
        ticket: u32,
        remaining: u32,
    },
    ScriptFinished {
        executed: u32,
        failed: bool,
        summary: ScriptSummaryItem,
    },
    HistoryWarning {
        message: String,
    },
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn millis(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// "N statements: X ok, Y failed", and where the time went (ADR-035).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScriptSummaryItem {
    pub statements: u32,
    pub executed: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub not_run: u32,
    pub stop: Option<ScriptStopItem>,
    pub execution_ms: f64,
    pub fetch_ms: f64,
}

impl From<spelaion_core::ScriptSummary> for ScriptSummaryItem {
    fn from(s: spelaion_core::ScriptSummary) -> Self {
        Self {
            statements: u32_of(s.statements),
            executed: u32_of(s.executed),
            succeeded: u32_of(s.succeeded),
            failed: u32_of(s.failed),
            not_run: u32_of(s.not_run()),
            stop: s.stop.map(ScriptStopItem::from),
            execution_ms: millis(s.execution),
            fetch_ms: millis(s.fetch),
        }
    }
}

/// Why a run stopped with statements left.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScriptStopItem {
    pub kind: ScriptStopKind,
    /// The statement it stopped at.
    pub index: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ScriptStopKind {
    Failed,
    Cancelled,
    TransactionAborted,
    TransactionRolledBack,
    Closed,
}

impl From<spelaion_core::ScriptStop> for ScriptStopItem {
    fn from(stop: spelaion_core::ScriptStop) -> Self {
        use spelaion_core::ScriptStop;
        let kind = match stop {
            ScriptStop::Failed { .. } => ScriptStopKind::Failed,
            ScriptStop::Cancelled { .. } => ScriptStopKind::Cancelled,
            ScriptStop::TransactionAborted { .. } => ScriptStopKind::TransactionAborted,
            ScriptStop::TransactionRolledBack { .. } => ScriptStopKind::TransactionRolledBack,
            ScriptStop::Closed { .. } => ScriptStopKind::Closed,
        };
        Self {
            kind,
            index: u32_of(stop.index()),
        }
    }
}

/// What a run does when a statement fails (ADR-035).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum OnErrorItem {
    Stop,
    Continue,
    Ask,
}

impl From<OnErrorItem> for spelaion_core::OnError {
    fn from(mode: OnErrorItem) -> Self {
        match mode {
            OnErrorItem::Stop => spelaion_core::OnError::Stop,
            OnErrorItem::Continue => spelaion_core::OnError::Continue,
            OnErrorItem::Ask => spelaion_core::OnError::Ask,
        }
    }
}

impl From<spelaion_core::ScriptErrorMode> for OnErrorItem {
    fn from(mode: spelaion_core::ScriptErrorMode) -> Self {
        match mode {
            spelaion_core::ScriptErrorMode::Stop => OnErrorItem::Stop,
            spelaion_core::ScriptErrorMode::Continue => OnErrorItem::Continue,
            spelaion_core::ScriptErrorMode::Ask => OnErrorItem::Ask,
        }
    }
}

impl From<OnErrorItem> for spelaion_core::ScriptErrorMode {
    fn from(mode: OnErrorItem) -> Self {
        match mode {
            OnErrorItem::Stop => spelaion_core::ScriptErrorMode::Stop,
            OnErrorItem::Continue => spelaion_core::ScriptErrorMode::Continue,
            OnErrorItem::Ask => spelaion_core::ScriptErrorMode::Ask,
        }
    }
}

/// How one run goes: its error mode and, if set, its own statement timeout.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RunOptionsItem {
    pub on_error: OnErrorItem,
    /// Seconds; `null` for the connection's `statement_timeout`, 0 for none in this run.
    pub timeout_seconds: Option<u32>,
}

/// The answer to a paused run.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ErrorAnswerItem {
    Stop,
    Skip,
    SkipAll,
}

impl From<ErrorAnswerItem> for spelaion_core::ErrorAnswer {
    fn from(answer: ErrorAnswerItem) -> Self {
        match answer {
            ErrorAnswerItem::Stop => spelaion_core::ErrorAnswer::Stop,
            ErrorAnswerItem::Skip => spelaion_core::ErrorAnswer::Skip,
            ErrorAnswerItem::SkipAll => spelaion_core::ErrorAnswer::SkipAll,
        }
    }
}

/// How scripts run from the editor, shared by every workspace.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScriptPreferencesItem {
    pub on_error: OnErrorItem,
    pub notify: bool,
    pub notify_after_seconds: u32,
    /// Seconds before "Run anyway" is enabled in a risky statement's confirmation.
    pub confirm_delay_seconds: u32,
}

impl From<spelaion_core::ScriptPreferences> for ScriptPreferencesItem {
    fn from(p: spelaion_core::ScriptPreferences) -> Self {
        Self {
            on_error: p.on_error.into(),
            notify: p.notify,
            notify_after_seconds: p.notify_after_seconds,
            confirm_delay_seconds: p.confirm_delay_seconds,
        }
    }
}

impl From<ScriptPreferencesItem> for spelaion_core::ScriptPreferences {
    fn from(p: ScriptPreferencesItem) -> Self {
        Self {
            on_error: p.on_error.into(),
            notify: p.notify,
            notify_after_seconds: p.notify_after_seconds,
            confirm_delay_seconds: p.confirm_delay_seconds,
        }
    }
}

impl From<CoreEvent> for ScriptEvent {
    fn from(event: CoreEvent) -> Self {
        match event {
            CoreEvent::StatementStarted { index, span } => ScriptEvent::StatementStarted {
                index: u32_of(index),
                start: u32_of(span.start),
                end: u32_of(span.end),
            },
            CoreEvent::ResultStarted {
                index,
                result,
                schema,
            } => ScriptEvent::ResultStarted {
                index: u32_of(index),
                result: u32::try_from(result.0).unwrap_or(u32::MAX),
                columns: schema
                    .columns
                    .iter()
                    .map(|c| ColumnItem {
                        name: c.name.clone(),
                        logical: c.logical.as_str().into(),
                        native_type: c.native_type.clone(),
                    })
                    .collect(),
            },
            CoreEvent::Rows {
                index,
                result,
                total_rows,
            } => ScriptEvent::Rows {
                index: u32_of(index),
                result: u32::try_from(result.0).unwrap_or(u32::MAX),
                total_rows: u32_of(total_rows),
            },
            CoreEvent::Notice { index, notice } => ScriptEvent::Notice {
                index: u32_of(index),
                severity: notice.severity,
                message: notice.message,
            },
            CoreEvent::StatementFinished {
                index,
                rows_affected,
                elapsed,
                execution,
                fetch,
            } => ScriptEvent::StatementFinished {
                index: u32_of(index),
                rows_affected: rows_affected.map(|n| n as f64),
                elapsed_ms: millis(elapsed),
                execution_ms: millis(execution),
                fetch_ms: millis(fetch),
            },
            CoreEvent::StatementFailed { index, error } => ScriptEvent::StatementFailed {
                index: u32_of(index),
                message: error.message,
                code: error.code,
                detail: error.detail,
                hint: error.hint,
                script_offset: error.script_offset.map(u32_of),
                cancelled: error.cancelled,
                timed_out: error.timed_out,
            },
            CoreEvent::ErrorPaused {
                index,
                ticket,
                remaining,
            } => ScriptEvent::ErrorPaused {
                index: u32_of(index),
                ticket: u32::try_from(ticket.0).unwrap_or(u32::MAX),
                remaining: u32_of(remaining),
            },
            CoreEvent::ScriptFinished {
                executed,
                failed,
                summary,
            } => ScriptEvent::ScriptFinished {
                executed: u32_of(executed),
                failed,
                summary: ScriptSummaryItem::from(summary),
            },
            CoreEvent::HistoryWarning { message } => ScriptEvent::HistoryWarning { message },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SchemaItem {
    pub name: String,
    pub system: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ObjectItem {
    pub schema: String,
    pub name: String,
    /// `table`, `partitioned_table`, `view`, `materialized_view`, `foreign_table`.
    pub kind: String,
    pub estimated_rows: Option<f64>,
    pub comment: Option<String>,
}

impl From<ObjectInfo> for ObjectItem {
    fn from(o: ObjectInfo) -> Self {
        let kind = match o.kind {
            ObjectKind::Table => "table",
            ObjectKind::PartitionedTable => "partitioned_table",
            ObjectKind::View => "view",
            ObjectKind::MaterializedView => "materialized_view",
            ObjectKind::ForeignTable => "foreign_table",
        };
        Self {
            schema: o.schema,
            name: o.name,
            kind: kind.into(),
            estimated_rows: o.estimated_rows.map(|n| n as f64),
            comment: o.comment,
        }
    }
}

/// The kinds of `list_schema_objects` (ADR-024): the explorer has a folder for each.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum SchemaObjectKindItem {
    Function,
    Procedure,
    Aggregate,
    Sequence,
    Trigger,
    Enum,
    Domain,
    Type,
    Event,
    Synonym,
}

impl From<SchemaObjectKind> for SchemaObjectKindItem {
    fn from(kind: SchemaObjectKind) -> Self {
        match kind {
            SchemaObjectKind::Function => Self::Function,
            SchemaObjectKind::Procedure => Self::Procedure,
            SchemaObjectKind::Aggregate => Self::Aggregate,
            SchemaObjectKind::Sequence => Self::Sequence,
            SchemaObjectKind::Trigger => Self::Trigger,
            SchemaObjectKind::Enum => Self::Enum,
            SchemaObjectKind::Domain => Self::Domain,
            SchemaObjectKind::Type => Self::Type,
            SchemaObjectKind::Event => Self::Event,
            SchemaObjectKind::Synonym => Self::Synonym,
        }
    }
}

impl From<SchemaObjectKindItem> for SchemaObjectKind {
    fn from(kind: SchemaObjectKindItem) -> Self {
        match kind {
            SchemaObjectKindItem::Function => Self::Function,
            SchemaObjectKindItem::Procedure => Self::Procedure,
            SchemaObjectKindItem::Aggregate => Self::Aggregate,
            SchemaObjectKindItem::Sequence => Self::Sequence,
            SchemaObjectKindItem::Trigger => Self::Trigger,
            SchemaObjectKindItem::Enum => Self::Enum,
            SchemaObjectKindItem::Domain => Self::Domain,
            SchemaObjectKindItem::Type => Self::Type,
            SchemaObjectKindItem::Event => Self::Event,
            SchemaObjectKindItem::Synonym => Self::Synonym,
        }
    }
}

/// A function, sequence, trigger, type… of a schema. The UI sends it back unchanged to ask
/// for its definition.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SchemaObjectItem {
    pub schema: String,
    pub name: String,
    pub kind: SchemaObjectKindItem,
    /// A function's arguments, a trigger's table…
    pub detail: Option<String>,
    pub comment: Option<String>,
}

impl From<SchemaObject> for SchemaObjectItem {
    fn from(o: SchemaObject) -> Self {
        Self {
            schema: o.schema,
            name: o.name,
            kind: o.kind.into(),
            detail: o.detail,
            comment: o.comment,
        }
    }
}

impl From<SchemaObjectItem> for SchemaObject {
    fn from(o: SchemaObjectItem) -> Self {
        Self {
            schema: o.schema,
            name: o.name,
            kind: o.kind.into(),
            detail: o.detail,
            comment: o.comment,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ServerObjectKindItem {
    Role,
    User,
    Login,
    Tablespace,
    Extension,
}

/// A role, login, tablespace or extension: outside every schema (ADR-024).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ServerObjectItem {
    pub name: String,
    pub kind: ServerObjectKindItem,
    pub detail: Option<String>,
    /// Built into the engine: shown only with "Show system objects".
    pub system: bool,
}

impl From<ServerObject> for ServerObjectItem {
    fn from(o: ServerObject) -> Self {
        Self {
            name: o.name,
            kind: match o.kind {
                ServerObjectKind::Role => ServerObjectKindItem::Role,
                ServerObjectKind::User => ServerObjectKindItem::User,
                ServerObjectKind::Login => ServerObjectKindItem::Login,
                ServerObjectKind::Tablespace => ServerObjectKindItem::Tablespace,
                ServerObjectKind::Extension => ServerObjectKindItem::Extension,
            },
            detail: o.detail,
            system: o.system,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnDetails {
    pub name: String,
    pub native_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableDetails {
    pub columns: Vec<ColumnDetails>,
    pub primary_key: Vec<String>,
    pub constraints: Vec<ConstraintDetails>,
    pub foreign_keys: Vec<ForeignKeyDetails>,
    pub indexes: Vec<IndexDetails>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKindItem {
    PrimaryKey,
    Unique,
    Check,
}

impl From<ConstraintKindItem> for ConstraintKind {
    fn from(kind: ConstraintKindItem) -> Self {
        match kind {
            ConstraintKindItem::PrimaryKey => Self::PrimaryKey,
            ConstraintKindItem::Unique => Self::Unique,
            ConstraintKindItem::Check => Self::Check,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConstraintDetails {
    /// None where the engine keeps no name (SQLite's primary key).
    pub name: Option<String>,
    pub kind: ConstraintKindItem,
    pub columns: Vec<String>,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeyDetails {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
}

/// Which way a foreign key goes from a table (see `spelaion_core::KeyDirection`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum KeyDirectionItem {
    Outgoing,
    Incoming,
}

impl From<spelaion_core::KeyDirection> for KeyDirectionItem {
    fn from(direction: spelaion_core::KeyDirection) -> Self {
        match direction {
            spelaion_core::KeyDirection::Outgoing => Self::Outgoing,
            spelaion_core::KeyDirection::Incoming => Self::Incoming,
        }
    }
}

impl From<KeyDirectionItem> for spelaion_core::KeyDirection {
    fn from(direction: KeyDirectionItem) -> Self {
        match direction {
            KeyDirectionItem::Outgoing => Self::Outgoing,
            KeyDirectionItem::Incoming => Self::Incoming,
        }
    }
}

/// A foreign key seen from a table: its own (outgoing) or another table's that points to it
/// (incoming). `schema` and `table` are the other table.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableKeyItem {
    pub direction: KeyDirectionItem,
    pub name: String,
    pub schema: String,
    pub table: String,
    /// This table's columns, in the key's order.
    pub columns: Vec<String>,
    /// The other table's columns, in the same order.
    pub other_columns: Vec<String>,
}

impl From<spelaion_core::TableKey> for TableKeyItem {
    fn from(key: spelaion_core::TableKey) -> Self {
        Self {
            direction: key.direction.into(),
            name: key.name,
            schema: key.schema,
            table: key.table,
            columns: key.columns,
            other_columns: key.other_columns,
        }
    }
}

/// The key the UI wants to follow, by name: the core looks its columns up in the catalog.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KeyRefItem {
    pub direction: KeyDirectionItem,
    pub schema: String,
    pub table: String,
    pub name: String,
}

impl From<KeyRefItem> for spelaion_core::KeyRef {
    fn from(key: KeyRefItem) -> Self {
        Self {
            direction: key.direction.into(),
            schema: key.schema,
            table: key.table,
            name: key.name,
        }
    }
}

/// Where a followed key leads: a table data tab of `schema.table` with the core's `filter`.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KeyTargetItem {
    pub schema: String,
    pub table: String,
    pub filter: String,
}

impl From<spelaion_core::KeyTarget> for KeyTargetItem {
    fn from(target: spelaion_core::KeyTarget) -> Self {
        Self {
            schema: target.schema,
            table: target.table,
            filter: target.filter,
        }
    }
}

/// One key of a row in the references panel: where it leads and its first rows (as text,
/// `None` for NULL); or the column that is NULL in this row (it references nothing through
/// this key); or why it could not be read.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RowReferenceItem {
    pub key: TableKeyItem,
    pub target: Option<KeyTargetItem>,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub more: bool,
    pub null_column: Option<String>,
    pub error: Option<String>,
}

impl From<spelaion_core::RowReference> for RowReferenceItem {
    fn from(reference: spelaion_core::RowReference) -> Self {
        let empty = Self {
            key: reference.key.into(),
            target: None,
            columns: Vec::new(),
            rows: Vec::new(),
            more: false,
            null_column: None,
            error: None,
        };
        match reference.outcome {
            spelaion_core::ReferenceOutcome::Rows(target, rows) => Self {
                target: Some(target.into()),
                columns: rows.columns,
                rows: rows.rows,
                more: rows.more,
                ..empty
            },
            spelaion_core::ReferenceOutcome::Null { column } => Self {
                null_column: Some(column),
                ..empty
            },
            spelaion_core::ReferenceOutcome::Failed(error) => Self {
                error: Some(error),
                ..empty
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct IndexDetails {
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    /// `CREATE INDEX …` as the engine writes it (or a comment for SQLite's automatic ones).
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EditableTableItem {
    pub schema: String,
    pub name: String,
    pub primary_key: Vec<String>,
}

impl From<TableInfo> for TableDetails {
    fn from(t: TableInfo) -> Self {
        Self {
            columns: t
                .columns
                .into_iter()
                .map(|c| ColumnDetails {
                    name: c.name,
                    native_type: c.native_type,
                    nullable: c.nullable,
                    default: c.default,
                    comment: c.comment,
                })
                .collect(),
            primary_key: t.primary_key,
            constraints: t
                .constraints
                .into_iter()
                .map(|c| ConstraintDetails {
                    name: c.name,
                    kind: match c.kind {
                        ConstraintKind::PrimaryKey => ConstraintKindItem::PrimaryKey,
                        ConstraintKind::Unique => ConstraintKindItem::Unique,
                        ConstraintKind::Check => ConstraintKindItem::Check,
                    },
                    columns: c.columns,
                    definition: c.definition,
                })
                .collect(),
            foreign_keys: t
                .foreign_keys
                .into_iter()
                .map(|fk| ForeignKeyDetails {
                    name: fk.name,
                    columns: fk.columns,
                    referenced_schema: fk.referenced_schema,
                    referenced_table: fk.referenced_table,
                    referenced_columns: fk.referenced_columns,
                })
                .collect(),
            indexes: t
                .indexes
                .into_iter()
                .map(|i| IndexDetails {
                    name: i.name,
                    unique: i.unique,
                    primary: i.primary,
                    definition: i.definition,
                })
                .collect(),
        }
    }
}

/// An editor session's connection is gone (ADR-028): the tab shows it and offers to
/// reconnect. Nothing was re-run.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct SessionLost {
    pub session: String,
    pub reason: String,
    /// Closed by its connection type after this many seconds idle (ADR-029), not lost.
    pub idle_seconds: Option<u32>,
    /// A transaction was open when it was lost: the server rolls back what was not committed
    /// (ADR-031).
    pub open_transaction: bool,
}

/// What the core last read about a transaction (ADR-031).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TransactionStateItem {
    Idle,
    Open,
    /// Open, but the engine will only roll it back.
    Failed,
}

/// An editor tab's transaction, as the core keeps it (ADR-031). The UI only shows it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TransactionStatusItem {
    /// The tab follows its transaction (an editor tab on an engine that can tell).
    pub supported: bool,
    pub auto_commit: bool,
    /// The connection type's smart commit.
    pub smart_commit: bool,
    /// Manual commit came from smart commit: it ends with the transaction.
    pub smart_switched: bool,
    pub state: Option<TransactionStateItem>,
    /// Statements run in the open transaction.
    pub statements: u32,
}

impl From<spelaion_core::TransactionStatus> for TransactionStatusItem {
    fn from(s: spelaion_core::TransactionStatus) -> Self {
        use spelaion_core::TransactionState;
        Self {
            supported: s.supported,
            auto_commit: s.auto_commit,
            smart_commit: s.smart_commit,
            smart_switched: s.smart_switched,
            state: s.state.map(|state| match state {
                TransactionState::Idle => TransactionStateItem::Idle,
                TransactionState::Open => TransactionStateItem::Open,
                TransactionState::Failed => TransactionStateItem::Failed,
            }),
            statements: s.statements,
        }
    }
}

/// What a Commit or Rollback said.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TransactionOutcomeItem {
    pub status: TransactionStatusItem,
    pub notices: Vec<NoticeItem>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NoticeItem {
    pub severity: String,
    pub message: String,
}

impl From<spelaion_core::TransactionOutcome> for TransactionOutcomeItem {
    fn from(o: spelaion_core::TransactionOutcome) -> Self {
        Self {
            status: o.status.into(),
            notices: o
                .notices
                .into_iter()
                .map(|n| NoticeItem {
                    severity: n.severity,
                    message: n.message,
                })
                .collect(),
        }
    }
}

/// An editor tab's transaction changed: after a statement, a Commit, a Rollback or the
/// auto-commit toggle (ADR-031).
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct TransactionChanged {
    pub session: String,
    pub status: TransactionStatusItem,
}

/// The window was asked to close while these tabs have a transaction open: the UI asks, then
/// calls `quit_app` (ADR-031).
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct QuitBlocked {
    pub sessions: Vec<String>,
}

/// A dialog nobody waits for any more (the connection test that asked was stopped, or
/// the answer took too long): the UI closes it.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct InteractionClosed {
    pub id: u32,
}

/// Dialog the tunnel needs to show. The UI answers with `answer_interaction`.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRequest {
    pub id: u32,
    pub request: InteractionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InteractionKind {
    UnknownHost {
        hop: u32,
        host: String,
        port: u32,
        algorithm: String,
        fingerprint: String,
    },
    Secret {
        title: String,
        prompt: String,
        echo: bool,
        label: SecretLabel,
    },
}

/// Structured context for our own prompts; remote SSH challenges stay verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SecretLabel {
    KeyPassphrase { path: String, retry: bool },
    Password { user: String, host: String },
    ProxyPassword { user: String, host: String },
    Challenge { host: String, instructions: String },
}

/// One engine in the "add connection" picker. `available`: it has a driver and can be used.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EngineItem {
    pub id: String,
    pub name: String,
    /// `relational`, `analytical` or `nosql`.
    pub category: String,
    /// `network` or `file`.
    pub access: String,
    pub default_port: Option<u16>,
    pub available: bool,
}

impl From<spelaion_core::EngineInfo> for EngineItem {
    fn from(info: spelaion_core::EngineInfo) -> Self {
        Self {
            id: info.id.into(),
            name: info.name.into(),
            category: info.category.as_str().into(),
            access: match info.access {
                spelaion_core::EngineAccess::Network => "network",
                spelaion_core::EngineAccess::File => "file",
            }
            .into(),
            default_port: info.default_port,
            available: info.engine.is_some(),
        }
    }
}

/// A tunnel that a new connection can use.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TunnelItem {
    pub name: String,
    pub personal: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum PasswordMode {
    None,
    Keychain,
    Prompt,
    Env,
    /// A reference the form cannot build (`op://…`, `cmd:…`, `keychain:<name>`), kept as is.
    Other,
    /// Short-lived tokens from a cloud provider's CLI (ADR-022).
    AwsRds,
    AzureAd,
    GoogleCloudSql,
}

impl PasswordMode {
    fn cloud_token(self) -> Option<spelaion_config::CloudToken> {
        use spelaion_config::CloudToken;
        match self {
            PasswordMode::AwsRds => Some(CloudToken::AwsRds),
            PasswordMode::AzureAd => Some(CloudToken::AzureAd),
            PasswordMode::GoogleCloudSql => Some(CloudToken::GoogleCloudSql),
            PasswordMode::None
            | PasswordMode::Keychain
            | PasswordMode::Prompt
            | PasswordMode::Env
            | PasswordMode::Other => None,
        }
    }

    fn from_cloud_token(token: spelaion_config::CloudToken) -> Self {
        use spelaion_config::CloudToken;
        match token {
            CloudToken::AwsRds => PasswordMode::AwsRds,
            CloudToken::AzureAd => PasswordMode::AzureAd,
            CloudToken::GoogleCloudSql => PasswordMode::GoogleCloudSql,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum TlsModeItem {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl From<spelaion_config::TlsModeConfig> for TlsModeItem {
    fn from(mode: spelaion_config::TlsModeConfig) -> Self {
        use spelaion_config::TlsModeConfig;
        match mode {
            TlsModeConfig::Disable => Self::Disable,
            TlsModeConfig::Prefer => Self::Prefer,
            TlsModeConfig::Require => Self::Require,
            TlsModeConfig::VerifyCa => Self::VerifyCa,
            TlsModeConfig::VerifyFull => Self::VerifyFull,
        }
    }
}

/// What a pasted connection URL fills in the form. The password goes only to the form's
/// password field, like a typed one.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UrlFields {
    pub engine: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub tls_mode: Option<TlsModeItem>,
}

impl From<spelaion_config::UrlConnection> for UrlFields {
    fn from(c: spelaion_config::UrlConnection) -> Self {
        Self {
            engine: c.engine.as_str().into(),
            host: c.host,
            port: c.port,
            database: c.database,
            user: c.user,
            password: c.password.map(|p| p.expose().to_string()),
            tls_mode: c.tls_mode.map(TlsModeItem::from),
        }
    }
}

/// What the "add connection" form sends. The password travels apart and is never stored in
/// the connection file.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionDraftItem {
    pub personal: bool,
    pub folder: String,
    pub name: String,
    /// Free text, over lines if needed; empty for none.
    pub description: String,
    /// As typed; the core normalises and validates them.
    pub tags: Vec<String>,
    /// An available engine id from the catalog (`postgres`, `sqlite`, `duckdb`…).
    pub engine: String,
    pub env: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    pub password_mode: PasswordMode,
    pub password_env: String,
    pub password_other: String,
    pub tls_mode: TlsModeItem,
    pub tls_ca: String,
    /// Client certificate and its private key (ADR-022); empty for none.
    pub tls_cert: String,
    pub tls_key: String,
    pub tunnel: String,
    /// Fixed local port of the SSH tunnel; null for a free one.
    pub local_port: Option<u16>,
    /// Through `kubectl port-forward` instead of a tunnel (ADR-015); `port` is the resource's.
    pub kubernetes: Option<KubernetesDraftItem>,
    /// SQLite on a server (with a tunnel): the program there; empty for `sqlite3`.
    pub sqlite3: String,
    pub read_only: bool,
    /// Seconds idle before a keep-alive (ADR-028); null or 0 for off.
    pub keepalive: Option<u32>,
    /// Seconds the database may take to connect (ADR-032); null for the default.
    pub connect_timeout: Option<u32>,
    /// Seconds a statement may run from the editor (ADR-035); `None` or 0 for no limit.
    pub statement_timeout: Option<u32>,
    pub mcp_exposed: bool,
    /// Agents may write (`[mcp] writes = "allow"`, ADR-019).
    pub mcp_writes: bool,
}

impl TryFrom<ConnectionDraftItem> for spelaion_config::ConnectionDraft {
    type Error = String;

    fn try_from(item: ConnectionDraftItem) -> Result<Self, String> {
        use spelaion_config::{PasswordChoice, Source, TlsModeConfig};
        let engine = engine_named(&item.engine)?;
        Ok(Self {
            source: if item.personal {
                Source::User
            } else {
                Source::Workspace
            },
            folder: item.folder,
            name: item.name,
            description: item.description,
            tags: item.tags,
            engine,
            env: item.env,
            host: item.host,
            port: item.port,
            database: item.database,
            user: item.user,
            password: match item.password_mode {
                PasswordMode::None => PasswordChoice::None,
                PasswordMode::Keychain => PasswordChoice::Keychain,
                PasswordMode::Prompt => PasswordChoice::Prompt,
                PasswordMode::Env => PasswordChoice::Env(item.password_env),
                PasswordMode::Other => PasswordChoice::Other(item.password_other),
                mode @ (PasswordMode::AwsRds
                | PasswordMode::AzureAd
                | PasswordMode::GoogleCloudSql) => {
                    PasswordChoice::CloudToken(mode.cloud_token().expect("a cloud token mode"))
                }
            },
            tls_mode: match item.tls_mode {
                TlsModeItem::Disable => TlsModeConfig::Disable,
                TlsModeItem::Prefer => TlsModeConfig::Prefer,
                TlsModeItem::Require => TlsModeConfig::Require,
                TlsModeItem::VerifyCa => TlsModeConfig::VerifyCa,
                TlsModeItem::VerifyFull => TlsModeConfig::VerifyFull,
            },
            tls_ca: item.tls_ca,
            tls_cert: item.tls_cert,
            tls_key: item.tls_key,
            tunnel: item.tunnel,
            local_port: item.local_port,
            kubernetes: item.kubernetes.map(Into::into),
            sqlite3: item.sqlite3,
            read_only: item.read_only,
            keepalive: item.keepalive,
            connect_timeout: item.connect_timeout,
            statement_timeout: item.statement_timeout,
            mcp_exposed: item.mcp_exposed,
            mcp_writes: item.mcp_writes,
            session: Default::default(),
        })
    }
}

impl From<spelaion_config::ConnectionDraft> for ConnectionDraftItem {
    fn from(draft: spelaion_config::ConnectionDraft) -> Self {
        use spelaion_config::{PasswordChoice, Source};
        let (password_mode, password_env, password_other) = match draft.password {
            PasswordChoice::None => (PasswordMode::None, String::new(), String::new()),
            PasswordChoice::Keychain => (PasswordMode::Keychain, String::new(), String::new()),
            PasswordChoice::Prompt => (PasswordMode::Prompt, String::new(), String::new()),
            PasswordChoice::Env(var) => (PasswordMode::Env, var, String::new()),
            PasswordChoice::Other(reference) => (PasswordMode::Other, String::new(), reference),
            PasswordChoice::CloudToken(token) => (
                PasswordMode::from_cloud_token(token),
                String::new(),
                String::new(),
            ),
        };
        Self {
            personal: match draft.source {
                Source::User => true,
                Source::Workspace => false,
            },
            folder: draft.folder,
            name: draft.name,
            description: draft.description,
            tags: draft.tags,
            engine: draft.engine.as_str().into(),
            env: draft.env,
            host: draft.host,
            port: draft.port,
            database: draft.database,
            user: draft.user,
            password_mode,
            password_env,
            password_other,
            tls_mode: draft.tls_mode.into(),
            tls_ca: draft.tls_ca,
            tls_cert: draft.tls_cert,
            tls_key: draft.tls_key,
            tunnel: draft.tunnel,
            local_port: draft.local_port,
            kubernetes: draft.kubernetes.map(Into::into),
            sqlite3: draft.sqlite3,
            read_only: draft.read_only,
            keepalive: draft.keepalive,
            connect_timeout: draft.connect_timeout,
            statement_timeout: draft.statement_timeout,
            mcp_exposed: draft.mcp_exposed,
            mcp_writes: draft.mcp_writes,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuthItem {
    Agent,
    Key,
    Password,
    KeyboardInteractive,
}

/// One hop of the tunnel editor. The password travels apart and never reaches the file.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HopDraftItem {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    /// Methods to try, in order.
    pub auth: Vec<SshAuthItem>,
    pub identity_file: String,
    pub password_mode: PasswordMode,
    pub password_env: String,
    pub password_other: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TunnelDraftItem {
    pub personal: bool,
    pub name: String,
    pub hops: Vec<HopDraftItem>,
    /// Seconds; null for the default (15), 0 for none.
    pub keepalive_interval: Option<u32>,
    /// Null for the default (3), 0 never gives up.
    pub keepalive_count: Option<u32>,
    /// Seconds; null for the default (15).
    pub connect_timeout: Option<u32>,
    /// A SOCKS5 or HTTP proxy in front of the first hop, or instead of hops (ADR-027).
    pub proxy: Option<ProxyDraftItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ProxyTypeItem {
    Socks5,
    Http,
}

/// The proxy section of the tunnel editor. Its password, if typed, travels apart.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProxyDraftItem {
    pub kind: ProxyTypeItem,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    pub password_mode: PasswordMode,
    pub password_env: String,
    pub password_other: String,
}

/// The Kubernetes tab of the connection form; empty fields mean the defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KubernetesDraftItem {
    pub context: String,
    pub namespace: String,
    pub resource: String,
    pub kubeconfig: String,
    pub kubectl: String,
}

impl From<KubernetesDraftItem> for spelaion_config::KubernetesDraft {
    fn from(k: KubernetesDraftItem) -> Self {
        Self {
            context: k.context,
            namespace: k.namespace,
            resource: k.resource,
            kubeconfig: k.kubeconfig,
            kubectl: k.kubectl,
        }
    }
}

/// A port of a Kubernetes resource, for the port picker.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KubePortItem {
    pub port: u16,
    pub name: Option<String>,
    pub container: Option<String>,
}

impl From<spelaion_core::kube::ResourcePort> for KubePortItem {
    fn from(p: spelaion_core::kube::ResourcePort) -> Self {
        Self {
            port: p.port,
            name: p.name,
            container: p.container,
        }
    }
}

impl From<spelaion_config::KubernetesDraft> for KubernetesDraftItem {
    fn from(k: spelaion_config::KubernetesDraft) -> Self {
        Self {
            context: k.context,
            namespace: k.namespace,
            resource: k.resource,
            kubeconfig: k.kubeconfig,
            kubectl: k.kubectl,
        }
    }
}

/// The personal range of local ports for SSH tunnels, both ends included.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TunnelPortsItem {
    pub min: u16,
    pub max: u16,
}

/// A tunnel file shown as a network profile (ADR-027), for Settings → Network. Only what the
/// file says: nothing is resolved, so a repository's profiles can be listed before trusting it.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NetworkProfileItem {
    pub name: String,
    pub personal: bool,
    /// `socks5 host:port` or `http host:port`.
    pub proxy: Option<String>,
    /// `user@host:port` per hop, in order.
    pub hops: Vec<String>,
    /// Display names of the connections that use it.
    pub used_by: Vec<String>,
}

impl NetworkProfileItem {
    pub fn of(config: &spelaion_config::Config, entry: &spelaion_config::TunnelEntry) -> Self {
        Self {
            name: entry.name.clone(),
            personal: entry.source == spelaion_config::Source::User,
            proxy: entry.file.proxy.as_ref().map(|proxy| {
                let kind = match proxy.kind {
                    spelaion_config::ProxyType::Socks5 => "socks5",
                    spelaion_config::ProxyType::Http => "http",
                };
                format!("{kind} {}:{}", proxy.host, proxy.port)
            }),
            hops: entry
                .file
                .hops
                .iter()
                .map(|hop| format!("{}@{}:{}", hop.user, hop.host, hop.port.unwrap_or(22)))
                .collect(),
            used_by: spelaion_config::tunnel_users(config, entry.source, &entry.name),
        }
    }
}

/// Result of testing a network profile from its editor (ADR-032).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TunnelTestOutcome {
    Tested {
        report: TestReportItem,
    },
    /// The repository is not trusted: ask before resolving anything.
    Untrusted,
}

impl From<SshAuthItem> for spelaion_config::SshAuth {
    fn from(item: SshAuthItem) -> Self {
        match item {
            SshAuthItem::Agent => Self::Agent,
            SshAuthItem::Key => Self::Key,
            SshAuthItem::Password => Self::Password,
            SshAuthItem::KeyboardInteractive => Self::KeyboardInteractive,
        }
    }
}

impl From<spelaion_config::SshAuth> for SshAuthItem {
    fn from(auth: spelaion_config::SshAuth) -> Self {
        match auth {
            spelaion_config::SshAuth::Agent => Self::Agent,
            spelaion_config::SshAuth::Key => Self::Key,
            spelaion_config::SshAuth::Password => Self::Password,
            spelaion_config::SshAuth::KeyboardInteractive => Self::KeyboardInteractive,
        }
    }
}

fn password_choice(
    mode: PasswordMode,
    env: String,
    other: String,
) -> spelaion_config::PasswordChoice {
    use spelaion_config::PasswordChoice;
    match mode {
        PasswordMode::None => PasswordChoice::None,
        PasswordMode::Keychain => PasswordChoice::Keychain,
        PasswordMode::Prompt => PasswordChoice::Prompt,
        PasswordMode::Env => PasswordChoice::Env(env),
        PasswordMode::Other => PasswordChoice::Other(other),
        // Not offered for SSH hops: the core refuses it with a message.
        mode @ (PasswordMode::AwsRds | PasswordMode::AzureAd | PasswordMode::GoogleCloudSql) => {
            PasswordChoice::CloudToken(mode.cloud_token().expect("a cloud token mode"))
        }
    }
}

fn password_fields(choice: spelaion_config::PasswordChoice) -> (PasswordMode, String, String) {
    use spelaion_config::PasswordChoice;
    match choice {
        PasswordChoice::None => (PasswordMode::None, String::new(), String::new()),
        PasswordChoice::Keychain => (PasswordMode::Keychain, String::new(), String::new()),
        PasswordChoice::Prompt => (PasswordMode::Prompt, String::new(), String::new()),
        PasswordChoice::Env(var) => (PasswordMode::Env, var, String::new()),
        PasswordChoice::Other(reference) => (PasswordMode::Other, String::new(), reference),
        PasswordChoice::CloudToken(token) => (
            PasswordMode::from_cloud_token(token),
            String::new(),
            String::new(),
        ),
    }
}

impl From<TunnelDraftItem> for spelaion_config::TunnelDraft {
    fn from(item: TunnelDraftItem) -> Self {
        use spelaion_config::{HopDraft, Source};
        Self {
            source: if item.personal {
                Source::User
            } else {
                Source::Workspace
            },
            name: item.name,
            hops: item
                .hops
                .into_iter()
                .map(|hop| HopDraft {
                    host: hop.host,
                    port: hop.port,
                    user: hop.user,
                    auth: hop.auth.into_iter().map(Into::into).collect(),
                    identity_file: hop.identity_file,
                    password: password_choice(
                        hop.password_mode,
                        hop.password_env,
                        hop.password_other,
                    ),
                })
                .collect(),
            options: spelaion_config::TunnelDraftOptions {
                keepalive_interval: item.keepalive_interval,
                keepalive_count: item.keepalive_count,
                connect_timeout: item.connect_timeout,
            },
            proxy: item.proxy.map(|proxy| spelaion_config::ProxyDraft {
                kind: match proxy.kind {
                    ProxyTypeItem::Socks5 => spelaion_config::ProxyType::Socks5,
                    ProxyTypeItem::Http => spelaion_config::ProxyType::Http,
                },
                host: proxy.host,
                port: proxy.port,
                user: proxy.user,
                password: password_choice(
                    proxy.password_mode,
                    proxy.password_env,
                    proxy.password_other,
                ),
            }),
        }
    }
}

impl From<spelaion_config::TunnelDraft> for TunnelDraftItem {
    fn from(draft: spelaion_config::TunnelDraft) -> Self {
        Self {
            personal: match draft.source {
                spelaion_config::Source::User => true,
                spelaion_config::Source::Workspace => false,
            },
            name: draft.name,
            hops: draft
                .hops
                .into_iter()
                .map(|hop| {
                    let (password_mode, password_env, password_other) =
                        password_fields(hop.password);
                    HopDraftItem {
                        host: hop.host,
                        port: hop.port,
                        user: hop.user,
                        auth: hop.auth.into_iter().map(Into::into).collect(),
                        identity_file: hop.identity_file,
                        password_mode,
                        password_env,
                        password_other,
                    }
                })
                .collect(),
            keepalive_interval: draft.options.keepalive_interval,
            keepalive_count: draft.options.keepalive_count,
            connect_timeout: draft.options.connect_timeout,
            proxy: draft.proxy.map(|proxy| {
                let (password_mode, password_env, password_other) = password_fields(proxy.password);
                ProxyDraftItem {
                    kind: match proxy.kind {
                        spelaion_config::ProxyType::Socks5 => ProxyTypeItem::Socks5,
                        spelaion_config::ProxyType::Http => ProxyTypeItem::Http,
                    },
                    host: proxy.host,
                    port: proxy.port,
                    user: proxy.user,
                    password_mode,
                    password_env,
                    password_other,
                }
            }),
        }
    }
}

/// Text format to copy or export rows in.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TextFormat {
    Tsv,
    Csv,
    Json,
    SqlInsert,
    Markdown,
    InList,
    Html,
}

/// How column names are copied (see `spelaion_core::NameStyle`).
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum NameStyleItem {
    Plain,
    Quoted,
    Qualified,
    List,
}

impl From<NameStyleItem> for spelaion_core::NameStyle {
    fn from(item: NameStyleItem) -> Self {
        match item {
            NameStyleItem::Plain => Self::Plain,
            NameStyleItem::Quoted => Self::Quoted,
            NameStyleItem::Qualified => Self::Qualified,
            NameStyleItem::List => Self::List,
        }
    }
}

/// What a search in a result found (`None` from `find_in_result` when a newer search on the
/// same result replaced it).
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct FindOutcome {
    pub count: u32,
    /// More matches than the core keeps: the count reads "N+".
    pub truncated: bool,
}

/// A cell that matches the search: its row in the current view and its column.
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct FindMatchItem {
    pub row: u32,
    pub column: u32,
}

/// The match stepped to, and its place among all of them (from 0).
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct FindStepItem {
    pub index: u32,
    pub row: u32,
    pub column: u32,
}

/// A DBeaver connection found by `scan_dbeaver`. No secret travels: only whether DBeaver saved
/// a password.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DbeaverItem {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// `postgres`, `mysql`, `mariadb`, `sqlite`, `duckdb`; `None` when SpelaionDB cannot open it.
    pub engine: Option<String>,
    /// Why it cannot be imported, if it cannot.
    pub unsupported: Option<String>,
    /// `host:port/database`, or the file for SQLite and DuckDB.
    pub target: String,
    pub user: String,
    pub env: String,
    pub has_password: bool,
    /// `user@host:port` of its SSH tunnel.
    pub ssh: Option<String>,
    /// `host:port` of its SOCKS proxy.
    pub proxy: Option<String>,
}

impl From<&spelaion_config::DbeaverConnection> for DbeaverItem {
    fn from(c: &spelaion_config::DbeaverConnection) -> Self {
        let file = c.engine.as_ref().is_ok_and(|e| e.is_file());
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            folder: c.folder.clone(),
            engine: c.engine.as_ref().ok().map(|e| e.as_str().to_string()),
            unsupported: c.engine.as_ref().err().cloned(),
            target: if file {
                c.database.clone()
            } else {
                let port = c.port.map(|p| format!(":{p}")).unwrap_or_default();
                let database = if c.database.is_empty() {
                    String::new()
                } else {
                    format!("/{}", c.database)
                };
                format!("{}{port}{database}", c.host)
            },
            user: c.user.clone(),
            env: c.env.clone(),
            has_password: c.password.is_some(),
            ssh: c.ssh.as_ref().map(|s| {
                let port = s.port.map(|p| format!(":{p}")).unwrap_or_default();
                format!("{}@{}{port}", s.user, s.host)
            }),
            proxy: c
                .proxy
                .as_ref()
                .map(|p| format!("{}:{}", p.host, p.port.unwrap_or(1080))),
        }
    }
}

/// What `scan_dbeaver` found, and where.
#[derive(Debug, Clone, Serialize, Type)]
pub struct DbeaverScan {
    pub dir: String,
    pub connections: Vec<DbeaverItem>,
}

/// One connection that could not be imported, and why.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ImportSkip {
    pub name: String,
    pub reason: String,
}

/// The outcome of an import: the names given to the new connections (`(DBeaver)` is added
/// when the name was taken) and those left out.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub tunnels: Vec<String>,
    pub skipped: Vec<ImportSkip>,
}

/// The engine behind the id the UI uses (`postgres`, `mysql`…).
pub fn engine_named(name: &str) -> Result<spelaion_config::Engine, String> {
    use spelaion_config::Engine;
    Ok(match name {
        "postgres" => Engine::Postgres,
        "sqlite" => Engine::Sqlite,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "duckdb" => Engine::Duckdb,
        "sqlserver" => Engine::Sqlserver,
        other => return Err(format!("unsupported engine: {other}")),
    })
}

/// What changing a connection's engine does to its settings (ADR-030), by the file's field
/// names. Values are what the form shows: a password only as its reference.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EngineChangeItem {
    pub from: String,
    pub to: String,
    pub kept: Vec<String>,
    pub changed: Vec<FieldChangeItem>,
    pub dropped: Vec<String>,
    pub required: Vec<String>,
}

impl From<spelaion_config::EngineChange> for EngineChangeItem {
    fn from(change: spelaion_config::EngineChange) -> Self {
        Self {
            from: change.from.as_str().into(),
            to: change.to.as_str().into(),
            kept: change.kept,
            changed: change
                .changed
                .into_iter()
                .map(|f| FieldChangeItem {
                    field: f.field,
                    before: f.before,
                    after: f.after,
                })
                .collect(),
            dropped: change.dropped,
            required: change.required,
        }
    }
}

/// The form of a connection mapped to another engine, and what that did.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MappedConnectionItem {
    pub draft: ConnectionDraftItem,
    pub change: EngineChangeItem,
}

/// A connection made by "Duplicate" (ADR-030).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DuplicatedItem {
    pub id: String,
    pub name: String,
    /// Its password was in the keychain and the copy has its own entry now.
    pub password_copied: bool,
}

/// A connection moved to another folder.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MovedItem {
    /// Its identifier now.
    pub id: String,
    /// Its keychain password moved to the new identifier's entry.
    pub password_moved: bool,
}

/// The clipboard form of a connection: no secrets, nothing that runs a program.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardConnectionItem {
    pub text: String,
    /// Settings left out of the text, by the file's field names.
    pub left_out: Vec<String>,
}

/// A personal connection made from clipboard text (ADR-030).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PastedItem {
    pub id: String,
    pub name: String,
    /// Settings of the text that were not taken, by the file's field names.
    pub left_out: Vec<String>,
}

/// A connection name pattern checked and shown for two sample connections.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct NamePatternPreview {
    /// For a PostgreSQL server; empty when the pattern is not valid.
    pub server: String,
    /// For a SQLite file.
    pub file: String,
    /// Why the pattern is not valid.
    pub error: Option<String>,
    /// Unknown variables, each shown to the user.
    pub unknown: Vec<String>,
    /// Every variable a pattern can use.
    pub variables: Vec<String>,
}

/// Why a statement asks for confirmation before running (see `spelaion_core::Risk`).
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum RiskKind {
    DeleteAll,
    UpdateAll,
    Drop,
    Truncate,
    UncheckedWrite,
    DataChange,
    Execution,
}

impl From<spelaion_core::Risk> for RiskKind {
    fn from(risk: spelaion_core::Risk) -> Self {
        use spelaion_core::Risk;
        match risk {
            Risk::DeleteAll => Self::DeleteAll,
            Risk::UpdateAll => Self::UpdateAll,
            Risk::Drop => Self::Drop,
            Risk::Truncate => Self::Truncate,
            Risk::UncheckedWrite => Self::UncheckedWrite,
            Risk::DataChange => Self::DataChange,
            Risk::Execution => Self::Execution,
        }
    }
}

/// A statement of a script that needs confirmation; `start`/`end` in UTF-8 bytes.
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct RiskyItem {
    pub index: u32,
    pub start: u32,
    pub end: u32,
    pub risk: RiskKind,
}

impl From<spelaion_core::RiskyStatement> for RiskyItem {
    fn from(r: spelaion_core::RiskyStatement) -> Self {
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Self {
            index: n(r.index),
            start: n(r.span.start),
            end: n(r.span.end),
            risk: r.risk.into(),
        }
    }
}

/// SQL the explorer can write for a table (see `spelaion_core::SqlTemplate`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum SqlTemplateItem {
    Preview,
    Select,
    SelectByKey,
    SelectIn,
    Insert,
    Upsert,
    Merge,
    Update,
    UpdateFrom,
    Delete,
    DeleteUsing,
}

impl From<SqlTemplateItem> for spelaion_core::SqlTemplate {
    fn from(item: SqlTemplateItem) -> Self {
        match item {
            SqlTemplateItem::Preview => Self::Preview,
            SqlTemplateItem::Select => Self::Select,
            SqlTemplateItem::SelectByKey => Self::SelectByKey,
            SqlTemplateItem::SelectIn => Self::SelectIn,
            SqlTemplateItem::Insert => Self::Insert,
            SqlTemplateItem::Upsert => Self::Upsert,
            SqlTemplateItem::Merge => Self::Merge,
            SqlTemplateItem::UpdateFrom => Self::UpdateFrom,
            SqlTemplateItem::DeleteUsing => Self::DeleteUsing,
            SqlTemplateItem::Update => Self::Update,
            SqlTemplateItem::Delete => Self::Delete,
        }
    }
}

impl From<spelaion_core::SqlTemplate> for SqlTemplateItem {
    fn from(template: spelaion_core::SqlTemplate) -> Self {
        use spelaion_core::SqlTemplate;
        match template {
            SqlTemplate::Preview => Self::Preview,
            SqlTemplate::Select => Self::Select,
            SqlTemplate::SelectByKey => Self::SelectByKey,
            SqlTemplate::SelectIn => Self::SelectIn,
            SqlTemplate::Insert => Self::Insert,
            SqlTemplate::Upsert => Self::Upsert,
            SqlTemplate::Merge => Self::Merge,
            SqlTemplate::UpdateFrom => Self::UpdateFrom,
            SqlTemplate::DeleteUsing => Self::DeleteUsing,
            SqlTemplate::Update => Self::Update,
            SqlTemplate::Delete => Self::Delete,
        }
    }
}

/// A column filter operator (see `spelaion_core::FilterOp`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum FilterOpItem {
    Eq,
    NotEq,
    Gt,
    Lt,
    Ge,
    Le,
    Like,
    NotLike,
    Contains,
    NotContains,
    IsNull,
    IsNotNull,
}

impl From<FilterOpItem> for spelaion_core::FilterOp {
    fn from(item: FilterOpItem) -> Self {
        match item {
            FilterOpItem::Eq => Self::Eq,
            FilterOpItem::NotEq => Self::NotEq,
            FilterOpItem::Gt => Self::Gt,
            FilterOpItem::Lt => Self::Lt,
            FilterOpItem::Ge => Self::Ge,
            FilterOpItem::Le => Self::Le,
            FilterOpItem::Like => Self::Like,
            FilterOpItem::NotLike => Self::NotLike,
            FilterOpItem::Contains => Self::Contains,
            FilterOpItem::NotContains => Self::NotContains,
            FilterOpItem::IsNull => Self::IsNull,
            FilterOpItem::IsNotNull => Self::IsNotNull,
        }
    }
}

/// One condition on a result column; all of them must hold.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ColumnFilterItem {
    pub column: u32,
    pub op: FilterOpItem,
    pub value: String,
}

impl From<ColumnFilterItem> for spelaion_core::ColumnFilter {
    fn from(item: ColumnFilterItem) -> Self {
        Self {
            column: item.column as usize,
            op: item.op.into(),
            value: item.value,
        }
    }
}

/// A page of a table for the table data tab (see `spelaion_core::TablePage`).
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TablePageItem {
    pub schema: String,
    pub name: String,
    pub filter: String,
    pub order_by: String,
    pub limit: u32,
    pub offset: u32,
}

impl From<TablePageItem> for spelaion_core::TablePage {
    fn from(item: TablePageItem) -> Self {
        Self {
            schema: item.schema,
            name: item.name,
            filter: item.filter,
            order_by: item.order_by,
            limit: item.limit.into(),
            offset: item.offset.into(),
        }
    }
}

/// A value typed in the grid for a result column (`None` is NULL).
#[derive(Debug, Clone, Deserialize, Type)]
pub struct CellEditItem {
    pub column: u32,
    pub value: Option<String>,
}

/// A pending change of a table data tab (ADR-014), by position in the page shown.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RowChangeItem {
    Update { row: u32, values: Vec<CellEditItem> },
    Delete { row: u32 },
    Insert { values: Vec<CellEditItem> },
}

impl From<RowChangeItem> for spelaion_core::RowChange {
    fn from(item: RowChangeItem) -> Self {
        let values = |values: Vec<CellEditItem>| {
            values
                .into_iter()
                .map(|v| {
                    (
                        v.column as usize,
                        match v.value {
                            Some(text) => spelaion_core::CellInput::Text(text),
                            None => spelaion_core::CellInput::Null,
                        },
                    )
                })
                .collect()
        };
        match item {
            RowChangeItem::Update { row, values: v } => Self::Update {
                row: row as usize,
                values: values(v),
            },
            RowChangeItem::Delete { row } => Self::Delete { row: row as usize },
            RowChangeItem::Insert { values: v } => Self::Insert { values: values(v) },
        }
    }
}

/// Rows left after filtering a result, out of all it has.
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct FilterOutcome {
    pub rows: u32,
    pub total: u32,
}

/// A piece of a script, in UTF-8 bytes like every position the core gives.
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct TextSpan {
    pub start: u32,
    pub end: u32,
}

/// Rows of a result by position, and the columns taken from each, in order (the grid leaves
/// out the columns it hides).
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CellRange {
    pub first_row: u32,
    pub rows: u32,
    pub columns: Vec<u32>,
}

/// What testing a connection from its form gives (ADR-032). The steps arrive as they change
/// through the command's channel; this is the end.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectionTestOutcome {
    Tested {
        report: TestReportItem,
    },
    /// The repository is not trusted: ask before resolving anything.
    Untrusted,
    /// A repository connection or tunnel is new or changed since it was trusted (ADR-022).
    Changed {
        item: String,
        new: bool,
        fields: Vec<FieldChangeItem>,
        root: String,
    },
    /// Personal `${user.…}` variables to ask for first (ADR-022).
    MissingVariables {
        names: Vec<String>,
    },
}

impl ConnectionTestOutcome {
    pub fn changed(root: std::path::PathBuf, change: spelaion_config::ItemChange) -> Self {
        Self::Changed {
            item: change.item,
            new: change.new,
            fields: change
                .fields
                .into_iter()
                .map(|f| FieldChangeItem {
                    field: f.field,
                    before: f.before,
                    after: f.after,
                })
                .collect(),
            root: root.to_string_lossy().into_owned(),
        }
    }
}

/// The end of a connection test: its steps and the diagnostics to copy, redacted by the core.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TestReportItem {
    pub ok: bool,
    pub stopped: bool,
    pub network_only: bool,
    pub server_version: Option<String>,
    pub steps: Vec<TestStepItem>,
    pub diagnostics: String,
}

impl From<spelaion_core::TestReport> for TestReportItem {
    fn from(report: spelaion_core::TestReport) -> Self {
        Self {
            ok: report.ok,
            stopped: report.stopped,
            network_only: match report.scope {
                spelaion_core::TestScope::Full => false,
                spelaion_core::TestScope::Network => true,
            },
            server_version: report.server_version,
            steps: report.steps.iter().map(TestStepItem::from).collect(),
            diagnostics: report.diagnostics,
        }
    }
}

/// One line of the connection test's log.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TestStepItem {
    pub kind: TestStepKindItem,
    pub status: TestStepStatusItem,
    pub failure: Option<TestFailureItem>,
    pub elapsed_ms: Option<u32>,
    /// The core's English line, for anything the UI does not translate.
    pub title: String,
}

impl From<&spelaion_core::TestStep> for TestStepItem {
    fn from(step: &spelaion_core::TestStep) -> Self {
        use spelaion_core::{StepKind, StepStatus};
        Self {
            kind: match &step.kind {
                StepKind::Resolve => TestStepKindItem::Resolve,
                StepKind::Proxy {
                    proxy_type,
                    host,
                    port,
                } => TestStepKindItem::Proxy {
                    proxy_type: proxy_type.clone(),
                    host: host.clone(),
                    port: *port,
                },
                StepKind::Hop {
                    hop,
                    user,
                    host,
                    port,
                    command,
                } => TestStepKindItem::Hop {
                    hop: u32::try_from(*hop).unwrap_or(u32::MAX).saturating_add(1),
                    user: user.clone(),
                    host: host.clone(),
                    port: *port,
                    command: *command,
                },
                StepKind::Kubernetes {
                    resource,
                    context,
                    namespace,
                } => TestStepKindItem::Kubernetes {
                    resource: resource.clone(),
                    context: context.clone(),
                    namespace: namespace.clone(),
                },
                StepKind::Target { host, port } => TestStepKindItem::Target {
                    host: host.clone(),
                    port: *port,
                },
                StepKind::Database {
                    engine,
                    database,
                    user,
                    file,
                } => TestStepKindItem::Database {
                    engine: engine.clone(),
                    database: database.clone(),
                    user: user.clone(),
                    file: *file,
                },
            },
            status: match step.status {
                StepStatus::Pending => TestStepStatusItem::Pending,
                StepStatus::Running => TestStepStatusItem::Running,
                StepStatus::Ok => TestStepStatusItem::Ok,
                StepStatus::Failed => TestStepStatusItem::Failed,
                StepStatus::Skipped => TestStepStatusItem::Skipped,
                StepStatus::Stopped => TestStepStatusItem::Stopped,
            },
            failure: step.failure.as_ref().map(|f| TestFailureItem {
                kind: f.kind.into(),
                message: f.message.clone(),
                hint: f.kind.hint().to_string(),
            }),
            elapsed_ms: step
                .elapsed
                .map(|e| u32::try_from(e.as_millis()).unwrap_or(u32::MAX)),
            title: step.title(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TestStepKindItem {
    Resolve,
    Proxy {
        proxy_type: String,
        host: String,
        port: u16,
    },
    /// `hop` counts from 1.
    Hop {
        hop: u32,
        user: String,
        host: String,
        port: u16,
        command: bool,
    },
    Kubernetes {
        resource: String,
        context: Option<String>,
        namespace: Option<String>,
    },
    Target {
        host: String,
        port: u16,
    },
    Database {
        engine: String,
        database: String,
        user: String,
        file: bool,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TestStepStatusItem {
    Pending,
    Running,
    Ok,
    Failed,
    Skipped,
    Stopped,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TestFailureItem {
    pub kind: FailureKindItem,
    pub message: String,
    /// The core's English hint; the UI shows its translation of `kind`.
    pub hint: String,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum FailureKindItem {
    Config,
    PasswordRequired,
    Cancelled,
    ProxyAuth,
    SshAuth,
    HostUnreachable,
    HostKeyChanged,
    HostKeyRejected,
    DbAuth,
    DbNotFound,
    Tls,
    Timeout,
    Kubernetes,
    Other,
}

impl From<spelaion_core::FailureKind> for FailureKindItem {
    fn from(kind: spelaion_core::FailureKind) -> Self {
        use spelaion_core::FailureKind;
        match kind {
            FailureKind::Config => Self::Config,
            FailureKind::PasswordRequired => Self::PasswordRequired,
            FailureKind::Cancelled => Self::Cancelled,
            FailureKind::ProxyAuth => Self::ProxyAuth,
            FailureKind::SshAuth => Self::SshAuth,
            FailureKind::HostUnreachable => Self::HostUnreachable,
            FailureKind::HostKeyChanged => Self::HostKeyChanged,
            FailureKind::HostKeyRejected => Self::HostKeyRejected,
            FailureKind::DbAuth => Self::DbAuth,
            FailureKind::DbNotFound => Self::DbNotFound,
            FailureKind::Tls => Self::Tls,
            FailureKind::Timeout => Self::Timeout,
            FailureKind::Kubernetes => Self::Kubernetes,
            FailureKind::Other => Self::Other,
        }
    }
}

// --- The MCP server page (ADR-033) ---------------------------------------------------------

/// Where the `spelaiondb` the clients will run was found.
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum McpCliSourceItem {
    Bundled,
    Path,
    Override,
}

impl From<spelaion_config::mcp_clients::CliSource> for McpCliSourceItem {
    fn from(source: spelaion_config::mcp_clients::CliSource) -> Self {
        use spelaion_config::mcp_clients::CliSource;
        match source {
            CliSource::Bundled => Self::Bundled,
            CliSource::Path => Self::Path,
            CliSource::Override => Self::Override,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpCliItem {
    pub path: String,
    pub source: McpCliSourceItem,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum McpInstallStateItem {
    NoFile,
    NotInstalled,
    Installed,
    Different,
    Unreadable,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum McpConfigLanguageItem {
    Json,
    Toml,
}

/// One AI client's card.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpClientItem {
    pub id: String,
    pub name: String,
    pub path: String,
    pub state: McpInstallStateItem,
    /// Why the file cannot be read, if it cannot.
    pub problem: Option<String>,
    /// The whole configuration, to copy into the file by hand.
    pub manual_config: String,
    pub manual_language: McpConfigLanguageItem,
    /// `claude mcp add …`, for Claude Code.
    pub add_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpClientsItem {
    /// `None`: no `spelaiondb` was found, so nothing can be installed.
    pub cli: Option<McpCliItem>,
    pub clients: Vec<McpClientItem>,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum McpDiffKindItem {
    Same,
    Added,
    Removed,
    Gap,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpDiffLineItem {
    pub kind: McpDiffKindItem,
    pub text: String,
    /// Lines left out, for a gap.
    pub skipped: u32,
}

impl From<spelaion_config::mcp_clients::DiffLine> for McpDiffLineItem {
    fn from(line: spelaion_config::mcp_clients::DiffLine) -> Self {
        use spelaion_config::mcp_clients::DiffLine;
        let (kind, text, skipped) = match line {
            DiffLine::Same(text) => (McpDiffKindItem::Same, text, 0),
            DiffLine::Added(text) => (McpDiffKindItem::Added, text, 0),
            DiffLine::Removed(text) => (McpDiffKindItem::Removed, text, 0),
            DiffLine::Gap(n) => (
                McpDiffKindItem::Gap,
                String::new(),
                u32::try_from(n).unwrap_or(u32::MAX),
            ),
        };
        Self {
            kind,
            text,
            skipped,
        }
    }
}

/// The change that installs our server in one client, shown before it is written.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpInstallPlanItem {
    pub client: String,
    pub path: String,
    /// The file does not exist yet.
    pub creates: bool,
    pub diff: Vec<McpDiffLineItem>,
    /// Sent back to write exactly this change.
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpInstallDoneItem {
    /// Where the previous file was kept, if there was one.
    pub backup: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ToolOutputItem {
    Json,
    Toon,
}

impl From<spelaion_core::ToolOutput> for ToolOutputItem {
    fn from(format: spelaion_core::ToolOutput) -> Self {
        match format {
            spelaion_core::ToolOutput::Json => Self::Json,
            spelaion_core::ToolOutput::Toon => Self::Toon,
        }
    }
}

impl From<ToolOutputItem> for spelaion_core::ToolOutput {
    fn from(format: ToolOutputItem) -> Self {
        match format {
            ToolOutputItem::Json => Self::Json,
            ToolOutputItem::Toon => Self::Toon,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpSettingsItem {
    pub read_only: bool,
    pub output_format: ToolOutputItem,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum McpVisibilityItem {
    Visible,
    NotExposed,
    Untrusted,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum McpAccessKindItem {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum McpReadOnlyReasonItem {
    AllAgents,
    Connection,
    WritesDenied,
    ConfirmUnsupported,
}

/// What agents can do on one connection, as the core decides it.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct McpAccessItem {
    pub id: String,
    pub name: String,
    pub engine: String,
    pub personal: bool,
    pub visibility: McpVisibilityItem,
    /// For a visible connection.
    pub access: Option<McpAccessKindItem>,
    pub reason: Option<McpReadOnlyReasonItem>,
    pub kind: Option<ConnectionTypeItem>,
}

impl McpAccessItem {
    pub fn new(
        entry: &spelaion_config::ConnectionEntry,
        view: spelaion_core::AgentView,
        kind: Option<ConnectionTypeItem>,
    ) -> Self {
        use spelaion_core::{AgentAccess, AgentView, ReadOnlyBecause};
        let (visibility, access, reason) = match view {
            AgentView::NotExposed => (McpVisibilityItem::NotExposed, None, None),
            AgentView::UntrustedRepository => (McpVisibilityItem::Untrusted, None, None),
            AgentView::Visible(AgentAccess::ReadWrite) => (
                McpVisibilityItem::Visible,
                Some(McpAccessKindItem::ReadWrite),
                None,
            ),
            AgentView::Visible(AgentAccess::ReadOnly(because)) => (
                McpVisibilityItem::Visible,
                Some(McpAccessKindItem::ReadOnly),
                Some(match because {
                    ReadOnlyBecause::AllAgents => McpReadOnlyReasonItem::AllAgents,
                    ReadOnlyBecause::Connection => McpReadOnlyReasonItem::Connection,
                    ReadOnlyBecause::WritesDenied => McpReadOnlyReasonItem::WritesDenied,
                    ReadOnlyBecause::ConfirmUnsupported => {
                        McpReadOnlyReasonItem::ConfirmUnsupported
                    }
                }),
            ),
        };
        Self {
            id: entry.id.to_string(),
            name: entry.name(),
            engine: entry.file.engine.as_str().into(),
            personal: entry.id.source == spelaion_config::Source::User,
            visibility,
            access,
            reason,
            kind,
        }
    }
}

// --- The MCP activity (ADR-033) -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum AuditStatusItem {
    Ok,
    Error,
    Refused,
    Cleared,
}

impl From<spelaion_core::AuditDecision> for AuditStatusItem {
    fn from(decision: spelaion_core::AuditDecision) -> Self {
        use spelaion_core::AuditDecision;
        match decision {
            AuditDecision::Executed => Self::Ok,
            AuditDecision::Failed => Self::Error,
            AuditDecision::Rejected => Self::Refused,
            AuditDecision::Cleared => Self::Cleared,
        }
    }
}

impl From<AuditStatusItem> for spelaion_core::AuditDecision {
    fn from(status: AuditStatusItem) -> Self {
        match status {
            AuditStatusItem::Ok => Self::Executed,
            AuditStatusItem::Error => Self::Failed,
            AuditStatusItem::Refused => Self::Rejected,
            AuditStatusItem::Cleared => Self::Cleared,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuditFilterItem {
    pub search: String,
    pub tool: Option<String>,
    pub status: Option<AuditStatusItem>,
    pub session: Option<String>,
}

impl From<AuditFilterItem> for spelaion_core::AuditFilter {
    fn from(f: AuditFilterItem) -> Self {
        Self {
            search: f.search,
            tool: f.tool,
            decision: f.status.map(Into::into),
            session: f.session,
        }
    }
}

/// One MCP call, with its secrets already masked by the core.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuditEventItem {
    #[specta(type = specta_typescript::Number)]
    pub id: i64,
    #[specta(type = specta_typescript::Number)]
    pub at_ms: i64,
    pub session: Option<String>,
    pub client: String,
    pub connection: String,
    pub tool: Option<String>,
    pub statement: String,
    pub status: AuditStatusItem,
    #[specta(type = Option<specta_typescript::Number>)]
    pub rows: Option<i64>,
    #[specta(type = Option<specta_typescript::Number>)]
    pub duration_ms: Option<i64>,
    pub detail: Option<String>,
}

impl From<spelaion_core::AuditRecord> for AuditEventItem {
    fn from(record: spelaion_core::AuditRecord) -> Self {
        let e = spelaion_core::audit::redacted(&record.entry);
        Self {
            id: record.id,
            at_ms: e.at_ms,
            session: e.session,
            client: e.client,
            connection: e.connection,
            tool: e.tool,
            statement: e.statement,
            status: e.decision.into(),
            rows: e.rows,
            duration_ms: e.duration_ms,
            detail: e.detail,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuditPageItem {
    pub events: Vec<AuditEventItem>,
    /// How many the filter keeps in all.
    #[specta(type = specta_typescript::Number)]
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AuditSessionItem {
    pub session: String,
    pub client: String,
    #[specta(type = specta_typescript::Number)]
    pub started_ms: i64,
    #[specta(type = specta_typescript::Number)]
    pub ended_ms: i64,
    #[specta(type = specta_typescript::Number)]
    pub calls: i64,
    #[specta(type = specta_typescript::Number)]
    pub refused: i64,
    #[specta(type = specta_typescript::Number)]
    pub failed: i64,
}

impl From<spelaion_core::AuditSession> for AuditSessionItem {
    fn from(s: spelaion_core::AuditSession) -> Self {
        Self {
            session: s.session,
            client: s.client,
            started_ms: s.started_ms,
            ended_ms: s.ended_ms,
            calls: s.calls,
            refused: s.refused,
            failed: s.failed,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum AuditFormatItem {
    Csv,
    Json,
}

/// Character encoding of an exported file (see `spelaion_core::TextEncoding`).
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TextEncodingItem {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Windows1252,
}

impl From<TextEncodingItem> for spelaion_core::TextEncoding {
    fn from(item: TextEncodingItem) -> Self {
        match item {
            TextEncodingItem::Utf8 => Self::Utf8,
            TextEncodingItem::Utf8Bom => Self::Utf8Bom,
            TextEncodingItem::Utf16Le => Self::Utf16Le,
            TextEncodingItem::Windows1252 => Self::Windows1252,
        }
    }
}

/// How an export writes its file (ADR-034). The core checks the delimiter.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportFileOptions {
    pub format: TextFormat,
    /// Column names as the first line (CSV and TSV).
    pub header: bool,
    /// One character (CSV only).
    pub delimiter: String,
    pub null_text: String,
    pub escape_formulas: bool,
    pub encoding: TextEncodingItem,
    /// Columns of the result by position, in order (the shown ones); `None`: all.
    pub columns: Option<Vec<u32>>,
}

impl ExportFileOptions {
    pub fn csv(&self) -> Result<spelaion_core::CsvOptions, String> {
        let mut chars = self.delimiter.chars();
        let (Some(delimiter), None) = (chars.next(), chars.next()) else {
            return Err("the delimiter must be exactly one character".into());
        };
        let csv = spelaion_core::CsvOptions {
            delimiter,
            null_text: self.null_text.clone(),
            escape_formulas: self.escape_formulas,
        };
        csv.check()?;
        Ok(csv)
    }
}

/// What writing an export file did.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummaryItem {
    pub rows: f64,
    pub bytes: f64,
    /// Characters the encoding lacks, written as `?`.
    pub replaced: f64,
}

impl From<spelaion_core::FileSummary> for ExportSummaryItem {
    fn from(summary: spelaion_core::FileSummary) -> Self {
        Self {
            rows: summary.rows as f64,
            bytes: summary.bytes as f64,
            replaced: summary.replaced as f64,
        }
    }
}

/// Rows and bytes written so far by a streaming export; `table` is the position of the table
/// being written when several are exported.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgressItem {
    pub table: Option<u32>,
    pub rows: f64,
    pub bytes: f64,
}

/// How a streaming export ended.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ExportOutcome {
    Done {
        summary: ExportSummaryItem,
    },
    Cancelled,
    /// The connection's type confirms execution: ask, then export again with `confirmed`.
    NeedsConfirmation,
    /// Not a read: only the loaded rows can be exported.
    NotARead {
        message: String,
    },
    Failed {
        message: String,
    },
}

impl From<Result<spelaion_core::FileSummary, spelaion_core::ExportError>> for ExportOutcome {
    fn from(result: Result<spelaion_core::FileSummary, spelaion_core::ExportError>) -> Self {
        use spelaion_core::ExportError;
        match result {
            Ok(summary) => Self::Done {
                summary: summary.into(),
            },
            Err(ExportError::Cancelled) => Self::Cancelled,
            Err(ExportError::NeedsConfirmation) => Self::NeedsConfirmation,
            Err(e @ ExportError::NotARead(_)) => Self::NotARead {
                message: e.to_string(),
            },
            Err(
                e @ (ExportError::Table(_)
                | ExportError::Database(_)
                | ExportError::Io { .. }
                | ExportError::Invalid(_)),
            ) => Self::Failed {
                message: e.to_string(),
            },
        }
    }
}

/// A table data tab's view to export: every row its filter keeps, in its order.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableExportItem {
    pub schema: String,
    pub name: String,
    pub filter: String,
    pub order_by: String,
}

/// A table or view chosen in the explorer.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RelationRefItem {
    pub schema: String,
    pub name: String,
}

/// A file written by `export_tables`.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportedFileItem {
    pub schema: String,
    pub name: String,
    pub path: String,
    pub summary: ExportSummaryItem,
}

/// How exporting several tables ended: the files written, and where it stopped if it did.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExportTablesOutcome {
    pub files: Vec<ExportedFileItem>,
    pub cancelled: bool,
    /// Position of the table that failed, with why.
    pub failed_table: Option<u32>,
    pub error: Option<String>,
}

/// What the export dialog says before running a query again (see `spelaion_core::RerunCheck`).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RerunCheckItem {
    pub refused: Option<String>,
    pub open_transaction: bool,
}

// ------------------------------------------------- SQL files and open tabs (ADR-036)

/// The encoding a `.sql` file was read in and is written back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum FileEncodingItem {
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl From<spelaion_core::FileEncoding> for FileEncodingItem {
    fn from(e: spelaion_core::FileEncoding) -> Self {
        match e {
            spelaion_core::FileEncoding::Utf8 => Self::Utf8,
            spelaion_core::FileEncoding::Utf8Bom => Self::Utf8Bom,
            spelaion_core::FileEncoding::Utf16Le => Self::Utf16Le,
            spelaion_core::FileEncoding::Utf16Be => Self::Utf16Be,
            spelaion_core::FileEncoding::Windows1252 => Self::Windows1252,
        }
    }
}

impl From<FileEncodingItem> for spelaion_core::FileEncoding {
    fn from(e: FileEncodingItem) -> Self {
        match e {
            FileEncodingItem::Utf8 => Self::Utf8,
            FileEncodingItem::Utf8Bom => Self::Utf8Bom,
            FileEncodingItem::Utf16Le => Self::Utf16Le,
            FileEncodingItem::Utf16Be => Self::Utf16Be,
            FileEncodingItem::Windows1252 => Self::Windows1252,
        }
    }
}

/// The line break a `.sql` file is written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum LineEndingItem {
    Lf,
    CrLf,
    Cr,
}

impl From<spelaion_core::LineEnding> for LineEndingItem {
    fn from(e: spelaion_core::LineEnding) -> Self {
        match e {
            spelaion_core::LineEnding::Lf => Self::Lf,
            spelaion_core::LineEnding::CrLf => Self::CrLf,
            spelaion_core::LineEnding::Cr => Self::Cr,
        }
    }
}

impl From<LineEndingItem> for spelaion_core::LineEnding {
    fn from(e: LineEndingItem) -> Self {
        match e {
            LineEndingItem::Lf => Self::Lf,
            LineEndingItem::CrLf => Self::CrLf,
            LineEndingItem::Cr => Self::Cr,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct FileFormatItem {
    pub encoding: FileEncodingItem,
    pub line_ending: LineEndingItem,
}

impl From<spelaion_core::FileFormat> for FileFormatItem {
    fn from(f: spelaion_core::FileFormat) -> Self {
        Self {
            encoding: f.encoding.into(),
            line_ending: f.line_ending.into(),
        }
    }
}

impl From<FileFormatItem> for spelaion_core::FileFormat {
    fn from(f: FileFormatItem) -> Self {
        Self {
            encoding: f.encoding.into(),
            line_ending: f.line_ending.into(),
        }
    }
}

/// A `.sql` file read by the core: its text with `\n` line breaks.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SqlFileItem {
    pub path: String,
    /// The file's name, for the tab's title.
    pub name: String,
    pub text: String,
    pub format: FileFormatItem,
    /// SHA-256 of the file's bytes, sent back with the next save.
    pub hash: String,
}

/// What a save did. The conflicts are answers, not errors: the app asks the user.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SaveFileOutcome {
    #[serde(rename_all = "camelCase")]
    Saved {
        path: String,
        name: String,
        hash: String,
        format: FileFormatItem,
    },
    /// The file changed on disk since it was read or saved: overwrite, reload or cancel.
    ChangedOnDisk,
    /// The file was deleted on disk since: write it again or cancel.
    DeletedOnDisk,
    /// The file's encoding cannot hold this character: save as UTF-8 instead?
    Unrepresentable { character: String, encoding: String },
}

/// The file behind a kept file tab.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OpenTabFileItem {
    pub path: String,
    pub hash: Option<String>,
    pub format: FileFormatItem,
    /// The text differs from the file: it is restored as unsaved.
    pub modified: bool,
}

/// An editor tab as kept between runs: text and settings, never results or session state.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct OpenTabItem {
    pub title: Option<String>,
    pub text: String,
    pub connection: Option<String>,
    pub pinned: bool,
    pub on_error: Option<OnErrorItem>,
    pub timeout_seconds: Option<u32>,
    pub snippet_name: Option<String>,
    pub file: Option<OpenTabFileItem>,
}

impl From<OpenTabItem> for spelaion_core::SavedTab {
    fn from(t: OpenTabItem) -> Self {
        Self {
            title: t.title,
            text: t.text,
            connection: t.connection,
            pinned: t.pinned,
            on_error: t.on_error.map(Into::into),
            timeout_seconds: t.timeout_seconds,
            snippet_name: t.snippet_name,
            file: t.file.map(|f| spelaion_core::SavedFile {
                path: f.path,
                hash: f.hash,
                encoding: f.format.encoding.into(),
                line_ending: f.format.line_ending.into(),
                modified: f.modified,
            }),
        }
    }
}

impl From<spelaion_core::SavedTab> for OpenTabItem {
    fn from(t: spelaion_core::SavedTab) -> Self {
        Self {
            title: t.title,
            text: t.text,
            connection: t.connection,
            pinned: t.pinned,
            on_error: t.on_error.map(Into::into),
            timeout_seconds: t.timeout_seconds,
            snippet_name: t.snippet_name,
            file: t.file.map(|f| OpenTabFileItem {
                path: f.path,
                hash: f.hash,
                format: FileFormatItem {
                    encoding: f.encoding.into(),
                    line_ending: f.line_ending.into(),
                },
                modified: f.modified,
            }),
        }
    }
}

/// Why a restored tab is not exactly as it was kept.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RestoreNoteItem {
    ConnectionMissing { connection: String },
    FileMissing,
    FileUnreadable { message: String },
    FileChangedOnDisk,
}

impl From<spelaion_core::RestoreNote> for RestoreNoteItem {
    fn from(n: spelaion_core::RestoreNote) -> Self {
        match n {
            spelaion_core::RestoreNote::ConnectionMissing { connection } => {
                Self::ConnectionMissing { connection }
            }
            spelaion_core::RestoreNote::FileMissing => Self::FileMissing,
            spelaion_core::RestoreNote::FileUnreadable { message } => {
                Self::FileUnreadable { message }
            }
            spelaion_core::RestoreNote::FileChangedOnDisk => Self::FileChangedOnDisk,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RestoredTabItem {
    pub tab: OpenTabItem,
    pub notes: Vec<RestoreNoteItem>,
}

/// The open folder's kept tabs, checked by the core. They come back disconnected.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RestoredTabsItem {
    /// Which folder they belong to: sent back with every `save_tabs`.
    pub scope: String,
    pub tabs: Vec<RestoredTabItem>,
    pub active: Option<u32>,
    /// The personal "reconnect at startup" setting: connect only, never run.
    pub reconnect: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct RecentFileItem {
    pub path: String,
    pub name: String,
    /// Still on disk (checked when listed).
    pub exists: bool,
}

/// The numbers about a grid selection (the calc panel), computed by the core over the stored
/// result. Values are text as the grid shows them; counts are exact up to 2^53.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SelectionStatsItem {
    pub cells: f64,
    pub nulls: f64,
    /// Cells that are not NULL.
    pub values: f64,
    pub distinct: f64,
    /// More different values than the core counts: `distinct` reads "N+".
    pub distinct_capped: bool,
    /// Numeric values: what sum, average and median take.
    pub numbers: f64,
    pub sum: Option<String>,
    pub avg: Option<String>,
    pub median: Option<String>,
    /// Too many numbers for a median.
    pub median_capped: bool,
    /// `None` when the selection mixes values that do not compare (numbers and text).
    pub min: Option<String>,
    pub max: Option<String>,
    /// The most frequent value, when one repeats.
    pub mode: Option<String>,
    pub mode_count: f64,
}

impl From<spelaion_core::SelectionStats> for SelectionStatsItem {
    fn from(s: spelaion_core::SelectionStats) -> Self {
        Self {
            cells: s.cells as f64,
            nulls: s.nulls as f64,
            values: s.values as f64,
            distinct: s.distinct as f64,
            distinct_capped: s.distinct_capped,
            numbers: s.numbers as f64,
            sum: s.sum,
            avg: s.avg,
            median: s.median,
            median_capped: s.median_capped,
            min: s.min,
            max: s.max,
            mode: s.mode,
            mode_count: s.mode_count as f64,
        }
    }
}

/// A column of a table data tab, for its statistics: the tab's table and filter.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnStatsTarget {
    pub schema: String,
    pub name: String,
    pub filter: String,
}

/// Whether a statistic could be computed.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StatStateItem {
    Done,
    /// The column's type has no such number (booleans have no minimum).
    NotForType,
    /// The engine refused it: its message.
    Failed {
        message: String,
    },
}

impl<T> From<&spelaion_core::Stat<T>> for StatStateItem {
    fn from(stat: &spelaion_core::Stat<T>) -> Self {
        match stat {
            spelaion_core::Stat::Value(_) => Self::Done,
            spelaion_core::Stat::NotForType => Self::NotForType,
            spelaion_core::Stat::Failed(message) => Self::Failed {
                message: message.clone(),
            },
        }
    }
}

/// One of a column's most frequent values.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TopValueItem {
    /// `None` for NULL.
    pub text: Option<String>,
    pub count: f64,
    /// The condition the core wrote to keep these rows (added to the tab's filter with
    /// `add_table_condition`); `None` when the value is too long to filter by.
    pub condition: Option<String>,
}

/// A table column's statistics, counted by the server with read-only queries.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnStatsItem {
    /// Rows the numbers describe (`sampleRows` when sampled).
    pub rows: f64,
    pub sampled: bool,
    pub sample_rows: f64,
    pub nulls: f64,
    pub distinct: Option<f64>,
    pub distinct_state: StatStateItem,
    pub min: Option<String>,
    pub max: Option<String>,
    pub range_state: StatStateItem,
    pub top: Vec<TopValueItem>,
    pub top_state: StatStateItem,
    /// The statements that ran, every one a read.
    pub statements: Vec<String>,
}

impl ColumnStatsItem {
    pub fn new(s: spelaion_core::ColumnStats, sample_rows: u64) -> Self {
        let distinct_state = StatStateItem::from(&s.distinct);
        let range_state = StatStateItem::from(&s.range);
        let top_state = StatStateItem::from(&s.top);
        let (min, max) = match s.range {
            spelaion_core::Stat::Value(range) => range,
            spelaion_core::Stat::NotForType | spelaion_core::Stat::Failed(_) => (None, None),
        };
        Self {
            rows: s.rows as f64,
            sampled: s.sampled,
            sample_rows: sample_rows as f64,
            nulls: s.nulls as f64,
            distinct: match s.distinct {
                spelaion_core::Stat::Value(n) => Some(n as f64),
                spelaion_core::Stat::NotForType | spelaion_core::Stat::Failed(_) => None,
            },
            distinct_state,
            min,
            max,
            range_state,
            top: match s.top {
                spelaion_core::Stat::Value(top) => top
                    .into_iter()
                    .map(|v| TopValueItem {
                        text: v.text,
                        count: v.count as f64,
                        condition: v.condition,
                    })
                    .collect(),
                spelaion_core::Stat::NotForType | spelaion_core::Stat::Failed(_) => Vec::new(),
            },
            top_state,
            statements: s.statements,
        }
    }
}

/// How asking for a column's statistics ended.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ColumnStatsOutcome {
    Done { stats: Box<ColumnStatsItem> },
    Cancelled,
}
