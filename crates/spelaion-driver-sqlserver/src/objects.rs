//! Routines, sequences, triggers, synonyms, types and principals for the explorer's folders
//! (ADR-024), from the catalog views of the database the connection is in. Routines and
//! triggers keep the text they were created with (`OBJECT_DEFINITION`); the rest is composed.

use spelaion_driver_api::{
    Dialect, DriverError, Result, SchemaObject, SchemaObjectKind, ServerObject, ServerObjectKind,
    terminated,
};

use crate::SqlServerConnection;
use crate::dialect::SqlServerDialect;
use crate::introspect::native_type;

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|s| !s.is_empty())
}

fn number(row: &[Option<String>], i: usize) -> i64 {
    text(row, i).parse().unwrap_or(0)
}

fn flag(row: &[Option<String>], i: usize) -> bool {
    text(row, i) == "1"
}

const FUNCTIONS: &str = "'FN', 'IF', 'TF', 'FS', 'FT', 'AF'";
const PROCEDURES: &str = "'P', 'PC', 'X', 'RF'";

pub async fn list_schema_objects(
    conn: &SqlServerConnection,
    schema: &str,
) -> Result<Vec<SchemaObject>> {
    let modules = conn
        .rows(
            &format!(
                "select o.name, \
                        case when o.type in ({FUNCTIONS}) then 'function' \
                             when o.type in ({PROCEDURES}) then 'procedure' \
                             when o.type = 'SO' then 'sequence' \
                             when o.type = 'TR' then 'trigger' \
                             else 'synonym' end, \
                        case o.type when 'TR' then object_name(o.parent_object_id) \
                             when 'SN' then (select sn.base_object_name from sys.synonyms sn \
                                             where sn.object_id = o.object_id) end, \
                        cast(ep.value as nvarchar(4000)) \
                 from sys.objects o \
                 join sys.schemas s on s.schema_id = o.schema_id \
                 left join sys.extended_properties ep \
                   on ep.class = 1 and ep.major_id = o.object_id and ep.minor_id = 0 \
                  and ep.name = 'MS_Description' \
                 where s.name = @P1 and o.is_ms_shipped = 0 \
                   and o.type in ({FUNCTIONS}, {PROCEDURES}, 'SO', 'TR', 'SN')"
            ),
            vec![schema.to_string()],
        )
        .await?;
    let types = conn
        .rows(
            "select t.name, t.is_table_type, type_name(t.system_type_id), t.max_length, \
                    t.precision, t.scale, cast(ep.value as nvarchar(4000)) \
             from sys.types t \
             join sys.schemas s on s.schema_id = t.schema_id \
             left join sys.extended_properties ep \
               on ep.class = 6 and ep.major_id = t.user_type_id and ep.minor_id = 0 \
              and ep.name = 'MS_Description' \
             where s.name = @P1 and t.is_user_defined = 1",
            vec![schema.to_string()],
        )
        .await?;
    let mut objects = modules
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                schema: schema.to_string(),
                name: text(row, 0),
                kind: match text(row, 1).as_str() {
                    "function" => SchemaObjectKind::Function,
                    "procedure" => SchemaObjectKind::Procedure,
                    "sequence" => SchemaObjectKind::Sequence,
                    "trigger" => SchemaObjectKind::Trigger,
                    "synonym" => SchemaObjectKind::Synonym,
                    other => return Err(DriverError::Other(format!("unexpected kind {other}"))),
                },
                detail: opt(row, 2),
                comment: opt(row, 3),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    objects.extend(types.iter().map(|row| SchemaObject {
        schema: schema.to_string(),
        name: text(row, 0),
        kind: SchemaObjectKind::Type,
        detail: Some(if flag(row, 1) {
            "TABLE".to_string()
        } else {
            native_type(
                &text(row, 2),
                number(row, 3),
                number(row, 4),
                number(row, 5),
            )
        }),
        comment: opt(row, 6),
    }));
    objects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(objects)
}

/// The server's logins (those a user without `VIEW ANY DEFINITION` sees: their own and a few
/// more), and the users and roles of the connection's database. The internal `##…##`
/// logins, the fixed roles and the built-in users are marked as system objects.
pub async fn list_server_objects(conn: &SqlServerConnection) -> Result<Vec<ServerObject>> {
    let rows = conn
        .rows(
            "select name, 'login', \
                    type_desc + case when is_disabled = 1 then ', DISABLED' else '' end, \
                    case when name like '##%' then 1 else 0 end \
             from sys.server_principals \
             where type in ('S', 'U', 'G', 'E', 'X', 'C', 'K') \
             union all \
             select name, case type when 'R' then 'role' else 'user' end, \
                    case when type <> 'R' then type_desc end, \
                    case when is_fixed_role = 1 \
                           or name in ('sys', 'INFORMATION_SCHEMA', 'guest', 'public') \
                         then 1 else 0 end \
             from sys.database_principals \
             where type in ('S', 'U', 'G', 'E', 'X', 'C', 'K', 'R') \
             order by 2, 1",
            vec![],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(ServerObject {
                name: text(row, 0),
                kind: match text(row, 1).as_str() {
                    "login" => ServerObjectKind::Login,
                    "user" => ServerObjectKind::User,
                    "role" => ServerObjectKind::Role,
                    other => return Err(DriverError::Other(format!("unexpected kind {other}"))),
                },
                detail: opt(row, 2),
                system: flag(row, 3),
            })
        })
        .collect()
}

