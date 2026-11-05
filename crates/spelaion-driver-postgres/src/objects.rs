//! Functions, sequences, triggers, types and server objects for the explorer's folders
//! (ADR-024). Objects that belong to an extension are left out: the extension stands for them.
//! Definitions quote names on the server (`format('%I')`); names arrive as parameters.

use spelaion_driver_api::{
    Dialect, DriverError, LikeFilter, Result, SchemaObject, SchemaObjectKind, ServerObject,
    ServerObjectKind, parameter_placeholder, terminated,
};
use tokio_postgres::Client;

use crate::introspect::{like_sql, query_params};
use crate::map_error;

/// `pg_depend` rows that make an object part of something else: of an extension (`e`), or
/// internal to another object (`i`: a range type's constructors, an identity column's sequence).
fn not_owned(catalog: &str, oid: &str) -> String {
    format!(
        "not exists (select 1 from pg_depend d where d.classid = '{catalog}'::regclass \
         and d.objid = {oid} and d.deptype in ('e', 'i'))"
    )
}

pub async fn list_schema_objects(
    client: &Client,
    schema: &str,
    like: &LikeFilter,
) -> Result<Vec<SchemaObject>> {
    let mut patterns = Vec::new();
    let narrow = like_sql(like, "o.name", &mut patterns, 1);
    let sql = format!(
        "select * from (select p.proname::text, \
                case p.prokind when 'p' then 'procedure' when 'a' then 'aggregate' \
                     else 'function' end, \
                pg_get_function_identity_arguments(p.oid), obj_description(p.oid, 'pg_proc') \
         from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
         where n.nspname = $1 and {proc} \
         union all \
         select c.relname::text, 'sequence', null, obj_description(c.oid, 'pg_class') \
         from pg_class c join pg_namespace n on n.oid = c.relnamespace \
         where n.nspname = $1 and c.relkind = 'S' and {class} \
         union all \
         select t.tgname::text, 'trigger', c.relname::text, obj_description(t.oid, 'pg_trigger') \
         from pg_trigger t join pg_class c on c.oid = t.tgrelid \
         join pg_namespace n on n.oid = c.relnamespace \
         where n.nspname = $1 and not t.tgisinternal and {table} \
         union all \
         select t.typname::text, \
                case t.typtype when 'e' then 'enum' when 'd' then 'domain' else 'type' end, \
                case t.typtype when 'd' then format_type(t.typbasetype, t.typtypmod) \
                     when 'r' then 'RANGE' end, \
                obj_description(t.oid, 'pg_type') \
         from pg_type t join pg_namespace n on n.oid = t.typnamespace \
         where n.nspname = $1 and t.typtype in ('e', 'd', 'c', 'r') and {type_} \
           and (t.typtype <> 'c' \
                or (select c.relkind from pg_class c where c.oid = t.typrelid) = 'c') \
         ) as o(name, kind, detail, comment) where true{narrow} \
         order by 1, 2, 3",
        proc = not_owned("pg_proc", "p.oid"),
        class = not_owned("pg_class", "c.oid"),
        table = not_owned("pg_class", "c.oid"),
        type_ = not_owned("pg_type", "t.oid"),
    );
    let rows = client
        .query(&sql, &query_params(&[schema], &patterns))
        .await
        .map_err(map_error)?;
    rows.iter()
        .map(|r| {
            let kind: String = r.get(1);
            Ok(SchemaObject {
                schema: schema.to_string(),
                name: r.get(0),
                kind: match kind.as_str() {
                    "function" => SchemaObjectKind::Function,
                    "procedure" => SchemaObjectKind::Procedure,
                    "aggregate" => SchemaObjectKind::Aggregate,
                    "sequence" => SchemaObjectKind::Sequence,
                    "trigger" => SchemaObjectKind::Trigger,
                    "enum" => SchemaObjectKind::Enum,
                    "domain" => SchemaObjectKind::Domain,
                    "type" => SchemaObjectKind::Type,
                    other => return Err(DriverError::Other(format!("unexpected kind {other}"))),
                },
                detail: r.get(2),
                comment: r.get(3),
            })
        })
        .collect()
}

