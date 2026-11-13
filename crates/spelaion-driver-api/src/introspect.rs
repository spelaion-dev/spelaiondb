//! Common relational model for the explorer, autocompletion and the MCP server.
//!
//! v0.1 only has relational engines. The driver-driven explorer (generic nodes for Redis,
//! Mongo…) from ADR-005 will come with the first non-relational engine.

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaInfo {
    pub name: String,
    /// System schema (`pg_catalog`, `information_schema`…).
    pub system: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ObjectInfo {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    /// Planner row estimate (cheap; not a `count(*)`).
    pub estimated_rows: Option<i64>,
    pub comment: Option<String>,
}

/// What a schema holds besides its relations (ADR-024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaObjectKind {
    Function,
    Procedure,
    Aggregate,
    Sequence,
    Trigger,
    Enum,
    Domain,
    /// Any other user-defined data type: composite, range, alias, table type…
    Type,
    /// A scheduled event (MySQL, MariaDB).
    Event,
    /// Another name for an object (SQL Server).
    Synonym,
}

/// `sql` without its trailing blanks and ending in `;`, as [`crate::Connection::object_definition`]
/// returns definitions.
pub fn terminated(sql: &str) -> String {
    let sql = sql.trim_end();
    if sql.ends_with(';') {
        sql.to_string()
    } else {
        format!("{sql};")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaObject {
    pub schema: String,
    pub name: String,
    pub kind: SchemaObjectKind,
    /// Shown next to the name, and what tells same-name objects apart: a function's
    /// arguments (PostgreSQL, DuckDB), a trigger's table. [`crate::Connection::object_definition`]
    /// uses it to find the object.
    pub detail: Option<String>,
    pub comment: Option<String>,
}

/// What lives outside every schema: on the server, or in the connection's database (ADR-024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerObjectKind {
    Role,
    User,
    Login,
    Tablespace,
    Extension,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ServerObject {
    pub name: String,
    pub kind: ServerObjectKind,
    /// A short description: a role's attributes, an extension's version and schema…
    pub detail: Option<String>,
    /// Built into the engine (PostgreSQL's `pg_*` roles, SQL Server's fixed roles, MySQL's
    /// internal accounts): the explorer hides it unless asked to show system objects.
    pub system: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TableInfo {
    pub object: ObjectInfo,
    pub columns: Vec<ColumnInfo>,
    /// Names of the primary key columns, in order.
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// Primary key, unique and check constraints (foreign keys are in `foreign_keys`).
    pub constraints: Vec<ConstraintInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    Check,
}

/// `CHECK (expr)` from a check clause, with or without its outer parentheses: engines that
/// keep them (`(total >= 0)`) are not given a second pair.
pub fn check_definition(clause: &str) -> String {
    let clause = clause.trim();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut wrapped = clause.starts_with('(');
    for (i, c) in clause.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth = depth.saturating_sub(1);
                // The first parenthesis closes before the end: `(a) and (b)`.
                if depth == 0 && i + 1 < clause.len() {
                    wrapped = false;
                }
            }
            (None, _) => {}
        }
    }
    if wrapped {
        format!("CHECK {clause}")
    } else {
        format!("CHECK ({clause})")
    }
}

#[cfg(test)]
mod tests {
    use super::check_definition;

    #[test]
    fn checks_get_one_pair_of_parentheses() {
        assert_eq!(check_definition("(`total` >= 0)"), "CHECK (`total` >= 0)");
        assert_eq!(check_definition("`total` >= 0"), "CHECK (`total` >= 0)");
        assert_eq!(
            check_definition("(a > 0) and (b > 0)"),
            "CHECK ((a > 0) and (b > 0))"
        );
        assert_eq!(check_definition("([id]>(0))"), "CHECK ([id]>(0))");
        assert_eq!(check_definition("(note <> ')')"), "CHECK (note <> ')')");
        assert_eq!(check_definition("(a) = ('(')"), "CHECK ((a) = ('('))");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConstraintInfo {
    /// None where the engine does not keep a name (SQLite's primary key).
    pub name: Option<String>,
    pub kind: ConstraintKind,
    /// The columns of a primary key or unique constraint, in order; empty for a check.
    pub columns: Vec<String>,
    /// As the engine writes it: `PRIMARY KEY (id)`, `UNIQUE (email)`, `CHECK (total >= 0)`.
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    /// Type as the engine would write it (`numeric(12,2)`, `character varying(80)`).
    pub native_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

/// A column of a schema's relation, as [`crate::Connection::list_columns`] lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SchemaColumn {
    pub table: String,
    pub name: String,
    /// Type as the engine would write it, like [`ColumnInfo::native_type`].
    pub native_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IndexInfo {
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    /// Full definition (`CREATE INDEX …`).
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
}

/// A foreign key of another table that points to the one asked about: what the table details'
/// "References" lists and what the references panel follows back to the referencing rows.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReferencingKey {
    /// The referencing table.
    pub schema: String,
    pub table: String,
    /// The constraint's name (SQLite has none: `fk_<id>`, as [`TableInfo::foreign_keys`]).
    pub name: String,
    /// Its columns in the referencing table, in the key's order.
    pub columns: Vec<String>,
    /// The columns of the referenced table they point to, in the same order.
    pub referenced_columns: Vec<String>,
}
