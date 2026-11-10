//! A table's size on disk, its indexes' size and its row estimate, from the catalog's own
//! statistics (nothing is counted or scanned). Every statement is a read checked by
//! [`crate::observe::read`]; the names are literals written by [`crate::export::sql_literal`].

use spelaion_driver_api::Value;

use crate::Session;
use crate::export::sql_literal;
use crate::observe::{read, whole_number};

/// Sizes in bytes. Any of them may be unknown: DuckDB only estimates rows, and a table never
/// analysed has no row estimate in PostgreSQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TableStats {
    /// The table, its indexes and its out-of-line data (TOAST, LOB pages).
    pub total_bytes: Option<i64>,
    /// The table without its indexes.
    pub table_bytes: Option<i64>,
    pub index_bytes: Option<i64>,
    /// The server's estimate of the number of rows (not a count).
    pub row_estimate: Option<i64>,
}

/// The statement for one table, or `None` when the engine keeps no such statistics (SQLite).
pub(crate) fn table_stats_sql(engine: &str, schema: &str, table: &str) -> Option<String> {
    let text = |value: &str| sql_literal(&Value::Text(value.to_string()), engine);
    let (schema, table) = (text(schema), text(table));
    match engine {
        // pg_partition_tree lists a partitioned table and its partitions (PostgreSQL 12+), and
        // nothing for any other table: a partitioned table's size is its partitions'.
        "postgres" => Some(format!(
            "SELECT SUM(pg_catalog.pg_total_relation_size(p.oid))::bigint AS total_bytes, \
             SUM(pg_catalog.pg_table_size(p.oid))::bigint AS table_bytes, \
             SUM(pg_catalog.pg_indexes_size(p.oid))::bigint AS index_bytes, \
             SUM(CASE WHEN p.reltuples < 0 THEN NULL ELSE p.reltuples END)::bigint AS row_estimate \
             FROM pg_catalog.pg_class AS c \
             JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace \
             LEFT JOIN LATERAL pg_catalog.pg_partition_tree(c.oid) AS t ON TRUE \
             JOIN pg_catalog.pg_class AS p ON p.oid = COALESCE(t.relid, c.oid) \
             WHERE n.nspname = {schema} AND c.relname = {table} AND c.relkind IN ('r', 'p', 'm') \
             GROUP BY c.oid"
        )),
        "mysql" => Some(format!(
            "SELECT DATA_LENGTH + INDEX_LENGTH AS total_bytes, DATA_LENGTH AS table_bytes, \
             INDEX_LENGTH AS index_bytes, TABLE_ROWS AS row_estimate \
             FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = {schema} AND TABLE_NAME = {table} AND TABLE_TYPE = 'BASE TABLE'"
        )),
        // Allocation units hang from a partition's hobt (in-row and row-overflow pages) or from
        // the partition itself (LOB pages). Heap and clustered index (index_id 0 and 1) are the
        // table; the rest are its indexes.
        "sqlserver" => Some(format!(
            "SELECT CONVERT(bigint, SUM(a.total_pages)) * 8192 AS total_bytes, \
             CONVERT(bigint, SUM(CASE WHEN p.index_id IN (0, 1) THEN a.total_pages ELSE 0 END)) * 8192 AS table_bytes, \
             CONVERT(bigint, SUM(CASE WHEN p.index_id > 1 THEN a.total_pages ELSE 0 END)) * 8192 AS index_bytes, \
             (SELECT CONVERT(bigint, SUM(r.rows)) FROM sys.partitions AS r WHERE r.object_id = o.object_id AND r.index_id IN (0, 1)) AS row_estimate \
             FROM sys.objects AS o \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id \
             JOIN sys.partitions AS p ON p.object_id = o.object_id \
             JOIN sys.allocation_units AS a ON a.container_id = CASE WHEN a.type = 2 THEN p.partition_id ELSE p.hobt_id END \
             WHERE s.name = {schema} AND o.name = {table} AND o.type = 'U' \
             GROUP BY o.object_id"
        )),
        "duckdb" => Some(format!(
            "SELECT CAST(NULL AS BIGINT) AS total_bytes, CAST(NULL AS BIGINT) AS table_bytes, \
             CAST(NULL AS BIGINT) AS index_bytes, CAST(estimated_size AS BIGINT) AS row_estimate \
             FROM duckdb_tables() \
             WHERE database_name = current_database() AND schema_name = {schema} AND table_name = {table}"
        )),
        _ => None,
    }
}

/// The statistics of one table: `None` when the engine keeps none or the object is not a
/// table (a view has no size of its own).
pub async fn table_stats(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<Option<TableStats>, String> {
    let Some(sql) = table_stats_sql(session.dialect().name(), schema, table) else {
        return Ok(None);
    };
    let rows = read(session, &sql).await?;
    let [total, data, index, estimate] = [
        rows.column("total_bytes")?,
        rows.column("table_bytes")?,
        rows.column("index_bytes")?,
        rows.column("row_estimate")?,
    ];
    Ok(rows.rows.first().map(|row| TableStats {
        total_bytes: whole_number(row[total].as_deref()),
        table_bytes: whole_number(row[data].as_deref()),
        index_bytes: whole_number(row[index].as_deref()),
        row_estimate: whole_number(row[estimate].as_deref()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_is_a_read_whatever_the_names() {
        for engine in ["postgres", "mysql", "sqlserver", "duckdb"] {
            for (schema, table) in [
                ("public", "orders"),
                ("it's", "x'; DROP TABLE t; --"),
                ("a\\", "' OR 1=1 --"),
                ("ünï", "名前"),
            ] {
                let sql = table_stats_sql(engine, schema, table).unwrap();
                crate::classify_read_only(engine, &sql)
                    .unwrap_or_else(|e| panic!("{engine}: {e}\n{sql}"));
            }
        }
        assert!(table_stats_sql("sqlite", "main", "t").is_none());
    }
}
