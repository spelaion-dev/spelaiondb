//! Schema compare (ADR-070): two schemas of the same engine, read through the catalog reads
//! the explorer and the structure forms already use, and a report of what differs, object by
//! object. The caller reads each side in a session of its own, opened for reads only; this
//! module reads, matches and compares, and never writes anything.
//!
//! - [`compare_support`] says whether a session can be compared, and why not.
//! - [`read_snapshot`] reads one side (`snapshot.rs`).
//! - [`compare`] compares two snapshots (`diff.rs`): a pure function, so every rule is tested
//!   without a server.
//! - [`compare_schemas`] does both for two sessions, as one job: one cancel, progress per
//!   object, one ending.

mod diff;
mod snapshot;

pub use diff::compare;
pub use snapshot::read_snapshot;

use spelaion_config::mcp_clients::DiffLine;
use spelaion_driver_api::ForeignKeyInfo;

use crate::ServerFamily;
use crate::job::JobCancel;
use crate::objects::EditableColumn;
use crate::session::Session;

/// The kinds of object a comparison reads and reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CompareObjectKind {
    Table,
    View,
    MaterializedView,
    Function,
    Procedure,
}

/// One side of a comparison as the catalog describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaSnapshot {
    pub schema: String,
    pub tables: Vec<TableSnapshot>,
    pub views: Vec<ViewSnapshot>,
    pub routines: Vec<RoutineSnapshot>,
}

/// A table as the structure forms read it (ADR-047), with its keys, checks and indexes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSnapshot {
    pub name: String,
    pub comment: Option<String>,
    /// The columns in their order, each field as SQL the engine writes.
    pub columns: Vec<EditableColumn>,
    /// The primary key's columns, in order (empty for none), and its name where the engine
    /// keeps one.
    pub primary_key: Vec<String>,
    pub primary_key_name: Option<String>,
    /// Unique and check constraints.
    pub constraints: Vec<ConstraintSnapshot>,
    /// Indexes that are not a key's or a constraint's own.
    pub indexes: Vec<IndexSnapshot>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    /// The `CREATE TABLE` text the engine keeps (SQLite, MySQL and MariaDB, DuckDB).
    pub kept_definition: Option<String>,
}

