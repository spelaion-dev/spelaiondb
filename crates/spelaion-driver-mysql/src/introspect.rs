//! MySQL/MariaDB catalog (`information_schema`) for the explorer, autocompletion and the MCP
//! server. In MySQL a "schema" is a database.

use spelaion_driver_api::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DriverError, ForeignKeyInfo, IndexInfo, LikeFilter,
    ObjectInfo, ObjectKind, ReferencingKey, Result, SchemaInfo, TableInfo,
};

use crate::{Flavor, MysqlConnection};

const SYSTEM_SCHEMAS: &[&str] = &["information_schema", "mysql", "performance_schema", "sys"];

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|s| !s.is_empty())
}

/// MySQL before 8.0.16 parsed CHECK constraints but ignored them. MariaDB is different.
fn mysql_without_checks(version: &str) -> bool {
    if version.to_ascii_lowercase().contains("mariadb") {
        return false;
    }
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
    let patch = parts.next().and_then(|part| {
        part.split(|c: char| !c.is_ascii_digit())
            .next()
            .and_then(|part| part.parse::<u32>().ok())
    });
    matches!((major, minor, patch), (Some(5), Some(_), Some(_)))
        || matches!((major, minor, patch), (Some(8), Some(0), Some(0..=15)))
}

/// `like` as SQL over `column`, its patterns appended to `params` as `?` (ADR-025).
pub(crate) fn like_sql(like: &LikeFilter, column: &str, params: &mut Vec<String>) -> String {
    like.sql(column, |pattern| {
        params.push(pattern.to_string());
        "?".to_string()
    })
}

