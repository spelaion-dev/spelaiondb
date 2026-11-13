//! Entity-relationship diagrams (ADR-046): a read-only picture of a schema's tables and the
//! foreign keys between them.
//!
//! - The tables and keys come from the catalog the explorer reads (ADR-024): `list_objects`,
//!   `describe_table` and `list_referencing_keys`, one table at a time, as owned data
//!   ([`ErModel`]). Nothing here names a driver.
//! - [`layout`] places the tables and routes the relations once per model and display options;
//!   the UI only draws the result, and panning or zooming never asks for it again.
//! - [`mermaid`] and [`dbml`] write the model as text for other tools, and
//!   [`save_diagram_text`] writes such a text to the file the user chose.

mod layout;
mod text;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use spelaion_driver_api::{
    Connection, ConstraintKind, DriverError, NameFilter, ObjectKind, TableInfo,
};

use crate::sql_file::replace_atomically;

pub use layout::{
    ErColumns, ErEdge, ErKeyMark, ErLayout, ErLayoutOptions, ErMetrics, ErNode, ErPoint, ErRow,
    ErSide, layout,
};
pub use text::{dbml, mermaid};

/// Most tables one diagram reads: past it, the rest (by name) are left out and the diagram says
/// how many. Choosing tables or focusing on one draws a part of a bigger schema.
pub const MAX_DIAGRAM_TABLES: usize = 500;

/// Largest text export the core writes (a 500-table Mermaid or DBML file is far smaller).
pub const MAX_DIAGRAM_TEXT_BYTES: usize = 16 * 1024 * 1024;

/// A table by schema and name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TableName {
    pub schema: String,
    pub name: String,
}

/// What a diagram shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagramScope {
    /// Every table of a schema, or those the explorer's table filter keeps (ADR-025).
    Schema {
        schema: String,
        filter: Option<NameFilter>,
    },
    /// The tables the user chose.
    Tables { tables: Vec<TableName> },
    /// One table and every table it points to or that points to it.
    Focus { table: TableName },
}

/// How many rows one end of a relation stands for, as crow's foot notation draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    /// Exactly one: the referenced end of a key whose columns are all `NOT NULL`.
    One,
    /// Zero or one: the referenced end of a nullable key, or the referencing end of a key whose
    /// columns are the table's primary key or a unique constraint.
    ZeroOrOne,
    /// Zero or more: the referencing end of any other key.
    ZeroOrMany,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErColumn {
    pub name: String,
    /// As the engine writes it (`numeric(12,2)`).
    pub native_type: String,
    pub nullable: bool,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErTable {
    pub schema: String,
    pub name: String,
    pub comment: Option<String>,
    pub columns: Vec<ErColumn>,
    /// Primary key columns, in order.
    pub primary_key: Vec<String>,
    /// The columns of each unique constraint.
    pub unique: Vec<Vec<String>>,
    /// Columns of any of its foreign keys, to tables in the diagram or not.
    pub foreign_key_columns: Vec<String>,
}

impl ErTable {
    fn key(&self) -> TableName {
        TableName {
            schema: self.schema.clone(),
            name: self.name.clone(),
        }
    }

    pub(crate) fn column(&self, name: &str) -> Option<&ErColumn> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Whether `columns` (in any order) are exactly the primary key or a unique constraint.
    fn is_unique(&self, columns: &[String]) -> bool {
        let wanted: BTreeSet<&str> = columns.iter().map(String::as_str).collect();
        let same = |key: &[String]| {
            !key.is_empty() && key.iter().map(String::as_str).collect::<BTreeSet<_>>() == wanted
        };
        same(&self.primary_key) || self.unique.iter().any(|key| same(key))
    }
}

/// A foreign key between two tables of the diagram: from the referencing table (the "many"
/// end) to the referenced one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErRelation {
    pub name: String,
    /// The referencing table's position in [`ErModel::tables`].
    pub from: usize,
    pub from_columns: Vec<String>,
    /// The referenced table's position in [`ErModel::tables`].
    pub to: usize,
    pub to_columns: Vec<String>,
    /// The referencing end.
    pub from_cardinality: Cardinality,
    /// The referenced end.
    pub to_cardinality: Cardinality,
    /// Its columns are all part of the referencing table's primary key.
    pub identifying: bool,
}

