//! Routines, triggers, events, sequences (MariaDB) and accounts for the explorer's folders
//! (ADR-024). Definitions come from `SHOW CREATE …`, with names quoted by our dialect.

use spelaion_driver_api::{
    Dialect, DriverError, LikeFilter, Result, SchemaObject, SchemaObjectKind, ServerObject,
    ServerObjectKind, terminated,
};

use crate::dialect::MysqlDialect;
use crate::introspect::like_sql;
use crate::{Flavor, MysqlConnection};

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|s| !s.is_empty())
}

pub(crate) async fn list_schema_objects(
    conn: &MysqlConnection,
    schema: &str,
    like: &LikeFilter,
) -> Result<Vec<SchemaObject>> {
    let mut params = vec![schema.to_string(); 4];
    let narrow = like_sql(like, "o.name", &mut params);
    // Routines of other types (MariaDB's packages, in its Oracle mode) are not listed.
    let rows = conn
        .rows(
            &format!(
                "select * from (select routine_name as name, lower(routine_type) as kind, \
                    null as detail, routine_comment as comment \
             from information_schema.routines \
             where routine_schema = ? and routine_type in ('FUNCTION', 'PROCEDURE') \
             union all \
             select table_name, 'sequence', null, table_comment from information_schema.tables \
             where table_schema = ? and table_type = 'SEQUENCE' \
             union all \
             select trigger_name, 'trigger', event_object_table, null \
             from information_schema.triggers where trigger_schema = ? \
             union all \
             select event_name, 'event', null, event_comment from information_schema.events \
             where event_schema = ?) o where true{narrow} \
             order by 1, 2"
            ),
            params,
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(SchemaObject {
                schema: schema.to_string(),
                name: text(row, 0),
                kind: match text(row, 1).as_str() {
                    "function" => SchemaObjectKind::Function,
                    "procedure" => SchemaObjectKind::Procedure,
                    "sequence" => SchemaObjectKind::Sequence,
                    "trigger" => SchemaObjectKind::Trigger,
                    "event" => SchemaObjectKind::Event,
                    other => return Err(DriverError::Other(format!("unexpected kind {other}"))),
                },
                detail: opt(row, 2),
                comment: opt(row, 3),
            })
        })
        .collect()
}

/// Accounts the server creates for itself.
const INTERNAL_ACCOUNTS: &[&str] = &[
    "mysql.sys",
    "mysql.session",
    "mysql.infoschema",
    "mariadb.sys",
];

/// Accounts from `mysql.user`, which only privileged users may read: for the others there are
/// none to show, not an error.
pub(crate) async fn list_server_objects(
    conn: &MysqlConnection,
    flavor: Flavor,
) -> Result<Vec<ServerObject>> {
    let sql = match flavor {
        Flavor::Mariadb => "select user, host, is_role from mysql.user order by user, host",
        Flavor::Mysql => "select user, host, 'N' from mysql.user order by user, host",
    };
    let rows = match conn.rows(sql, vec![]).await {
        Ok(rows) => rows,
        Err(DriverError::Database { detail, .. })
            if detail.as_deref() == Some("MySQL error 1142") =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };
    Ok(rows
        .iter()
        .map(|row| {
            let (user, host) = (text(row, 0), text(row, 1));
            let system = INTERNAL_ACCOUNTS.contains(&user.as_str());
            if text(row, 2) == "Y" {
                ServerObject {
                    name: user,
                    kind: ServerObjectKind::Role,
                    detail: None,
                    system,
                }
            } else {
                ServerObject {
                    name: format!("{}@{}", quote_account(&user), quote_account(&host)),
                    kind: ServerObjectKind::User,
                    detail: None,
                    system,
                }
            }
        })
        .collect())
}

/// An account part as `CREATE USER` writes it (`'app'@'%'`).
fn quote_account(part: &str) -> String {
    format!("'{}'", part.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `SHOW CREATE TABLE`, which writes a view's `CREATE VIEW` too (both in its second column):
/// the server's own text, with the table's indexes, keys and comments in it.
pub(crate) async fn relation_definition(
    conn: &MysqlConnection,
    schema: &str,
    name: &str,
) -> Result<String> {
    let sql = format!(
        "SHOW CREATE TABLE {}.{}",
        MysqlDialect.quote_identifier(schema),
        MysqlDialect.quote_identifier(name)
    );
    let rows = conn.rows(&sql, vec![]).await?;
    rows.first()
        .and_then(|row| opt(row, 1))
        .map(|d| terminated(&d))
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))
}