pub(crate) async fn list_schemas(
    conn: &MysqlConnection,
    like: &LikeFilter,
) -> Result<Vec<SchemaInfo>> {
    let mut params = Vec::new();
    let narrow = like_sql(like, "schema_name", &mut params);
    let rows = conn
        .rows(
            &format!(
                "select schema_name from information_schema.schemata where true{narrow} \
                 order by schema_name"
            ),
            params,
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

/// MariaDB's sequences are tables to `information_schema`; they have a folder of their own.
const OBJECTS_SQL: &str = "select table_schema, table_name, table_type, table_rows, table_comment
     from information_schema.tables where table_schema = ? and table_type <> 'SEQUENCE'";

fn object(row: &[Option<String>]) -> ObjectInfo {
    let kind = match text(row, 2).as_str() {
        "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
        // BASE TABLE, SYSTEM VERSIONED (MariaDB)…
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

pub(crate) async fn list_objects(
    conn: &MysqlConnection,
    schema: &str,
    like: &LikeFilter,
) -> Result<Vec<ObjectInfo>> {
    let mut params = vec![schema.to_string()];
    let narrow = like_sql(like, "table_name", &mut params);
    let rows = conn
        .rows(
            &format!("{OBJECTS_SQL}{narrow} order by table_name"),
            params,
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

    // Check constraints: MySQL names them per schema, MariaDB per table.
    let same_table = match flavor {
        Flavor::Mariadb => " and cc.table_name = tc.table_name",
        Flavor::Mysql => "",
    };
    let constraint_rows = conn
        .rows(
            &format!(
                "select tc.constraint_name, tc.constraint_type, k.column_name, cc.check_clause
                 from information_schema.table_constraints tc
                 left join information_schema.key_column_usage k
                   on k.constraint_schema = tc.constraint_schema and k.table_name = tc.table_name
                  and k.constraint_name = tc.constraint_name
                 left join information_schema.check_constraints cc
                   on cc.constraint_schema = tc.constraint_schema
                  and cc.constraint_name = tc.constraint_name{same_table}
                 where tc.table_schema = ? and tc.table_name = ?
                   and tc.constraint_type in ('PRIMARY KEY', 'UNIQUE', 'CHECK')
                 order by tc.constraint_type <> 'PRIMARY KEY', tc.constraint_name,
                          k.ordinal_position"
            ),
            params(),
        )
        .await;
    let constraint_rows = match constraint_rows {
        Ok(rows) => rows,
        Err(error) => {
            // Do not hide permission errors, missing tables on newer servers, or MariaDB's
            // real CHECK constraints. Only retry the known missing catalog on old MySQL.
            let missing_checks = matches!(
                &error,
                DriverError::Database { message, .. }
                    if message.to_ascii_lowercase().contains("check_constraints")
                        && message.to_ascii_lowercase().contains("unknown table")
            );
            if !missing_checks || !matches!(flavor, Flavor::Mysql) {
                return Err(error);
            }
            let version_rows = conn.rows("select version()", vec![]).await?;
            let version = version_rows
                .first()
                .map(|row| text(row, 0))
                .unwrap_or_default();
            if !mysql_without_checks(&version) {
                return Err(error);
            }
            conn.rows(
                "select tc.constraint_name, tc.constraint_type, k.column_name, null
                 from information_schema.table_constraints tc
                 left join information_schema.key_column_usage k
                   on k.constraint_schema = tc.constraint_schema and k.table_name = tc.table_name
                  and k.constraint_name = tc.constraint_name
                 where tc.table_schema = ? and tc.table_name = ?
                   and tc.constraint_type in ('PRIMARY KEY', 'UNIQUE')
                 order by tc.constraint_type <> 'PRIMARY KEY', tc.constraint_name,
                          k.ordinal_position",
                params(),
            )
            .await?
        }
    };
    let mut constraints: Vec<ConstraintInfo> = Vec::new();
    for row in &constraint_rows {
        let constraint = text(row, 0);
        let kind = match text(row, 1).as_str() {
            "PRIMARY KEY" => ConstraintKind::PrimaryKey,
            "UNIQUE" => ConstraintKind::Unique,
            _ => ConstraintKind::Check,
        };
        match constraints.last_mut() {
            Some(c) if c.name.as_deref() == Some(constraint.as_str()) && c.kind == kind => {
                c.columns.extend(opt(row, 2));
            }
            _ => constraints.push(ConstraintInfo {
                name: Some(constraint),
                kind,
                columns: opt(row, 2).into_iter().collect(),
                definition: opt(row, 3)
                    .map(|clause| spelaion_driver_api::check_definition(&clause))
                    .unwrap_or_default(),
            }),
        }
    }
    for c in &mut constraints {
        let list = c
            .columns
            .iter()
            .map(|col| quote(col))
            .collect::<Vec<_>>()
            .join(", ");
        match c.kind {
            ConstraintKind::PrimaryKey => c.definition = format!("PRIMARY KEY ({list})"),
            ConstraintKind::Unique => c.definition = format!("UNIQUE ({list})"),
            ConstraintKind::Check => {}
        }
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

/// The foreign keys pointing to `schema.name`, from any database of the server.
pub async fn list_referencing_keys(
    conn: &MysqlConnection,
    schema: &str,
    name: &str,
) -> Result<Vec<ReferencingKey>> {
    let rows = conn
        .rows(
            "select table_schema, table_name, constraint_name, column_name, referenced_column_name
             from information_schema.key_column_usage
             where referenced_table_schema = ? and referenced_table_name = ?
             order by table_schema, table_name, constraint_name, ordinal_position",
            vec![schema.to_string(), name.to_string()],
        )
        .await?;
    Ok(group_referencing_keys(&rows))
}

/// Rows of (schema, table, key, column, referenced column), ordered by key and position, as
/// one [`ReferencingKey`] per key.
fn group_referencing_keys(rows: &[Vec<Option<String>>]) -> Vec<ReferencingKey> {
    let mut keys: Vec<ReferencingKey> = Vec::new();
    for row in rows {
        let (schema, table, name) = (text(row, 0), text(row, 1), text(row, 2));
        match keys.last_mut() {
            Some(key) if key.schema == schema && key.table == table && key.name == name => {
                key.columns.push(text(row, 3));
                key.referenced_columns.push(text(row, 4));
            }
            _ => keys.push(ReferencingKey {
                schema,
                table,
                name,
                columns: vec![text(row, 3)],
                referenced_columns: vec![text(row, 4)],
            }),
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::mysql_without_checks;

    #[test]
    fn old_mysql_has_no_check_constraint_catalog() {
        for version in ["5.7.44", "5.6.51-log", "8.0.15-commercial"] {
            assert!(mysql_without_checks(version), "{version}");
        }
        for version in [
            "8.0.16",
            "8.4.0",
            "9.0.0",
            "5.5.5-10.11.6-MariaDB",
            "5.7",
            "unknown",
        ] {
            assert!(!mysql_without_checks(version), "{version}");
        }
    }
}
