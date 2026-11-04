//! Changes to the rows of a table data tab (ADR-014): the core writes the UPDATE, DELETE and
//! INSERT statements and applies them in one transaction, each touching exactly one row.
//! The primary key comes from describing the table, its values from the stored, typed
//! result; what the user typed is converted by column type and every identifier is quoted
//! for the engine.

use std::collections::HashSet;

use futures_util::StreamExt;
use spelunk_driver_api::{Dialect, LogicalType, ObjectKind, TableInfo, Value};
use sqlparser::ast::{Expr, GroupByExpr, SelectItem, SetExpr, Statement, TableFactor};
use sqlparser::parser::Parser;

use crate::export::sql_literal;
use crate::generate::{qualified, scalar_count, scalar_value};
use crate::guard::sql_dialect;
use crate::params::is_number;
use crate::results::{ResultId, StoredResult};
use crate::session::{CoreEvent, Session};

/// Identify only direct columns from one real table. Complex projections, joins and
/// derived tables are deliberately excluded: their result columns may not identify rows.
pub fn editable_query_source(dialect: &dyn Dialect, sql: &str) -> Option<(Option<String>, String)> {
    if dialect.split_statements(sql).len() != 1 {
        return None;
    }
    let statements = Parser::parse_sql(sql_dialect(dialect.name()).as_ref(), sql).ok()?;
    let [Statement::Query(query)] = statements.as_slice() else {
        return None;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let projection_is_plain = match select.projection.as_slice() {
        [SelectItem::Wildcard(options)] => {
            options.opt_ilike.is_none()
                && options.opt_exclude.is_none()
                && options.opt_except.is_none()
                && options.opt_replace.is_none()
                && options.opt_rename.is_none()
                && options.opt_alias.is_none()
        }
        items => {
            !items.is_empty()
                && items
                    .iter()
                    .all(|item| matches!(item, SelectItem::UnnamedExpr(Expr::Identifier(_))))
        }
    };
    if !projection_is_plain
        || query.with.is_some()
        || select.distinct.is_some()
        || select.into.is_some()
        || select.exclude.is_some()
        || select.having.is_some()
        || select.qualify.is_some()
        || !select.lateral_views.is_empty()
        || !select.connect_by.is_empty()
        || !matches!(&select.group_by, GroupByExpr::Expressions(items, _) if items.is_empty())
    {
        return None;
    }
    let [from] = select.from.as_slice() else {
        return None;
    };
    if !from.joins.is_empty() {
        return None;
    }
    let TableFactor::Table {
        name,
        args: None,
        version: None,
        ..
    } = &from.relation
    else {
        return None;
    };
    let parts: Vec<_> = name
        .0
        .iter()
        .map(|part| part.as_ident().map(|id| id.value.clone()))
        .collect::<Option<_>>()?;
    match parts.as_slice() {
        [name] => Some((None, name.clone())),
        [schema, name] => Some((Some(schema.clone()), name.clone())),
        _ => None,
    }
}

/// The table whose rows a query's `result` shows, so it can be edited like a table data tab
/// (ADR-014), or `None` when the result must stay read-only: the connection is read-only, the
/// query is not a plain SELECT of one table ([`editable_query_source`]), its table name may
/// not be the one the engine used to run it, the object is not a table, or the result lacks
/// the primary key or holds a column the table does not have (or one column twice).
pub async fn editable_query_table(
    session: &Session,
    result: ResultId,
    sql: &str,
    read_only: bool,
) -> Result<Option<TableInfo>, String> {
    if read_only {
        return Ok(None);
    }
    let Some((schema, name)) = editable_query_source(session.dialect().as_ref(), sql) else {
        return Ok(None);
    };
    let columns: Vec<String> = {
        let stored = session
            .results()
            .get(result)
            .ok_or("the result was freed")?;
        let stored = stored.read().map_err(|_| "result lock poisoned")?;
        stored
            .schema()
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect()
    };
    let Some(schema) = query_table_schema(session, schema, &name).await? else {
        return Ok(None);
    };
    let conn = session.connection();
    let objects = conn
        .list_objects(&schema)
        .await
        .map_err(|e| e.to_string())?;
    let mut named = objects.iter().filter(|o| o.name == name);
    let (Some(object), None) = (named.next(), named.next()) else {
        return Ok(None);
    };
    match object.kind {
        ObjectKind::Table | ObjectKind::PartitionedTable => {}
        ObjectKind::View | ObjectKind::MaterializedView | ObjectKind::ForeignTable => {
            return Ok(None);
        }
    }
    let table = conn
        .describe_table(&schema, &name)
        .await
        .map_err(|e| e.to_string())?;
    let distinct = columns.iter().collect::<HashSet<_>>().len() == columns.len();
    let has_key =
        !table.primary_key.is_empty() && table.primary_key.iter().all(|k| columns.contains(k));
    let all_of_table = columns
        .iter()
        .all(|c| table.columns.iter().any(|t| t.name == *c));
    Ok((distinct && has_key && all_of_table).then_some(table))
}

/// The schema of the table a query names, exactly as the engine resolved it when it ran the
/// query, or `None` when that is not certain. The edits are written with the qualified name,
/// so an unqualified name must not reach a different table: a temporary table shadows a
/// permanent one of the same name in PostgreSQL, SQLite and DuckDB, and the search path or
/// the user's default schema decide the rest.
async fn query_table_schema(
    session: &Session,
    schema: Option<String>,
    name: &str,
) -> Result<Option<String>, String> {
    let engine = session.dialect().name();
    let quoted = session.dialect().quote_identifier(name);
    let literal = |text: &str| sql_literal(&Value::Text(text.to_string()), engine);
    let conn = session.connection();
    match (engine, schema) {
        // `pg_temp` is not a listed schema, so a qualified temporary table finds no object.
        ("postgres", Some(schema)) => Ok(Some(schema)),
        ("postgres", None) => {
            // The search path, as the server resolves it (temporary schema first).
            let sql = format!(
                "SELECT n.nspname::text FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.oid = pg_catalog.to_regclass({})",
                literal(&quoted)
            );
            let schema = text(scalar_value(session, &sql).await?);
            Ok(schema.filter(|schema| !schema.starts_with("pg_temp")))
        }
        // Temporary tables shadow `db.table` too, so reads and edits reach the same table.
        ("mysql" | "mariadb", Some(schema)) => Ok(Some(schema)),
        ("mysql" | "mariadb", None) => conn.current_database().await.map_err(|e| e.to_string()),
        ("sqlserver", Some(schema)) => Ok(Some(schema)),
        ("sqlserver", None) => {
            // The user's default schema, then `dbo`; a synonym resolves to its own schema,
            // where no table has its name.
            let sql = format!("SELECT OBJECT_SCHEMA_NAME(OBJECT_ID({}))", literal(&quoted));
            Ok(text(scalar_value(session, &sql).await?))
        }
        ("sqlite", Some(schema)) => Ok(Some(schema)),
        // `temp` is listed once it holds anything, so a shadowed table is found twice.
        ("sqlite", None) => unique_schema(session, name).await,
        // `a.b` is a database or a schema; temporary tables shadow unqualified names.
        ("duckdb", Some(_)) => Ok(None),
        ("duckdb", None) => {
            let sql = format!(
                "SELECT (SELECT COUNT(*) FROM duckdb_tables() WHERE temporary AND table_name = {0}) \
                 + (SELECT COUNT(*) FROM duckdb_views() WHERE temporary AND view_name = {0})",
                literal(name)
            );
            if scalar_count(session, &sql).await? > 0 {
                return Ok(None);
            }
            unique_schema(session, name).await
        }
        _ => Ok(None),
    }
}

/// The text of a scalar (`NULL` and other types are none).
fn text(value: Option<Value>) -> Option<String> {
    if let Some(Value::Text(text)) = value {
        Some(text)
    } else {
        None
    }
}

/// The only listed schema with an object called `name`, if there is exactly one.
async fn unique_schema(session: &Session, name: &str) -> Result<Option<String>, String> {
    let conn = session.connection();
    let mut found = None;
    for schema in conn.list_schemas().await.map_err(|e| e.to_string())? {
        let objects = conn
            .list_objects(&schema.name)
            .await
            .map_err(|e| e.to_string())?;
        if objects.iter().any(|o| o.name == name) {
            if found.is_some() {
                return Ok(None);
            }
            found = Some(schema.name);
        }
    }
    Ok(found)
}

/// A value typed in the grid (or NULL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellInput {
    Null,
    Text(String),
}

