//! Macros, sequences and types for the explorer's folders (ADR-024), from DuckDB's catalog
//! functions. DuckDB keeps no `CREATE` text for them: definitions are composed from the
//! catalog, with names quoted by our dialect.

use duckdb::types::Value;
use duckdb::{Connection, params, params_from_iter};
use spelaion_driver_api::{
    Dialect, DriverError, LikeFilter, Result, SchemaObject, SchemaObjectKind, terminated,
};

use crate::dialect::DuckdbDialect;
use crate::exec::map_error;
use crate::introspect::{current_database, like_sql, locate};

/// A macro's parameters as shown and matched (`a, b`): overloads differ in them.
const PARAMETERS: &str = "array_to_string(parameters, ', ')";

pub fn list_schema_objects(
    conn: &Connection,
    shown: &str,
    like: &LikeFilter,
) -> Result<Vec<SchemaObject>> {
    let (database, schema) = locate(conn, shown)?;
    let mut values = vec![Value::Text(database.clone()), Value::Text(schema.clone())];
    let narrow = like_sql(like, "o.name", &mut values);
    let mut statement = conn
        .prepare(&format!(
            "select * from (select function_name, 'function', {PARAMETERS}, comment \
               from duckdb_functions() \
               where database_name = ?1 and schema_name = ?2 and not internal \
                 and function_type in ('macro', 'table_macro') \
             union all \
             select sequence_name, 'sequence', null, comment from duckdb_sequences() \
               where database_name = ?1 and schema_name = ?2 \
             union all \
             select type_name, case logical_type when 'ENUM' then 'enum' else 'type' end, \
                    case when logical_type <> 'ENUM' then logical_type end, comment \
               from duckdb_types() \
               where database_name = ?1 and schema_name = ?2 and not internal) \
               as o(name, kind, detail, comment) \
             where true{narrow} \
             order by 1, 2, 3"
        ))
        .map_err(map_error)?;
    let rows = statement
        .query_map(params_from_iter(values), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(map_error)?;
    rows.map(|row| {
        let (name, kind, detail, comment) = row.map_err(map_error)?;
        Ok(SchemaObject {
            schema: shown.to_string(),
            name,
            kind: match kind.as_str() {
                "function" => SchemaObjectKind::Function,
                "sequence" => SchemaObjectKind::Sequence,
                "enum" => SchemaObjectKind::Enum,
                "type" => SchemaObjectKind::Type,
                other => return Err(DriverError::Other(format!("unexpected kind {other}"))),
            },
            detail,
            comment: comment.filter(|c| !c.is_empty()),
        })
    })
    .collect()
}

/// `SELECT schema.macro(:x)`, or `SELECT * FROM` a table macro: each parameter a placeholder.
/// DuckDB's functions are macros; it has no procedures.
pub fn procedure_call(conn: &Connection, object: &SchemaObject) -> Result<String> {
    if object.kind != SchemaObjectKind::Function {
        return Err(DriverError::Other(format!(
            "{} is not a function: DuckDB has no procedures",
            object.name
        )));
    }
    let (database, schema) = locate(conn, &object.schema)?;
    let quote = |name: &str| DuckdbDialect.quote_identifier(name);
    let qualified = if database == current_database(conn)? {
        format!("{}.{}", quote(&schema), quote(&object.name))
    } else {
        format!(
            "{}.{}.{}",
            quote(&database),
            quote(&schema),
            quote(&object.name)
        )
    };
    let mut statement = conn
        .prepare(&format!(
            "select function_type, {PARAMETERS} from duckdb_functions() \
             where database_name = ?1 and schema_name = ?2 and function_name = ?3 \
               and {PARAMETERS} = coalesce(?4, '') \
               and function_type in ('macro', 'table_macro')"
        ))
        .map_err(map_error)?;
    let mut rows = statement
        .query(params![
            database,
            schema,
            object.name,
            object.detail.as_deref()
        ])
        .map_err(map_error)?;
    let Some(row) = rows.next().map_err(map_error)? else {
        return Err(DriverError::Other(format!(
            "{}.{} does not exist",
            object.schema, object.name
        )));
    };
    let kind: String = row.get(0).map_err(map_error)?;
    let parameters: String = row.get(1).map_err(map_error)?;
    let arguments: Vec<String> = parameters
        .split(", ")
        .filter(|p| !p.is_empty())
        .enumerate()
        .map(|(i, name)| spelaion_driver_api::parameter_placeholder(name, i + 1))
        .collect();
    let call = format!("{qualified}({})", arguments.join(", "));
    Ok(if kind == "table_macro" {
        format!("SELECT * FROM {call};")
    } else {
        format!("SELECT {call};")
    })
}

/// The `CREATE TABLE` or `CREATE VIEW` DuckDB keeps (`duckdb_tables().sql`), then the
/// table's indexes.
pub fn relation_definition(conn: &Connection, shown: &str, name: &str) -> Result<String> {
    let (database, schema) = locate(conn, shown)?;
    let mut statement = conn
        .prepare(
            "select sql, 0 from duckdb_tables() \
               where database_name = ?1 and schema_name = ?2 and table_name = ?3 \
             union all \
             select sql, 0 from duckdb_views() \
               where database_name = ?1 and schema_name = ?2 and view_name = ?3 \
                 and not internal \
             union all \
             select sql, 1 from duckdb_indexes() \
               where database_name = ?1 and schema_name = ?2 and table_name = ?3 \
                 and sql is not null \
             order by 2",
        )
        .map_err(map_error)?;
    let rows = statement
        .query_map(params![database, schema, name], |r| {
            r.get::<_, Option<String>>(0)
        })
        .map_err(map_error)?;
    let mut statements = Vec::new();
    for row in rows {
        if let Some(sql) = row.map_err(map_error)? {
            statements.push(terminated(&sql));
        }
    }
    if statements.is_empty() {
        return Err(DriverError::Other(format!("{shown}.{name} does not exist")));
    }
    Ok(statements.join("\n\n"))
}

pub fn object_definition(conn: &Connection, object: &SchemaObject) -> Result<String> {
    let (database, schema) = locate(conn, &object.schema)?;
    let quote = |name: &str| DuckdbDialect.quote_identifier(name);
    let qualified = if database == current_database(conn)? {
        format!("{}.{}", quote(&schema), quote(&object.name))
    } else {
        format!(
            "{}.{}.{}",
            quote(&database),
            quote(&schema),
            quote(&object.name)
        )
    };
    let missing =
        || DriverError::Other(format!("{}.{} does not exist", object.schema, object.name));
    let one = |sql: &str, detail: Option<&str>| -> Result<Option<Vec<String>>> {
        let mut statement = conn.prepare(sql).map_err(map_error)?;
        let mut rows = statement
            .query(params![database, schema, object.name, detail])
            .map_err(map_error)?;
        let Some(row) = rows.next().map_err(map_error)? else {
            return Ok(None);
        };
        let width = row.as_ref().column_count();
        (0..width)
            .map(|i| row.get::<_, String>(i).map_err(map_error))
            .collect::<Result<Vec<_>>>()
            .map(Some)
    };
    match object.kind {
        SchemaObjectKind::Function => {
            let row = one(
                &format!(
                    "select {PARAMETERS}, function_type, macro_definition from duckdb_functions() \
                     where database_name = ?1 and schema_name = ?2 and function_name = ?3 \
                       and {PARAMETERS} = coalesce(?4, '') \
                       and function_type in ('macro', 'table_macro')"
                ),
                object.detail.as_deref(),
            )?
            .ok_or_else(missing)?;
            let table = if row[1] == "table_macro" {
                "TABLE "
            } else {
                ""
            };
            Ok(format!(
                "CREATE MACRO {qualified}({}) AS {table}{};",
                row[0], row[2]
            ))
        }
        SchemaObjectKind::Sequence => {
            let row = one(
                "select increment_by::varchar, min_value::varchar, max_value::varchar, \
                        start_value::varchar, \
                        case when cycle then ' CYCLE' else ' NO CYCLE' end \
                 from duckdb_sequences() \
                 where database_name = ?1 and schema_name = ?2 and sequence_name = ?3 \
                   and ?4 is null",
                None,
            )?
            .ok_or_else(missing)?;
            Ok(format!(
                "CREATE SEQUENCE {qualified} INCREMENT BY {} MINVALUE {} MAXVALUE {} START {}{};",
                row[0], row[1], row[2], row[3], row[4]
            ))
        }
        SchemaObjectKind::Enum => {
            let row = one(
                "select array_to_string(list_transform(labels, \
                            lambda x: '''' || replace(x, '''', '''''') || ''''), ', ') \
                 from duckdb_types() \
                 where database_name = ?1 and schema_name = ?2 and type_name = ?3 \
                   and logical_type = 'ENUM' and ?4 is null",
                None,
            )?
            .ok_or_else(missing)?;
            Ok(format!("CREATE TYPE {qualified} AS ENUM ({});", row[0]))
        }
        SchemaObjectKind::Type => {
            one(
                "select 1::varchar from duckdb_types() \
                 where database_name = ?1 and schema_name = ?2 and type_name = ?3 \
                   and not internal and ?4 is null",
                None,
            )?
            .ok_or_else(missing)?;
            // The full type (`DECIMAL(12,2)`, `STRUCT(x INTEGER, y INTEGER)`), as DuckDB writes it.
            let full: String = conn
                .query_row(&format!("select typeof(null::{qualified})"), [], |r| {
                    r.get(0)
                })
                .map_err(map_error)?;
            Ok(format!("CREATE TYPE {qualified} AS {full};"))
        }
        SchemaObjectKind::Procedure
        | SchemaObjectKind::Aggregate
        | SchemaObjectKind::Trigger
        | SchemaObjectKind::Domain
        | SchemaObjectKind::Event
        | SchemaObjectKind::Synonym => Err(DriverError::Other(format!(
            "DuckDB has no {:?} objects",
            object.kind
        ))),
    }
}
