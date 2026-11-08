//! PostgreSQL catalog queries for the explorer, autocompletion and the MCP server.

use spelaion_driver_api::{
    ColumnInfo, ConstraintInfo, ConstraintKind, DriverError, ForeignKeyInfo, IndexInfo, LikeFilter,
    ObjectInfo, ObjectKind, ReferencingKey, Result, SchemaInfo, TableInfo,
};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row};

use crate::map_error;

/// `like` as SQL over `column`, its patterns appended to `params` as `$n` after `before`
/// other parameters (ADR-025).
pub(crate) fn like_sql(
    like: &LikeFilter,
    column: &str,
    params: &mut Vec<String>,
    before: usize,
) -> String {
    like.sql(column, |pattern| {
        params.push(pattern.to_string());
        format!("${}", before + params.len())
    })
}

/// Parameters for `client.query`: the fixed ones first, then the patterns.
pub(crate) fn query_params<'a>(
    fixed: &'a [&'a str],
    patterns: &'a [String],
) -> Vec<&'a (dyn ToSql + Sync)> {
    fixed
        .iter()
        .map(|p| p as &(dyn ToSql + Sync))
        .chain(patterns.iter().map(|p| p as &(dyn ToSql + Sync)))
        .collect()
}

pub async fn list_schemas(client: &Client, like: &LikeFilter) -> Result<Vec<SchemaInfo>> {
    let mut patterns = Vec::new();
    let narrow = like_sql(like, "nspname::text", &mut patterns, 0);
    let sql = format!(
        r"select nspname::text,
                 nspname in ('pg_catalog', 'information_schema') or nspname like 'pg\_%' as system
          from pg_namespace
          where nspname not like 'pg\_toast%' and nspname not like 'pg\_temp\_%'{narrow}
          order by system, nspname"
    );
    let rows = client
        .query(&sql, &query_params(&[], &patterns))
        .await
        .map_err(map_error)?;
    Ok(rows
        .iter()
        .map(|r| SchemaInfo {
            name: r.get(0),
            system: r.get(1),
        })
        .collect())
}

const OBJECTS_SQL: &str = r"
    select n.nspname::text, c.relname::text, c.relkind::text,
           case when c.reltuples < 0 then null else c.reltuples::int8 end,
           obj_description(c.oid, 'pg_class')
    from pg_class c
    join pg_namespace n on n.oid = c.relnamespace
    where c.relkind in ('r', 'p', 'v', 'm', 'f')";

fn object_from_row(r: &Row) -> Result<ObjectInfo> {
    let kind: String = r.get(2);
    Ok(ObjectInfo {
        schema: r.get(0),
        name: r.get(1),
        kind: match kind.as_str() {
            "r" => ObjectKind::Table,
            "p" => ObjectKind::PartitionedTable,
            "v" => ObjectKind::View,
            "m" => ObjectKind::MaterializedView,
            "f" => ObjectKind::ForeignTable,
            other => return Err(DriverError::Other(format!("unexpected relkind: {other}"))),
        },
        estimated_rows: r.get(3),
        comment: r.get(4),
    })
}

pub async fn list_objects(
    client: &Client,
    schema: &str,
    like: &LikeFilter,
) -> Result<Vec<ObjectInfo>> {
    let mut patterns = Vec::new();
    let narrow = like_sql(like, "c.relname::text", &mut patterns, 1);
    // Partitions are skipped: they will appear under their parent table.
    let sql = format!(
        "{OBJECTS_SQL} and n.nspname = $1 and not c.relispartition{narrow} order by c.relname"
    );
    let rows = client
        .query(&sql, &query_params(&[schema], &patterns))
        .await
        .map_err(map_error)?;
    rows.iter().map(object_from_row).collect()
}

