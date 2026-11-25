//! One side of a comparison, read only through what the explorer and the structure forms
//! already read (ADR-070, point 2): `list_objects`, each table as its structure form reads it
//! (`read_current`), `relation_definition` for views, `list_schema_objects` and
//! `object_definition` for functions and procedures.

use spelaion_driver_api::{ConstraintKind, ObjectKind, SchemaObjectKind, TableInfo};

use super::{
    CompareError, CompareProgress, CompareSide, ConstraintKindCompared, ConstraintSnapshot,
    IndexSnapshot, RoutineKind, RoutineSnapshot, SchemaSnapshot, TableSnapshot, ViewSnapshot,
};
use crate::job::JobCancel;
use crate::objects::alter::read_current;
use crate::session::Session;

fn side_name(side: CompareSide) -> &'static str {
    match side {
        CompareSide::Source => "source",
        CompareSide::Target => "target",
    }
}

/// Reads `schema` on `session`, telling `progress` after each object. `cancel` is bound to the
/// session while it reads, so a cancel stops the statement in flight.
pub async fn read_snapshot(
    session: &Session,
    schema: &str,
    side: CompareSide,
    cancel: &JobCancel,
    progress: &(dyn Fn(CompareProgress) + Send + Sync),
) -> Result<SchemaSnapshot, CompareError> {
    let failed = |message: String| CompareError::Failed {
        side: side_name(side),
        message,
    };
    // A failure that came after a cancel is the cancel's: the engine reports a cancelled
    // statement as an error.
    let ended = |error: CompareError| {
        if cancel.is_cancelled() {
            CompareError::Cancelled
        } else {
            error
        }
    };
    let Some(_attached) = cancel.attach(session) else {
        return Err(CompareError::Cancelled);
    };
    let connection = session.connection();
    let objects = connection
        .list_objects(schema)
        .await
        .map_err(|e| ended(failed(e.to_string())))?;
    let routines = connection
        .list_schema_objects(schema)
        .await
        .map_err(|e| ended(failed(e.to_string())))?
        .into_iter()
        .filter_map(|object| {
            let kind = match object.kind {
                SchemaObjectKind::Function => RoutineKind::Function,
                SchemaObjectKind::Procedure => RoutineKind::Procedure,
                SchemaObjectKind::Aggregate
                | SchemaObjectKind::Sequence
                | SchemaObjectKind::Trigger
                | SchemaObjectKind::Enum
                | SchemaObjectKind::Domain
                | SchemaObjectKind::Type
                | SchemaObjectKind::Event
                | SchemaObjectKind::Synonym
                | SchemaObjectKind::Sink
                | SchemaObjectKind::Connection
                | SchemaObjectKind::Secret => return None,
            };
            Some((kind, object))
        })
        .collect::<Vec<_>>();
    let relations: Vec<_> = objects
        .into_iter()
        .filter(|o| {
            match o.kind {
                ObjectKind::Table
                | ObjectKind::PartitionedTable
                | ObjectKind::View
                | ObjectKind::MaterializedView => true,
                // Foreign tables and the kinds of engines whose language is not SQL are not
                // compared (`NotCompared::OtherObjects`).
                ObjectKind::ForeignTable
                | ObjectKind::Collection
                | ObjectKind::NodeLabel
                | ObjectKind::RelationshipType => false,
            }
        })
        .collect();
    let total = relations.len() + routines.len();
    let mut done = 0;
    let mut snapshot = SchemaSnapshot {
        schema: schema.to_string(),
        tables: Vec::new(),
        views: Vec::new(),
        routines: Vec::new(),
    };
    progress(CompareProgress { side, done, total });
    for object in relations {
        if cancel.is_cancelled() {
            return Err(CompareError::Cancelled);
        }
        match object.kind {
            ObjectKind::Table | ObjectKind::PartitionedTable => {
                let current = read_current(session, schema, &object.name)
                    .await
                    .map_err(|e| ended(failed(format!("{}: {e}", object.name))))?;
                // DuckDB keeps the table's text too, which the structure forms do not read.
                let kept = match (current.ddl, session.dialect().name()) {
                    (None, "duckdb") => Some(
                        connection
                            .relation_definition(schema, &object.name)
                            .await
                            .map_err(|e| ended(failed(format!("{}: {e}", object.name))))?,
                    ),
                    (kept, _) => kept,
                };
                snapshot
                    .tables
                    .push(table(current.info, current.columns, kept));
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                let definition = connection
                    .relation_definition(schema, &object.name)
                    .await
                    .map_err(|e| ended(failed(format!("{}: {e}", object.name))))?;
                snapshot.views.push(ViewSnapshot {
                    name: object.name,
                    materialized: object.kind == ObjectKind::MaterializedView,
                    definition,
                });
            }
            ObjectKind::ForeignTable
            | ObjectKind::Collection
            | ObjectKind::NodeLabel
            | ObjectKind::RelationshipType => {}
        }
        done += 1;
        progress(CompareProgress { side, done, total });
    }
    for (kind, object) in routines {
        if cancel.is_cancelled() {
            return Err(CompareError::Cancelled);
        }
        let definition = connection
            .object_definition(&object)
            .await
            .map_err(|e| ended(failed(format!("{}: {e}", object.name))))?;
        snapshot.routines.push(RoutineSnapshot {
            name: object.name,
            kind,
            detail: object.detail,
            definition,
        });
        done += 1;
        progress(CompareProgress { side, done, total });
    }
    Ok(snapshot)
}

/// A table's snapshot from what its structure form reads.
fn table(
    info: TableInfo,
    columns: Vec<crate::objects::EditableColumn>,
    kept_definition: Option<String>,
) -> TableSnapshot {
    let primary_key_name = info
        .constraints
        .iter()
        .find(|c| c.kind == ConstraintKind::PrimaryKey)
        .and_then(|c| c.name.clone());
    let constraints: Vec<ConstraintSnapshot> = info
        .constraints
        .iter()
        .filter_map(|c| {
            let kind = match c.kind {
                ConstraintKind::PrimaryKey => return None,
                ConstraintKind::Unique => ConstraintKindCompared::Unique,
                ConstraintKind::Check => ConstraintKindCompared::Check,
            };
            Some(ConstraintSnapshot {
                name: c.name.clone(),
                kind,
                columns: c.columns.clone(),
                definition: c.definition.clone(),
            })
        })
        .collect();
    // A key's or a unique constraint's own index is compared as the key or the constraint.
    let indexes = info
        .indexes
        .iter()
        .filter(|i| {
            !i.primary
                && primary_key_name.as_deref() != Some(i.name.as_str())
                && !constraints
                    .iter()
                    .any(|c| c.name.as_deref() == Some(i.name.as_str()))
        })
        .map(|i| IndexSnapshot {
            name: i.name.clone(),
            unique: i.unique,
            definition: i.definition.clone(),
        })
        .collect();
    TableSnapshot {
        name: info.object.name,
        comment: info.object.comment.filter(|c| !c.trim().is_empty()),
        columns,
        primary_key: info.primary_key,
        primary_key_name,
        constraints,
        indexes,
        foreign_keys: info.foreign_keys,
        kept_definition,
    }
}
