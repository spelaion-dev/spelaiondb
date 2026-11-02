//! SQLite catalog: `sqlite_schema` and the `pragma_*` table-valued functions.

use spelunk_driver_api::{
    ColumnInfo, Dialect, DriverError, ForeignKeyInfo, IndexInfo, ObjectInfo, ObjectKind, Result,
    SchemaInfo, TableInfo,
};

use crate::dialect::SqliteDialect;
use crate::exec::map_error;

pub fn list_schemas(conn: &rusqlite::Connection) -> Result<Vec<SchemaInfo>> {
    let mut stmt = conn
        .prepare("select name from pragma_database_list order by seq")
        .map_err(map_error)?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(map_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_error)?;
    Ok(names
        .into_iter()
        .map(|name| SchemaInfo {
            name,
            system: false,
        })
        .collect())
}

/// `sqlite_schema` of a given schema (main, temp or an attached database).
fn schema_table(schema: &str) -> String {
    format!("{}.sqlite_schema", SqliteDialect.quote_identifier(schema))
}

pub fn list_objects(conn: &rusqlite::Connection, schema: &str) -> Result<Vec<ObjectInfo>> {
    let sql = format!(
        r"select name, type from {} where type in ('table', 'view') and name not like 'sqlite\_%' escape '\' order by name",
        schema_table(schema)
    );
    let mut stmt = conn.prepare(&sql).map_err(map_error)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(map_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(map_error)?;
    Ok(rows
        .into_iter()
        .map(|(name, kind)| ObjectInfo {
            schema: schema.to_string(),
            name,
            kind: if kind == "view" {
                ObjectKind::View
            } else {
                ObjectKind::Table
            },
            // SQLite keeps no cheap estimates (only with ANALYZE, in sqlite_stat1).
            estimated_rows: None,
            comment: None,
        })
        .collect())
}

pub fn describe_table(conn: &rusqlite::Connection, schema: &str, name: &str) -> Result<TableInfo> {
    let object = list_objects(conn, schema)?
        .into_iter()
        .find(|o| o.name == name)
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;

    // hidden = 1: hidden columns of virtual tables; 2 and 3: generated (shown).
    let mut stmt = conn
        .prepare(r#"select name, type, "notnull", dflt_value, pk from pragma_table_xinfo(?1, ?2) where hidden <> 1 order by cid"#)
        .map_err(map_error)?;
    let raw: Vec<(String, String, bool, Option<String>, i64)> = stmt
        .query_map([name, schema], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .map_err(map_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(map_error)?;

    let mut pk: Vec<(i64, String)> = raw
        .iter()
        .filter(|c| c.4 > 0)
        .map(|c| (c.4, c.0.clone()))
        .collect();
    pk.sort();
    let columns = raw
        .into_iter()
        .map(|(name, native_type, notnull, default, _)| ColumnInfo {
            name,
            native_type,
            nullable: !notnull,
            default,
            comment: None,
        })
        .collect();

    let mut stmt = conn
        .prepare("select name, \"unique\", origin from pragma_index_list(?1, ?2) order by name")
        .map_err(map_error)?;
    let index_rows: Vec<(String, bool, String)> = stmt
        .query_map([name, schema], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(map_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(map_error)?;
    let index_sql = format!(
        "select sql from {} where type = 'index' and name = ?1",
        schema_table(schema)
    );
    let mut indexes = Vec::with_capacity(index_rows.len());
    for (index_name, unique, origin) in index_rows {
        // Automatic indexes (PRIMARY KEY, UNIQUE) have no SQL of their own.
        let sql: Option<String> = conn
            .query_row(&index_sql, [&index_name], |r| r.get(0))
            .map_err(map_error)?;
        let definition = sql.unwrap_or_else(|| {
            format!(
                "-- automatic index for {}",
                if origin == "pk" {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                }
            )
        });
        indexes.push(IndexInfo {
            name: index_name,
            unique,
            primary: origin == "pk",
            definition,
        });
    }

    let mut stmt = conn
        .prepare(r#"select id, "table", "from", "to" from pragma_foreign_key_list(?1, ?2) order by id, seq"#)
        .map_err(map_error)?;
    let fk_rows: Vec<(i64, String, String, Option<String>)> = stmt
        .query_map([name, schema], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map_err(map_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(map_error)?;
    let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
    for (id, table, from, to) in fk_rows {
        if foreign_keys
            .last()
            .is_none_or(|fk| fk.name != format!("fk_{id}"))
        {
            foreign_keys.push(ForeignKeyInfo {
                // SQLite does not expose the constraint name in this pragma.
                name: format!("fk_{id}"),
                columns: Vec::new(),
                referenced_schema: schema.to_string(),
                referenced_table: table,
                referenced_columns: Vec::new(),
            });
        }
        let fk = foreign_keys.last_mut().expect("just added");
        fk.columns.push(from);
        if let Some(to) = to {
            fk.referenced_columns.push(to);
        }
    }
    // An empty `to` means "the primary key of the referenced table".
    for fk in &mut foreign_keys {
        if fk.referenced_columns.is_empty() {
            fk.referenced_columns = describe_pk(conn, schema, &fk.referenced_table)?;
        }
    }

    Ok(TableInfo {
        object,
        columns,
        primary_key: pk.into_iter().map(|(_, n)| n).collect(),
        indexes,
        foreign_keys,
    })
}

fn describe_pk(conn: &rusqlite::Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("select name from pragma_table_info(?1, ?2) where pk > 0 order by pk")
        .map_err(map_error)?;
    stmt.query_map([table, schema], |r| r.get(0))
        .map_err(map_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(map_error)
}
