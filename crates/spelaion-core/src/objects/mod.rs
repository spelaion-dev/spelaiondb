//! What the explorer does with an object besides reading it (ADR-026): its name as the engine
//! writes it, for the clipboard, the statements that rename or drop it, and those that create a
//! new one or an index on chosen columns (opened in the editor, never run from here), and the
//! search behind "Open object by name", and an editor session's active schema. Statements are
//! written
//! here with each engine's syntax and quoting; the UI only says what to change, and a change
//! runs only once the user has seen its SQL and confirmed it.

mod alter;
mod create;
mod ddl;
mod privileges;
mod rebuild;
mod source;
mod users;

pub use alter::{
    AlterationPlan, ColumnEdit, DroppedConstraint, EditableColumn, PlanRisk, TableAlteration,
    TableStructure, alteration_plan, apply_alteration, table_structure,
};
pub use create::{
    NewColumn, NewForeignKey, NewIndex, NewTable, apply_create_table, create_table_plan,
};
pub use privileges::{
    GrantedPrivilege, PrivilegeChange, PrivilegeScope, PrivilegeTarget, apply_privilege_change,
    principal_privileges, privilege_options, privilege_plan, privilege_scope_fields,
    privilege_scopes,
};
pub use source::{SourceTarget, editable_source};
pub use users::{
    NewPrincipal, PrincipalActions, PrincipalChange, PrincipalKind, PrincipalTarget,
    apply_principal_change, principal_actions, principal_locked, principal_plan,
};

use spelaion_driver_api::{
    Connection, ConstraintKind, Dialect, DriverError, NameFilter, ObjectInfo, ObjectKind,
    SchemaObject, SchemaObjectKind, Value,
};

use crate::edit::run_one;
use crate::generate::scalar_metadata;
use crate::session::Session;

/// A name as the engine writes it in SQL: `schema.table`, `schema.table.column`…, each part
/// quoted only where the engine needs it. SQLite's and DuckDB's `main` schema is left out, as
/// in the SQL the explorer generates; an empty schema means a name outside every schema (a
/// schema itself).
pub fn qualified_name(dialect: &dyn Dialect, schema: &str, names: &[String]) -> String {
    let main =
        matches!(dialect.name(), "sqlite" | "duckdb") && schema == "main" && !names.is_empty();
    let schema = (!schema.is_empty() && !main).then_some(schema);
    schema
        .into_iter()
        .chain(names.iter().map(String::as_str))
        .map(|part| dialect.quote_identifier(part))
        .collect::<Vec<_>>()
        .join(".")
}

/// What "Open object by name" found: a relation or another object of a schema.
#[derive(Debug, Clone, PartialEq)]
pub enum FoundObject {
    Relation(ObjectInfo),
    Other(SchemaObject),
}

impl FoundObject {
    pub fn name(&self) -> &str {
        match self {
            Self::Relation(o) => &o.name,
            Self::Other(o) => &o.name,
        }
    }
    pub fn schema(&self) -> &str {
        match self {
            Self::Relation(o) => &o.schema,
            Self::Other(o) => &o.schema,
        }
    }
}

/// The most objects one search returns: enough to choose from, small enough to show.
pub const SEARCH_LIMIT: usize = 100;

/// A pattern of ADR-025 that matches names containing `text` literally, ignoring case.
fn contains_pattern(text: &str) -> String {
    let mut pattern = String::from("*");
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '%' | '?') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('*');
    pattern
}

/// The objects whose name contains `text` (ignoring case) in the schemas the explorer shows:
/// those `schemas` keeps, system ones only with `show_system`, and among their objects those
/// `objects` keeps (ADR-025). The name goes into the catalog queries as a pattern, so big
/// catalogs are narrowed on the server. Exact names come first, then those that start with the
/// text, then the rest, at most `limit`.
pub async fn search_objects(
    connection: &dyn Connection,
    text: &str,
    schemas: &NameFilter,
    objects: &NameFilter,
    show_system: bool,
    limit: usize,
) -> Result<Vec<FoundObject>, DriverError> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let search = NameFilter {
        include: vec![contains_pattern(text)],
        exclude: Vec::new(),
    };
    let mut found = Vec::new();
    for schema in connection.list_schemas_matching(schemas).await? {
        if schema.system && !show_system {
            continue;
        }
        let relations = connection
            .list_objects_matching(&schema.name, &search)
            .await?;
        found.extend(
            relations
                .into_iter()
                .filter(|o| objects.matches(&o.name))
                .map(FoundObject::Relation),
        );
        // Without its functions or triggers a schema still has its tables, as in the tree.
        let others = connection
            .list_schema_objects_matching(&schema.name, &search)
            .await
            .unwrap_or_default();
        found.extend(
            others
                .into_iter()
                .filter(|o| objects.matches(&o.name))
                .map(FoundObject::Other),
        );
    }
    let lower = text.to_lowercase();
    let rank = |o: &FoundObject| {
        let name = o.name().to_lowercase();
        if name == lower {
            0
        } else if name.starts_with(&lower) {
            1
        } else {
            2
        }
    };
    found.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
            .then_with(|| a.schema().cmp(b.schema()))
    });
    found.truncate(limit);
    Ok(found)
}

/// Makes `schema` where this session finds unqualified names: `search_path` on PostgreSQL (the
/// schema, then `public`), the database on MySQL and MariaDB, `SET schema` on DuckDB. Only the
/// session changes, never the saved connection.
pub async fn set_active_schema(session: &Session, schema: &str) -> Result<(), ObjectError> {
    let dialect = session.dialect();
    let failed = |message: String| ObjectError::Failed {
        index: 1,
        total: 1,
        message,
    };
    let statement = match dialect.name() {
        "mysql" => {
            return session
                .connection()
                .select_database(schema)
                .await
                .map_err(|e| failed(e.to_string()));
        }
        "postgres" => {
            let quoted = dialect.quote_identifier(schema);
            if schema == "public" {
                format!("SET search_path TO {quoted}")
            } else {
                format!("SET search_path TO {quoted}, public")
            }
        }
        "duckdb" => format!("SET schema = '{}'", schema.replace('\'', "''")),
        _ => {
            return Err(ObjectError::Unsupported(
                "this engine has no schema per session: qualify the names with their schema".into(),
            ));
        }
    };
    run_one(session, &statement)
        .await
        .map(|_| ())
        .map_err(failed)
}

/// The schema where this session finds unqualified names (for "Link with editor").
pub async fn active_schema(session: &Session) -> Result<Option<String>, String> {
    let sql = match session.dialect().name() {
        "mysql" => {
            return session
                .connection()
                .current_database()
                .await
                .map_err(|e| e.to_string());
        }
        "sqlite" => return Ok(Some("main".into())),
        "sqlserver" => "SELECT SCHEMA_NAME()",
        _ => "SELECT current_schema()",
    };
    Ok(match scalar_metadata(session, sql).await? {
        None | Some(Value::Null) => None,
        Some(Value::Text(text)) => Some(text),
        Some(other) => Some(other.to_string()),
    })
}

