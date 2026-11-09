//! The SQLite catalog queries (`sqlite_schema` and the `pragma_*` table-valued functions)
//! and how their rows become the common model. Shared by the local driver (rusqlite) and by
//! SQLite on a server (`sqlite3` over SSH, ADR-018): names go into the SQL as literals, so
//! the same text works for both.

use rusqlite::types::Value as SqlValue;
use spelaion_driver_api::{
    ColumnInfo, ConstraintInfo, ConstraintKind, Dialect, DriverError, ForeignKeyInfo, IndexInfo,
    LikeFilter, ObjectInfo, ObjectKind, ReferencingKey, Result, SchemaColumn, SchemaInfo,
    SchemaObject, SchemaObjectKind, TableInfo, terminated,
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

/// The schemas (main, temp, attached databases), narrowed by `like` (ADR-025). Patterns go
/// in as literals: the same text runs locally and through a server's `sqlite3`.
pub(crate) fn schemas_sql(like: &LikeFilter) -> String {
    format!(
        "select name from pragma_database_list where true{} order by seq",
        like.sql("name", literal)
    )
}

pub(crate) fn schemas(rows: &[Row]) -> Vec<SchemaInfo> {
    rows.iter()
        .map(|row| SchemaInfo {
            name: text(row, 0),
            system: false,
        })
        .collect()
}

pub(crate) fn objects_sql(schema: &str, like: &LikeFilter) -> String {
    format!(
        r"select name, type from {} where type in ('table', 'view') and name not like 'sqlite\_%' escape '\'{} order by name",
        schema_table(schema),
        like.sql("name", literal)
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
pub(crate) fn schema_objects_sql(schema: &str, like: &LikeFilter) -> String {
    format!(
        "select name, tbl_name from {} where type = 'trigger'{} order by name",
        schema_table(schema),
        like.sql("name", literal)
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

/// The `CREATE TABLE` or `CREATE VIEW` SQLite keeps, then the `CREATE INDEX` of the table's
/// own indexes (automatic ones have no text: they come from its constraints).
pub(crate) fn relation_definition_sql(schema: &str, name: &str) -> String {
    format!(
        "select sql from {table} where name = {name} and type in ('table', 'view') \
         union all \
         select * from (select sql from {table} where tbl_name = {name} and type = 'index' \
                        and sql is not null order by name)",
        table = schema_table(schema),
        name = literal(name)
    )
}

pub(crate) fn relation_definition(schema: &str, name: &str, rows: &[Row]) -> Result<String> {
    if rows.is_empty() {
        return Err(DriverError::Other(format!(
            "{schema}.{name} does not exist"
        )));
    }
    Ok(rows
        .iter()
        .filter_map(|row| optional_text(row, 0))
        .map(|sql| terminated(&sql))
        .collect::<Vec<_>>()
        .join("\n\n"))
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

/// Every column of the schema's tables and views (those of `objects_sql`), with the same
/// columns as `columns_sql` shows.
pub(crate) fn schema_columns_sql(schema: &str) -> String {
    format!(
        r"select m.name, c.name, c.type from {} m join pragma_table_xinfo(m.name, {}) c where m.type in ('table', 'view') and m.name not like 'sqlite\_%' escape '\' and c.hidden <> 1 order by m.name, c.cid",
        schema_table(schema),
        literal(schema)
    )
}

pub(crate) fn schema_columns(rows: &[Row]) -> Vec<SchemaColumn> {
    rows.iter()
        .map(|row| SchemaColumn {
            table: text(row, 0),
            name: text(row, 1),
            native_type: text(row, 2),
        })
        .collect()
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

/// The columns of the indexes behind UNIQUE constraints (`origin = 'u'`), in order.
pub(crate) fn unique_columns_sql(schema: &str, name: &str) -> String {
    format!(
        "select il.name, ii.name from pragma_index_list({table}, {schema}) il, \
         pragma_index_info(il.name, {schema}) ii where il.origin = 'u' order by il.name, ii.seqno",
        table = literal(name),
        schema = literal(schema)
    )
}

pub(crate) fn foreign_keys_sql(schema: &str, name: &str) -> String {
    format!(
        r#"select id, "table", "from", "to" from pragma_foreign_key_list({}, {}) order by id, seq"#,
        literal(name),
        literal(schema)
    )
}

/// The foreign keys of every table of `schema` that point to `name` (a key and the table it
/// references always share a database file). The pragma keeps the referenced table's name as
/// the key's DDL wrote it: compared ignoring case, as SQLite resolves names.
pub(crate) fn referencing_keys_sql(schema: &str, name: &str) -> String {
    format!(
        r#"select m.name, p.id, p."from", p."to"
           from {} m, pragma_foreign_key_list(m.name, {}) p
           where m.type = 'table' and p."table" = {} collate nocase
           order by m.name, p.id, p.seq"#,
        schema_table(schema),
        literal(schema),
        literal(name)
    )
}

/// One key per (table, id) of [`referencing_keys_sql`]'s rows. A key written without its
/// referenced columns points to `primary_key`, the referenced table's.
pub(crate) fn referencing_keys(
    schema: &str,
    rows: &[Row],
    primary_key: &[String],
) -> Vec<ReferencingKey> {
    let mut keys: Vec<ReferencingKey> = Vec::new();
    let mut implicit: Vec<bool> = Vec::new();
    for row in rows {
        let (table, name) = (text(row, 0), format!("fk_{}", integer(row, 1)));
        if keys
            .last()
            .is_none_or(|key| key.table != table || key.name != name)
        {
            keys.push(ReferencingKey {
                schema: schema.to_string(),
                table,
                name,
                columns: Vec::new(),
                referenced_columns: Vec::new(),
            });
            implicit.push(false);
        }
        let key = keys.last_mut().expect("just added");
        key.columns.push(text(row, 2));
        match optional_text(row, 3) {
            Some(to) => key.referenced_columns.push(to),
            None => *implicit.last_mut().expect("just added") = true,
        }
    }
    for (key, implicit) in keys.iter_mut().zip(implicit) {
        if implicit {
            key.referenced_columns = primary_key.to_vec();
        }
    }
    keys
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
    unique_rows: &[Row],
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

    let primary_key: Vec<String> = pk.into_iter().map(|(_, n)| n).collect();
    let quoted = |columns: &[String]| {
        columns
            .iter()
            .map(|c| SqliteDialect.quote_identifier(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut constraints = Vec::new();
    if !primary_key.is_empty() {
        constraints.push(ConstraintInfo {
            name: None,
            kind: ConstraintKind::PrimaryKey,
            definition: format!("PRIMARY KEY ({})", quoted(&primary_key)),
            columns: primary_key.clone(),
        });
    }
    for row in unique_rows {
        let index = text(row, 0);
        match constraints.last_mut() {
            Some(ConstraintInfo {
                name: Some(name),
                columns,
                ..
            }) if *name == index => columns.push(text(row, 1)),
            _ => constraints.push(ConstraintInfo {
                name: Some(index),
                kind: ConstraintKind::Unique,
                columns: vec![text(row, 1)],
                definition: String::new(),
            }),
        }
    }
    for c in &mut constraints {
        if c.kind == ConstraintKind::Unique {
            c.definition = format!("UNIQUE ({})", quoted(&c.columns));
        }
    }

    (
        TableInfo {
            object,
            columns,
            primary_key,
            indexes,
            foreign_keys,
            constraints,
        },
        to_primary_keys,
    )
}