const SEQUENCE_SQL: &str = "select 'CREATE SEQUENCE ' + quotename(s.name) + '.' + quotename(q.name) \
        + ' AS ' + type_name(q.user_type_id) \
        + ' START WITH ' + cast(q.start_value as nvarchar(100)) \
        + ' INCREMENT BY ' + cast(q.increment as nvarchar(100)) \
        + ' MINVALUE ' + cast(q.minimum_value as nvarchar(100)) \
        + ' MAXVALUE ' + cast(q.maximum_value as nvarchar(100)) \
        + case when q.is_cycling = 1 then ' CYCLE' else ' NO CYCLE' end \
        + case when q.is_cached = 0 then ' NO CACHE' \
               when q.cache_size is not null then ' CACHE ' + cast(q.cache_size as nvarchar(20)) \
               else '' end \
    from sys.sequences q join sys.schemas s on s.schema_id = q.schema_id \
    where s.name = @P1 and q.name = @P2";

const SYNONYM_SQL: &str = "select 'CREATE SYNONYM ' + quotename(s.name) + '.' + quotename(sn.name) \
        + ' FOR ' + sn.base_object_name \
    from sys.synonyms sn join sys.schemas s on s.schema_id = sn.schema_id \
    where s.name = @P1 and sn.name = @P2";

pub async fn object_definition(
    conn: &SqlServerConnection,
    object: &SchemaObject,
) -> Result<String> {
    let params = || vec![object.schema.clone(), object.name.clone()];
    let missing =
        || DriverError::Other(format!("{}.{} does not exist", object.schema, object.name));
    let single = |rows: Vec<Vec<Option<String>>>| {
        rows.first()
            .and_then(|row| opt(row, 0))
            .map(|d| terminated(&d))
            .ok_or_else(missing)
    };
    match object.kind {
        SchemaObjectKind::Function | SchemaObjectKind::Procedure | SchemaObjectKind::Trigger => {
            let rows = conn
                .rows(
                    "select o.object_id, object_definition(o.object_id) \
                     from sys.objects o join sys.schemas s on s.schema_id = o.schema_id \
                     where s.name = @P1 and o.name = @P2",
                    params(),
                )
                .await?;
            let row = rows.first().ok_or_else(missing)?;
            // NULL for a module created WITH ENCRYPTION, or without VIEW DEFINITION.
            opt(row, 1).map(|d| terminated(&d)).ok_or_else(|| {
                DriverError::Other(format!(
                    "the definition of {}.{} is encrypted or not visible to this user",
                    object.schema, object.name
                ))
            })
        }
        SchemaObjectKind::Sequence => single(conn.rows(SEQUENCE_SQL, params()).await?),
        SchemaObjectKind::Synonym => single(conn.rows(SYNONYM_SQL, params()).await?),
        SchemaObjectKind::Type => type_definition(conn, object).await,
        SchemaObjectKind::Aggregate
        | SchemaObjectKind::Enum
        | SchemaObjectKind::Domain
        | SchemaObjectKind::Event => Err(DriverError::Other(format!(
            "SQL Server has no {:?} objects",
            object.kind
        ))),
    }
}

/// An alias type (`CREATE TYPE … FROM nvarchar(50) NOT NULL`) or a table type with its columns.
async fn type_definition(conn: &SqlServerConnection, object: &SchemaObject) -> Result<String> {
    let rows = conn
        .rows(
            "select t.is_table_type, type_name(t.system_type_id), t.max_length, t.precision, \
                    t.scale, t.is_nullable, tt.type_table_object_id \
             from sys.types t join sys.schemas s on s.schema_id = t.schema_id \
             left join sys.table_types tt on tt.user_type_id = t.user_type_id \
             where s.name = @P1 and t.name = @P2 and t.is_user_defined = 1",
            vec![object.schema.clone(), object.name.clone()],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        DriverError::Other(format!("{}.{} does not exist", object.schema, object.name))
    })?;
    let name = format!(
        "{}.{}",
        SqlServerDialect.quote_identifier(&object.schema),
        SqlServerDialect.quote_identifier(&object.name)
    );
    if !flag(row, 0) {
        let base = native_type(
            &text(row, 1),
            number(row, 2),
            number(row, 3),
            number(row, 4),
        );
        let null = if flag(row, 5) { "" } else { " NOT NULL" };
        return Ok(format!("CREATE TYPE {name} FROM {base}{null};"));
    }
    let columns = conn
        .rows(
            "select c.name, type_name(c.user_type_id), c.max_length, c.precision, c.scale, \
                    c.is_nullable \
             from sys.columns c where c.object_id = @P1 order by c.column_id",
            vec![text(row, 6)],
        )
        .await?;
    let columns: Vec<String> = columns
        .iter()
        .map(|c| {
            format!(
                "    {} {}{}",
                SqlServerDialect.quote_identifier(&text(c, 0)),
                native_type(&text(c, 1), number(c, 2), number(c, 3), number(c, 4)),
                if flag(c, 5) { " NULL" } else { " NOT NULL" }
            )
        })
        .collect();
    Ok(format!(
        "CREATE TYPE {name} AS TABLE (\n{}\n);",
        columns.join(",\n")
    ))
}