/// An object of the explorer that can be changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectTarget {
    /// A schema (a database on MySQL and MariaDB).
    Schema { name: String },
    /// A table, view, materialized view or foreign table.
    Relation {
        schema: String,
        name: String,
        kind: ObjectKind,
    },
    Column {
        schema: String,
        table: String,
        name: String,
    },
    /// An index of a table; `primary` when it is the primary key's.
    Index {
        schema: String,
        table: String,
        name: String,
        primary: bool,
    },
    /// A primary key, unique or check constraint of a table, by its name.
    Constraint {
        schema: String,
        table: String,
        name: String,
        kind: ConstraintKind,
    },
    ForeignKey {
        schema: String,
        table: String,
        name: String,
    },
    /// A function, sequence, trigger, type… as `list_schema_objects` lists it: a change to one
    /// runs only if the catalog still lists it exactly so (its `detail` goes into the SQL).
    SchemaObject(SchemaObject),
}

/// What to do with an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectChange {
    /// Give it a new name, in the same schema (or table, for a column).
    Rename {
        target: ObjectTarget,
        new_name: String,
    },
    /// Drop it; `cascade` also drops what depends on it, where the engine can.
    Drop { target: ObjectTarget, cascade: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ObjectError {
    #[error("{0}")]
    Unsupported(String),
    #[error("write the new name")]
    NoName,
    #[error("choose one column or more")]
    NoColumns,
    #[error("the new name is the one it already has")]
    SameName,
    #[error("the connection is read-only: its objects cannot be changed")]
    ReadOnly,
    #[error("a change to an object runs only after its SQL is shown and confirmed")]
    NeedsConfirmation,
    #[error("{0} is not in the catalog any more: refresh the explorer")]
    Gone(String),
    #[error("the table changed since its SQL was shown: review the new SQL")]
    Changed,
    #[error("this session has a transaction open: commit or roll it back first")]
    TransactionOpen,
    #[error("statement {index} of {total} failed: {message}")]
    Failed {
        index: usize,
        total: usize,
        message: String,
    },
}

/// What "Create" in the explorer can start: the statement opens in a new tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateKind {
    Table,
    View,
    MaterializedView,
    Sequence,
    /// A schema; a database on MySQL and MariaDB.
    Schema,
}

/// What can be added on the columns chosen in the explorer: the statement opens in a new tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewIndexKind {
    Index,
    UniqueIndex,
    UniqueConstraint,
    PrimaryKey,
}

/// A table element to prepare in the SQL editor. Expressions and trigger bodies remain
/// editable SQL; creating an element never executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewTableElement {
    Column {
        name: String,
        data_type: String,
        nullable: bool,
        default: String,
    },
    Constraint {
        name: String,
        kind: String,
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

/// Writes the requested creation statement for review in the editor (ADR-026).
pub fn new_table_element_sql(
    dialect: &dyn Dialect,
    server_version: &str,
    schema: &str,
    table: &str,
    element: &NewTableElement,
) -> Result<String, ObjectError> {
    let engine = dialect.name();
    let identifier = |s: &str| -> Result<String, ObjectError> {
        if s.trim().is_empty() {
            return Err(ObjectError::NoName);
        }
        Ok(dialect.quote_identifier(s))
    };
    let list = |items: &[String]| -> Result<String, ObjectError> {
        if items.is_empty() {
            return Err(ObjectError::NoColumns);
        }
        items
            .iter()
            .map(|s| identifier(s))
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.join(", "))
    };
    let required = |s: &str| -> Result<String, ObjectError> {
        if s.trim().is_empty() {
            Err(ObjectError::Unsupported(
                "complete the required fields".into(),
            ))
        } else {
            Ok(s.trim().to_string())
        }
    };
    let target = qualified_name(dialect, schema, &[table.to_string()]);
    let sql = match element {
        NewTableElement::Column {
            name,
            data_type,
            nullable,
            default,
        } => {
            let ty = required(data_type)?;
            let nullability = if *nullable { "" } else { " NOT NULL" };
            let default = if default.trim().is_empty() {
                String::new()
            } else {
                format!(" DEFAULT {}", default.trim())
            };
            let add = if engine == "sqlserver" {
                "ADD"
            } else {
                "ADD COLUMN"
            };
            format!(
                "ALTER TABLE {target} {add} {} {ty}{default}{nullability};\n",
                identifier(name)?
            )
        }
        NewTableElement::Constraint {
            name,
            kind,
            columns,
            expression,
        } => {
            if matches!(engine, "sqlite" | "duckdb") {
                return Err(ObjectError::Unsupported(
                    "this engine cannot add constraints to an existing table".into(),
                ));
            }
            let clause = match kind.as_str() {
                "unique" => format!("UNIQUE ({})", list(columns)?),
                "primaryKey" => format!("PRIMARY KEY ({})", list(columns)?),
                "check" if supports_added_checks(dialect, server_version) => {
                    format!("CHECK ({})", required(expression)?)
                }
                "check" => {
                    return Err(ObjectError::Unsupported(
                        "this server does not enforce added CHECK constraints".into(),
                    ));
                }
                _ => return Err(ObjectError::Unsupported("unknown constraint kind".into())),
            };
            if engine == "mysql" && kind == "primaryKey" {
                format!("ALTER TABLE {target} ADD {clause};\n")
            } else {
                format!(
                    "ALTER TABLE {target} ADD CONSTRAINT {} {clause};\n",
                    identifier(name)?
                )
            }
        }
        NewTableElement::ForeignKey {
            name,
            columns,
            referenced_schema,
            referenced_table,
            referenced_columns,
        } => {
            if matches!(engine, "sqlite" | "duckdb") {
                return Err(ObjectError::Unsupported(
                    "this engine cannot add foreign keys to an existing table".into(),
                ));
            }
            if columns.len() != referenced_columns.len() {
                return Err(ObjectError::Unsupported(
                    "choose the same number of local and referenced columns".into(),
                ));
            }
            let referenced = qualified_name(
                dialect,
                &required(referenced_schema)?,
                &[required(referenced_table)?],
            );
            format!(
                "ALTER TABLE {target} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {referenced} ({});\n",
                identifier(name)?,
                list(columns)?,
                list(referenced_columns)?
            )
        }
        NewTableElement::Index {
            name,
            columns,
            unique,
        } => {
            let unique = if *unique { "UNIQUE " } else { "" };
            let (index, on) = if engine == "sqlite" {
                (
                    qualified_name(dialect, schema, &[required(name)?]),
                    identifier(table)?,
                )
            } else {
                (identifier(name)?, target)
            };
            format!(
                "CREATE {unique}INDEX {index} ON {on} ({});\n",
                list(columns)?
            )
        }
        NewTableElement::Trigger {
            name,
            timing,
            event,
            body,
        } => {
            if engine == "duckdb" {
                return Err(ObjectError::Unsupported(
                    "this engine does not support triggers".into(),
                ));
            }
            let timing = match timing.as_str() {
                "before" => "BEFORE",
                "after" => "AFTER",
                _ => return Err(ObjectError::Unsupported("unknown trigger timing".into())),
            };
            let event = match event.as_str() {
                "insert" => "INSERT",
                "update" => "UPDATE",
                "delete" => "DELETE",
                _ => return Err(ObjectError::Unsupported("unknown trigger event".into())),
            };
            let body = required(body)?;
            if engine == "sqlserver" && timing == "BEFORE" {
                return Err(ObjectError::Unsupported(
                    "SQL Server does not have BEFORE triggers".into(),
                ));
            }
            match engine {
                "postgres" => {
                    let function = match body.split_once('.') {
                        Some((function_schema, function_name)) => qualified_name(
                            dialect,
                            &required(function_schema)?,
                            &[required(function_name)?],
                        ),
                        None => identifier(&body)?,
                    };
                    format!(
                        "CREATE TRIGGER {} {timing} {event} ON {target} FOR EACH ROW EXECUTE FUNCTION {function}();\n",
                        identifier(name)?
                    )
                }
                "mysql" => format!(
                    "DELIMITER $$\nCREATE TRIGGER {} {timing} {event} ON {target} FOR EACH ROW {}$$\nDELIMITER ;\n",
                    identifier(name)?,
                    body.trim_end_matches(';')
                ),
                "sqlite" => format!(
                    "CREATE TRIGGER {} {timing} {event} ON {} FOR EACH ROW BEGIN\n  {};\nEND;\n",
                    qualified_name(dialect, schema, &[required(name)?]),
                    identifier(table)?,
                    body.trim_end_matches(';')
                ),
                "sqlserver" => format!(
                    "CREATE TRIGGER {} ON {target} {timing} {event} AS\nBEGIN\n  {};\nEND;\n",
                    qualified_name(dialect, schema, &[required(name)?]),
                    body.trim_end_matches(';')
                ),
                _ => {
                    return Err(ObjectError::Unsupported(
                        "this engine does not support trigger creation".into(),
                    ));
                }
            }
        }
    };
    Ok(sql)
}

