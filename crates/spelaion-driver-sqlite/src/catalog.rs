//! The SQLite catalog queries (`sqlite_schema` and the `pragma_*` table-valued functions)
//! and how their rows become the common model. Shared by the local driver (rusqlite) and by
//! SQLite on a server (`sqlite3` over SSH, ADR-018): names go into the SQL as literals, so
//! the same text works for both.

use rusqlite::types::Value as SqlValue;
use spelaion_driver_api::{
    ColumnInfo, Dialect, DriverError, ForeignKeyInfo, IndexInfo, ObjectInfo, ObjectKind, Result,
    SchemaInfo, SchemaObject, SchemaObjectKind, TableInfo, terminated,
};

use crate::dialect::SqliteDialect;

pub(crate) type Row = Vec<SqlValue>;

/// A string literal (`'it''s'`).
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn text(row: &Row, i: usize) -> String {
    match row.get(i) {
        Some(SqlValue::Text(s)) => s.clone(),
        Some(SqlValue::Integer(n)) => n.to_string(),
        Some(SqlValue::Real(x)) => x.to_string(),
        _ => String::new(),
    }
}

fn optional_text(row: &Row, i: usize) -> Option<String> {
    match row.get(i) {
        None | Some(SqlValue::Null) => None,
        Some(_) => Some(text(row, i)),
    }
}