pub async fn list_server_objects(client: &Client) -> Result<Vec<ServerObject>> {
    let rows = client
        .query(
            "select rolname::text, 'role', \
                    nullif(concat_ws(', ', case when rolsuper then 'SUPERUSER' end, \
                                     case when rolcanlogin then 'LOGIN' end, \
                                     case when rolcreatedb then 'CREATEDB' end, \
                                     case when rolcreaterole then 'CREATEROLE' end, \
                                     case when rolreplication then 'REPLICATION' end), ''), \
                    rolname ~ '^pg_' \
             from pg_roles \
             union all \
             select spcname::text, 'tablespace', nullif(pg_tablespace_location(oid), ''), false \
             from pg_tablespace \
             union all \
             select e.extname::text, 'extension', e.extversion || ' · ' || n.nspname, false \
             from pg_extension e join pg_namespace n on n.oid = e.extnamespace \
             order by 2, 1",
            &[],
        )
        .await
        .map_err(map_error)?;
    rows.iter()
        .map(|r| {
            let kind: String = r.get(1);
            Ok(ServerObject {
                name: r.get(0),
                kind: match kind.as_str() {
                    "role" => ServerObjectKind::Role,
                    "tablespace" => ServerObjectKind::Tablespace,
                    "extension" => ServerObjectKind::Extension,
                    other => return Err(DriverError::Other(format!("unexpected kind {other}"))),
                },
                detail: r.get(2),
                system: r.get(3),
            })
        })
        .collect()
}

/// A function, procedure or aggregate by its schema, name and identity arguments.
const ROUTINE: &str = "from pg_proc p join pg_namespace n on n.oid = p.pronamespace \
     where n.nspname = $1 and p.proname = $2 and pg_get_function_identity_arguments(p.oid) = $3";

const AGGREGATE_SQL: &str = r"select format(E'CREATE AGGREGATE %I.%I(%s) (\n    SFUNC = %s,\n    STYPE = %s%s%s%s\n);',
        n.nspname, p.proname, pg_get_function_identity_arguments(p.oid), a.aggtransfn::regproc,
        format_type(a.aggtranstype, null),
        case when a.aggfinalfn <> 0 then E',\n    FINALFUNC = ' || a.aggfinalfn::regproc end,
        case when a.aggcombinefn <> 0 then E',\n    COMBINEFUNC = ' || a.aggcombinefn::regproc end,
        case when a.agginitval is not null then E',\n    INITCOND = ' || quote_literal(a.agginitval) end)
    from pg_aggregate a join pg_proc p on p.oid = a.aggfnoid
    join pg_namespace n on n.oid = p.pronamespace
    where n.nspname = $1 and p.proname = $2 and pg_get_function_identity_arguments(p.oid) = $3";

const SEQUENCE_SQL: &str = "select format('CREATE SEQUENCE %I.%I AS %s INCREMENT BY %s MINVALUE %s \
        MAXVALUE %s START WITH %s CACHE %s%s;', n.nspname, c.relname, \
        format_type(s.seqtypid, null), s.seqincrement, s.seqmin, s.seqmax, s.seqstart, s.seqcache, \
        case when s.seqcycle then ' CYCLE' else ' NO CYCLE' end) \
    from pg_sequence s join pg_class c on c.oid = s.seqrelid \
    join pg_namespace n on n.oid = c.relnamespace \
    where n.nspname = $1 and c.relname = $2";

const TRIGGER_SQL: &str = "select pg_get_triggerdef(t.oid, true) \
    from pg_trigger t join pg_class c on c.oid = t.tgrelid \
    join pg_namespace n on n.oid = c.relnamespace \
    where n.nspname = $1 and t.tgname = $2 and c.relname = $3 and not t.tgisinternal";

/// Enums, domains (with their CHECK constraints), composite and range types.
const TYPE_SQL: &str = r"select case t.typtype
      when 'e' then format('CREATE TYPE %I.%I AS ENUM (%s);', n.nspname, t.typname,
          (select string_agg(quote_literal(e.enumlabel), ', ' order by e.enumsortorder)
           from pg_enum e where e.enumtypid = t.oid))
      when 'd' then format('CREATE DOMAIN %I.%I AS %s%s%s%s%s;', n.nspname, t.typname,
          format_type(t.typbasetype, t.typtypmod),
          case when t.typcollation <> 0 and t.typcollation <>
                    (select b.typcollation from pg_type b where b.oid = t.typbasetype)
               then ' COLLATE ' || t.typcollation::regcollation end,
          case when t.typdefault is not null then ' DEFAULT ' || t.typdefault end,
          case when t.typnotnull then ' NOT NULL' end,
          (select string_agg(' CONSTRAINT ' || quote_ident(c.conname) || ' '
                             || pg_get_constraintdef(c.oid, true), '' order by c.conname)
           from pg_constraint c where c.contypid = t.oid and c.contype = 'c'))
      when 'c' then format(E'CREATE TYPE %I.%I AS (\n%s\n);', n.nspname, t.typname,
          (select string_agg('    ' || quote_ident(a.attname) || ' '
                             || format_type(a.atttypid, a.atttypmod), E',\n' order by a.attnum)
           from pg_attribute a
           where a.attrelid = t.typrelid and a.attnum > 0 and not a.attisdropped))
      when 'r' then (select format('CREATE TYPE %I.%I AS RANGE (SUBTYPE = %s%s);', n.nspname,
          t.typname, format_type(r.rngsubtype, null),
          case when r.rngsubdiff <> 0 then ', SUBTYPE_DIFF = ' || r.rngsubdiff::regproc end)
          from pg_range r where r.rngtypid = t.oid)
    end
    from pg_type t join pg_namespace n on n.oid = t.typnamespace
    where n.nspname = $1 and t.typname = $2 and t.typtype in ('e', 'd', 'c', 'r')";