/// What the explorer may offer on this engine; the UI shows only these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectActions {
    /// Views (and materialized views) can be renamed: not in SQLite.
    pub rename_views: bool,
    /// Schemas can be dropped: not SQLite's, which are attached files.
    pub drop_schemas: bool,
    /// Constraints and foreign keys can be dropped from their table: not in SQLite or DuckDB,
    /// which must create the table again.
    pub drop_constraints: bool,
    /// Unique constraints and foreign keys are dropped by creating the table again (SQLite,
    /// ADR-047): through the structure form's script, not a lone `DROP`.
    pub rebuild_drops: bool,
    /// Functions and procedures have a source to edit (ADR-047): not SQLite, which has none.
    pub routine_source: bool,
    /// Functions can be called with their parameters (procedures always): not SQLite.
    pub routine_calls: bool,
    /// What "Create" offers, in menu order.
    pub create: Vec<CreateKind>,
    /// What can be added on chosen columns: SQLite and DuckDB add constraints only when the
    /// table is created, so they get indexes.
    pub new_index: Vec<NewIndexKind>,
    /// An editor tab's schema can be set from the tree: not on SQL Server or SQLite, which
    /// have no schema per session.
    pub active_schema: bool,
    pub add_constraints: bool,
    pub add_triggers: bool,
    pub before_triggers: bool,
    pub trigger_function: bool,
    pub add_check_constraints: bool,
}

pub fn object_actions(dialect: &dyn Dialect) -> ObjectActions {
    let engine = dialect.name();
    let create = match engine {
        "postgres" => vec![
            CreateKind::Table,
            CreateKind::View,
            CreateKind::MaterializedView,
            CreateKind::Sequence,
            CreateKind::Schema,
        ],
        "sqlserver" | "duckdb" => vec![
            CreateKind::Table,
            CreateKind::View,
            CreateKind::Sequence,
            CreateKind::Schema,
        ],
        // MariaDB has sequences and MySQL does not; they share this dialect.
        "mysql" => vec![CreateKind::Table, CreateKind::View, CreateKind::Schema],
        _ => vec![CreateKind::Table, CreateKind::View],
    };
    ObjectActions {
        rename_views: engine != "sqlite",
        drop_schemas: engine != "sqlite",
        drop_constraints: !matches!(engine, "sqlite" | "duckdb"),
        rebuild_drops: engine == "sqlite",
        routine_source: engine != "sqlite",
        routine_calls: engine != "sqlite",
        create,
        active_schema: matches!(engine, "postgres" | "mysql" | "duckdb"),
        add_constraints: matches!(engine, "postgres" | "mysql" | "sqlserver"),
        add_triggers: matches!(engine, "postgres" | "mysql" | "sqlserver" | "sqlite"),
        before_triggers: matches!(engine, "postgres" | "mysql" | "sqlite"),
        trigger_function: engine == "postgres",
        add_check_constraints: matches!(engine, "postgres" | "sqlserver"),
        new_index: if matches!(engine, "sqlite" | "duckdb") {
            vec![NewIndexKind::Index, NewIndexKind::UniqueIndex]
        } else {
            vec![
                NewIndexKind::Index,
                NewIndexKind::UniqueIndex,
                NewIndexKind::UniqueConstraint,
                NewIndexKind::PrimaryKey,
            ]
        },
    }
}

