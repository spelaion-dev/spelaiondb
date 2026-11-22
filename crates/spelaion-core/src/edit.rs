//! Changes to the rows of a table data tab (ADR-014): the core writes the UPDATE, DELETE and
//! INSERT statements and applies them in one transaction, each touching exactly one row.
//! The primary key comes from describing the table, its values from the stored, typed
//! result; what the user typed is converted by column type and every identifier is quoted
//! for the engine.

use std::collections::HashSet;

use futures_util::StreamExt;
use spelaion_driver_api::{Dialect, LogicalType, ObjectKind, TableInfo, TransactionState, Value};
use sqlparser::ast::{Expr, GroupByExpr, SelectItem, SetExpr, Statement, TableFactor};
use sqlparser::parser::Parser;

use crate::export::sql_literal;
use crate::generate::{qualified, scalar_count, scalar_metadata, scalar_value};
use crate::guard::sql_dialect;
use crate::params::is_number;
use crate::results::{ResultId, ResultSource, ResultTable, StoredResult};
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
    match query_result_table(session, result, sql).await {
        Ok((_, table)) => Ok(Some(table)),
        Err(UpdateCopyError::Failed(message)) => Err(message),
        Err(_) => Ok(None),
    }
}

/// Engines whose query results [`query_table_schema`] can match to the table the engine read.
const QUERY_TABLE_ENGINES: [&str; 5] = ["postgres", "mysql", "sqlserver", "sqlite", "duckdb"];

/// The table a query's `result` shows, with the target its statements are written through,
/// or why its rows cannot be matched to that table's rows: the rule of
/// [`editable_query_table`], with its reasons (SDB-388).
async fn query_result_table(
    session: &Session,
    result: ResultId,
    sql: &str,
) -> Result<(EditTarget, TableInfo), UpdateCopyError> {
    let engine = session.dialect().name();
    if !QUERY_TABLE_ENGINES.contains(&engine) {
        return Err(UpdateCopyError::QueryOnEngine);
    }
    let Some((schema, name)) = editable_query_source(session.dialect().as_ref(), sql) else {
        return Err(UpdateCopyError::NotOneTable);
    };
    // Rows read through another session or in another database are never editable here
    // (SDB-137): the table and its key would be this session's, the values the other's.
    let target = session.edit_target().await?;
    let columns = result_columns(session, result, &target)?;
    let Some(schema) = query_table_schema(session, schema, &name)
        .await
        .map_err(UpdateCopyError::Failed)?
    else {
        return Err(UpdateCopyError::TableNotCertain(name));
    };
    let conn = session.connection();
    let objects = conn
        .list_objects(&schema)
        .await
        .map_err(|e| UpdateCopyError::Failed(e.to_string()))?;
    let mut named = objects.iter().filter(|o| o.name == name);
    let (Some(object), None) = (named.next(), named.next()) else {
        return Err(UpdateCopyError::TableNotCertain(name));
    };
    match object.kind {
        ObjectKind::Table | ObjectKind::PartitionedTable => {}
        ObjectKind::View
        | ObjectKind::MaterializedView
        | ObjectKind::ForeignTable
        | ObjectKind::Collection
        | ObjectKind::NodeLabel
        | ObjectKind::RelationshipType => {
            return Err(UpdateCopyError::NotATable(name));
        }
    }
    let table = conn
        .describe_table(&schema, &name)
        .await
        .map_err(|e| UpdateCopyError::Failed(e.to_string()))?;
    check_result_columns(&table, &columns, true)?;
    // The result keeps the table it was matched to (SDB-433): the first time it is recorded,
    // afterwards it must still be the same table.
    let Some(identity) = table_identity(session, &table.object.schema, &table.object.name)
        .await
        .map_err(UpdateCopyError::Failed)?
    else {
        return Err(UpdateCopyError::TableNotCertain(name));
    };
    let bound = ResultTable {
        schema: table.object.schema.clone(),
        name: table.object.name.clone(),
        identity,
    };
    let stored = session
        .results()
        .stored(result)
        .map_err(|e| UpdateCopyError::Failed(e.to_string()))?;
    let same = stored
        .write()
        .map_err(|_| UpdateCopyError::Failed("result lock poisoned".into()))?
        .bind_table(bound);
    if !same {
        return Err(UpdateCopyError::TableChanged(name));
    }
    Ok((target, table))
}

