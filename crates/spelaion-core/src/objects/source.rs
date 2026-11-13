//! "Edit source" (ADR-047): a view's, function's or procedure's definition written as the
//! statement that replaces it, to change and run in the editor, where the usual barriers apply
//! (read-only mode, the guard's confirmations, the connection's type). Nothing runs from here.
//!
//! - PostgreSQL, DuckDB and MariaDB: `CREATE OR REPLACE` (PostgreSQL's functions and procedures
//!   already come so from `pg_get_functiondef`; a materialized view cannot be replaced).
//! - MySQL: `CREATE OR REPLACE VIEW`; a function or procedure has no `OR REPLACE`, so it is
//!   dropped and created again (`DROP … IF EXISTS`, then the body between `DELIMITER` lines).
//! - SQL Server: `CREATE OR ALTER`.
//! - SQLite: `DROP VIEW IF EXISTS`, then the view's own `CREATE VIEW`.

use spelaion_driver_api::{SchemaObject, SchemaObjectKind};

use super::ddl::{self, Flavor, Tok};
use super::{ObjectError, qualified_name};
use crate::session::Session;

/// What "Edit source" opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceTarget {
    View {
        schema: String,
        name: String,
    },
    /// A function or a procedure, as `list_schema_objects` lists it.
    Routine(SchemaObject),
}

fn failed(message: String) -> ObjectError {
    ObjectError::Failed {
        index: 0,
        total: 0,
        message,
    }
}

/// The definition of `target` as a script that replaces it when it runs.
pub async fn editable_source(
    session: &Session,
    target: &SourceTarget,
) -> Result<String, ObjectError> {
    let dialect = session.dialect();
    let engine = dialect.name();
    let connection = session.connection();
    let (definition, schema, name, kind) = match target {
        SourceTarget::View { schema, name } => (
            connection
                .relation_definition(schema, name)
                .await
                .map_err(|e| failed(e.to_string()))?,
            schema,
            name,
            None,
        ),
        SourceTarget::Routine(object) => {
            if !matches!(
                object.kind,
                SchemaObjectKind::Function | SchemaObjectKind::Procedure
            ) {
                return Err(ObjectError::Unsupported(
                    "only views, functions and procedures have a source to edit".into(),
                ));
            }
            (
                connection
                    .object_definition(object)
                    .await
                    .map_err(|e| failed(e.to_string()))?,
                &object.schema,
                &object.name,
                Some(object.kind),
            )
        }
    };
    let mariadb = engine == "mysql"
        && connection
            .server_version()
            .await
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("mariadb");
    let qualified = qualified_name(dialect.as_ref(), schema, std::slice::from_ref(name));
    replace_form(engine, mariadb, &definition, kind, &qualified)
}