/// One change, by position in the result shown (`row`) and result column (`column`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowChange {
    Update {
        row: usize,
        values: Vec<(usize, CellInput)>,
    },
    Delete {
        row: usize,
    },
    /// Only the columns given; the others take their default.
    Insert {
        values: Vec<(usize, CellInput)>,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    #[error("{0} has no primary key: its rows cannot be told apart, so it is read-only")]
    NoPrimaryKey(String),
    #[error("the rows shown do not include the primary key column {0}")]
    KeyNotInResult(String),
    #[error("there is no row {0} on this page")]
    NoSuchRow(usize),
    #[error("column {0} is not a column of the table")]
    NotATableColumn(String),
    #[error("{column}: \"{value}\" is not {expected}")]
    BadValue {
        column: String,
        value: String,
        expected: &'static str,
    },
    #[error("there is nothing to apply")]
    Nothing,
    #[error("this connection is read-only")]
    ReadOnly,
    #[error("changes to a production database need a confirmation before they are applied")]
    NeedsConfirmation,
    #[error("change {index} failed, nothing was applied: {message}")]
    Failed { index: usize, message: String },
    #[error(
        "change {index} would affect {rows} rows instead of one (the row changed or vanished meanwhile?), nothing was applied"
    )]
    NotOneRow { index: usize, rows: String },
}