/// What the engine identifies the table `schema.name` by, read now, or `None` when the engine
/// has no such identity or no single table has that name (SDB-433). It changes when the table
/// is dropped and created again, or another table takes its name, so a result's rows are
/// never written into a table that only shares the name of the one they were read from.
///
/// - PostgreSQL: the table's `oid`.
/// - SQL Server: its `object_id` and `create_date`.
/// - DuckDB: its `table_oid` in the current database.
/// - SQLite: the schema's `schema_version`, which every change to that schema's tables,
///   indexes, views or triggers moves on, so any such change asks for the query again.
/// - MySQL and MariaDB: its `CREATE_TIME`, to the second (they expose no table id without the
///   `PROCESS` privilege). A table dropped and created again within the same second as the
///   original, with no time between them for the read, is the one case it cannot tell apart.
async fn table_identity(
    session: &Session,
    schema: &str,
    name: &str,
) -> Result<Option<String>, String> {
    let engine = session.dialect().name();
    let literal = |text: &str| sql_literal(&Value::Text(text.to_string()), engine);
    let sql = match engine {
        "postgres" => format!(
            "SELECT c.oid::text FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {} AND c.relname = {} AND c.relkind IN ('r', 'p')",
            literal(schema),
            literal(name)
        ),
        "sqlserver" => format!(
            "SELECT CONVERT(varchar(20), o.object_id) + ' ' + CONVERT(varchar(30), o.create_date, 126) \
             FROM sys.objects o WHERE o.object_id = OBJECT_ID({}) AND o.type = 'U'",
            literal(&qualified(session.dialect().as_ref(), schema, name))
        ),
        "duckdb" => format!(
            "SELECT CASE WHEN COUNT(*) = 1 THEN CAST(MIN(table_oid) AS VARCHAR) END \
             FROM duckdb_tables() WHERE database_name = current_database() \
             AND schema_name = {} AND table_name = {} AND NOT temporary",
            literal(schema),
            literal(name)
        ),
        "sqlite" => {
            let quoted = session.dialect().quote_identifier(schema);
            let exists = format!(
                "SELECT COUNT(*) FROM {quoted}.sqlite_master WHERE type = 'table' AND name = {}",
                literal(name)
            );
            if scalar_count_metadata(session, &exists).await? != 1 {
                return Ok(None);
            }
            format!("PRAGMA {quoted}.schema_version")
        }
        "mysql" | "mariadb" => format!(
            "SELECT CAST(CREATE_TIME AS CHAR) FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND TABLE_TYPE = 'BASE TABLE'",
            literal(schema),
            literal(name)
        ),
        _ => return Ok(None),
    };
    Ok(match scalar_metadata(session, &sql).await? {
        Some(Value::Text(text)) => Some(text),
        Some(Value::Int(n)) => Some(n.to_string()),
        Some(Value::Decimal(text)) => Some(text),
        _ => None,
    })
}

/// A count read as core metadata (outside the user's transaction statement count).
async fn scalar_count_metadata(session: &Session, sql: &str) -> Result<i64, String> {
    match scalar_metadata(session, sql).await? {
        Some(Value::Int(n)) => Ok(n),
        Some(Value::Decimal(text) | Value::Text(text)) => text
            .parse()
            .map_err(|_| format!("unexpected count: {text}")),
        other => Err(format!("unexpected count: {other:?}")),
    }
}

/// The session and table a result's edits are written through (SDB-433), decided by the core:
///
/// - rows read through another session, or in another database, are refused (SDB-137);
/// - a query result the core found editable is written only to the table it was matched to:
///   `schema.name` must name that table, and the name must still hold the same table
///   ([`table_identity`]); a table dropped, replaced or no longer certain refuses the edit;
/// - any other result is a table data page, whose rows its own SELECT of `schema.name` read.
///
/// The table's description, with its primary key, is always read again from the catalog.
pub async fn result_edit_table(
    session: &Session,
    result: ResultId,
    schema: &str,
    name: &str,
) -> Result<(EditTarget, TableInfo), EditError> {
    let target = session.edit_target().await?;
    let failed = EditError::Unavailable;
    let bound = {
        let stored = session
            .results()
            .stored(result)
            .map_err(|e| failed(e.to_string()))?;
        let stored = stored
            .read()
            .map_err(|_| failed("result lock poisoned".into()))?;
        target.check(&stored)?;
        stored.table().cloned()
    };
    if let Some(bound) = bound {
        if bound.schema != schema || bound.name != name {
            return Err(EditError::NotThisTable(bound.name));
        }
        match table_identity(session, &bound.schema, &bound.name).await {
            Ok(Some(identity)) if identity == bound.identity => {}
            Ok(_) => return Err(EditError::TableChanged(bound.name)),
            Err(message) => {
                return Err(EditError::TableUnverified {
                    table: bound.name,
                    message,
                });
            }
        }
    }
    let table = session
        .connection()
        .describe_table(schema, name)
        .await
        .map_err(|e| failed(e.to_string()))?;
    Ok((target, table))
}