/// The tables and relations of a diagram, read from the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ErModel {
    /// Sorted by schema and name.
    pub tables: Vec<ErTable>,
    pub relations: Vec<ErRelation>,
    /// The focused table's position, for [`DiagramScope::Focus`].
    pub focus: Option<usize>,
    /// Foreign keys that point to a table outside the diagram.
    pub outside_keys: usize,
    /// Tables in scope left out past [`MAX_DIAGRAM_TABLES`].
    pub left_out: usize,
    /// Tables that could not be described (dropped meanwhile, no permission…).
    pub unreadable: usize,
    /// What went wrong with the first of them.
    pub first_error: Option<String>,
}

/// How far the catalog read has got: `read` of `total` tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadProgress {
    pub read: usize,
    pub total: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum DiagramError {
    #[error("{0}")]
    Catalog(#[from] DriverError),
    #[error("could not write {path}: {message}")]
    File { path: String, message: String },
    #[error("the diagram text is too large to save ({bytes} bytes)")]
    TooLarge { bytes: usize },
}

/// The relations a diagram can draw: tables, partitioned tables and foreign tables (views have
/// no keys of their own).
fn is_table(kind: ObjectKind) -> bool {
    match kind {
        ObjectKind::Table | ObjectKind::PartitionedTable | ObjectKind::ForeignTable => true,
        ObjectKind::View | ObjectKind::MaterializedView => false,
    }
}

/// Reads the tables of `scope` from the catalog, one `describe_table` each, telling
/// `on_progress` after each one. A table that cannot be described is counted and left out; the
/// read goes on with the others.
pub async fn read_model(
    conn: &dyn Connection,
    scope: &DiagramScope,
    mut on_progress: impl FnMut(ReadProgress) + Send,
) -> Result<ErModel, DiagramError> {
    let (mut names, focus) = match scope {
        DiagramScope::Schema { schema, filter } => {
            let objects = match filter {
                Some(filter) => conn.list_objects_matching(schema, filter).await?,
                None => conn.list_objects(schema).await?,
            };
            let names = objects
                .into_iter()
                .filter(|o| is_table(o.kind))
                .map(|o| TableName {
                    schema: o.schema,
                    name: o.name,
                })
                .collect::<Vec<_>>();
            (names, None)
        }
        DiagramScope::Tables { tables } => (tables.clone(), None),
        DiagramScope::Focus { table } => {
            // The focused table first: its keys say which tables come with it.
            let main = conn.describe_table(&table.schema, &table.name).await?;
            let referencing = conn
                .list_referencing_keys(&table.schema, &table.name)
                .await?;
            let mut names = main
                .foreign_keys
                .iter()
                .map(|fk| TableName {
                    schema: fk.referenced_schema.clone(),
                    name: fk.referenced_table.clone(),
                })
                .chain(referencing.into_iter().map(|key| TableName {
                    schema: key.schema,
                    name: key.table,
                }))
                .filter(|name| name != table)
                .collect::<Vec<_>>();
            names.sort();
            names.dedup();
            (names, Some(main))
        }
    };
    names.sort();
    names.dedup();
    let room = MAX_DIAGRAM_TABLES - usize::from(focus.is_some());
    let left_out = names.len().saturating_sub(room);
    names.truncate(room);

    let total = names.len() + usize::from(focus.is_some());
    let mut described = Vec::with_capacity(total);
    let focus_name = focus.as_ref().map(|t| TableName {
        schema: t.object.schema.clone(),
        name: t.object.name.clone(),
    });
    if let Some(main) = focus {
        described.push(main);
        on_progress(ReadProgress { read: 1, total });
    }
    let mut unreadable = 0;
    let mut first_error = None;
    for name in names {
        match conn.describe_table(&name.schema, &name.name).await {
            Ok(table) => described.push(table),
            Err(e) => {
                unreadable += 1;
                first_error.get_or_insert_with(|| format!("{}.{}: {e}", name.schema, name.name));
            }
        }
        on_progress(ReadProgress {
            read: described.len() + unreadable,
            total,
        });
    }
    let mut model = ErModel::from_tables(described, focus_name.as_ref());
    model.left_out = left_out;
    model.unreadable = unreadable;
    model.first_error = first_error;
    Ok(model)
}

impl ErModel {
    /// The model of these described tables: sorted by schema and name, and every foreign key
    /// between two of them as a relation. `focus` names the focused table, if any.
    pub fn from_tables(described: Vec<TableInfo>, focus: Option<&TableName>) -> Self {
        let mut keyed = described
            .into_iter()
            .map(|info| {
                let table = ErTable {
                    schema: info.object.schema.clone(),
                    name: info.object.name.clone(),
                    comment: info.object.comment.clone().filter(|c| !c.trim().is_empty()),
                    columns: info
                        .columns
                        .iter()
                        .map(|c| ErColumn {
                            name: c.name.clone(),
                            native_type: c.native_type.clone(),
                            nullable: c.nullable,
                            comment: c.comment.clone().filter(|c| !c.trim().is_empty()),
                        })
                        .collect(),
                    primary_key: info.primary_key.clone(),
                    unique: info
                        .constraints
                        .iter()
                        .filter(|c| c.kind == ConstraintKind::Unique && !c.columns.is_empty())
                        .map(|c| c.columns.clone())
                        .collect(),
                    foreign_key_columns: {
                        let mut columns: Vec<String> = info
                            .foreign_keys
                            .iter()
                            .flat_map(|fk| fk.columns.iter().cloned())
                            .collect();
                        columns.sort();
                        columns.dedup();
                        columns
                    },
                };
                (table, info)
            })
            .collect::<Vec<_>>();
        keyed.sort_by_key(|(table, _)| table.key());
        keyed.dedup_by(|a, b| a.0.key() == b.0.key());

        let exact: HashMap<TableName, usize> = keyed
            .iter()
            .enumerate()
            .map(|(i, (t, _))| (t.key(), i))
            .collect();
        // Engines that fold names (MySQL with `lower_case_table_names`) may write a referenced
        // table in another case: the one table that matches ignoring case is it.
        let find = |schema: &str, name: &str| -> Option<usize> {
            let key = TableName {
                schema: schema.to_string(),
                name: name.to_string(),
            };
            exact.get(&key).copied().or_else(|| {
                let mut matching = keyed.iter().enumerate().filter(|(_, (t, _))| {
                    t.schema.eq_ignore_ascii_case(schema) && t.name.eq_ignore_ascii_case(name)
                });
                match (matching.next(), matching.next()) {
                    (Some((i, _)), None) => Some(i),
                    _ => None,
                }
            })
        };

        let mut relations = Vec::new();
        let mut outside_keys = 0;
        for (from, (table, info)) in keyed.iter().enumerate() {
            for fk in &info.foreign_keys {
                let Some(to) = find(&fk.referenced_schema, &fk.referenced_table) else {
                    outside_keys += 1;
                    continue;
                };
                // A primary key column is required even where the catalog says otherwise
                // (SQLite lets an `INT PRIMARY KEY` hold NULL).
                let all_required = fk.columns.iter().all(|c| {
                    table.primary_key.contains(c) || table.column(c).is_none_or(|c| !c.nullable)
                });
                let identifying = !fk.columns.is_empty()
                    && fk.columns.iter().all(|c| table.primary_key.contains(c));
                relations.push(ErRelation {
                    name: fk.name.clone(),
                    from,
                    from_columns: fk.columns.clone(),
                    to,
                    to_columns: fk.referenced_columns.clone(),
                    from_cardinality: if table.is_unique(&fk.columns) {
                        Cardinality::ZeroOrOne
                    } else {
                        Cardinality::ZeroOrMany
                    },
                    to_cardinality: if all_required {
                        Cardinality::One
                    } else {
                        Cardinality::ZeroOrOne
                    },
                    identifying,
                });
            }
        }
        let focus = focus.and_then(|name| exact.get(name).copied());
        Self {
            tables: keyed.into_iter().map(|(t, _)| t).collect(),
            relations,
            focus,
            outside_keys,
            left_out: 0,
            unreadable: 0,
            first_error: None,
        }
    }

    /// Whether every table is in one schema (the text exports then write bare names).
    pub fn single_schema(&self) -> bool {
        self.tables
            .windows(2)
            .all(|pair| pair[0].schema == pair[1].schema)
    }
}

/// Writes a diagram's text export (Mermaid or DBML, from [`mermaid`] or [`dbml`]) over `path`,
/// whole or not at all.
pub fn save_diagram_text(path: &Path, text: &str) -> Result<(), DiagramError> {
    if text.len() > MAX_DIAGRAM_TEXT_BYTES {
        return Err(DiagramError::TooLarge { bytes: text.len() });
    }
    replace_atomically(path, text.as_bytes()).map_err(|e| DiagramError::File {
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use spelaion_driver_api::{
        ColumnInfo, ConstraintInfo, ForeignKeyInfo, ObjectInfo, ObjectKind, TableInfo,
    };

    use super::*;

    /// A described table: `columns` as `(name, type, nullable)`.
    pub(crate) fn table(
        schema: &str,
        name: &str,
        columns: &[(&str, &str, bool)],
        primary_key: &[&str],
        foreign_keys: &[(&str, &[&str], &str, &[&str])],
    ) -> TableInfo {
        TableInfo {
            object: ObjectInfo {
                schema: schema.into(),
                name: name.into(),
                kind: ObjectKind::Table,
                estimated_rows: None,
                comment: None,
            },
            columns: columns
                .iter()
                .map(|(name, ty, nullable)| ColumnInfo {
                    name: (*name).into(),
                    native_type: (*ty).into(),
                    nullable: *nullable,
                    default: None,
                    comment: None,
                })
                .collect(),
            primary_key: primary_key.iter().map(|c| (*c).into()).collect(),
            indexes: vec![],
            foreign_keys: foreign_keys
                .iter()
                .map(|(fk, columns, target, referenced)| ForeignKeyInfo {
                    name: (*fk).into(),
                    columns: columns.iter().map(|c| (*c).into()).collect(),
                    referenced_schema: schema.into(),
                    referenced_table: (*target).into(),
                    referenced_columns: referenced.iter().map(|c| (*c).into()).collect(),
                })
                .collect(),
            constraints: vec![],
        }
    }

    #[test]
    fn relations_get_their_cardinalities() {
        let mut person = table(
            "s",
            "person",
            &[
                ("id", "int", false),
                ("boss", "int", true),
                ("passport", "int", false),
            ],
            &["id"],
            &[("person_boss", &["boss"], "person", &["id"])],
        );
        person.constraints.push(ConstraintInfo {
            name: Some("one_passport".into()),
            kind: ConstraintKind::Unique,
            columns: vec!["passport".into()],
            definition: "UNIQUE (passport)".into(),
        });
        let passport = table("s", "passport", &[("id", "int", false)], &["id"], &[]);
        let mut person_fk = person.clone();
        person_fk.foreign_keys.push(ForeignKeyInfo {
            name: "person_passport".into(),
            columns: vec!["passport".into()],
            referenced_schema: "S".into(),
            referenced_table: "PASSPORT".into(),
            referenced_columns: vec!["id".into()],
        });
        let line = table(
            "s",
            "line",
            &[("person", "int", false), ("n", "int", false)],
            &["person", "n"],
            &[
                ("line_person", &["person"], "person", &["id"]),
                ("elsewhere", &["n"], "gone", &["id"]),
            ],
        );
        let model = ErModel::from_tables(
            vec![line, person_fk, passport],
            Some(&TableName {
                schema: "s".into(),
                name: "person".into(),
            }),
        );
        let names: Vec<_> = model.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["line", "passport", "person"]);
        assert_eq!(model.focus, Some(2));
        assert_eq!(model.outside_keys, 1);
        let by_name = |name: &str| model.relations.iter().find(|r| r.name == name).unwrap();
        let line_person = by_name("line_person");
        assert_eq!((line_person.from, line_person.to), (0, 2));
        assert_eq!(line_person.from_cardinality, Cardinality::ZeroOrMany);
        assert_eq!(line_person.to_cardinality, Cardinality::One);
        assert!(line_person.identifying);
        let boss = by_name("person_boss");
        assert_eq!((boss.from, boss.to), (2, 2));
        assert_eq!(boss.to_cardinality, Cardinality::ZeroOrOne);
        assert!(!boss.identifying);
        // Found ignoring case; a unique key makes the referencing end "zero or one".
        let one = by_name("person_passport");
        assert_eq!((one.from, one.to), (2, 1));
        assert_eq!(one.from_cardinality, Cardinality::ZeroOrOne);
        assert_eq!(one.to_cardinality, Cardinality::One);
    }
}