fn integer(row: &Row, i: usize) -> i64 {
    match row.get(i) {
        Some(SqlValue::Integer(n)) => *n,
        Some(SqlValue::Text(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// `sqlite_schema` of a given schema (main, temp or an attached database).
fn schema_table(schema: &str) -> String {
    format!("{}.sqlite_schema", SqliteDialect.quote_identifier(schema))
}

pub(crate) const SCHEMAS_SQL: &str = "select name from pragma_database_list order by seq";

pub(crate) fn schemas(rows: &[Row]) -> Vec<SchemaInfo> {
    rows.iter()
        .map(|row| SchemaInfo {
            name: text(row, 0),
            system: false,
        })
        .collect()
}

pub(crate) fn objects_sql(schema: &str) -> String {
    format!(
        r"select name, type from {} where type in ('table', 'view') and name not like 'sqlite\_%' escape '\' order by name",
        schema_table(schema)
    )
}

pub(crate) fn objects(schema: &str, rows: &[Row]) -> Vec<ObjectInfo> {
    rows.iter()
        .map(|row| ObjectInfo {
            schema: schema.to_string(),
            name: text(row, 0),
            kind: if text(row, 1) == "view" {
                ObjectKind::View
            } else {
                ObjectKind::Table
            },
            // SQLite keeps no cheap estimates (only with ANALYZE, in sqlite_stat1).
            estimated_rows: None,
            comment: None,
        })
        .collect()
}

/// Triggers are all a SQLite schema holds besides its tables and views (ADR-024).
pub(crate) fn schema_objects_sql(schema: &str) -> String {
    format!(
        "select name, tbl_name from {} where type = 'trigger' order by name",
        schema_table(schema)
    )
}

pub(crate) fn schema_objects(schema: &str, rows: &[Row]) -> Vec<SchemaObject> {
    rows.iter()
        .map(|row| SchemaObject {
            schema: schema.to_string(),
            name: text(row, 0),
            kind: SchemaObjectKind::Trigger,
            detail: optional_text(row, 1),
            comment: None,
        })
        .collect()
}

/// The `CREATE TRIGGER` SQLite keeps, or an error for kinds SQLite does not have.
pub(crate) fn definition_sql(object: &SchemaObject) -> Result<String> {
    match object.kind {
        SchemaObjectKind::Trigger => Ok(format!(
            "select sql from {} where type = 'trigger' and name = {}",
            schema_table(&object.schema),
            literal(&object.name)
        )),
        SchemaObjectKind::Function
        | SchemaObjectKind::Procedure
        | SchemaObjectKind::Aggregate
        | SchemaObjectKind::Sequence
        | SchemaObjectKind::Enum
        | SchemaObjectKind::Domain
        | SchemaObjectKind::Type
        | SchemaObjectKind::Event
        | SchemaObjectKind::Synonym => Err(DriverError::Other(format!(
            "SQLite has no {:?} objects",
            object.kind
        ))),
    }
}

pub(crate) fn definition(object: &SchemaObject, rows: &[Row]) -> Result<String> {
    rows.first()
        .and_then(|row| optional_text(row, 0))
        .map(|sql| terminated(&sql))
        .ok_or_else(|| {
            DriverError::Other(format!("{}.{} does not exist", object.schema, object.name))
        })
}

/// hidden = 1: hidden columns of virtual tables; 2 and 3: generated (shown).
pub(crate) fn columns_sql(schema: &str, name: &str) -> String {
    format!(
        r#"select name, type, "notnull", dflt_value, pk from pragma_table_xinfo({}, {}) where hidden <> 1 order by cid"#,
        literal(name),
        literal(schema)
    )
}

/// Automatic indexes (PRIMARY KEY, UNIQUE) have no SQL of their own: `sql` is NULL.
pub(crate) fn indexes_sql(schema: &str, name: &str) -> String {
    format!(
        r#"select il.name, il."unique", il.origin, s.sql from pragma_index_list({}, {}) il left join {} s on s.type = 'index' and s.name = il.name order by il.name"#,
        literal(name),
        literal(schema),
        schema_table(schema)
    )
}

pub(crate) fn foreign_keys_sql(schema: &str, name: &str) -> String {
    format!(
        r#"select id, "table", "from", "to" from pragma_foreign_key_list({}, {}) order by id, seq"#,
        literal(name),
        literal(schema)
    )
}

pub(crate) fn primary_key_sql(schema: &str, table: &str) -> String {
    format!(
        "select name from pragma_table_info({}, {}) where pk > 0 order by pk",
        literal(table),
        literal(schema)
    )
}

/// The first column of every row.
pub(crate) fn names(rows: &[Row]) -> Vec<String> {
    rows.iter().map(|row| text(row, 0)).collect()
}

/// The table from its catalog rows. The returned positions are the foreign keys whose
/// referenced columns are "the primary key of the referenced table" (an empty `to`): the
/// caller fills them with [`primary_key_sql`].
pub(crate) fn table(
    object: ObjectInfo,
    column_rows: &[Row],
    index_rows: &[Row],
    foreign_key_rows: &[Row],
) -> (TableInfo, Vec<usize>) {
    let schema = object.schema.clone();
    let mut pk: Vec<(i64, String)> = column_rows
        .iter()
        .filter(|row| integer(row, 4) > 0)
        .map(|row| (integer(row, 4), text(row, 0)))
        .collect();
    pk.sort();
    let columns = column_rows
        .iter()
        .map(|row| ColumnInfo {
            name: text(row, 0),
            native_type: text(row, 1),
            nullable: integer(row, 2) == 0,
            default: optional_text(row, 3),
            comment: None,
        })
        .collect();

    let indexes = index_rows
        .iter()
        .map(|row| {
            let origin = text(row, 2);
            IndexInfo {
                name: text(row, 0),
                unique: integer(row, 1) != 0,
                primary: origin == "pk",
                definition: optional_text(row, 3).unwrap_or_else(|| {
                    format!(
                        "-- automatic index for {}",
                        if origin == "pk" {
                            "PRIMARY KEY"
                        } else {
                            "UNIQUE"
                        }
                    )
                }),
            }
        })
        .collect();

    let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
    for row in foreign_key_rows {
        let name = format!("fk_{}", integer(row, 0));
        if foreign_keys.last().is_none_or(|fk| fk.name != name) {
            foreign_keys.push(ForeignKeyInfo {
                // SQLite does not expose the constraint name in this pragma.
                name,
                columns: Vec::new(),
                referenced_schema: schema.clone(),
                referenced_table: text(row, 1),
                referenced_columns: Vec::new(),
            });
        }
        let fk = foreign_keys.last_mut().expect("just added");
        fk.columns.push(text(row, 2));
        if let Some(to) = optional_text(row, 3) {
            fk.referenced_columns.push(to);
        }
    }
    let to_primary_keys = foreign_keys
        .iter()
        .enumerate()
        .filter(|(_, fk)| fk.referenced_columns.is_empty())
        .map(|(i, _)| i)
        .collect();

    (
        TableInfo {
            object,
            columns,
            primary_key: pk.into_iter().map(|(_, n)| n).collect(),
            indexes,
            foreign_keys,
        },
        to_primary_keys,
    )
}