/// `definition` written so that running it replaces the object (see the module's list).
pub(crate) fn replace_form(
    engine: &str,
    mariadb: bool,
    definition: &str,
    routine: Option<SchemaObjectKind>,
    qualified: &str,
) -> Result<String, ObjectError> {
    let flavor = if engine == "mysql" {
        Flavor::Mysql
    } else {
        Flavor::Sqlite
    };
    let tokens =
        ddl::tokenize(definition, flavor).map_err(|e| ObjectError::Unsupported(e.to_string()))?;
    let word = |i: usize, w: &str| {
        tokens.get(i).is_some_and(|t| {
            t.kind == Tok::Word && definition[t.start..t.end].eq_ignore_ascii_case(w)
        })
    };
    let create = tokens
        .iter()
        .position(|t| {
            t.kind == Tok::Word && definition[t.start..t.end].eq_ignore_ascii_case("create")
        })
        .ok_or_else(|| {
            ObjectError::Unsupported("the definition is not a CREATE statement".into())
        })?;
    let after_create = tokens[create].end;
    let already = word(create + 1, "or");
    if word(create + 1, "materialized") {
        return Err(ObjectError::Unsupported(
            "a materialized view cannot be replaced: drop it and create it again".into(),
        ));
    }
    let insert = |words: &str| {
        if already {
            definition.to_string()
        } else {
            format!(
                "{}{words}{}",
                &definition[..after_create],
                &definition[after_create..]
            )
        }
    };
    let body = definition.trim_end();
    Ok(match (engine, routine) {
        ("sqlserver", _) => insert(" OR ALTER"),
        ("sqlite", _) => format!("DROP VIEW IF EXISTS {qualified};\n{body}\n"),
        ("mysql", Some(kind)) => {
            let what = if kind == SchemaObjectKind::Procedure {
                "PROCEDURE"
            } else {
                "FUNCTION"
            };
            // A body holds `;`: the statement ends at the delimiter instead.
            let statement = body.trim_end_matches(';').trim_end();
            if mariadb {
                format!(
                    "DELIMITER $$\n{}$$\nDELIMITER ;\n",
                    insert_in(statement, after_create, already, " OR REPLACE")
                )
            } else {
                format!(
                    "DROP {what} IF EXISTS {qualified};\nDELIMITER $$\n{statement}$$\nDELIMITER ;\n"
                )
            }
        }
        _ => insert(" OR REPLACE"),
    })
}

fn insert_in(statement: &str, at: usize, already: bool, words: &str) -> String {
    if already || at > statement.len() {
        statement.to_string()
    } else {
        format!("{}{words}{}", &statement[..at], &statement[at..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_engine_replaces_in_its_own_words() {
        let view = "CREATE VIEW v AS SELECT 1;";
        assert_eq!(
            replace_form("duckdb", false, view, None, "v").unwrap(),
            "CREATE OR REPLACE VIEW v AS SELECT 1;"
        );
        assert_eq!(
            replace_form(
                "sqlserver",
                false,
                "-- note\nCREATE PROCEDURE dbo.p AS SELECT 1",
                Some(SchemaObjectKind::Procedure),
                "dbo.p"
            )
            .unwrap(),
            "-- note\nCREATE OR ALTER PROCEDURE dbo.p AS SELECT 1"
        );
        assert_eq!(
            replace_form("sqlite", false, view, None, "v").unwrap(),
            "DROP VIEW IF EXISTS v;\nCREATE VIEW v AS SELECT 1;\n"
        );
        // Already replacing: left as it is.
        let function = "CREATE OR REPLACE FUNCTION f() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$";
        assert_eq!(
            replace_form(
                "postgres",
                false,
                function,
                Some(SchemaObjectKind::Function),
                "f"
            )
            .unwrap(),
            function
        );
        let procedure = "CREATE DEFINER=`root`@`%` PROCEDURE `p`()\nBEGIN\n  SELECT 1;\nEND;";
        assert_eq!(
            replace_form(
                "mysql",
                false,
                procedure,
                Some(SchemaObjectKind::Procedure),
                "`p`"
            )
            .unwrap(),
            "DROP PROCEDURE IF EXISTS `p`;\nDELIMITER $$\nCREATE DEFINER=`root`@`%` PROCEDURE `p`()\nBEGIN\n  SELECT 1;\nEND$$\nDELIMITER ;\n"
        );
        assert_eq!(
            replace_form(
                "mysql",
                true,
                procedure,
                Some(SchemaObjectKind::Procedure),
                "`p`"
            )
            .unwrap(),
            "DELIMITER $$\nCREATE OR REPLACE DEFINER=`root`@`%` PROCEDURE `p`()\nBEGIN\n  SELECT 1;\nEND$$\nDELIMITER ;\n"
        );
        assert!(
            replace_form(
                "postgres",
                false,
                "CREATE MATERIALIZED VIEW m AS SELECT 1",
                None,
                "m"
            )
            .is_err()
        );
        assert!(replace_form("postgres", false, "SELECT 1", None, "x").is_err());
    }
}
