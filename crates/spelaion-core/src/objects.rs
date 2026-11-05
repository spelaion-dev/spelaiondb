//! What the explorer does with an object besides reading it (ADR-026): its name as the engine
//! writes it, for the clipboard, and the statements that rename or drop it. Statements are written
//! here with each engine's syntax and quoting; the UI only says what to change, and a change
//! runs only once the user has seen its SQL and confirmed it.

use spelaion_driver_api::{ConstraintKind, Dialect, ObjectKind, SchemaObject, SchemaObjectKind};

use crate::edit::run_one;
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
    #[error("the new name is the one it already has")]
    SameName,
    #[error("the connection is read-only: its objects cannot be changed")]
    ReadOnly,
    #[error("a change to an object runs only after its SQL is shown and confirmed")]
    NeedsConfirmation,
    #[error("{0} is not in the catalog any more: refresh the explorer")]
    Gone(String),
    #[error("statement {index} of {total} failed: {message}")]
    Failed {
        index: usize,
        total: usize,
        message: String,
    },
}

/// What the explorer may offer on this engine; the UI shows only these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectActions {
    /// Views (and materialized views) can be renamed: not in SQLite.
    pub rename_views: bool,
    /// Schemas can be dropped: not SQLite's, which are attached files.
    pub drop_schemas: bool,
    /// Constraints and foreign keys can be dropped from their table: not in SQLite or DuckDB,
    /// which must create the table again.
    pub drop_constraints: bool,
}

pub fn object_actions(dialect: &dyn Dialect) -> ObjectActions {
    let engine = dialect.name();
    ObjectActions {
        rename_views: engine != "sqlite",
        drop_schemas: engine != "sqlite",
        drop_constraints: !matches!(engine, "sqlite" | "duckdb"),
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
}