/// What kind of table constraint (a primary key is [`TableSnapshot::primary_key`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstraintKindCompared {
    Unique,
    Check,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintSnapshot {
    /// `None` where the engine keeps no name (SQLite).
    pub name: Option<String>,
    pub kind: ConstraintKindCompared,
    pub columns: Vec<String>,
    /// As the catalog writes it (`UNIQUE (a, b)`, `CHECK (price > 0)`).
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSnapshot {
    pub name: String,
    pub unique: bool,
    /// The `CREATE INDEX` the driver writes.
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewSnapshot {
    pub name: String,
    pub materialized: bool,
    /// As `relation_definition` writes it.
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineSnapshot {
    pub name: String,
    pub kind: RoutineKind,
    /// What tells same-name routines apart: PostgreSQL's and DuckDB's arguments.
    pub detail: Option<String>,
    /// As `object_definition` writes it.
    pub definition: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoutineKind {
    Function,
    Procedure,
}

impl RoutineKind {
    fn compared(self) -> CompareObjectKind {
        match self {
            RoutineKind::Function => CompareObjectKind::Function,
            RoutineKind::Procedure => CompareObjectKind::Procedure,
        }
    }
}

/// A comparison's report with the two snapshots it was made from, which the migration script
/// is written from: the caller keeps it in the core, never in the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparedSchemas {
    pub report: SchemaComparison,
    pub source: SchemaSnapshot,
    pub target: SchemaSnapshot,
}

/// What a comparison found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaComparison {
    pub source_schema: String,
    pub target_schema: String,
    /// Every object that differs, sorted by kind and name.
    pub objects: Vec<ObjectComparison>,
    /// Objects that are the same on both sides.
    pub same: usize,
    /// What this comparison does not compare, said in the report's header.
    pub not_compared: Vec<NotCompared>,
}

impl SchemaComparison {
    /// No object differs.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectComparison {
    pub kind: CompareObjectKind,
    pub name: String,
    pub detail: Option<String>,
    pub status: ObjectStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectStatus {
    OnlyInSource,
    OnlyInTarget,
    Changed(ObjectChanges),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectChanges {
    /// What differs inside the object: the table itself, its columns, keys, constraints,
    /// indexes and foreign keys, or a view's or routine's source.
    pub parts: Vec<PartComparison>,
    /// A view's or routine's source, line by line, when it differs.
    pub source_diff: Vec<DiffLine>,
    /// The source's text names the source schema, which is not the target's: the script would
    /// keep that name (ADR-070, point 4).
    pub names_source_schema: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartComparison {
    pub part: ComparedPart,
    pub status: PartStatus,
}

/// A part of an object, by its name where it has one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComparedPart {
    /// The object itself: a table's comment, a view's or routine's source.
    Object,
    Column(String),
    PrimaryKey,
    /// A unique or check constraint, by its name (or its definition, without one).
    Constraint(String),
    Index(String),
    ForeignKey(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartStatus {
    /// With its definition as the source writes it.
    OnlyInSource(String),
    /// With its definition as the target writes it.
    OnlyInTarget(String),
    Changed(Vec<PropertyChange>),
}

/// One property side by side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyChange {
    pub property: Property,
    pub source: Option<String>,
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Property {
    Type,
    Nullable,
    Default,
    Comment,
    /// A generated column's being generated.
    Generated,
    Columns,
    Definition,
    ReferencedTable,
    ReferencedColumns,
    Unique,
    Source,
}

/// What a comparison does not compare yet (ADR-070's consequences).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotCompared {
    /// A foreign key's `ON DELETE` and `ON UPDATE`: the driver contract does not report them.
    ForeignKeyActions,
    /// Identity and auto-increment details, collations and other column attributes the
    /// contract does not report.
    ColumnAttributes,
    /// The order of the columns.
    ColumnOrder,
    /// Sequences, triggers, data types, aggregates, partitions, foreign tables and privileges.
    OtherObjects,
}

/// Which side of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareSide {
    Source,
    Target,
}

/// What a comparison tells while it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareProgress {
    pub side: CompareSide,
    /// Objects of this side read so far, and in all (known once its lists are read).
    pub done: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CompareError {
    /// The session's engine, or the pair, cannot be compared: the reason.
    #[error("{0}")]
    Unsupported(String),
    #[error("the comparison was cancelled")]
    Cancelled,
    #[error("reading the {side} schema failed: {message}")]
    Failed { side: &'static str, message: String },
}

/// Whether `session` can take part in a comparison, or why not (ADR-070, point 1): the engines
/// whose structure forms exist, and among servers of their protocols only the engine itself.
pub fn compare_support(session: &Session) -> Result<(), String> {
    let dialect = session.dialect();
    let engine = dialect.name();
    let family = session.server_family();
    let refused = |name: &str| {
        Err(format!(
            "schema compare is not available on {name}: it comes with its structure forms"
        ))
    };
    match engine {
        "sqlite" | "duckdb" => Ok(()),
        "postgres" | "mysql" | "sqlserver" => match family {
            None if engine == "sqlserver" => Ok(()),
            None => refused("a server whose version was not read"),
            Some(family) => match family {
                ServerFamily::PostgreSql | ServerFamily::MySql | ServerFamily::MariaDb => Ok(()),
                ServerFamily::CockroachDb
                | ServerFamily::YugabyteDb
                | ServerFamily::RisingWave
                | ServerFamily::Materialize
                | ServerFamily::CrateDb
                | ServerFamily::Unchecked(_)
                | ServerFamily::OceanBase
                | ServerFamily::TiDb
                | ServerFamily::SingleStore
                | ServerFamily::Doris
                | ServerFamily::GreptimeDb
                | ServerFamily::Databend
                | ServerFamily::StarRocks
                | ServerFamily::Babelfish => refused(family.name()),
            },
        },
        other => refused(other),
    }
}

/// The engine as a comparison tells two sides apart: MySQL is not compared with MariaDB.
fn compared_engine(session: &Session) -> String {
    match session.server_family() {
        Some(ServerFamily::MariaDb) => "mariadb".to_string(),
        Some(_) | None => session.dialect().name().to_string(),
    }
}

/// Reads `source_schema` on `source` and `target_schema` on `target` and compares them. Each
/// side's session is the caller's, opened for reads only; `cancel` stops the statement being
/// read and every one after it.
pub async fn compare_schemas(
    source: &Session,
    source_schema: &str,
    target: &Session,
    target_schema: &str,
    cancel: &JobCancel,
    progress: &(dyn Fn(CompareProgress) + Send + Sync),
) -> Result<ComparedSchemas, CompareError> {
    compare_support(source).map_err(CompareError::Unsupported)?;
    compare_support(target).map_err(CompareError::Unsupported)?;
    let (from, to) = (compared_engine(source), compared_engine(target));
    if from != to {
        return Err(CompareError::Unsupported(format!(
            "both schemas must be on the same engine: the source is {from} and the target {to}"
        )));
    }
    let source_snapshot =
        read_snapshot(source, source_schema, CompareSide::Source, cancel, progress).await?;
    let target_snapshot =
        read_snapshot(target, target_schema, CompareSide::Target, cancel, progress).await?;
    let report = compare(
        source.dialect().as_ref(),
        &source_snapshot,
        &target_snapshot,
    );
    Ok(ComparedSchemas {
        report,
        source: source_snapshot,
        target: target_snapshot,
    })
}