/// The names of `result`'s columns, refused unless its rows were read through `target`.
fn result_columns(
    session: &Session,
    result: ResultId,
    target: &EditTarget,
) -> Result<Vec<String>, UpdateCopyError> {
    let stored = session
        .results()
        .stored(result)
        .map_err(|e| UpdateCopyError::Failed(e.to_string()))?;
    let stored = stored
        .read()
        .map_err(|_| UpdateCopyError::Failed("result lock poisoned".into()))?;
    target.check(&stored)?;
    Ok(stored
        .schema()
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect())
}

/// Whether a result with these column names identifies `table`'s rows: its primary key is in
/// it, no column appears twice and, for a query (`every_column`), every column is the table's.
fn check_result_columns(
    table: &TableInfo,
    columns: &[String],
    every_column: bool,
) -> Result<(), UpdateCopyError> {
    if table.primary_key.is_empty() {
        return Err(UpdateCopyError::NoPrimaryKey(table.object.name.clone()));
    }
    let mut seen = HashSet::new();
    if let Some(twice) = columns.iter().find(|c| !seen.insert(c.as_str())) {
        return Err(UpdateCopyError::ColumnTwice(twice.clone()));
    }
    if let Some(key) = table.primary_key.iter().find(|k| !columns.contains(k)) {
        return Err(UpdateCopyError::KeyNotInResult(key.clone()));
    }
    if every_column
        && let Some(other) = columns
            .iter()
            .find(|c| !table.columns.iter().any(|t| t.name == **c))
    {
        return Err(UpdateCopyError::NotATableColumn(other.clone()));
    }
    Ok(())
}

/// Where the rows copied as UPDATE statements come from (SDB-388).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateSource {
    /// A table data tab's page of this table (the core wrote its SELECT, `table_data.rs`).
    Table { schema: String, name: String },
    /// A query's result, with the statement that produced it.
    Query { sql: String },
}

/// The table rows are copied as UPDATE statements of, decided by the core
/// ([`update_copy_table`]) as an edit's would be (ADR-014), and the session they were read
/// through. Only the core makes one.
pub struct UpdateTable {
    pub(crate) target: EditTarget,
    pub(crate) table: TableInfo,
}

impl UpdateTable {
    /// The table the statements update.
    pub fn table(&self) -> &TableInfo {
        &self.table
    }
}

/// Why rows cannot be copied as UPDATE statements: never guessed (SDB-388).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum UpdateCopyError {
    #[error(
        "this engine's rows are not edited by the app, so they are not copied as UPDATE statements"
    )]
    NoEditing,
    #[error(
        "on this engine a query's rows are not matched to a table: open the table's data and copy them from there"
    )]
    QueryOnEngine,
    #[error(
        "the result is not a plain SELECT of one table (it has a join, an expression, a subquery or more than one statement): its rows cannot be matched to one table's rows"
    )]
    NotOneTable,
    #[error(
        "could not tell for certain which table {0} is (a temporary table or the search path may hide another): qualify its name and run the query again"
    )]
    TableNotCertain(String),
    /// The name no longer holds the table the result's rows were matched to (SDB-433).
    #[error(
        "{0} is no longer the table these rows were read from (it was dropped or replaced): run the query again"
    )]
    TableChanged(String),
    #[error("{0} is not a table: only a table's rows can be copied as UPDATE statements")]
    NotATable(String),
    #[error("{0} has no primary key: its rows cannot be told apart")]
    NoPrimaryKey(String),
    #[error("the result does not include the primary key column {0}")]
    KeyNotInResult(String),
    #[error("the result has the column {0} twice")]
    ColumnTwice(String),
    #[error("{0} is not a column of the table")]
    NotATableColumn(String),
    /// ADR-051: the result holds the mask, not the value.
    #[error(
        "{0} is masked: its values are not in the result, so it cannot be written into an UPDATE (leave it out of the selection)"
    )]
    Masked(String),
    #[error(
        "select at least one column that is not part of the primary key: those are what the UPDATE sets"
    )]
    OnlyKeyColumns,
    #[error("a row has no value in its key column {0}, so no UPDATE can find it")]
    NullKey(String),
    #[error("there is no row {0} in the result")]
    NoSuchRow(usize),
    #[error("these rows were read from another connection or database: run the query again")]
    OtherSource,
    #[error("{0}")]
    Failed(String),
}