/// Whether changes may be applied on this connection: never when it is read-only, and on
/// production only after the user confirmed the statements.
pub fn check_edit_policy(
    read_only: bool,
    production: bool,
    confirmed: bool,
) -> Result<(), EditError> {
    if read_only {
        Err(EditError::ReadOnly)
    } else if production && !confirmed {
        Err(EditError::NeedsConfirmation)
    } else {
        Ok(())
    }
}

/// The statements for `changes` to the table whose page is `result`.
pub fn edit_statements(
    dialect: &dyn Dialect,
    table: &TableInfo,
    result: &StoredResult,
    changes: &[RowChange],
) -> Result<Vec<String>, EditError> {
    let name = qualified(dialect, &table.object.schema, &table.object.name);
    if table.primary_key.is_empty() {
        return Err(EditError::NoPrimaryKey(table.object.name.clone()));
    }
    let columns = &result.schema().columns;
    let keys: Vec<(usize, &str)> = table
        .primary_key
        .iter()
        .map(|key| {
            columns
                .iter()
                .position(|c| &c.name == key)
                .map(|i| (i, key.as_str()))
                .ok_or_else(|| EditError::KeyNotInResult(key.clone()))
        })
        .collect::<Result<_, _>>()?;
    let column = |i: usize| -> Result<(&str, LogicalType), EditError> {
        let meta = columns
            .get(i)
            .ok_or_else(|| EditError::NotATableColumn(format!("#{i}")))?;
        if !table.columns.iter().any(|c| c.name == meta.name) {
            return Err(EditError::NotATableColumn(meta.name.clone()));
        }
        Ok((meta.name.as_str(), meta.logical))
    };
    let where_key = |row: usize| -> Result<String, EditError> {
        let batch = result
            .window(row, 1)
            .into_iter()
            .next()
            .ok_or(EditError::NoSuchRow(row))?;
        let conditions: Vec<String> = keys
            .iter()
            .map(|(i, key)| {
                let value = Value::from_array(batch.column(*i).as_ref(), 0, columns[*i].logical)
                    .unwrap_or(Value::Null);
                format!(
                    "{} = {}",
                    dialect.quote_identifier(key),
                    sql_literal(&value, dialect.name())
                )
            })
            .collect();
        Ok(format!("WHERE {}", conditions.join(" AND ")))
    };
    let mut statements = Vec::new();
    for change in changes {
        match change {
            RowChange::Update { row, values } => {
                if values.is_empty() {
                    continue;
                }
                let set = values
                    .iter()
                    .map(|(i, input)| {
                        let (name, logical) = column(*i)?;
                        Ok(format!(
                            "{} = {}",
                            dialect.quote_identifier(name),
                            literal(dialect.name(), name, logical, input)?
                        ))
                    })
                    .collect::<Result<Vec<_>, EditError>>()?;
                statements.push(format!(
                    "UPDATE {name} SET {} {}",
                    set.join(", "),
                    where_key(*row)?
                ));
            }
            RowChange::Delete { row } => {
                statements.push(format!("DELETE FROM {name} {}", where_key(*row)?));
            }
            RowChange::Insert { values } => {
                let mut names = Vec::new();
                let mut literals = Vec::new();
                for (i, input) in values {
                    let (column_name, logical) = column(*i)?;
                    names.push(dialect.quote_identifier(column_name));
                    literals.push(literal(dialect.name(), column_name, logical, input)?);
                }
                statements.push(if names.is_empty() {
                    // MySQL has no DEFAULT VALUES; an empty list means the same there.
                    if dialect.name() == "mysql" {
                        format!("INSERT INTO {name} () VALUES ()")
                    } else {
                        format!("INSERT INTO {name} DEFAULT VALUES")
                    }
                } else {
                    format!(
                        "INSERT INTO {name} ({}) VALUES ({})",
                        names.join(", "),
                        literals.join(", ")
                    )
                });
            }
        }
    }
    if statements.is_empty() {
        return Err(EditError::Nothing);
    }
    Ok(statements)
}