/// `CALL schema.proc(a => :a, b => NULL)`: IN, INOUT and VARIADIC parameters as placeholders,
/// OUT ones as NULL (the call returns them as a row). Named when every parameter has a name.
pub async fn procedure_call(client: &Client, object: &SchemaObject) -> Result<String> {
    if object.kind != SchemaObjectKind::Procedure {
        return Err(DriverError::Other(format!(
            "{} is not a procedure",
            object.name
        )));
    }
    let detail = object.detail.clone().unwrap_or_default();
    let row = client
        .query_opt(
            &format!(
                "select format('%I.%I', n.nspname, p.proname), \
                        coalesce(p.proargnames, '{{}}'::text[]), \
                        coalesce(p.proargmodes::text[], '{{}}'::text[]), \
                        coalesce(array_length(p.proallargtypes, 1), p.pronargs::int) {ROUTINE}"
            ),
            &[&object.schema, &object.name, &detail],
        )
        .await
        .map_err(map_error)?
        .ok_or_else(|| {
            DriverError::Other(format!("{}.{} does not exist", object.schema, object.name))
        })?;
    let qualified: String = row.get(0);
    let names: Vec<String> = row.get(1);
    let modes: Vec<String> = row.get(2);
    let count: i32 = row.get(3);
    let count = usize::try_from(count).unwrap_or(0);
    let named = (0..count).all(|i| names.get(i).is_some_and(|n| !n.is_empty()));
    let quote = |name: &str| crate::dialect::PostgresDialect.quote_identifier(name);
    let arguments: Vec<String> = (0..count)
        .map(|i| {
            let name = names.get(i).map(String::as_str).unwrap_or("");
            let value = if modes.get(i).map(String::as_str) == Some("o") {
                "NULL".to_string()
            } else {
                parameter_placeholder(name, i + 1)
            };
            if named {
                format!("{} => {value}", quote(name))
            } else {
                value
            }
        })
        .collect();
    Ok(format!("CALL {qualified}({});", arguments.join(", ")))
}

pub async fn object_definition(client: &Client, object: &SchemaObject) -> Result<String> {
    let detail = object.detail.clone().unwrap_or_default();
    let (sql, with_detail) = match object.kind {
        SchemaObjectKind::Function | SchemaObjectKind::Procedure => {
            (format!("select pg_get_functiondef(p.oid) {ROUTINE}"), true)
        }
        SchemaObjectKind::Aggregate => (AGGREGATE_SQL.to_string(), true),
        SchemaObjectKind::Sequence => (SEQUENCE_SQL.to_string(), false),
        SchemaObjectKind::Trigger => (TRIGGER_SQL.to_string(), true),
        SchemaObjectKind::Enum | SchemaObjectKind::Domain | SchemaObjectKind::Type => {
            (TYPE_SQL.to_string(), false)
        }
        SchemaObjectKind::Event | SchemaObjectKind::Synonym => {
            return Err(DriverError::Other(format!(
                "PostgreSQL has no {:?} objects",
                object.kind
            )));
        }
    };
    let row = if with_detail {
        client
            .query_opt(&sql, &[&object.schema, &object.name, &detail])
            .await
    } else {
        client
            .query_opt(&sql, &[&object.schema, &object.name])
            .await
    }
    .map_err(map_error)?;
    let definition: Option<String> = row.and_then(|r| r.get(0));
    definition.map(|d| terminated(&d)).ok_or_else(|| {
        DriverError::Other(format!("{}.{} does not exist", object.schema, object.name))
    })
}