impl From<EditError> for UpdateCopyError {
    fn from(error: EditError) -> Self {
        match error {
            EditError::OtherSource => Self::OtherSource,
            EditError::NoPrimaryKey(_)
            | EditError::KeyNotInResult(_)
            | EditError::NoSuchRow(_)
            | EditError::NoSuchNewRow(_)
            | EditError::TooMuchToPaste(_)
            | EditError::NotATableColumn(_)
            | EditError::BadValue { .. }
            | EditError::Nothing
            | EditError::ReadOnly
            | EditError::NeedsConfirmation
            | EditError::Failed { .. }
            | EditError::TransactionOpen
            | EditError::NotAtomic(_)
            | EditError::NotOneRow { .. }
            | EditError::Masked(_)
            | EditError::Source(_)
            | EditError::NotThisTable(_)
            | EditError::TableChanged(_)
            | EditError::TableUnverified { .. }
            | EditError::Unavailable(_) => Self::Failed(error.to_string()),
        }
    }
}

/// The table `result`'s rows are copied as UPDATE statements of (SDB-388), decided as an edit
/// is (ADR-014): refused on an engine whose edits the app does not apply (no transactions), for
/// rows read through another session or in another database (SDB-137), and for a table
/// without a primary key or a result without all of it. A query must be a plain SELECT of one
/// table, resolved as [`editable_query_table`] resolves it. The primary key always comes from
/// the catalog. A read-only connection may copy: copying writes nothing.
pub async fn update_copy_table(
    session: &Session,
    result: ResultId,
    source: UpdateSource,
) -> Result<UpdateTable, UpdateCopyError> {
    // Edits are applied in a transaction (`apply_edits`): an engine without one has none.
    if !session.connection().capabilities().transactions {
        return Err(UpdateCopyError::NoEditing);
    }
    let (target, table) = match source {
        UpdateSource::Query { sql } => query_result_table(session, result, &sql).await?,
        UpdateSource::Table { schema, name } => {
            let target = session.edit_target().await?;
            let columns = result_columns(session, result, &target)?;
            let table = session
                .connection()
                .describe_table(&schema, &name)
                .await
                .map_err(|e| UpdateCopyError::Failed(e.to_string()))?;
            check_result_columns(&table, &columns, false)?;
            (target, table)
        }
    };
    Ok(UpdateTable { target, table })
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

/// A value typed in the grid (or NULL), the bytes of a file loaded into a binary cell
/// (`value_view::read_value_file`), or the column's default (ADR-044).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellInput {
    Null,
    Text(String),
    Bytes(Vec<u8>),
    /// `SET column = DEFAULT`; a new row leaves the column out.
    Default,
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
    #[error("there is no new row {0}")]
    NoSuchNewRow(usize),
    #[error("the clipboard holds more than {0} bytes: too much to paste into the grid")]
    TooMuchToPaste(usize),
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
    #[error(
        "this connection's type confirms data changes: they need a confirmation before they are applied"
    )]
    NeedsConfirmation,
    #[error("change {index} failed, nothing was applied: {message}")]
    Failed { index: usize, message: String },
    #[error(
        "a transaction is open on this tab: commit or roll it back before applying table edits"
    )]
    TransactionOpen,
    /// Several changes at once on a server whose transactions do not hold (SDB-354): one that
    /// failed would leave the ones before it applied.
    #[error(
        "{0}: apply one change at a time, so that a change that fails cannot leave the others applied"
    )]
    NotAtomic(String),
    #[error(
        "change {index} would affect {rows} rows instead of one (the row changed or vanished meanwhile?), nothing was applied"
    )]
    NotOneRow { index: usize, rows: String },
    /// The column is masked (ADR-051): the page holds the mask, not its values.
    #[error("{0} is masked: its cells cannot be edited, and rows cannot be changed by it")]
    Masked(String),
    /// The rows were read through another session, or in another database (SDB-137).
    #[error("these rows were read from another connection or database: run the query again")]
    OtherSource,
    /// The session could not say which database it is in.
    #[error("could not tell which database this tab is in: {0}")]
    Source(String),
    /// A query result's rows belong to another table than the one named (SDB-433).
    #[error("these rows were read from {0}, not from the table named: run the query again")]
    NotThisTable(String),
    /// The table's name no longer holds the table the rows were read from (SDB-433).
    #[error(
        "{0} is no longer the table these rows were read from (it was dropped or replaced): run the query again"
    )]
    TableChanged(String),
    /// Whether the name still holds that table could not be checked (SDB-433).
    #[error("could not check that {table} is still the table these rows were read from: {message}")]
    TableUnverified { table: String, message: String },
    /// The result is no longer kept, or the table could not be described.
    #[error("{0}")]
    Unavailable(String),
}

