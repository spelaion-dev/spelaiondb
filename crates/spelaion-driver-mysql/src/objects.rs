//! Routines, triggers, events, sequences (MariaDB) and accounts for the explorer's folders
//! (ADR-024). Definitions come from `SHOW CREATE …`, with names quoted by our dialect.

use spelaion_driver_api::{
    Dialect, DriverError, Result, SchemaObject, SchemaObjectKind, ServerObject, ServerObjectKind,
    terminated,
};

use crate::dialect::MysqlDialect;
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
) -> Result<Vec<SchemaObject>> {
    // Routines of other types (MariaDB's packages, in its Oracle mode) are not listed.
    let rows = conn
        .rows(
            "select routine_name, lower(routine_type), null, routine_comment \
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
             where event_schema = ? \
             order by 1, 2",
            vec![schema.to_string(); 4],
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