/// `CALL schema.proc(:a, @b)`: IN parameters as placeholders; OUT and INOUT ones through
/// session variables (INOUT set first), shown by a SELECT after the call.
pub(crate) async fn procedure_call(
    conn: &MysqlConnection,
    object: &SchemaObject,
) -> Result<String> {
    if object.kind == SchemaObjectKind::Function {
        return function_call(conn, object).await;
    }
    if object.kind != SchemaObjectKind::Procedure {
        return Err(DriverError::Other(format!(
            "{} is not a procedure or a function",
            object.name
        )));
    }
    let exists = conn
        .rows(
            "select 1 from information_schema.routines \
             where routine_schema = ? and routine_name = ? and routine_type = 'PROCEDURE'",
            vec![object.schema.clone(), object.name.clone()],
        )
        .await?;
    if exists.is_empty() {
        return Err(DriverError::Other(format!(
            "{}.{} does not exist",
            object.schema, object.name
        )));
    }
    let rows = conn
        .rows(
            "select parameter_name, parameter_mode from information_schema.parameters \
             where specific_schema = ? and specific_name = ? and routine_type = 'PROCEDURE' \
             order by ordinal_position",
            vec![object.schema.clone(), object.name.clone()],
        )
        .await?;
    let mut before = Vec::new();
    let mut arguments = Vec::new();
    let mut outputs = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let name = text(row, 0);
        let placeholder = spelaion_driver_api::parameter_placeholder(&name, i + 1);
        let variable = format!("@{}", &placeholder[1..]);
        match text(row, 1).as_str() {
            "IN" => arguments.push(placeholder),
            mode => {
                if mode == "INOUT" {
                    before.push(format!("SET {variable} = {placeholder};"));
                }
                outputs.push(format!(
                    "{variable} AS {}",
                    MysqlDialect.quote_identifier(&placeholder[1..])
                ));
                arguments.push(variable);
            }
        }
    }
    let mut statements = before;
    statements.push(format!(
        "CALL {}.{}({});",
        MysqlDialect.quote_identifier(&object.schema),
        MysqlDialect.quote_identifier(&object.name),
        arguments.join(", ")
    ));
    if !outputs.is_empty() {
        statements.push(format!("SELECT {};", outputs.join(", ")));
    }
    Ok(statements.join("\n"))
}

/// `SELECT schema.f(:a, :b)`: a function's parameters as placeholders, in order.
async fn function_call(conn: &MysqlConnection, object: &SchemaObject) -> Result<String> {
    let exists = conn
        .rows(
            "select 1 from information_schema.routines \
             where routine_schema = ? and routine_name = ? and routine_type = 'FUNCTION'",
            vec![object.schema.clone(), object.name.clone()],
        )
        .await?;
    if exists.is_empty() {
        return Err(DriverError::Other(format!(
            "{}.{} does not exist",
            object.schema, object.name
        )));
    }
    // Position 0 is the function's result.
    let rows = conn
        .rows(
            "select parameter_name from information_schema.parameters \
             where specific_schema = ? and specific_name = ? and routine_type = 'FUNCTION' \
               and ordinal_position > 0 \
             order by ordinal_position",
            vec![object.schema.clone(), object.name.clone()],
        )
        .await?;
    let arguments: Vec<String> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| spelaion_driver_api::parameter_placeholder(&text(row, 0), i + 1))
        .collect();
    Ok(format!(
        "SELECT {}.{}({});",
        MysqlDialect.quote_identifier(&object.schema),
        MysqlDialect.quote_identifier(&object.name),
        arguments.join(", ")
    ))
}

pub(crate) async fn object_definition(
    conn: &MysqlConnection,
    object: &SchemaObject,
) -> Result<String> {
    let (what, column) = match object.kind {
        SchemaObjectKind::Function => ("FUNCTION", 2),
        SchemaObjectKind::Procedure => ("PROCEDURE", 2),
        SchemaObjectKind::Trigger => ("TRIGGER", 2),
        SchemaObjectKind::Event => ("EVENT", 3),
        SchemaObjectKind::Sequence => ("SEQUENCE", 1),
        SchemaObjectKind::Aggregate
        | SchemaObjectKind::Enum
        | SchemaObjectKind::Domain
        | SchemaObjectKind::Type
        | SchemaObjectKind::Synonym => {
            return Err(DriverError::Other(format!(
                "MySQL has no {:?} objects",
                object.kind
            )));
        }
    };
    let sql = format!(
        "SHOW CREATE {what} {}.{}",
        MysqlDialect.quote_identifier(&object.schema),
        MysqlDialect.quote_identifier(&object.name)
    );
    let rows = conn.rows(&sql, vec![]).await?;
    let row = rows.first().ok_or_else(|| {
        DriverError::Other(format!("{}.{} does not exist", object.schema, object.name))
    })?;
    // Without the privileges to see a routine's body, the server answers with a NULL.
    opt(row, column).map(|d| terminated(&d)).ok_or_else(|| {
        DriverError::Other(format!(
            "no permission to see the definition of {}.{}",
            object.schema, object.name
        ))
    })
}
