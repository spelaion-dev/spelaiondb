//! What the explorer does with an object besides reading it (ADR-026): its name as the engine
//! writes it, for the clipboard, and the statements that rename it. Statements are written
//! here with each engine's syntax and quoting; the UI only says what to change, and a change
//! runs only once the user has seen its SQL and confirmed it.

use spelaion_driver_api::{Dialect, ObjectKind};

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
}

/// What to do with an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectChange {
    /// Give it a new name, in the same schema (or table, for a column).
    Rename {
        target: ObjectTarget,
        new_name: String,
    },
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
}

pub fn object_actions(dialect: &dyn Dialect) -> ObjectActions {
    ObjectActions {
        rename_views: dialect.name() != "sqlite",
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
}