pub async fn describe_table(client: &Client, schema: &str, name: &str) -> Result<TableInfo> {
    let sql = format!("{OBJECTS_SQL} and n.nspname = $1 and c.relname = $2");
    let row = client
        .query_opt(&sql, &[&schema, &name])
        .await
        .map_err(map_error)?
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;
    let object = object_from_row(&row)?;

    let oid: u32 = client
        .query_one(
            "select c.oid from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where n.nspname = $1 and c.relname = $2",
            &[&schema, &name],
        )
        .await
        .map_err(map_error)?
        .get(0);

    let columns = client
        .query(
            "select a.attname::text, format_type(a.atttypid, a.atttypmod), not a.attnotnull,
                    pg_get_expr(d.adbin, d.adrelid), col_description(a.attrelid, a.attnum)
             from pg_attribute a
             left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
             where a.attrelid = $1 and a.attnum > 0 and not a.attisdropped
             order by a.attnum",
            &[&oid],
        )
        .await
        .map_err(map_error)?
        .iter()
        .map(|r| ColumnInfo {
            name: r.get(0),
            native_type: r.get(1),
            nullable: r.get(2),
            default: r.get(3),
            comment: r.get(4),
        })
        .collect();

    let primary_key = client
        .query(
            "select a.attname::text
             from pg_index i
             cross join lateral unnest(i.indkey) with ordinality as k(attnum, ord)
             join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
             where i.indrelid = $1 and i.indisprimary
             order by k.ord",
            &[&oid],
        )
        .await
        .map_err(map_error)?
        .iter()
        .map(|r| r.get(0))
        .collect();

    let indexes = client
        .query(
            "select ic.relname::text, i.indisunique, i.indisprimary, pg_get_indexdef(i.indexrelid)
             from pg_index i join pg_class ic on ic.oid = i.indexrelid
             where i.indrelid = $1
             order by ic.relname",
            &[&oid],
        )
        .await
        .map_err(map_error)?
        .iter()
        .map(|r| IndexInfo {
            name: r.get(0),
            unique: r.get(1),
            primary: r.get(2),
            definition: r.get(3),
        })
        .collect();

    let foreign_keys = client
        .query(
            "select con.conname::text,
                    array(select a.attname::text from unnest(con.conkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n order by k.o),
                    rn.nspname::text, rc.relname::text,
                    array(select a.attname::text from unnest(con.confkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n order by k.o)
             from pg_constraint con
             join pg_class rc on rc.oid = con.confrelid
             join pg_namespace rn on rn.oid = rc.relnamespace
             where con.conrelid = $1 and con.contype = 'f'
             order by con.conname",
            &[&oid],
        )
        .await
        .map_err(map_error)?
        .iter()
        .map(|r| ForeignKeyInfo {
            name: r.get(0),
            columns: r.get(1),
            referenced_schema: r.get(2),
            referenced_table: r.get(3),
            referenced_columns: r.get(4),
        })
        .collect();

    let constraints = client
        .query(
            "select c.conname::text, c.contype::text, pg_get_constraintdef(c.oid, true),
                    array(select a.attname::text
                          from unnest(c.conkey) with ordinality as k(attnum, ord)
                          join pg_attribute a on a.attrelid = c.conrelid and a.attnum = k.attnum
                          order by k.ord)
             from pg_constraint c
             where c.conrelid = $1 and c.contype in ('p', 'u', 'c')
             order by c.contype <> 'p', c.conname",
            &[&oid],
        )
        .await
        .map_err(map_error)?
        .iter()
        .map(|r| {
            let kind: String = r.get(1);
            let kind = match kind.as_str() {
                "p" => ConstraintKind::PrimaryKey,
                "u" => ConstraintKind::Unique,
                _ => ConstraintKind::Check,
            };
            ConstraintInfo {
                name: Some(r.get(0)),
                kind,
                columns: if kind == ConstraintKind::Check {
                    Vec::new()
                } else {
                    r.get(3)
                },
                definition: r.get(2),
            }
        })
        .collect();

    Ok(TableInfo {
        object,
        columns,
        primary_key,
        indexes,
        foreign_keys,
        constraints,
    })
}

/// The foreign keys pointing to `schema.name`. Those of partitions are left out: they are
/// copies of their parent table's key, which is listed.
pub async fn list_referencing_keys(
    client: &Client,
    schema: &str,
    name: &str,
) -> Result<Vec<ReferencingKey>> {
    let rows = client
        .query(
            "select n.nspname::text, c.relname::text, con.conname::text,
                    array(select a.attname::text from unnest(con.conkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n order by k.o),
                    array(select a.attname::text from unnest(con.confkey) with ordinality k(n, o)
                          join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n order by k.o)
             from pg_constraint con
             join pg_class c on c.oid = con.conrelid
             join pg_namespace n on n.oid = c.relnamespace
             where con.contype = 'f' and not c.relispartition
               and con.confrelid = (select t.oid from pg_class t
                                    join pg_namespace tn on tn.oid = t.relnamespace
                                    where tn.nspname = $1 and t.relname = $2)
             order by 1, 2, 3",
            &[&schema, &name],
        )
        .await
        .map_err(map_error)?;
    Ok(rows
        .iter()
        .map(|r| ReferencingKey {
            schema: r.get(0),
            table: r.get(1),
            name: r.get(2),
            columns: r.get(3),
            referenced_columns: r.get(4),
        })
        .collect())
}
