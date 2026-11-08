//! Schemas, tables and columns from SQL Server's catalog views (`sys.*`) of the database the
//! connection is in. Lists are grouped here rather than with `STRING_AGG`, which SQL Server
//! only has from 2017.

use spelaion_driver_api::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DriverError, ForeignKeyInfo, IndexInfo, LikeFilter,
    ObjectInfo, ObjectKind, ReferencingKey, Result, SchemaInfo, TableInfo,
};

use crate::SqlServerConnection;

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn flag(row: &[Option<String>], i: usize) -> bool {
    row.get(i).cloned().flatten().is_some_and(|v| v == "1")
}

/// `like` as SQL over `column`, its patterns appended to `params` as `@Pn` (ADR-025).
pub(crate) fn like_sql(like: &LikeFilter, column: &str, params: &mut Vec<String>) -> String {
    like.sql(column, |pattern| {
        params.push(pattern.to_string());
        format!("@P{}", params.len())
    })
}

pub async fn list_schemas(
    conn: &SqlServerConnection,
    like: &LikeFilter,
) -> Result<Vec<SchemaInfo>> {
    let mut params = Vec::new();
    let narrow = like_sql(like, "name", &mut params);
    // The `db_*` role schemas (16384…) and `guest` hold nothing of the user's.
    let rows = conn
        .rows(
            &format!(
                "select name, case when name in ('sys', 'INFORMATION_SCHEMA', 'guest') \
                         or schema_id >= 16384 then 1 else 0 end \
                 from sys.schemas where 1 = 1{narrow} order by 2, name"
            ),
            params,
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| SchemaInfo {
            name: text(row, 0),
            system: flag(row, 1),
        })
        .collect())
}

async fn objects(
    conn: &SqlServerConnection,
    schema: &str,
    name: Option<&str>,
    like: &LikeFilter,
) -> Result<Vec<ObjectInfo>> {
    let mut params = vec![schema.to_string(), name.unwrap_or("").to_string()];
    let narrow = like_sql(like, "o.name", &mut params);
    let rows = conn
        .rows(
            &format!(
                "select o.name, case o.type when 'V' then 1 else 0 end, \
                    (select sum(p.rows) from sys.partitions p \
                      where p.object_id = o.object_id and p.index_id in (0, 1)), \
                    cast(ep.value as nvarchar(4000)) \
             from sys.objects o \
             join sys.schemas s on s.schema_id = o.schema_id \
             left join sys.extended_properties ep \
               on ep.class = 1 and ep.major_id = o.object_id and ep.minor_id = 0 \
              and ep.name = 'MS_Description' \
             where s.name = @P1 and o.type in ('U', 'V') and o.is_ms_shipped = 0 \
               and (@P2 = '' or o.name = @P2){narrow} \
             order by o.name"
            ),
            params,
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| ObjectInfo {
            schema: schema.to_string(),
            name: text(row, 0),
            kind: if flag(row, 1) {
                ObjectKind::View
            } else {
                ObjectKind::Table
            },
            estimated_rows: row.get(2).cloned().flatten().and_then(|n| n.parse().ok()),
            comment: row.get(3).cloned().flatten().filter(|c| !c.is_empty()),
        })
        .collect())
}

pub async fn list_objects(
    conn: &SqlServerConnection,
    schema: &str,
    like: &LikeFilter,
) -> Result<Vec<ObjectInfo>> {
    objects(conn, schema, None, like).await
}

/// The type as written in `CREATE TABLE`: `nvarchar(80)`, `varchar(max)`, `decimal(12,2)`.
pub(crate) fn native_type(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    match name {
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!("{name}({max_length})")
            }
        }
        // Lengths of the national types count bytes: two per character.
        "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!("{name}({})", max_length / 2)
            }
        }
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{name}({scale})"),
        _ => name.to_string(),
    }
}