/// Runs the statements in one transaction: each must affect exactly one row, or everything
/// is rolled back. Returns how many were applied.
pub async fn apply_edits(session: &Session, statements: &[String]) -> Result<usize, EditError> {
    if statements.is_empty() {
        return Err(EditError::Nothing);
    }
    // In T-SQL a lone BEGIN opens a block, not a transaction.
    let begin = if session.dialect().name() == "sqlserver" {
        "BEGIN TRANSACTION"
    } else {
        "BEGIN"
    };
    run_one(session, begin)
        .await
        .map_err(|message| EditError::Failed { index: 0, message })?;
    for (i, statement) in statements.iter().enumerate() {
        let index = i + 1;
        let outcome = match run_one(session, statement).await {
            Err(message) => Err(EditError::Failed { index, message }),
            Ok(Some(1)) => Ok(()),
            Ok(rows) => Err(EditError::NotOneRow {
                index,
                rows: rows.map_or_else(|| "an unknown number of".into(), |n| n.to_string()),
            }),
        };
        if let Err(e) = outcome {
            // Best effort: if even the rollback fails, the session closes the transaction.
            let _ = run_one(session, "ROLLBACK").await;
            return Err(e);
        }
    }
    if let Err(message) = run_one(session, "COMMIT").await {
        let _ = run_one(session, "ROLLBACK").await;
        return Err(EditError::Failed {
            index: statements.len() + 1,
            message,
        });
    }
    Ok(statements.len())
}