/// Older MySQL releases parse CHECK but silently ignore it. MariaDB may include a
/// 5.5.5 compatibility prefix in its reported version.
pub fn supports_added_checks(dialect: &dyn Dialect, version: &str) -> bool {
    match dialect.name() {
        "postgres" | "sqlserver" => true,
        "mysql" => {
            let mariadb = version.to_ascii_lowercase().contains("mariadb");
            let part = if mariadb {
                version
                    .split('-')
                    .rev()
                    .find(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
            } else {
                Some(version)
            };
            let parsed = part.and_then(|part| {
                let mut numbers = part.split('.');
                let major = numbers.next()?.parse::<u32>().ok()?;
                let minor = numbers.next()?.parse::<u32>().ok()?;
                let patch = numbers
                    .next()?
                    .split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse::<u32>()
                    .ok()?;
                Some((major, minor, patch))
            });
            parsed.is_some_and(|v| {
                if mariadb {
                    v >= (10, 2, 1)
                } else {
                    v >= (8, 0, 16)
                }
            })
        }
        _ => false,
    }
}

/// An index or constraint on `columns` of a table, in that order, named after them
/// (`orders_customer_id_idx`, `_key`, `_pkey`), to finish in the editor.
pub fn new_index_sql(
    dialect: &dyn Dialect,
    schema: &str,
    table: &str,
    columns: &[String],
    kind: NewIndexKind,
) -> Result<String, ObjectError> {
    if columns.is_empty() {
        return Err(ObjectError::NoColumns);
    }
    let engine = dialect.name();
    let quote = |name: &str| dialect.quote_identifier(name);
    let suffix = match kind {
        NewIndexKind::Index => "idx",
        NewIndexKind::UniqueIndex | NewIndexKind::UniqueConstraint => "key",
        NewIndexKind::PrimaryKey => "pkey",
    };
    let name = match kind {
        NewIndexKind::PrimaryKey => format!("{table}_{suffix}"),
        _ => format!("{table}_{}_{suffix}", columns.join("_")),
    };
    let list = columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    let target = qualified_name(dialect, schema, &[table.to_string()]);
    Ok(match kind {
        NewIndexKind::Index | NewIndexKind::UniqueIndex => {
            let unique = if kind == NewIndexKind::UniqueIndex {
                "UNIQUE "
            } else {
                ""
            };
            // SQLite names the index's schema, never the table's.
            let (index, on) = if engine == "sqlite" {
                (qualified_name(dialect, schema, &[name]), quote(table))
            } else {
                (quote(&name), target)
            };
            format!("CREATE {unique}INDEX {index} ON {on} ({list});\n")
        }
        NewIndexKind::UniqueConstraint => {
            format!(
                "ALTER TABLE {target} ADD CONSTRAINT {} UNIQUE ({list});\n",
                quote(&name)
            )
        }
        // MySQL and MariaDB call every primary key PRIMARY: a name would be ignored.
        NewIndexKind::PrimaryKey if engine == "mysql" => {
            format!("ALTER TABLE {target} ADD PRIMARY KEY ({list});\n")
        }
        NewIndexKind::PrimaryKey => {
            format!(
                "ALTER TABLE {target} ADD CONSTRAINT {} PRIMARY KEY ({list});\n",
                quote(&name)
            )
        }
    })
}

/// The statement that starts a new object, to finish in the editor: a placeholder name, and
/// for a table an identity key and a text column in the engine's types. A schema ignores
/// `schema`.
pub fn create_sql(dialect: &dyn Dialect, kind: CreateKind, schema: &str) -> String {
    let engine = dialect.name();
    let name = |name: &str| qualified_name(dialect, schema, &[name.to_string()]);
    match kind {
        CreateKind::Table => {
            let (id, text) = match engine {
                "postgres" => ("bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY", "text"),
                "mysql" => ("BIGINT AUTO_INCREMENT PRIMARY KEY", "VARCHAR(255)"),
                "sqlserver" => ("BIGINT IDENTITY(1, 1) PRIMARY KEY", "NVARCHAR(255)"),
                "sqlite" => ("INTEGER PRIMARY KEY", "TEXT"),
                _ => ("BIGINT PRIMARY KEY", "VARCHAR"),
            };
            format!(
                "CREATE TABLE {} (\n  id {id},\n  name {text} NOT NULL\n);\n",
                name("new_table")
            )
        }
        CreateKind::View => format!("CREATE VIEW {} AS\nSELECT 1 AS id;\n", name("new_view")),
        CreateKind::MaterializedView => format!(
            "CREATE MATERIALIZED VIEW {} AS\nSELECT 1 AS id;\n",
            name("new_materialized_view")
        ),
        CreateKind::Sequence => format!(
            "CREATE SEQUENCE {} START WITH 1 INCREMENT BY 1;\n",
            name("new_sequence")
        ),
        CreateKind::Schema if engine == "mysql" => {
            format!(
                "CREATE DATABASE {};\n",
                dialect.quote_identifier("new_database")
            )
        }
        CreateKind::Schema => format!(
            "CREATE SCHEMA {};\n",
            dialect.quote_identifier("new_schema")
        ),
    }
}

/// Whether dropping this object can cascade to what depends on it: every drop on PostgreSQL,
/// a schema on DuckDB (its other drops have no CASCADE).
pub fn can_cascade(dialect: &dyn Dialect, target: &ObjectTarget) -> bool {
    match dialect.name() {
        "postgres" => true,
        "duckdb" => matches!(target, ObjectTarget::Schema { .. }),
        _ => false,
    }
}

/// The statements of a change, in order, without their final `;`.
pub fn change_statements(
    dialect: &dyn Dialect,
    change: &ObjectChange,
) -> Result<Vec<String>, ObjectError> {
    match change {
        ObjectChange::Rename { target, new_name } => {
            rename(dialect, target, new_name).map(|statement| vec![statement])
        }
        ObjectChange::Drop { target, cascade } => {
            if *cascade && !can_cascade(dialect, target) {
                return Err(ObjectError::Unsupported(
                    "this engine cannot drop it in cascade".into(),
                ));
            }
            drop(dialect, target, *cascade).map(|statement| vec![statement])
        }
    }
}

/// A change as a script to show, or to open in the editor: each statement ends in `;`.
pub fn change_script(dialect: &dyn Dialect, change: &ObjectChange) -> Result<String, ObjectError> {
    Ok(change_statements(dialect, change)?
        .iter()
        .map(|statement| format!("{statement};\n"))
        .collect())
}

/// Runs a change on the explorer's session: never on a read-only connection, and only once the
/// user has confirmed the SQL they were shown. Its statements are written here again from the
/// change, never taken from the UI, and run in order up to the first that fails.
pub async fn apply_change(
    session: &Session,
    change: &ObjectChange,
    read_only: bool,
    confirmed: bool,
) -> Result<usize, ObjectError> {
    if read_only {
        return Err(ObjectError::ReadOnly);
    }
    if !confirmed {
        return Err(ObjectError::NeedsConfirmation);
    }
    let statements = change_statements(session.dialect().as_ref(), change)?;
    let (ObjectChange::Rename { target, .. } | ObjectChange::Drop { target, .. }) = change;
    if let ObjectTarget::SchemaObject(object) = target {
        let listed = session
            .connection()
            .list_schema_objects(&object.schema)
            .await
            .map_err(|e| ObjectError::Failed {
                index: 0,
                total: statements.len(),
                message: e.to_string(),
            })?;
        let same = |o: &SchemaObject| {
            o.name == object.name && o.kind == object.kind && o.detail == object.detail
        };
        if !listed.iter().any(same) {
            return Err(ObjectError::Gone(object.name.clone()));
        }
    }
    let total = statements.len();
    for (i, statement) in statements.iter().enumerate() {
        run_one(session, statement)
            .await
            .map_err(|message| ObjectError::Failed {
                index: i + 1,
                total,
                message,
            })?;
    }
    Ok(total)
}

/// A T-SQL Unicode string literal (`sp_rename` takes names as strings).
fn nstring(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

fn rename(
    dialect: &dyn Dialect,
    target: &ObjectTarget,
    new_name: &str,
) -> Result<String, ObjectError> {
    if new_name.trim().is_empty() {
        return Err(ObjectError::NoName);
    }
    let engine = dialect.name();
    let quote = |name: &str| dialect.quote_identifier(name);
    match target {
        ObjectTarget::Schema { .. }
        | ObjectTarget::Index { .. }
        | ObjectTarget::Constraint { .. }
        | ObjectTarget::ForeignKey { .. }
        | ObjectTarget::SchemaObject(_) => Err(ObjectError::Unsupported(
            "only tables, views and columns can be renamed here".into(),
        )),
        ObjectTarget::Relation { schema, name, kind } => {
            if name == new_name {
                return Err(ObjectError::SameName);
            }
            let old = qualified_name(dialect, schema, std::slice::from_ref(name));
            let view = matches!(kind, ObjectKind::View | ObjectKind::MaterializedView);
            match engine {
                // `RENAME TABLE` renames views too; the new name stays in the same database.
                "mysql" => Ok(format!(
                    "RENAME TABLE {old} TO {}",
                    qualified_name(dialect, schema, &[new_name.to_string()])
                )),
                // The new name is a plain string: brackets would become part of it.
                "sqlserver" => Ok(format!(
                    "EXEC sp_rename {}, {}",
                    nstring(&old),
                    nstring(new_name)
                )),
                "sqlite" if view => Err(ObjectError::Unsupported(
                    "SQLite cannot rename a view: drop it and create it again".into(),
                )),
                _ => {
                    let what = match kind {
                        ObjectKind::Table | ObjectKind::PartitionedTable => "TABLE",
                        ObjectKind::View => "VIEW",
                        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
                        ObjectKind::ForeignTable => "FOREIGN TABLE",
                    };
                    Ok(format!("ALTER {what} {old} RENAME TO {}", quote(new_name)))
                }
            }
        }
        ObjectTarget::Column {
            schema,
            table,
            name,
        } => {
            if name == new_name {
                return Err(ObjectError::SameName);
            }
            let table_name = qualified_name(dialect, schema, std::slice::from_ref(table));
            match engine {
                "sqlserver" => Ok(format!(
                    "EXEC sp_rename {}, {}, 'COLUMN'",
                    nstring(&qualified_name(
                        dialect,
                        schema,
                        &[table.clone(), name.clone()]
                    )),
                    nstring(new_name)
                )),
                _ => Ok(format!(
                    "ALTER TABLE {table_name} RENAME COLUMN {} TO {}",
                    quote(name),
                    quote(new_name)
                )),
            }
        }
    }
}

fn drop(
    dialect: &dyn Dialect,
    target: &ObjectTarget,
    cascade: bool,
) -> Result<String, ObjectError> {
    let engine = dialect.name();
    let quote = |name: &str| dialect.quote_identifier(name);
    let qualified = |schema: &str, name: &str| qualified_name(dialect, schema, &[name.to_string()]);
    let cascade = if cascade { " CASCADE" } else { "" };
    let unsupported = |what: &str| Err(ObjectError::Unsupported(what.into()));
    let rebuild = "the table must be created again without it";
    Ok(match target {
        ObjectTarget::Schema { name } => match engine {
            "sqlite" => {
                return unsupported("SQLite's schemas are attached files: detach it instead");
            }
            "mysql" => format!("DROP DATABASE {}", quote(name)),
            _ => format!("DROP SCHEMA {}{cascade}", quote(name)),
        },
        ObjectTarget::Relation { schema, name, kind } => {
            let what = match kind {
                ObjectKind::Table | ObjectKind::PartitionedTable => "TABLE",
                ObjectKind::View => "VIEW",
                ObjectKind::MaterializedView => "MATERIALIZED VIEW",
                ObjectKind::ForeignTable => "FOREIGN TABLE",
            };
            format!("DROP {what} {}{cascade}", qualified(schema, name))
        }
        ObjectTarget::Column {
            schema,
            table,
            name,
        } => format!(
            "ALTER TABLE {} DROP COLUMN {}{cascade}",
            qualified(schema, table),
            quote(name)
        ),
        ObjectTarget::Index {
            schema,
            table,
            name,
            primary,
        } => match engine {
            "sqlite" | "duckdb" if *primary => return unsupported(rebuild),
            "mysql" if *primary => {
                format!("ALTER TABLE {} DROP PRIMARY KEY", qualified(schema, table))
            }
            // The primary key's index belongs to its constraint, which has the same name.
            "postgres" | "sqlserver" if *primary => format!(
                "ALTER TABLE {} DROP CONSTRAINT {}{cascade}",
                qualified(schema, table),
                quote(name)
            ),
            "mysql" | "sqlserver" => {
                format!("DROP INDEX {} ON {}", quote(name), qualified(schema, table))
            }
            _ => format!("DROP INDEX {}{cascade}", qualified(schema, name)),
        },
        ObjectTarget::Constraint {
            schema,
            table,
            name,
            kind,
        } => {
            let table = qualified(schema, table);
            match (engine, kind) {
                ("sqlite" | "duckdb", _) => return unsupported(rebuild),
                ("mysql", ConstraintKind::PrimaryKey) => {
                    format!("ALTER TABLE {table} DROP PRIMARY KEY")
                }
                // A unique constraint is an index on MySQL and MariaDB.
                ("mysql", ConstraintKind::Unique) => {
                    format!("ALTER TABLE {table} DROP INDEX {}", quote(name))
                }
                _ => format!(
                    "ALTER TABLE {table} DROP CONSTRAINT {}{cascade}",
                    quote(name)
                ),
            }
        }
        ObjectTarget::ForeignKey {
            schema,
            table,
            name,
        } => {
            let table = qualified(schema, table);
            match engine {
                "sqlite" | "duckdb" => return unsupported(rebuild),
                "mysql" => format!("ALTER TABLE {table} DROP FOREIGN KEY {}", quote(name)),
                _ => format!(
                    "ALTER TABLE {table} DROP CONSTRAINT {}{cascade}",
                    quote(name)
                ),
            }
        }
        ObjectTarget::SchemaObject(object) => drop_schema_object(dialect, object, cascade)?,
    })
}

fn drop_schema_object(
    dialect: &dyn Dialect,
    object: &SchemaObject,
    cascade: &str,
) -> Result<String, ObjectError> {
    let engine = dialect.name();
    let name = qualified_name(dialect, &object.schema, std::slice::from_ref(&object.name));
    // PostgreSQL tells overloads apart by their arguments, which the catalog wrote in `detail`.
    let arguments = || format!("({})", object.detail.as_deref().unwrap_or_default());
    let unsupported = || {
        Err(ObjectError::Unsupported(format!(
            "{} cannot be dropped here on this engine",
            object.name
        )))
    };
    Ok(match (object.kind, engine) {
        (SchemaObjectKind::Function, "postgres") => {
            format!("DROP FUNCTION {name}{}{cascade}", arguments())
        }
        (SchemaObjectKind::Procedure, "postgres") => {
            format!("DROP PROCEDURE {name}{}{cascade}", arguments())
        }
        (SchemaObjectKind::Aggregate, "postgres") => {
            format!("DROP AGGREGATE {name}{}{cascade}", arguments())
        }
        // DuckDB's functions are macros.
        (SchemaObjectKind::Function, "duckdb") => format!("DROP MACRO {name}"),
        (SchemaObjectKind::Function, _) => format!("DROP FUNCTION {name}"),
        (SchemaObjectKind::Procedure, _) => format!("DROP PROCEDURE {name}"),
        (SchemaObjectKind::Aggregate, _) => return unsupported(),
        (SchemaObjectKind::Sequence, _) => format!("DROP SEQUENCE {name}{cascade}"),
        (SchemaObjectKind::Trigger, "postgres") => {
            let Some(table) = &object.detail else {
                return unsupported();
            };
            format!(
                "DROP TRIGGER {} ON {}{cascade}",
                dialect.quote_identifier(&object.name),
                qualified_name(dialect, &object.schema, std::slice::from_ref(table))
            )
        }
        (SchemaObjectKind::Trigger, _) => format!("DROP TRIGGER {name}"),
        (SchemaObjectKind::Domain, "postgres") => format!("DROP DOMAIN {name}{cascade}"),
        (SchemaObjectKind::Enum | SchemaObjectKind::Type, "postgres" | "duckdb" | "sqlserver") => {
            format!("DROP TYPE {name}{cascade}")
        }
        (SchemaObjectKind::Event, "mysql") => format!("DROP EVENT {name}"),
        (SchemaObjectKind::Synonym, "sqlserver") => format!("DROP SYNONYM {name}"),
        (SchemaObjectKind::Domain | SchemaObjectKind::Enum | SchemaObjectKind::Type, _)
        | (SchemaObjectKind::Event | SchemaObjectKind::Synonym, _) => return unsupported(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_duckdb::DuckdbDialect;
    use spelaion_driver_mysql::MysqlDialect;
    use spelaion_driver_postgres::PostgresDialect;
    use spelaion_driver_sqlite::SqliteDialect;
    use spelaion_driver_sqlserver::SqlServerDialect;

    #[test]
    fn added_checks_require_a_server_that_enforces_them() {
        for version in ["5.7.44", "8.0.15", "unknown", "5.5.5-10.1.48-MariaDB"] {
            assert!(!supports_added_checks(&MysqlDialect, version), "{version}");
        }
        for version in ["8.0.16", "8.4.0", "10.2.1-MariaDB", "5.5.5-10.11.6-MariaDB"] {
            assert!(supports_added_checks(&MysqlDialect, version), "{version}");
        }
        assert!(!supports_added_checks(&SqliteDialect, "3.46.0"));
        assert!(supports_added_checks(&PostgresDialect, "16.0"));
        let element = NewTableElement::Constraint {
            name: "check_it".into(),
            kind: "check".into(),
            columns: vec![],
            expression: "id > 0".into(),
        };
        assert!(new_table_element_sql(&MysqlDialect, "5.7.44", "s", "t", &element).is_err());
        assert!(new_table_element_sql(&MysqlDialect, "8.0.16", "s", "t", &element).is_ok());
    }

    fn names(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn qualified_names_quote_each_part_as_the_engine_does() {
        let pg = PostgresDialect;
        assert_eq!(
            qualified_name(&pg, "public", &names(&["users"])),
            "public.users"
        );
        assert_eq!(
            qualified_name(&pg, "Sales", &names(&["Order Lines", "qty"])),
            "\"Sales\".\"Order Lines\".qty"
        );
        // A part that holds the quote character cannot end the quoting early.
        assert_eq!(qualified_name(&pg, "", &names(&["a\"b"])), "\"a\"\"b\"");
        assert_eq!(
            qualified_name(&MysqlDialect, "shop", &names(&["orders"])),
            "`shop`.`orders`"
        );
        assert_eq!(
            qualified_name(&SqlServerDialect, "dbo", &names(&["Order]s"])),
            "dbo.[Order]]s]"
        );
    }

    #[test]
    fn main_is_left_out_where_the_engine_has_it() {
        let engines: [&dyn Dialect; 2] = [&SqliteDialect, &DuckdbDialect];
        for dialect in engines {
            assert_eq!(qualified_name(dialect, "main", &names(&["t", "c"])), "t.c");
            // The schema itself keeps its name.
            assert_eq!(qualified_name(dialect, "", &names(&["main"])), "main");
            assert_eq!(qualified_name(dialect, "other", &names(&["t"])), "other.t");
        }
        assert_eq!(
            qualified_name(&PostgresDialect, "main", &names(&["t"])),
            "main.t"
        );
    }

    fn rename_sql(dialect: &dyn Dialect, target: ObjectTarget, new_name: &str) -> String {
        change_script(
            dialect,
            &ObjectChange::Rename {
                target,
                new_name: new_name.into(),
            },
        )
        .unwrap()
    }

    fn relation(schema: &str, name: &str, kind: ObjectKind) -> ObjectTarget {
        ObjectTarget::Relation {
            schema: schema.into(),
            name: name.into(),
            kind,
        }
    }

    fn column(schema: &str, table: &str, name: &str) -> ObjectTarget {
        ObjectTarget::Column {
            schema: schema.into(),
            table: table.into(),
            name: name.into(),
        }
    }

    #[test]
    fn rename_is_written_in_each_engines_form() {
        let pg = PostgresDialect;
        assert_eq!(
            rename_sql(
                &pg,
                relation("public", "orders", ObjectKind::Table),
                "Orders 2"
            ),
            "ALTER TABLE public.orders RENAME TO \"Orders 2\";\n"
        );
        assert_eq!(
            rename_sql(&pg, relation("s", "v", ObjectKind::MaterializedView), "w"),
            "ALTER MATERIALIZED VIEW s.v RENAME TO w;\n"
        );
        assert_eq!(
            rename_sql(&pg, relation("s", "f", ObjectKind::ForeignTable), "g"),
            "ALTER FOREIGN TABLE s.f RENAME TO g;\n"
        );
        assert_eq!(
            rename_sql(&pg, column("s", "t", "a"), "b"),
            "ALTER TABLE s.t RENAME COLUMN a TO b;\n"
        );
        assert_eq!(
            rename_sql(&MysqlDialect, relation("shop", "v", ObjectKind::View), "w"),
            "RENAME TABLE `shop`.`v` TO `shop`.`w`;\n"
        );
        assert_eq!(
            rename_sql(&DuckdbDialect, relation("main", "v", ObjectKind::View), "w"),
            "ALTER VIEW v RENAME TO w;\n"
        );
        assert_eq!(
            rename_sql(
                &SqliteDialect,
                relation("main", "t", ObjectKind::Table),
                "u"
            ),
            "ALTER TABLE t RENAME TO u;\n"
        );
    }

    #[test]
    fn sql_server_renames_with_strings_that_cannot_be_escaped() {
        let mssql = SqlServerDialect;
        // A quote in either name stays inside its string literal.
        assert_eq!(
            rename_sql(
                &mssql,
                relation("dbo", "it's", ObjectKind::Table),
                "new'name"
            ),
            "EXEC sp_rename N'dbo.[it''s]', N'new''name';\n"
        );
        assert_eq!(
            rename_sql(&mssql, column("dbo", "t", "a"), "b"),
            "EXEC sp_rename N'dbo.t.a', N'b', 'COLUMN';\n"
        );
    }

    #[test]
    fn what_cannot_be_renamed_says_why() {
        let change = |target, new_name: &str| ObjectChange::Rename {
            target,
            new_name: new_name.into(),
        };
        assert!(matches!(
            change_statements(
                &SqliteDialect,
                &change(relation("main", "v", ObjectKind::View), "w")
            ),
            Err(ObjectError::Unsupported(_))
        ));
        assert!(!object_actions(&SqliteDialect).rename_views);
        assert!(object_actions(&PostgresDialect).rename_views);
        assert_eq!(
            change_statements(
                &PostgresDialect,
                &change(relation("s", "t", ObjectKind::Table), "  ")
            ),
            Err(ObjectError::NoName)
        );
        assert_eq!(
            change_statements(&PostgresDialect, &change(column("s", "t", "a"), "a")),
            Err(ObjectError::SameName)
        );
    }

    fn drop_sql(dialect: &dyn Dialect, target: ObjectTarget, cascade: bool) -> String {
        change_script(dialect, &ObjectChange::Drop { target, cascade }).unwrap()
    }

    fn object(
        schema: &str,
        name: &str,
        kind: SchemaObjectKind,
        detail: Option<&str>,
    ) -> ObjectTarget {
        ObjectTarget::SchemaObject(SchemaObject {
            schema: schema.into(),
            name: name.into(),
            kind,
            detail: detail.map(String::from),
            comment: None,
        })
    }

    fn index(table: &str, name: &str, primary: bool) -> ObjectTarget {
        ObjectTarget::Index {
            schema: "s".into(),
            table: table.into(),
            name: name.into(),
            primary,
        }
    }

    #[test]
    fn drop_is_written_in_each_engines_form() {
        let pg = PostgresDialect;
        assert_eq!(
            drop_sql(&pg, relation("s", "v", ObjectKind::MaterializedView), true),
            "DROP MATERIALIZED VIEW s.v CASCADE;\n"
        );
        assert_eq!(
            drop_sql(&pg, ObjectTarget::Schema { name: "Old".into() }, false),
            "DROP SCHEMA \"Old\";\n"
        );
        assert_eq!(
            drop_sql(
                &MysqlDialect,
                ObjectTarget::Schema { name: "old".into() },
                false
            ),
            "DROP DATABASE `old`;\n"
        );
        assert_eq!(
            drop_sql(&pg, column("s", "t", "a"), false),
            "ALTER TABLE s.t DROP COLUMN a;\n"
        );
        // An index: by itself, on its table, or through the primary key it belongs to.
        assert_eq!(
            drop_sql(&pg, index("t", "t_a", false), false),
            "DROP INDEX s.t_a;\n"
        );
        assert_eq!(
            drop_sql(&pg, index("t", "t_pkey", true), false),
            "ALTER TABLE s.t DROP CONSTRAINT t_pkey;\n"
        );
        assert_eq!(
            drop_sql(&MysqlDialect, index("t", "PRIMARY", true), false),
            "ALTER TABLE `s`.`t` DROP PRIMARY KEY;\n"
        );
        assert_eq!(
            drop_sql(&SqlServerDialect, index("t", "t_a", false), false),
            "DROP INDEX t_a ON s.t;\n"
        );
        assert_eq!(
            drop_sql(
                &SqliteDialect,
                ObjectTarget::Index {
                    schema: "main".into(),
                    table: "t".into(),
                    name: "t_a".into(),
                    primary: false,
                },
                false
            ),
            "DROP INDEX t_a;\n"
        );
        let fk = |dialect: &dyn Dialect| {
            drop_sql(
                dialect,
                ObjectTarget::ForeignKey {
                    schema: "s".into(),
                    table: "t".into(),
                    name: "t_fk".into(),
                },
                false,
            )
        };
        assert_eq!(
            fk(&MysqlDialect),
            "ALTER TABLE `s`.`t` DROP FOREIGN KEY `t_fk`;\n"
        );
        assert_eq!(fk(&pg), "ALTER TABLE s.t DROP CONSTRAINT t_fk;\n");
        let unique = ObjectTarget::Constraint {
            schema: "s".into(),
            table: "t".into(),
            name: "t_a_key".into(),
            kind: ConstraintKind::Unique,
        };
        assert_eq!(
            drop_sql(&MysqlDialect, unique.clone(), false),
            "ALTER TABLE `s`.`t` DROP INDEX `t_a_key`;\n"
        );
        assert_eq!(
            drop_sql(&SqlServerDialect, unique, false),
            "ALTER TABLE s.t DROP CONSTRAINT t_a_key;\n"
        );
    }

    #[test]
    fn schema_objects_are_dropped_by_their_kind() {
        let pg = PostgresDialect;
        // Overloads are told apart by the arguments the catalog wrote.
        assert_eq!(
            drop_sql(
                &pg,
                object(
                    "s",
                    "f",
                    SchemaObjectKind::Function,
                    Some("a integer, b text")
                ),
                true
            ),
            "DROP FUNCTION s.f(a integer, b text) CASCADE;\n"
        );
        assert_eq!(
            drop_sql(
                &pg,
                object("s", "p", SchemaObjectKind::Procedure, None),
                false
            ),
            "DROP PROCEDURE s.p();\n"
        );
        assert_eq!(
            drop_sql(
                &pg,
                object(
                    "s",
                    "check_stock",
                    SchemaObjectKind::Trigger,
                    Some("product")
                ),
                false
            ),
            "DROP TRIGGER check_stock ON s.product;\n"
        );
        assert_eq!(
            drop_sql(
                &pg,
                object("s", "mood", SchemaObjectKind::Enum, None),
                false
            ),
            "DROP TYPE s.mood;\n"
        );
        assert_eq!(
            drop_sql(
                &pg,
                object("s", "positive", SchemaObjectKind::Domain, Some("integer")),
                false
            ),
            "DROP DOMAIN s.positive;\n"
        );
        assert_eq!(
            drop_sql(
                &MysqlDialect,
                object("shop", "nightly", SchemaObjectKind::Event, None),
                false
            ),
            "DROP EVENT `shop`.`nightly`;\n"
        );
        assert_eq!(
            drop_sql(
                &DuckdbDialect,
                object("main", "twice", SchemaObjectKind::Function, Some("x")),
                false
            ),
            "DROP MACRO twice;\n"
        );
        assert_eq!(
            drop_sql(
                &SqliteDialect,
                object("main", "trg", SchemaObjectKind::Trigger, Some("t")),
                false
            ),
            "DROP TRIGGER trg;\n"
        );
        assert_eq!(
            drop_sql(
                &SqlServerDialect,
                object("dbo", "other", SchemaObjectKind::Synonym, None),
                false
            ),
            "DROP SYNONYM dbo.other;\n"
        );
    }

    #[test]
    fn cascade_and_what_cannot_be_dropped() {
        let drop = |target, cascade| ObjectChange::Drop { target, cascade };
        let table = relation("main", "t", ObjectKind::Table);
        assert!(can_cascade(&PostgresDialect, &table));
        assert!(!can_cascade(&DuckdbDialect, &table));
        assert!(can_cascade(
            &DuckdbDialect,
            &ObjectTarget::Schema { name: "s".into() }
        ));
        assert!(!can_cascade(&MysqlDialect, &table));
        assert!(matches!(
            change_statements(&MysqlDialect, &drop(table.clone(), true)),
            Err(ObjectError::Unsupported(_))
        ));
        assert!(matches!(
            change_statements(
                &SqliteDialect,
                &drop(
                    ObjectTarget::Schema {
                        name: "main".into()
                    },
                    false
                )
            ),
            Err(ObjectError::Unsupported(_))
        ));
        let fk = ObjectTarget::ForeignKey {
            schema: "main".into(),
            table: "t".into(),
            name: "fk".into(),
        };
        assert!(matches!(
            change_statements(&DuckdbDialect, &drop(fk, false)),
            Err(ObjectError::Unsupported(_))
        ));
        let actions = object_actions(&SqliteDialect);
        assert!(!actions.drop_schemas && !actions.drop_constraints);
        let actions = object_actions(&PostgresDialect);
        assert!(actions.drop_schemas && actions.drop_constraints);
        // Only tables, views and columns are renamed.
        assert!(matches!(
            change_statements(
                &PostgresDialect,
                &ObjectChange::Rename {
                    target: ObjectTarget::Schema { name: "s".into() },
                    new_name: "t".into(),
                }
            ),
            Err(ObjectError::Unsupported(_))
        ));
    }

    #[test]
    fn create_starts_from_the_engines_own_types() {
        assert_eq!(
            create_sql(&PostgresDialect, CreateKind::Table, "sales"),
            "CREATE TABLE sales.new_table (\n  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,\n  name text NOT NULL\n);\n"
        );
        assert_eq!(
            create_sql(&SqliteDialect, CreateKind::Table, "main"),
            "CREATE TABLE new_table (\n  id INTEGER PRIMARY KEY,\n  name TEXT NOT NULL\n);\n"
        );
        assert_eq!(
            create_sql(&MysqlDialect, CreateKind::Schema, "shop"),
            "CREATE DATABASE `new_database`;\n"
        );
        assert_eq!(
            create_sql(&SqlServerDialect, CreateKind::View, "dbo"),
            "CREATE VIEW dbo.new_view AS\nSELECT 1 AS id;\n"
        );
        // Each engine offers only what it has.
        assert!(
            !object_actions(&SqliteDialect)
                .create
                .contains(&CreateKind::Schema)
        );
        assert!(
            !object_actions(&MysqlDialect)
                .create
                .contains(&CreateKind::Sequence)
        );
        assert!(
            object_actions(&PostgresDialect)
                .create
                .contains(&CreateKind::MaterializedView)
        );
        assert!(
            !object_actions(&DuckdbDialect)
                .create
                .contains(&CreateKind::MaterializedView)
        );
    }

    #[test]
    fn indexes_and_constraints_on_chosen_columns() {
        let cols = names(&["customer_id", "created"]);
        assert_eq!(
            new_index_sql(&PostgresDialect, "s", "orders", &cols, NewIndexKind::Index).unwrap(),
            "CREATE INDEX orders_customer_id_created_idx ON s.orders (customer_id, created);\n"
        );
        assert_eq!(
            new_index_sql(
                &MysqlDialect,
                "shop",
                "orders",
                &cols,
                NewIndexKind::UniqueConstraint
            )
            .unwrap(),
            "ALTER TABLE `shop`.`orders` ADD CONSTRAINT `orders_customer_id_created_key` UNIQUE (`customer_id`, `created`);\n"
        );
        assert_eq!(
            new_index_sql(
                &MysqlDialect,
                "shop",
                "orders",
                &names(&["id"]),
                NewIndexKind::PrimaryKey
            )
            .unwrap(),
            "ALTER TABLE `shop`.`orders` ADD PRIMARY KEY (`id`);\n"
        );
        assert_eq!(
            new_index_sql(
                &SqlServerDialect,
                "dbo",
                "orders",
                &names(&["id"]),
                NewIndexKind::PrimaryKey
            )
            .unwrap(),
            "ALTER TABLE dbo.orders ADD CONSTRAINT orders_pkey PRIMARY KEY (id);\n"
        );
        // SQLite puts the schema on the index, never on its table.
        assert_eq!(
            new_index_sql(
                &SqliteDialect,
                "other",
                "t",
                &names(&["a"]),
                NewIndexKind::UniqueIndex
            )
            .unwrap(),
            "CREATE UNIQUE INDEX other.t_a_key ON t (a);\n"
        );
        assert_eq!(
            new_index_sql(&PostgresDialect, "s", "t", &[], NewIndexKind::Index),
            Err(ObjectError::NoColumns)
        );
        assert_eq!(
            object_actions(&SqliteDialect).new_index,
            [NewIndexKind::Index, NewIndexKind::UniqueIndex]
        );
    }

    #[test]
    fn a_search_matches_its_text_literally() {
        let filter = |text: &str| NameFilter {
            include: vec![contains_pattern(text)],
            exclude: Vec::new(),
        };
        assert!(filter("ord").matches("big_ORDERS"));
        assert!(!filter("ord").matches("customer"));
        // Wildcards typed in the box are only characters.
        assert!(filter("a*b").matches("xa*by"));
        assert!(!filter("a*b").matches("axxb"));
        assert!(filter("50%").matches("rate_50%"));
        assert!(!filter("50%").matches("rate_500"));
        assert!(filter("a?").matches("a?"));
        assert!(!filter("a?").matches("ab"));
    }
}
