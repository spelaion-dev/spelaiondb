//! MySQL/MariaDB catalog (`information_schema`) for the explorer, autocompletion and the MCP
//! server. In MySQL a "schema" is a database.

use spelaion_driver_api::{
    ColumnInfo, DriverError, ForeignKeyInfo, IndexInfo, ObjectInfo, ObjectKind, Result, SchemaInfo,
    TableInfo,
};

use crate::{Flavor, MysqlConnection};

const SYSTEM_SCHEMAS: &[&str] = &["information_schema", "mysql", "performance_schema", "sys"];

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|s| !s.is_empty())
}

pub(crate) async fn list_schemas(conn: &MysqlConnection) -> Result<Vec<SchemaInfo>> {
    let rows = conn
        .rows(
            "select schema_name from information_schema.schemata order by schema_name",
            vec![],
        )
        .await?;
    let mut schemas: Vec<SchemaInfo> = rows
        .iter()
        .map(|row| {
            let name = text(row, 0);
            SchemaInfo {
                system: SYSTEM_SCHEMAS.contains(&name.to_ascii_lowercase().as_str()),
                name,
            }
        })
        .collect();
    schemas.sort_by(|a, b| (a.system, &a.name).cmp(&(b.system, &b.name)));
    Ok(schemas)
}

const OBJECTS_SQL: &str = "select table_schema, table_name, table_type, table_rows, table_comment
     from information_schema.tables where table_schema = ?";

fn object(row: &[Option<String>]) -> ObjectInfo {
    let kind = match text(row, 2).as_str() {
        "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
        // BASE TABLE, SYSTEM VERSIONED (MariaDB), SEQUENCE (MariaDB)…
        _ => ObjectKind::Table,
    };
    ObjectInfo {
        schema: text(row, 0),
        name: text(row, 1),
        kind,
        // `table_rows` is InnoDB's estimate (cheap; not a `count(*)`); null for views.
        estimated_rows: opt(row, 3).and_then(|n| n.parse().ok()),
        comment: opt(row, 4),
    }
}

pub(crate) async fn list_objects(conn: &MysqlConnection, schema: &str) -> Result<Vec<ObjectInfo>> {
    let rows = conn
        .rows(
            &format!("{OBJECTS_SQL} order by table_name"),
            vec![schema.to_string()],
        )
        .await?;
    Ok(rows.iter().map(|row| object(row)).collect())
}

pub(crate) async fn describe_table(
    conn: &MysqlConnection,
    flavor: Flavor,
    schema: &str,
    name: &str,
) -> Result<TableInfo> {
    let params = || vec![schema.to_string(), name.to_string()];
    let object = conn
        .rows(&format!("{OBJECTS_SQL} and table_name = ?"), params())
        .await?
        .first()
        .map(|row| object(row))
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;

    let columns = conn
        .rows(
            "select column_name, column_type, is_nullable, column_default, column_comment, extra
             from information_schema.columns
             where table_schema = ? and table_name = ?
             order by ordinal_position",
            params(),
        )
        .await?
        .iter()
        .map(|row| {
            // MariaDB quotes string defaults (`'x'`) and writes NULL as the text `NULL`.
            let default = match flavor {
                Flavor::Mariadb => opt(row, 3).filter(|d| d != "NULL"),
                Flavor::Mysql => opt(row, 3),
            };
            ColumnInfo {
                name: text(row, 0),
                native_type: text(row, 1),
                nullable: text(row, 2) == "YES",
                default,
                comment: opt(row, 4),
            }
        })
        .collect();

    // One row per indexed column, in order.
    let index_rows = conn
        .rows(
            "select index_name, non_unique, column_name
             from information_schema.statistics
             where table_schema = ? and table_name = ?
             order by index_name = 'PRIMARY' desc, index_name, seq_in_index",
            params(),
        )
        .await?;
    let mut indexes: Vec<(String, bool, Vec<String>)> = Vec::new();
    for row in &index_rows {
        let index = text(row, 0);
        let column = text(row, 2);
        match indexes.last_mut() {
            Some((last, _, columns)) if *last == index => columns.push(column),
            _ => indexes.push((index, text(row, 1) == "0", vec![column])),
        }
    }
    let quote = |s: &str| format!("`{}`", s.replace('`', "``"));
    let primary_key = indexes
        .iter()
        .find(|(index, _, _)| index == "PRIMARY")
        .map(|(_, _, columns)| columns.clone())
        .unwrap_or_default();
    let indexes = indexes
        .into_iter()
        .map(|(index, unique, columns)| {
            let primary = index == "PRIMARY";
            let list = columns
                .iter()
                .map(|c| quote(c))
                .collect::<Vec<_>>()
                .join(", ");
            let definition = if primary {
                format!("ALTER TABLE {} ADD PRIMARY KEY ({list})", quote(name))
            } else {
                format!(
                    "CREATE {}INDEX {} ON {} ({list})",
                    if unique { "UNIQUE " } else { "" },
                    quote(&index),
                    quote(name)
                )
            };
            IndexInfo {
                name: index,
                unique,
                primary,
                definition,
            }
        })
        .collect();

    let fk_rows = conn
        .rows(
            "select constraint_name, column_name, referenced_table_schema,
                    referenced_table_name, referenced_column_name
             from information_schema.key_column_usage
             where table_schema = ? and table_name = ? and referenced_table_name is not null
             order by constraint_name, ordinal_position",
            params(),
        )
        .await?;
    let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
    for row in &fk_rows {
        let constraint = text(row, 0);
        match foreign_keys.last_mut() {
            Some(fk) if fk.name == constraint => {
                fk.columns.push(text(row, 1));
                fk.referenced_columns.push(text(row, 4));
            }
            _ => foreign_keys.push(ForeignKeyInfo {
                name: constraint,
                columns: vec![text(row, 1)],
                referenced_schema: text(row, 2),
                referenced_table: text(row, 3),
                referenced_columns: vec![text(row, 4)],
            }),
        }
    }

    Ok(TableInfo {
        object,
        columns,
        primary_key,
        indexes,
        foreign_keys,
    })
}