/// What an edit is written through: a session and the database it is in now (SDB-137), from
/// [`Session::edit_target`]. Only the core makes one, and [`edit_statements`] writes nothing
/// for rows read through another session (another connection, or the tab after it
/// reconnected) or in another database.
pub struct EditTarget {
    source: ResultSource,
    dialect: std::sync::Arc<dyn Dialect>,
}

impl EditTarget {
    pub(crate) fn new(source: ResultSource, dialect: std::sync::Arc<dyn Dialect>) -> Self {
        Self { source, dialect }
    }

    /// The engine's SQL dialect.
    pub fn dialect(&self) -> &dyn Dialect {
        self.dialect.as_ref()
    }

    /// Whether `result`'s rows were read through this session, in this database.
    pub fn check(&self, result: &StoredResult) -> Result<(), EditError> {
        if result.source() == Some(&self.source) {
            Ok(())
        } else {
            Err(EditError::OtherSource)
        }
    }
}

/// Whether changes may be applied on this connection: never when it is read-only, and where
/// its type confirms data changes (Production, ADR-029) only after the user confirmed the
/// statements.
pub fn check_edit_policy(
    read_only: bool,
    confirm_data_changes: bool,
    confirmed: bool,
) -> Result<(), EditError> {
    if read_only {
        Err(EditError::ReadOnly)
    } else if confirm_data_changes && !confirmed {
        Err(EditError::NeedsConfirmation)
    } else {
        Ok(())
    }
}

