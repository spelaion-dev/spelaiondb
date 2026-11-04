//! SQLite catalog of a local file, through rusqlite (queries and rows in `catalog.rs`).

use rusqlite::types::Value as SqlValue;
use spelaion_driver_api::{DriverError, ObjectInfo, Result, SchemaInfo, TableInfo};

use crate::catalog::{self, Row};
use crate::exec::map_error;

pub(crate) fn query(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(sql).map_err(map_error)?;
    let width = stmt.column_count();
    stmt.query_map([], |r| {
        (0..width)
            .map(|i| r.get::<_, SqlValue>(i))
            .collect::<rusqlite::Result<Row>>()
    })
    .map_err(map_error)?
    .collect::<rusqlite::Result<Vec<Row>>>()
    .map_err(map_error)
}

pub fn list_schemas(conn: &rusqlite::Connection) -> Result<Vec<SchemaInfo>> {
    Ok(catalog::schemas(&query(conn, catalog::SCHEMAS_SQL)?))
}

pub fn list_objects(conn: &rusqlite::Connection, schema: &str) -> Result<Vec<ObjectInfo>> {
    Ok(catalog::objects(
        schema,
        &query(conn, &catalog::objects_sql(schema))?,
    ))
}

pub fn describe_table(conn: &rusqlite::Connection, schema: &str, name: &str) -> Result<TableInfo> {
    let object = list_objects(conn, schema)?
        .into_iter()
        .find(|o| o.name == name)
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;
    let (mut table, to_primary_keys) = catalog::table(
        object,
        &query(conn, &catalog::columns_sql(schema, name))?,
        &query(conn, &catalog::indexes_sql(schema, name))?,
        &query(conn, &catalog::foreign_keys_sql(schema, name))?,
        &query(conn, &catalog::unique_columns_sql(schema, name))?,
    );
    for i in to_primary_keys {
        let referenced = table.foreign_keys[i].referenced_table.clone();
        table.foreign_keys[i].referenced_columns = catalog::names(&query(
            conn,
            &catalog::primary_key_sql(schema, &referenced),
        )?);
    }
    Ok(table)
}
