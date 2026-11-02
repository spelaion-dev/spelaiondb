//! Common relational model for the explorer, autocompletion and the MCP server.
//!
//! v0.1 only has relational engines. The driver-driven explorer (generic nodes for Redis,
//! Mongo…) from ADR-005 will come with the first non-relational engine.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaInfo {
    pub name: String,
    /// System schema (`pg_catalog`, `information_schema`…).
    pub system: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectInfo {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    /// Planner row estimate (cheap; not a `count(*)`).
    pub estimated_rows: Option<i64>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    pub object: ObjectInfo,
    pub columns: Vec<ColumnInfo>,
    /// Names of the primary key columns, in order.
    pub primary_key: Vec<String>,
    pub indexes: Vec<IndexInfo>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// Type as the engine would write it (`numeric(12,2)`, `character varying(80)`).
    pub native_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub unique: bool,
    pub primary: bool,
    /// Full definition (`CREATE INDEX …`).
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
}
