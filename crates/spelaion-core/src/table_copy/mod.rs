//! Copying tables between connections (SDB-35, ADR-069): an import whose rows come from a read
//! of another connection. This part maps the columns across engines ([`types`]) and writes each
//! value as the text the target reads ([`values`]); the copy itself reuses the streaming
//! export's read (ADR-034) and the import's load (ADR-038).

pub mod types;
pub mod values;

use spelaion_driver_api::LogicalType;

pub use types::{Loss, Portable, TargetType, portable, target_type};
pub use values::text_for;

/// A source column as the copy sees it: its name, its type as its engine writes it, and the
/// logical type its plugin reads it as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceColumn {
    pub name: String,
    pub native_type: String,
    pub logical: LogicalType,
    pub nullable: bool,
}

/// What one column becomes on the target, to show before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnPlan {
    pub name: String,
    pub source_type: String,
    pub kind: Portable,
    pub target: TargetType,
}

impl ColumnPlan {
    /// Whether the column cannot be copied to this target as it is (ADR-069's point 8).
    pub fn blocked(&self) -> bool {
        self.target.losses.iter().any(Loss::blocks)
    }
}

/// What `column`, on `source` (a dialect's name), becomes on `target`.
pub fn plan_column(source: &str, column: &SourceColumn, target: &str) -> ColumnPlan {
    let kind = portable(source, &column.native_type, column.logical);
    ColumnPlan {
        name: column.name.clone(),
        source_type: column.native_type.clone(),
        kind,
        target: target_type(target, kind, column.nullable),
    }
}

/// The engines a copy can write to: those whose connections load rows (`insert_rows`), by
/// their dialect's name (MariaDB reads SQL as MySQL does).
pub const TARGETS: [&str; 6] = [
    "postgres",
    "mysql",
    "sqlserver",
    "sqlite",
    "duckdb",
    "clickhouse",
];