/// The statements for `changes` to the table whose page is `result`, written through
/// `target`: refused unless the rows were read through it, in its current database (SDB-137).
pub fn edit_statements(
    target: &EditTarget,
    table: &TableInfo,
    result: &StoredResult,
    changes: &[RowChange],
) -> Result<Vec<String>, EditError> {
    target.check(result)?;
    let dialect = target.dialect();
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
        // A masked key column holds the mask, not the value the row is found by (ADR-051).
        if let Some((_, key)) = keys.iter().find(|(i, _)| result.is_masked(*i)) {
            return Err(EditError::Masked((*key).to_string()));
        }
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
                        if result.is_masked(*i) {
                            return Err(EditError::Masked(name.to_string()));
                        }
                        let value = match input {
                            CellInput::Default => default_value(dialect.name(), table, name),
                            other => literal(dialect.name(), name, logical, other)?,
                        };
                        Ok(format!("{} = {value}", dialect.quote_identifier(name)))
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
                    // Left out, the column takes its default.
                    if *input == CellInput::Default {
                        continue;
                    }
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
    // One transaction makes several changes all or nothing; where it does not hold, only one
    // change at a time can be (SDB-354).
    if statements.len() > 1
        && let Some(reason) = session.atomic_writes_refusal()
    {
        return Err(EditError::NotAtomic(reason));
    }
    // Edits are a transaction of their own (ADR-031): never nested in, nor committing, one the
    // user left open on this session.
    if let Ok(TransactionState::Open | TransactionState::Failed) =
        session.connection().transaction_state().await
    {
        return Err(EditError::TransactionOpen);
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
pub(crate) async fn run_one(session: &Session, sql: &str) -> Result<Option<u64>, String> {
    let mut events = session.run_generated(sql);
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
            | CoreEvent::ResultEnded { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    match failure {
        Some(message) => Err(message),
        None => Ok(affected),
    }
}

/// The value `SET column = DEFAULT` gives. SQLite has no `DEFAULT` there, so the column's
/// default expression is written as the catalog holds it (the database's own text, what an
/// INSERT without the column would evaluate), or NULL without one, as SQLite does.
fn default_value(dialect: &str, table: &TableInfo, column: &str) -> String {
    match dialect {
        "sqlite" => table
            .columns
            .iter()
            .find(|c| c.name == column)
            .and_then(|c| c.default.as_deref())
            .map_or_else(
                || "NULL".to_string(),
                |expression| format!("({expression})"),
            ),
        _ => "DEFAULT".to_string(),
    }
}

/// Refuses an input the column's type cannot take (a pending value is checked when it is
/// given, so a bulk edit or a paste fails at once, not when it is applied).
pub(crate) fn check_input(
    column: &str,
    logical: LogicalType,
    input: &CellInput,
) -> Result<(), EditError> {
    match input {
        CellInput::Default => Ok(()),
        other => literal("postgres", column, logical, other).map(|_| ()),
    }
}

/// What the user typed, as a literal for a column of this type.
fn literal(
    dialect: &str,
    column: &str,
    logical: LogicalType,
    input: &CellInput,
) -> Result<String, EditError> {
    let text = match input {
        CellInput::Null => return Ok("NULL".into()),
        // Written by the caller: `DEFAULT`, or the column left out of an INSERT.
        CellInput::Default => return Ok("DEFAULT".into()),
        CellInput::Text(text) => text,
        // Raw bytes only go to a binary column; anything else takes text.
        CellInput::Bytes(bytes) => {
            return if logical == LogicalType::Bytes {
                Ok(sql_literal(&Value::Bytes(bytes.clone()), dialect))
            } else {
                Err(EditError::BadValue {
                    column: column.to_string(),
                    value: format!("{} bytes", bytes.len()),
                    expected: "text",
                })
            };
        }
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
        // The grid shows a time zone as `+00` (UTC), which SQL Server does not read back: it
        // takes `+00:00`.
        LogicalType::TimestampTz if dialect == "sqlserver" => Value::Text(sqlserver_offset(text)),
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

/// `… 08:00:00+02` → `… 08:00:00+02:00`: an offset of whole hours, as the grid writes it, with
/// the minutes SQL Server needs. Anything else is left as typed.
fn sqlserver_offset(text: &str) -> String {
    let trimmed = text.trim_end();
    let bytes = trimmed.as_bytes();
    let n = bytes.len();
    let hours_only = n >= 4
        && matches!(bytes[n - 3], b'+' | b'-')
        && bytes[n - 2].is_ascii_digit()
        && bytes[n - 1].is_ascii_digit()
        && bytes[n - 4].is_ascii_digit()
        && trimmed[..n - 3].contains(':');
    if hours_only {
        format!("{trimmed}:00")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editable_query_requires_one_unambiguous_table_projection() {
        let dialect = spelaion_dialect::SqliteDialect;
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
        // A loaded file's bytes: the engine's binary literal, and only for a binary column.
        let bytes = CellInput::Bytes(vec![0x89, b'P', 0]);
        assert_eq!(
            literal("postgres", "c", LogicalType::Bytes, &bytes).unwrap(),
            "'\\x895000'"
        );
        assert_eq!(
            literal("sqlserver", "c", LogicalType::Bytes, &bytes).unwrap(),
            "0x895000"
        );
        assert_eq!(
            literal("sqlite", "c", LogicalType::Bytes, &bytes).unwrap(),
            "X'895000'"
        );
        assert!(literal("postgres", "c", LogicalType::Text, &bytes).is_err());
        // SQL Server reads a time zone written as the grid shows it.
        let tz = |dialect, text: &str| {
            literal(
                dialect,
                "c",
                LogicalType::TimestampTz,
                &CellInput::Text(text.into()),
            )
            .unwrap()
        };
        assert_eq!(
            tz("sqlserver", "2024-01-01 08:00:00.5+00"),
            "N'2024-01-01 08:00:00.5+00:00'"
        );
        assert_eq!(
            tz("sqlserver", "2024-01-01 08:00:00-05"),
            "N'2024-01-01 08:00:00-05:00'"
        );
        assert_eq!(
            tz("sqlserver", "2024-01-01 08:00:00 +05:30"),
            "N'2024-01-01 08:00:00 +05:30'"
        );
        assert_eq!(tz("sqlserver", "2024-01-01"), "N'2024-01-01'");
        assert_eq!(
            tz("postgres", "2024-01-01 08:00:00+00"),
            "'2024-01-01 08:00:00+00'"
        );
        assert_eq!(
            literal(
                "mysql",
                "c",
                LogicalType::Text,
                &CellInput::Text("a\\b".into())
            )
            .unwrap(),
            // Read the same with and without NO_BACKSLASH_ESCAPES (SDB-161).
            "_utf8mb4 X'615C62'"
        );
    }
}