/// Rows affected by one statement, or its error.
async fn run_one(session: &Session, sql: &str) -> Result<Option<u64>, String> {
    let mut events = session.run_script(sql);
    let mut affected = None;
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFinished { rows_affected, .. } => affected = rows_affected,
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            CoreEvent::ResultStarted { result, .. } => {
                // Not expected from these statements; never kept.
                session.results().remove(result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    match failure {
        Some(message) => Err(message),
        None => Ok(affected),
    }
}

/// What the user typed, as a literal for a column of this type.
fn literal(
    dialect: &str,
    column: &str,
    logical: LogicalType,
    input: &CellInput,
) -> Result<String, EditError> {
    let CellInput::Text(text) = input else {
        return Ok("NULL".into());
    };
    let bad = |expected| EditError::BadValue {
        column: column.to_string(),
        value: text.clone(),
        expected,
    };
    let trimmed = text.trim();
    let value = match logical {
        LogicalType::Int => Value::Int(trimmed.parse().map_err(|_| bad("a whole number"))?),
        LogicalType::Float | LogicalType::Decimal => {
            if !is_number(trimmed) {
                return Err(bad("a number"));
            }
            Value::Decimal(trimmed.to_string())
        }
        LogicalType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" => Value::Bool(true),
            "false" | "f" | "0" | "no" => Value::Bool(false),
            _ => return Err(bad("true or false")),
        },
        LogicalType::Bytes => {
            let hex = trimmed
                .strip_prefix("\\x")
                .ok_or_else(|| bad("bytes written as \\x followed by hex digits"))?;
            if hex.len() % 2 != 0 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(bad("bytes written as \\x followed by hex digits"));
            }
            Value::Bytes(
                (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))
                    .collect(),
            )
        }
        LogicalType::Text
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => Value::Text(text.clone()),
    };
    Ok(sql_literal(&value, dialect))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_query_requires_one_unambiguous_table_projection() {
        let dialect = spelunk_driver_sqlite::SqliteDialect;
        assert_eq!(
            editable_query_source(
                &dialect,
                "select * from main.orders where id > 2 order by id limit 10"
            ),
            Some((Some("main".into()), "orders".into()))
        );
        assert_eq!(
            editable_query_source(&dialect, "select * from orders"),
            Some((None, "orders".into()))
        );
        assert_eq!(
            editable_query_source(&dialect, "select id, name from orders"),
            Some((None, "orders".into()))
        );
        for sql in [
            "select id + 1 as id from orders",
            "select distinct * from orders",
            "select * from orders join other on orders.id = other.id",
            "select * from (select * from orders) t",
            "with orders as (select * from other) select * from orders",
            "select * from orders union select * from other",
            "select * from orders; select * from other",
            "delete from orders",
        ] {
            assert_eq!(editable_query_source(&dialect, sql), None, "{sql}");
        }
    }

    #[test]
    fn read_only_refuses_and_production_asks() {
        assert_eq!(
            check_edit_policy(true, false, true),
            Err(EditError::ReadOnly)
        );
        assert_eq!(
            check_edit_policy(true, true, true),
            Err(EditError::ReadOnly)
        );
        assert_eq!(
            check_edit_policy(false, true, false),
            Err(EditError::NeedsConfirmation)
        );
        assert_eq!(check_edit_policy(false, true, true), Ok(()));
        assert_eq!(check_edit_policy(false, false, false), Ok(()));
    }

    #[test]
    fn typed_values_become_checked_literals() {
        let lit =
            |logical, text: &str| literal("postgres", "c", logical, &CellInput::Text(text.into()));
        assert_eq!(lit(LogicalType::Int, " 42 ").unwrap(), "42");
        assert!(lit(LogicalType::Int, "4.2").is_err());
        assert_eq!(lit(LogicalType::Decimal, "-0.50").unwrap(), "-0.50");
        assert!(lit(LogicalType::Float, "1e5").is_err());
        assert_eq!(lit(LogicalType::Bool, "Yes").unwrap(), "TRUE");
        assert_eq!(lit(LogicalType::Bytes, "\\xdead").unwrap(), "'\\xdead'");
        assert!(lit(LogicalType::Bytes, "\\xdea").is_err());
        assert_eq!(lit(LogicalType::Text, "O'Brien").unwrap(), "'O''Brien'");
        assert_eq!(
            lit(LogicalType::Date, "2026-11-03").unwrap(),
            "'2026-11-03'"
        );
        assert_eq!(
            literal("postgres", "c", LogicalType::Int, &CellInput::Null).unwrap(),
            "NULL"
        );
        assert_eq!(
            literal(
                "mysql",
                "c",
                LogicalType::Text,
                &CellInput::Text("a\\b".into())
            )
            .unwrap(),
            "'a\\\\b'"
        );
    }
}