pub async fn describe_table(
    conn: &SqlServerConnection,
    schema: &str,
    name: &str,
) -> Result<TableInfo> {
    let object = objects(conn, schema, Some(name), &LikeFilter::default())
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;
    let qualified = vec![schema.to_string(), name.to_string()];

    let rows = conn
        .rows(
            "select c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                    dc.definition, cast(ep.value as nvarchar(4000)) \
             from sys.columns c \
             join sys.types t on t.user_type_id = c.user_type_id \
             left join sys.default_constraints dc on dc.object_id = c.default_object_id \
             left join sys.extended_properties ep \
               on ep.class = 1 and ep.major_id = c.object_id and ep.minor_id = c.column_id \
              and ep.name = 'MS_Description' \
             where c.object_id = object_id(quotename(@P1) + '.' + quotename(@P2)) \
             order by c.column_id",
            qualified.clone(),
        )
        .await?;
    let number = |row: &[Option<String>], i| text(row, i).parse::<i64>().unwrap_or(0);
    let columns = rows
        .iter()
        .map(|row| ColumnInfo {
            name: text(row, 0),
            native_type: native_type(
                &text(row, 1),
                number(row, 2),
                number(row, 3),
                number(row, 4),
            ),
            nullable: flag(row, 5),
            default: row.get(6).cloned().flatten(),
            comment: row.get(7).cloned().flatten().filter(|c| !c.is_empty()),
        })
        .collect();

    // One row per index column, in order; grouped by index.
    let rows = conn
        .rows(
            "select i.name, i.is_unique, i.is_primary_key, i.type_desc, c.name, ic.is_descending_key \
             from sys.indexes i \
             join sys.index_columns ic on ic.object_id = i.object_id and ic.index_id = i.index_id \
             join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
             where i.object_id = object_id(quotename(@P1) + '.' + quotename(@P2)) \
               and i.name is not null and ic.is_included_column = 0 \
             order by i.is_primary_key desc, i.name, ic.key_ordinal",
            qualified.clone(),
        )
        .await?;
    let mut indexes: Vec<(IndexInfo, String, Vec<String>)> = Vec::new();
    let mut primary_key = Vec::new();
    for row in &rows {
        let index_name = text(row, 0);
        let column = text(row, 4);
        let column_sql = if flag(row, 5) {
            format!("[{column}] DESC")
        } else {
            format!("[{column}]")
        };
        if flag(row, 2) {
            primary_key.push(column.clone());
        }
        match indexes.last_mut() {
            Some((index, _, cols)) if index.name == index_name => cols.push(column_sql),
            _ => indexes.push((
                IndexInfo {
                    name: index_name,
                    unique: flag(row, 1),
                    primary: flag(row, 2),
                    definition: String::new(),
                },
                text(row, 3),
                vec![column_sql],
            )),
        }
    }
    let indexes = indexes
        .into_iter()
        .map(|(mut index, kind, cols)| {
            index.definition = if index.primary {
                format!("PRIMARY KEY {kind} ({})", cols.join(", "))
            } else {
                format!(
                    "CREATE {}{kind} INDEX [{}] ON [{schema}].[{name}] ({})",
                    if index.unique { "UNIQUE " } else { "" },
                    index.name,
                    cols.join(", ")
                )
            };
            index
        })
        .collect();

    let rows = conn
        .rows(
            "select fk.name, pc.name, schema_name(rt.schema_id), rt.name, rc.name \
             from sys.foreign_keys fk \
             join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
             join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id \
             join sys.tables rt on rt.object_id = fkc.referenced_object_id \
             join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id \
             where fk.parent_object_id = object_id(quotename(@P1) + '.' + quotename(@P2)) \
             order by fk.name, fkc.constraint_column_id",
            qualified.clone(),
        )
        .await?;
    let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
    for row in &rows {
        let fk_name = text(row, 0);
        match foreign_keys.last_mut() {
            Some(fk) if fk.name == fk_name => {
                fk.columns.push(text(row, 1));
                fk.referenced_columns.push(text(row, 4));
            }
            _ => foreign_keys.push(ForeignKeyInfo {
                name: fk_name,
                columns: vec![text(row, 1)],
                referenced_schema: text(row, 2),
                referenced_table: text(row, 3),
                referenced_columns: vec![text(row, 4)],
            }),
        }
    }

    // Primary key and unique constraints (one row per column), then checks.
    let rows = conn
        .rows(
            "select kc.name, kc.type, c.name, null, case kc.type when 'PK' then 0 else 1 end \
             from sys.key_constraints kc \
             join sys.index_columns ic \
               on ic.object_id = kc.parent_object_id and ic.index_id = kc.unique_index_id \
             join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
             where kc.parent_object_id = object_id(quotename(@P1) + '.' + quotename(@P2)) \
               and ic.is_included_column = 0 \
             union all \
             select cc.name, 'C', null, cc.definition, 2 from sys.check_constraints cc \
             where cc.parent_object_id = object_id(quotename(@P1) + '.' + quotename(@P2)) \
             order by 5, 1",
            qualified,
        )
        .await?;
    let mut constraints: Vec<ConstraintInfo> = Vec::new();
    for row in &rows {
        let constraint = text(row, 0);
        let kind = match text(row, 1).trim() {
            "PK" => ConstraintKind::PrimaryKey,
            "UQ" => ConstraintKind::Unique,
            _ => ConstraintKind::Check,
        };
        match constraints.last_mut() {
            Some(c)
                if c.name.as_deref() == Some(constraint.as_str())
                    && c.kind != ConstraintKind::Check =>
            {
                c.columns.push(text(row, 2));
            }
            _ => constraints.push(ConstraintInfo {
                name: Some(constraint),
                kind,
                columns: if kind == ConstraintKind::Check {
                    Vec::new()
                } else {
                    vec![text(row, 2)]
                },
                definition: if kind == ConstraintKind::Check {
                    spelaion_driver_api::check_definition(&text(row, 3))
                } else {
                    String::new()
                },
            }),
        }
    }
    for c in &mut constraints {
        let list = c
            .columns
            .iter()
            .map(|col| format!("[{col}]"))
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

/// The foreign keys pointing to `schema.name`, from any schema of the database.
pub async fn list_referencing_keys(
    conn: &SqlServerConnection,
    schema: &str,
    name: &str,
) -> Result<Vec<ReferencingKey>> {
    let rows = conn
        .rows(
            "select schema_name(pt.schema_id), pt.name, fk.name, pc.name, rc.name              from sys.foreign_keys fk              join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id              join sys.tables pt on pt.object_id = fkc.parent_object_id              join sys.columns pc on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id              join sys.columns rc on rc.object_id = fkc.referenced_object_id and rc.column_id = fkc.referenced_column_id              where fk.referenced_object_id = object_id(quotename(@P1) + '.' + quotename(@P2))              order by 1, 2, 3, fkc.constraint_column_id",
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
    use super::*;

    #[test]
    fn types_read_like_create_table() {
        assert_eq!(native_type("nvarchar", 160, 0, 0), "nvarchar(80)");
        assert_eq!(native_type("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(native_type("decimal", 9, 12, 2), "decimal(12,2)");
        assert_eq!(native_type("datetime2", 8, 27, 7), "datetime2(7)");
        assert_eq!(native_type("int", 4, 10, 0), "int");
    }
}
