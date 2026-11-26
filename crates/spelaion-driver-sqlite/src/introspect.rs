//! SQLite catalog of a local file, through rusqlite (queries and rows in
//! `spelaion-sqlite-schema`'s `catalog`, ADR-058).

use spelaion_driver_api::{LikeFilter, ObjectInfo, ReferencingKey, Result, SchemaInfo, TableInfo};

use spelaion_sqlite_schema::catalog::{self, Row};

use crate::exec::{map_error, owned};

pub(crate) fn query(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(sql).map_err(map_error)?;
    let width = stmt.column_count();
    stmt.query_map([], |r| {
        (0..width)
            .map(|i| r.get_ref(i).map(owned))
            .collect::<rusqlite::Result<Row>>()
    })
    .map_err(map_error)?
    .collect::<rusqlite::Result<Vec<Row>>>()
    .map_err(map_error)
}

pub fn list_schemas(conn: &rusqlite::Connection) -> Result<Vec<SchemaInfo>> {
    Ok(catalog::schemas(&query(
        conn,
        &catalog::schemas_sql(&LikeFilter::default()),
    )?))
}

pub fn list_objects(conn: &rusqlite::Connection, schema: &str) -> Result<Vec<ObjectInfo>> {
    Ok(catalog::objects(
        schema,
        &query(conn, &catalog::objects_sql(schema, &LikeFilter::default()))?,
    ))
}

pub fn describe_table(conn: &rusqlite::Connection, schema: &str, name: &str) -> Result<TableInfo> {
    let (object, checks) = catalog::described_object(
        schema,
        name,
        &query(conn, &catalog::described_object_sql(schema, name))?,
    )?;
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
    table.constraints.extend(checks);
    Ok(table)
}

/// Every table of `schema`, as [`describe_table`] describes each, in five catalog queries
/// (SDB-220).
pub fn describe_tables(conn: &rusqlite::Connection, schema: &str) -> Result<Vec<TableInfo>> {
    let [objects, columns, indexes, uniques, foreign_keys] = catalog::tables_sql(schema);
    catalog::tables(
        schema,
        &[
            query(conn, &objects)?,
            query(conn, &columns)?,
            query(conn, &indexes)?,
            query(conn, &uniques)?,
            query(conn, &foreign_keys)?,
        ],
    )
}

pub fn list_referencing_keys(
    conn: &rusqlite::Connection,
    schema: &str,
    name: &str,
) -> Result<Vec<ReferencingKey>> {
    let rows = query(conn, &catalog::referencing_keys_sql(schema, name))?;
    let primary_key = catalog::names(&query(conn, &catalog::primary_key_sql(schema, name))?);
    Ok(catalog::referencing_keys(schema, &rows, &primary_key))
}
