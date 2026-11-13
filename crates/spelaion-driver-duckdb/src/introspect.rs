//! Schemas, tables and columns from DuckDB's catalog functions (`duckdb_schemas()`,
//! `duckdb_tables()`…). Schemas of the open database keep their name (`main`); those of
//! attached databases are shown as `database.schema`.

use duckdb::types::Value;
use duckdb::{Connection, params, params_from_iter};
use spelaion_driver_api::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DriverError, ForeignKeyInfo, IndexInfo, LikeFilter,
    ObjectInfo, ObjectKind, ReferencingKey, Result, SchemaColumn, SchemaInfo, TableInfo,
};

use crate::exec::map_error;

pub(crate) fn current_database(conn: &Connection) -> Result<String> {
    conn.query_row("select current_database()", [], |r| r.get(0))
        .map_err(map_error)
}

/// `like` as SQL over `column`, its patterns appended to `params` as `?n` (ADR-025).
pub(crate) fn like_sql(like: &LikeFilter, column: &str, params: &mut Vec<Value>) -> String {
    like.sql(column, |pattern| {
        params.push(Value::Text(pattern.to_string()));
        format!("?{}", params.len())
    })
}

pub fn list_schemas(conn: &Connection, like: &LikeFilter) -> Result<Vec<SchemaInfo>> {
    let current = current_database(conn)?;
    // The name shown, which the explorer's filter matches: `database.schema` when attached.
    let shown = "case when database_name = current_database() then schema_name \
                 else database_name || '.' || schema_name end";
    let mut values = Vec::new();
    let narrow = like_sql(like, shown, &mut values);
    let mut statement = conn
        .prepare(&format!(
            "select database_name, schema_name, \
                    schema_name in ('information_schema', 'pg_catalog') as system \
             from duckdb_schemas() \
             where database_name not in ('system', 'temp'){narrow} \
             order by database_name <> current_database(), database_name, system, schema_name"
        ))
        .map_err(map_error)?;
    let rows = statement
        .query_map(params_from_iter(values), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .map_err(map_error)?;
    rows.map(|row| {
        let (database, schema, system) = row.map_err(map_error)?;
        Ok(SchemaInfo {
            name: if database == current {
                schema
            } else {
                format!("{database}.{schema}")
            },
            system,
        })
    })
    .collect()
}

/// `schema` as shown by [`list_schemas`] → (database, schema).
pub(crate) fn locate(conn: &Connection, schema: &str) -> Result<(String, String)> {
    let current = current_database(conn)?;
    if let Some((database, rest)) = schema.split_once('.') {
        let attached: bool = conn
            .query_row(
                "select count(*) > 0 from duckdb_databases() where database_name = ?",
                params![database],
                |r| r.get(0),
            )
            .map_err(map_error)?;
        if attached {
            return Ok((database.to_string(), rest.to_string()));
        }
    }
    Ok((current, schema.to_string()))
}

fn objects(
    conn: &Connection,
    database: &str,
    schema: &str,
    name: Option<&str>,
    like: &LikeFilter,
) -> Result<Vec<ObjectInfo>> {
    let mut values = vec![
        Value::Text(database.to_string()),
        Value::Text(schema.to_string()),
        name.map_or(Value::Null, |n| Value::Text(n.to_string())),
    ];
    let narrow = like_sql(like, "o.name", &mut values);
    let mut statement = conn
        .prepare(&format!(
            "select * from (select table_name, false, estimated_size, comment from duckdb_tables() \
               where database_name = ?1 and schema_name = ?2 and (?3 is null or table_name = ?3) \
             union all \
             select view_name, true, null, comment from duckdb_views() \
               where database_name = ?1 and schema_name = ?2 and (?3 is null or view_name = ?3) \
                 and not internal) as o(name, is_view, estimated_size, comment) \
             where true{narrow} \
             order by 1"
        ))
        .map_err(map_error)?;
    let shown = schema_label(conn, database, schema)?;
    let rows = statement
        .query_map(params_from_iter(values), |r| {
            Ok(ObjectInfo {
                schema: shown.clone(),
                name: r.get(0)?,
                kind: if r.get::<_, bool>(1)? {
                    ObjectKind::View
                } else {
                    ObjectKind::Table
                },
                estimated_rows: r.get(2)?,
                comment: r.get::<_, Option<String>>(3)?.filter(|c| !c.is_empty()),
            })
        })
        .map_err(map_error)?;
    rows.map(|row| row.map_err(map_error)).collect()
}

fn schema_label(conn: &Connection, database: &str, schema: &str) -> Result<String> {
    Ok(if database == current_database(conn)? {
        schema.to_string()
    } else {
        format!("{database}.{schema}")
    })
}

pub fn list_objects(conn: &Connection, schema: &str, like: &LikeFilter) -> Result<Vec<ObjectInfo>> {
    let (database, schema) = locate(conn, schema)?;
    objects(conn, &database, &schema, None, like)
}

/// Every column of the schema's tables and views (those `list_objects` lists).
pub fn list_columns(conn: &Connection, schema: &str) -> Result<Vec<SchemaColumn>> {
    let (database, schema) = locate(conn, schema)?;
    let mut statement = conn
        .prepare(
            "select table_name, column_name, data_type from duckdb_columns()              where database_name = ? and schema_name = ? and not internal              order by table_name, column_index",
        )
        .map_err(map_error)?;
    statement
        .query_map(params![database, schema], |r| {
            Ok(SchemaColumn {
                table: r.get(0)?,
                name: r.get(1)?,
                native_type: r.get(2)?,
            })
        })
        .map_err(map_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_error)
}

pub fn describe_table(conn: &Connection, schema: &str, name: &str) -> Result<TableInfo> {
    let (database, schema) = locate(conn, schema)?;
    let object = objects(conn, &database, &schema, Some(name), &LikeFilter::default())?
        .into_iter()
        .next()
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;

    let mut statement = conn
        .prepare(
            "select column_name, data_type, is_nullable, column_default, comment from duckdb_columns() \
             where database_name = ? and schema_name = ? and table_name = ? order by column_index",
        )
        .map_err(map_error)?;
    let columns = statement
        .query_map(params![database, schema, name], |r| {
            Ok(ColumnInfo {
                name: r.get(0)?,
                native_type: r.get(1)?,
                nullable: r.get(2)?,
                default: r.get(3)?,
                comment: r.get::<_, Option<String>>(4)?.filter(|c| !c.is_empty()),
            })
        })
        .map_err(map_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_error)?;

    // Lists come back joined by a character no name contains, to keep to plain text.
    let mut statement = conn
        .prepare(
            "select constraint_type, constraint_name, \
                    array_to_string(constraint_column_names, chr(1)), \
                    referenced_table, array_to_string(referenced_column_names, chr(1)), \
                    constraint_text \
             from duckdb_constraints() \
             where database_name = ? and schema_name = ? and table_name = ? \
               and constraint_type in ('PRIMARY KEY', 'FOREIGN KEY', 'UNIQUE', 'CHECK') \
             order by constraint_index",
        )
        .map_err(map_error)?;
    let split = |s: Option<String>| -> Vec<String> {
        s.filter(|s| !s.is_empty())
            .map(|s| s.split('\u{1}').map(String::from).collect())
            .unwrap_or_default()
    };
    let mut primary_key = Vec::new();
    let mut foreign_keys = Vec::new();
    let mut indexes = Vec::new();
    let mut constraints = Vec::new();
    let rows = statement
        .query_map(params![database, schema, name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(map_error)?;
    for row in rows {
        let (kind, constraint_name, columns_text, referenced_table, referenced_columns, text) =
            row.map_err(map_error)?;
        let constraint_columns = split(columns_text);
        let constraint_kind = match kind.as_str() {
            "PRIMARY KEY" => Some(ConstraintKind::PrimaryKey),
            "UNIQUE" => Some(ConstraintKind::Unique),
            "CHECK" => Some(ConstraintKind::Check),
            _ => None,
        };
        if let Some(kind) = constraint_kind {
            constraints.push(ConstraintInfo {
                name: constraint_name.clone(),
                kind,
                columns: if kind == ConstraintKind::Check {
                    Vec::new()
                } else {
                    constraint_columns.clone()
                },
                definition: text.unwrap_or_default(),
            });
        }
        if kind == "UNIQUE" || kind == "CHECK" {
            continue;
        }
        if kind == "PRIMARY KEY" {
            indexes.push(IndexInfo {
                name: constraint_name
                    .clone()
                    .unwrap_or_else(|| format!("{name}_pkey")),
                unique: true,
                primary: true,
                definition: format!("PRIMARY KEY ({})", constraint_columns.join(", ")),
            });
            primary_key = constraint_columns;
        } else {
            foreign_keys.push(ForeignKeyInfo {
                name: constraint_name.unwrap_or_default(),
                columns: constraint_columns,
                referenced_schema: schema_label(conn, &database, &schema)?,
                referenced_table: referenced_table.unwrap_or_default(),
                referenced_columns: split(referenced_columns),
            });
        }
    }

    let mut statement = conn
        .prepare(
            "select index_name, is_unique, coalesce(sql, '') from duckdb_indexes() \
             where database_name = ? and schema_name = ? and table_name = ? order by index_name",
        )
        .map_err(map_error)?;
    let found = statement
        .query_map(params![database, schema, name], |r| {
            Ok(IndexInfo {
                name: r.get(0)?,
                unique: r.get(1)?,
                primary: false,
                definition: r.get(2)?,
            })
        })
        .map_err(map_error)?;
    for index in found {
        indexes.push(index.map_err(map_error)?);
    }

    Ok(TableInfo {
        object,
        columns,
        primary_key,
        indexes,
        foreign_keys,
        constraints,
    })
}

/// The foreign keys pointing to `schema.name`. DuckDB keeps a key and the table it references
/// in one schema, so they come from that schema (`referenced_table` is written as the key's
/// DDL wrote it: compared ignoring case, as DuckDB resolves names).
pub fn list_referencing_keys(
    conn: &Connection,
    schema: &str,
    name: &str,
) -> Result<Vec<ReferencingKey>> {
    let (database, schema_name) = locate(conn, schema)?;
    let label = schema_label(conn, &database, &schema_name)?;
    let mut statement = conn
        .prepare(
            "select table_name, coalesce(constraint_name, ''), \
                    array_to_string(constraint_column_names, chr(1)), \
                    array_to_string(referenced_column_names, chr(1)) \
             from duckdb_constraints() \
             where database_name = ? and schema_name = ? and constraint_type = 'FOREIGN KEY' \
               and lower(referenced_table) = lower(?) \
             order by table_name, constraint_name, constraint_index",
        )
        .map_err(map_error)?;
    let split = |s: Option<String>| -> Vec<String> {
        s.filter(|s| !s.is_empty())
            .map(|s| s.split('\u{1}').map(String::from).collect())
            .unwrap_or_default()
    };
    let rows = statement
        .query_map(params![database, schema_name, name], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(map_error)?;
    let mut keys = Vec::new();
    for row in rows {
        let (table, key, columns, referenced) = row.map_err(map_error)?;
        keys.push(ReferencingKey {
            schema: label.clone(),
            table,
            name: key,
            columns: split(columns),
            referenced_columns: split(referenced),
        });
    }
    Ok(keys)
}
