//! Privileges of an account (ADR-048): grant or revoke them globally, on a database, on a schema
//! or on one object, and read what an account has. The form says what to grant; the core writes
//! `GRANT` / `REVOKE` with the engine's names for each level, only with the privileges that
//! level has (the names are the core's own, never typed SQL), and runs the script only as shown.

use spelaion_config::Safeguards;
use spelaion_driver_api::{Dialect, Value};

use super::alter::{AlterationPlan, Step, run_plan};
use super::users::{PrincipalKind, PrincipalTarget, parse_account};
use super::{ObjectError, nstring};
use crate::generate::short_rows;
use crate::session::Session;

/// What a privilege is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivilegeScope {
    /// The whole server (MySQL `*.*`, SQL Server's server permissions).
    Global,
    /// A database (PostgreSQL and MySQL: by name; SQL Server: the connection's database).
    Database,
    /// A schema (PostgreSQL, SQL Server).
    Schema,
    /// Every table of a schema at once (PostgreSQL's `ALL TABLES IN SCHEMA`).
    AllTablesInSchema,
    Table,
    Sequence,
    Function,
    Procedure,
    /// Membership of a role (`GRANT role TO account`, SQL Server's `ALTER ROLE … ADD MEMBER`).
    Role,
}

/// Where a privilege applies: its scope and the names it needs (empty when it needs none).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivilegeTarget {
    pub scope: Option<PrivilegeScope>,
    /// A database's name (PostgreSQL and MySQL databases, MySQL's objects).
    pub database: String,
    /// A schema's name (PostgreSQL and SQL Server schemas and objects).
    pub schema: String,
    /// An object's or role's name.
    pub name: String,
}

/// Grant or revoke privileges of one scope to an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegeChange {
    pub grantee: PrincipalTarget,
    pub target: PrivilegeTarget,
    /// Names from [`privilege_options`]; a role's membership has none.
    pub privileges: Vec<String>,
    pub revoke: bool,
    /// Granting: `WITH GRANT OPTION` (`WITH ADMIN OPTION` for a role). Revoking: only the
    /// option is taken away.
    pub grant_option: bool,
}

/// One privilege an account has, as the catalog says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedPrivilege {
    pub target: PrivilegeTarget,
    pub privilege: String,
    /// It may pass it on (`WITH GRANT OPTION`, `WITH ADMIN OPTION`).
    pub grantable: bool,
    /// SQL Server's `DENY`: refused, which wins over a grant.
    pub denied: bool,
}

const PG_TABLE: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
];

const MYSQL_GLOBAL: &[&str] = &[
    "ALL PRIVILEGES",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "REFERENCES",
    "CREATE TEMPORARY TABLES",
    "LOCK TABLES",
    "EXECUTE",
    "CREATE VIEW",
    "SHOW VIEW",
    "CREATE ROUTINE",
    "ALTER ROUTINE",
    "EVENT",
    "TRIGGER",
    "PROCESS",
    "RELOAD",
    "SHOW DATABASES",
    "CREATE USER",
    "REPLICATION CLIENT",
    "REPLICATION SLAVE",
    "FILE",
];

const MYSQL_DATABASE: &[&str] = &[
    "ALL PRIVILEGES",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "REFERENCES",
    "CREATE TEMPORARY TABLES",
    "LOCK TABLES",
    "EXECUTE",
    "CREATE VIEW",
    "SHOW VIEW",
    "CREATE ROUTINE",
    "ALTER ROUTINE",
    "EVENT",
    "TRIGGER",
];

const MYSQL_TABLE: &[&str] = &[
    "ALL PRIVILEGES",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "CREATE",
    "DROP",
    "ALTER",
    "INDEX",
    "REFERENCES",
    "CREATE VIEW",
    "SHOW VIEW",
    "TRIGGER",
];

const MSSQL_SERVER: &[&str] = &[
    "CONNECT SQL",
    "VIEW SERVER STATE",
    "VIEW ANY DATABASE",
    "VIEW ANY DEFINITION",
    "CREATE ANY DATABASE",
    "ALTER ANY DATABASE",
    "ALTER ANY LOGIN",
    "ALTER TRACE",
    "CONTROL SERVER",
];

const MSSQL_DATABASE: &[&str] = &[
    "CONNECT",
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "EXECUTE",
    "CREATE TABLE",
    "CREATE VIEW",
    "CREATE PROCEDURE",
    "CREATE FUNCTION",
    "CREATE SCHEMA",
    "ALTER",
    "VIEW DEFINITION",
    "VIEW DATABASE STATE",
    "SHOWPLAN",
    "BACKUP DATABASE",
    "CONTROL",
];

const MSSQL_SCHEMA: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "EXECUTE",
    "REFERENCES",
    "ALTER",
    "VIEW DEFINITION",
    "CONTROL",
];

const MSSQL_TABLE: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "REFERENCES",
    "ALTER",
    "VIEW DEFINITION",
    "CONTROL",
];

const MSSQL_ROUTINE: &[&str] = &["EXECUTE", "ALTER", "VIEW DEFINITION", "CONTROL"];

/// The scopes an account of `kind` can be granted on the engine, in the order the form offers
/// them.
pub fn privilege_scopes(dialect: &dyn Dialect, kind: PrincipalKind) -> Vec<PrivilegeScope> {
    use PrivilegeScope::*;
    match (dialect.name(), kind) {
        ("postgres", _) => vec![
            Database,
            Schema,
            AllTablesInSchema,
            Table,
            Sequence,
            Function,
            Procedure,
            Role,
        ],
        ("mysql", _) => vec![Global, Database, Table, Function, Procedure, Role],
        // A login holds the server's permissions and server roles; a user or role of the
        // database, the rest.
        ("sqlserver", PrincipalKind::Login) => vec![Global, Role],
        ("sqlserver", _) => vec![Database, Schema, Table, Function, Procedure, Role],
        _ => Vec::new(),
    }
}

/// The privileges a scope has on the engine (none for a role's membership).
pub fn privilege_options(dialect: &dyn Dialect, scope: PrivilegeScope) -> &'static [&'static str] {
    use PrivilegeScope::*;
    match (dialect.name(), scope) {
        ("postgres", Database) => &["CONNECT", "CREATE", "TEMPORARY"],
        ("postgres", Schema) => &["USAGE", "CREATE"],
        ("postgres", AllTablesInSchema | Table) => PG_TABLE,
        ("postgres", Sequence) => &["USAGE", "SELECT", "UPDATE"],
        ("postgres", Function | Procedure) => &["EXECUTE"],
        ("mysql", Global) => MYSQL_GLOBAL,
        ("mysql", Database) => MYSQL_DATABASE,
        ("mysql", Table) => MYSQL_TABLE,
        ("mysql", Function | Procedure) => &["EXECUTE", "ALTER ROUTINE"],
        ("sqlserver", Global) => MSSQL_SERVER,
        ("sqlserver", Database) => MSSQL_DATABASE,
        ("sqlserver", Schema) => MSSQL_SCHEMA,
        ("sqlserver", Table) => MSSQL_TABLE,
        ("sqlserver", Function | Procedure) => MSSQL_ROUTINE,
        _ => &[],
    }
}

/// The names a scope needs on the engine, for the form to ask: (database, schema, name).
pub fn privilege_scope_fields(dialect: &dyn Dialect, scope: PrivilegeScope) -> (bool, bool, bool) {
    use PrivilegeScope::*;
    match (dialect.name(), scope) {
        (_, Role) => (false, false, true),
        ("postgres", Database) | ("mysql", Database) => (true, false, false),
        ("postgres", Schema | AllTablesInSchema) | ("sqlserver", Schema) => (false, true, false),
        ("postgres" | "sqlserver", Table | Sequence | Function | Procedure) => (false, true, true),
        ("mysql", Table | Function | Procedure) => (true, false, true),
        _ => (false, false, false),
    }
}

fn required(text: &str, what: &str) -> Result<String, ObjectError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ObjectError::Unsupported(format!("write the {what}")));
    }
    if text.chars().any(char::is_control) {
        return Err(ObjectError::Unsupported(format!(
            "the {what} cannot hold control characters"
        )));
    }
    Ok(text.to_string())
}

/// The account a statement names: quoted, a MySQL user as `` `name`@`host` ``.
fn grantee(dialect: &dyn Dialect, target: &PrincipalTarget) -> Result<String, ObjectError> {
    if dialect.name() == "mysql" {
        return Ok(match parse_account(&target.name) {
            Some((user, host)) => format!(
                "{}@{}",
                dialect.quote_identifier(&user),
                dialect.quote_identifier(&host)
            ),
            None => dialect.quote_identifier(&required(&target.name, "account's name")?),
        });
    }
    Ok(dialect.quote_identifier(&required(&target.name, "account's name")?))
}

/// The script of a change; nothing runs.
pub fn privilege_plan(
    dialect: &dyn Dialect,
    change: &PrivilegeChange,
) -> Result<AlterationPlan, ObjectError> {
    let statement = privilege_statement(dialect, change)?;
    Ok(AlterationPlan {
        before: Vec::new(),
        steps: vec![Step::Run(statement)],
        after: Vec::new(),
        begin: None,
        note: None,
        rebuild: false,
    })
}

fn privilege_statement(
    dialect: &dyn Dialect,
    change: &PrivilegeChange,
) -> Result<String, ObjectError> {
    use PrivilegeScope::*;
    let engine = dialect.name();
    let scope = change
        .target
        .scope
        .ok_or_else(|| ObjectError::Unsupported("choose what to grant it on".into()))?;
    if !privilege_scopes(dialect, change.grantee.kind).contains(&scope) {
        return Err(ObjectError::Unsupported(format!(
            "this account cannot be granted privileges on that ({scope:?}) here"
        )));
    }
    let q = |name: &str| dialect.quote_identifier(name);
    let who = grantee(dialect, &change.grantee)?;
    let t = &change.target;

    if scope == Role {
        let role = required(&t.name, "role")?;
        // A MySQL role may be listed as an account ('r'@'%').
        let role_sql = if engine == "mysql" {
            grantee(
                dialect,
                &PrincipalTarget {
                    kind: PrincipalKind::Role,
                    name: role.clone(),
                },
            )?
        } else {
            q(&role)
        };
        return Ok(match engine {
            "sqlserver" => {
                if change.grant_option {
                    return Err(ObjectError::Unsupported(
                        "SQL Server roles have no admin option".into(),
                    ));
                }
                format!(
                    "ALTER {}ROLE {role_sql} {} MEMBER {who}",
                    if change.grantee.kind == PrincipalKind::Login {
                        "SERVER "
                    } else {
                        ""
                    },
                    if change.revoke { "DROP" } else { "ADD" }
                )
            }
            _ => match (change.revoke, change.grant_option) {
                (false, false) => format!("GRANT {role_sql} TO {who}"),
                (false, true) => format!("GRANT {role_sql} TO {who} WITH ADMIN OPTION"),
                (true, false) => format!("REVOKE {role_sql} FROM {who}"),
                (true, true) if engine == "postgres" => {
                    format!("REVOKE ADMIN OPTION FOR {role_sql} FROM {who}")
                }
                (true, true) => {
                    return Err(ObjectError::Unsupported(
                        "MySQL and MariaDB revoke a role as a whole".into(),
                    ));
                }
            },
        });
    }

    // Each privilege must be one the scope has: they are written as the core's own words.
    let options = privilege_options(dialect, scope);
    let mut privileges = Vec::new();
    for privilege in &change.privileges {
        let wanted = privilege.trim().to_ascii_uppercase();
        let Some(known) = options.iter().find(|o| **o == wanted) else {
            return Err(ObjectError::Unsupported(format!(
                "{} is not a privilege of that scope",
                privilege.trim()
            )));
        };
        if !privileges.contains(known) {
            privileges.push(*known);
        }
    }
    if privileges.is_empty() {
        return Err(ObjectError::Unsupported(
            "choose one privilege or more".into(),
        ));
    }
    let list = privileges.join(", ");

    let on = match (engine, scope) {
        ("postgres", Database) => format!("DATABASE {}", q(&required(&t.database, "database")?)),
        ("postgres", Schema) => format!("SCHEMA {}", q(&required(&t.schema, "schema")?)),
        ("postgres", AllTablesInSchema) => {
            format!(
                "ALL TABLES IN SCHEMA {}",
                q(&required(&t.schema, "schema")?)
            )
        }
        ("postgres", Table | Sequence | Function | Procedure) => format!(
            "{} {}.{}",
            match scope {
                Table => "TABLE",
                Sequence => "SEQUENCE",
                Function => "FUNCTION",
                _ => "PROCEDURE",
            },
            q(&required(&t.schema, "schema")?),
            q(&required(&t.name, "object's name")?)
        ),
        ("mysql", Global) => "*.*".into(),
        ("mysql", Database) => format!("{}.*", q(&required(&t.database, "database")?)),
        ("mysql", Table | Function | Procedure) => format!(
            "{}{}.{}",
            match scope {
                Function => "FUNCTION ",
                Procedure => "PROCEDURE ",
                _ => "",
            },
            q(&required(&t.database, "database")?),
            q(&required(&t.name, "object's name")?)
        ),
        ("sqlserver", Global | Database) => String::new(),
        ("sqlserver", Schema) => format!("SCHEMA::{}", q(&required(&t.schema, "schema")?)),
        ("sqlserver", Table | Function | Procedure) => format!(
            "OBJECT::{}.{}",
            q(&required(&t.schema, "schema")?),
            q(&required(&t.name, "object's name")?)
        ),
        _ => {
            return Err(ObjectError::Unsupported(
                "this engine has no privileges there".into(),
            ));
        }
    };
    let on = if on.is_empty() {
        String::new()
    } else {
        format!(" ON {on}")
    };
    let statement = match (change.revoke, change.grant_option) {
        (false, false) => format!("GRANT {list}{on} TO {who}"),
        (false, true) => format!("GRANT {list}{on} TO {who} WITH GRANT OPTION"),
        (true, false) if engine == "sqlserver" => format!("REVOKE {list}{on} FROM {who} CASCADE"),
        (true, false) => format!("REVOKE {list}{on} FROM {who}"),
        (true, true) if engine == "mysql" => {
            return Err(ObjectError::Unsupported(
                "MySQL and MariaDB revoke GRANT OPTION as its own privilege: revoke the privilege"
                    .into(),
            ));
        }
        (true, true) => format!("REVOKE GRANT OPTION FOR {list}{on} FROM {who} CASCADE"),
    };
    // SQL Server grants server permissions only from master: run there, without changing the
    // session's database.
    Ok(if engine == "sqlserver" && scope == Global {
        format!("EXEC master.sys.sp_executesql {}", nstring(&statement))
    } else {
        statement
    })
}

/// Grants or revokes, never on a read-only connection, and only as the script the user was
/// shown. Statements the connection's type asks about run only `confirmed`. Returns how many
/// statements ran.
pub async fn apply_privilege_change(
    session: &Session,
    change: &PrivilegeChange,
    shown: &str,
    read_only: bool,
    safeguards: &Safeguards,
    confirmed: bool,
) -> Result<usize, ObjectError> {
    if read_only {
        return Err(ObjectError::ReadOnly);
    }
    let dialect = session.dialect();
    let plan = privilege_plan(dialect.as_ref(), change)?;
    if plan.script() != shown {
        return Err(ObjectError::Changed);
    }
    if !confirmed && !plan.risks(dialect.as_ref(), safeguards).is_empty() {
        return Err(ObjectError::NeedsConfirmation);
    }
    run_plan(session, &plan).await
}

fn text(value: &Value) -> String {
    match value {
        Value::Text(text) | Value::Decimal(text) | Value::Native(text) => text.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        _ => String::new(),
    }
}

fn scope_named(name: &str) -> Option<PrivilegeScope> {
    use PrivilegeScope::*;
    Some(match name {
        "global" => Global,
        "database" => Database,
        "schema" => Schema,
        "table" => Table,
        "sequence" => Sequence,
        "function" => Function,
        "procedure" => Procedure,
        "role" => Role,
        _ => return None,
    })
}

/// A standard SQL string literal: quotes doubled.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// What an account has: its privileges on each scope and the roles it is a member of, read
/// from the catalog (PostgreSQL's ACLs, MySQL's `SHOW GRANTS`, SQL Server's permissions).
pub async fn principal_privileges(
    session: &Session,
    target: &PrincipalTarget,
) -> Result<Vec<GrantedPrivilege>, ObjectError> {
    let dialect = session.dialect();
    let failed = |message: String| ObjectError::Failed {
        index: 1,
        total: 1,
        message,
    };
    if dialect.name() == "mysql" {
        let sql = format!("SHOW GRANTS FOR {}", grantee(dialect.as_ref(), target)?);
        let (_, rows) = short_rows(session, &sql, true).await.map_err(failed)?;
        return Ok(rows
            .iter()
            .filter_map(|row| row.first().map(text))
            .flat_map(|grant| parse_mysql_grant(&grant))
            .collect());
    }
    let sql = match dialect.name() {
        "postgres" => postgres_privileges_sql(&target.name),
        "sqlserver" if target.kind == PrincipalKind::Login => format!(
            "SELECT 'global', '', '', '', p.permission_name, p.state_desc \
             FROM sys.server_permissions p \
             JOIN sys.server_principals s ON s.principal_id = p.grantee_principal_id \
             WHERE s.name = {name} \
             UNION ALL \
             SELECT 'role', '', '', r.name, 'MEMBER', 'GRANT' \
             FROM sys.server_role_members m \
             JOIN sys.server_principals r ON r.principal_id = m.role_principal_id \
             JOIN sys.server_principals s ON s.principal_id = m.member_principal_id \
             WHERE s.name = {name} \
             ORDER BY 1, 4, 5",
            name = nstring(&target.name)
        ),
        "sqlserver" => format!(
            "SELECT CASE p.class WHEN 0 THEN 'database' WHEN 3 THEN 'schema' \
                    ELSE CASE WHEN o.type IN ('P', 'PC', 'X') THEN 'procedure' \
                              WHEN o.type IN ('FN', 'IF', 'TF', 'FS', 'FT', 'AF') THEN 'function' \
                              ELSE 'table' END END, \
                    DB_NAME(), COALESCE(sc.name, os.name, ''), COALESCE(o.name, ''), \
                    p.permission_name, p.state_desc \
             FROM sys.database_permissions p \
             LEFT JOIN sys.schemas sc ON p.class = 3 AND sc.schema_id = p.major_id \
             LEFT JOIN sys.objects o ON p.class = 1 AND o.object_id = p.major_id \
             LEFT JOIN sys.schemas os ON os.schema_id = o.schema_id \
             WHERE p.grantee_principal_id = DATABASE_PRINCIPAL_ID({name}) \
               AND p.class IN (0, 1, 3) AND p.minor_id = 0 \
             UNION ALL \
             SELECT 'role', '', '', r.name, 'MEMBER', 'GRANT' \
             FROM sys.database_role_members m \
             JOIN sys.database_principals r ON r.principal_id = m.role_principal_id \
             WHERE m.member_principal_id = DATABASE_PRINCIPAL_ID({name}) \
             ORDER BY 1, 3, 4, 5",
            name = nstring(&target.name)
        ),
        _ => return Ok(Vec::new()),
    };
    let (_, rows) = short_rows(session, &sql, true).await.map_err(failed)?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let cell = |i: usize| row.get(i).map(text).unwrap_or_default();
            let scope = scope_named(&cell(0))?;
            let state = cell(5);
            Some(GrantedPrivilege {
                target: PrivilegeTarget {
                    scope: Some(scope),
                    database: cell(1),
                    schema: cell(2),
                    name: cell(3),
                },
                privilege: cell(4),
                grantable: matches!(state.as_str(), "true" | "YES" | "GRANT_WITH_GRANT_OPTION"),
                denied: state == "DENY",
            })
        })
        .collect())
}

fn postgres_privileges_sql(role: &str) -> String {
    let schemas = "left(n.nspname, 3) <> 'pg_' AND n.nspname <> 'information_schema'";
    format!(
        "WITH r AS (SELECT oid FROM pg_roles WHERE rolname = {role}) \
         SELECT 'database', d.datname::text, '', '', a.privilege_type, a.is_grantable::text \
         FROM pg_database d CROSS JOIN r \
         CROSS JOIN LATERAL aclexplode(coalesce(d.datacl, acldefault('d', d.datdba))) a \
         WHERE a.grantee = r.oid \
         UNION ALL \
         SELECT 'schema', '', n.nspname::text, '', a.privilege_type, a.is_grantable::text \
         FROM pg_namespace n CROSS JOIN r \
         CROSS JOIN LATERAL aclexplode(coalesce(n.nspacl, acldefault('n', n.nspowner))) a \
         WHERE a.grantee = r.oid AND {schemas} \
         UNION ALL \
         SELECT CASE c.relkind WHEN 'S' THEN 'sequence' ELSE 'table' END, '', n.nspname::text, \
                c.relname::text, a.privilege_type, a.is_grantable::text \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace CROSS JOIN r \
         CROSS JOIN LATERAL aclexplode(coalesce(c.relacl, \
             acldefault(CASE WHEN c.relkind = 'S' THEN 's' ELSE 'r' END::\"char\", c.relowner))) a \
         WHERE a.grantee = r.oid AND c.relkind IN ('r', 'v', 'm', 'S', 'f', 'p') AND {schemas} \
         UNION ALL \
         SELECT CASE p.prokind WHEN 'p' THEN 'procedure' ELSE 'function' END, '', \
                n.nspname::text, p.proname::text, a.privilege_type, a.is_grantable::text \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace CROSS JOIN r \
         CROSS JOIN LATERAL aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a \
         WHERE a.grantee = r.oid AND {schemas} \
         UNION ALL \
         SELECT 'role', '', '', g.rolname::text, 'MEMBER', m.admin_option::text \
         FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid CROSS JOIN r \
         WHERE m.member = r.oid \
         ORDER BY 1, 2, 3, 4, 5",
        role = quoted(role),
    )
}

/// A backquoted MySQL name at the start of `text` (`` `a``b` `` → ``a`b``), and what follows.
fn backquoted(text: &str) -> Option<(String, &str)> {
    let rest = text.strip_prefix('`')?;
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '`' {
            if matches!(chars.peek(), Some((_, '`'))) {
                chars.next();
                out.push('`');
                continue;
            }
            return Some((out, &rest[i + 1..]));
        }
        out.push(c);
    }
    None
}

/// The account part of a role grant as the explorer lists it (`'r'@'%'`), or a bare name.
fn listed_account(text: &str) -> String {
    match backquoted(text) {
        Some((user, host)) => match host.strip_prefix('@').and_then(backquoted) {
            Some((host, _)) => format!(
                "'{}'@'{}'",
                user.replace('\\', "\\\\").replace('\'', "\\'"),
                host.replace('\\', "\\\\").replace('\'', "\\'")
            ),
            None => user,
        },
        None => text.to_string(),
    }
}

/// The privileges of one `SHOW GRANTS` line (`GRANT SELECT, INSERT ON `db`.* TO …`), or the
/// roles of a role grant (`GRANT `r`@`%` TO …`). The account and anything after it (MariaDB's
/// `IDENTIFIED BY PASSWORD`) are never read.
fn parse_mysql_grant(grant: &str) -> Vec<GrantedPrivilege> {
    use PrivilegeScope::*;
    let Some(body) = grant.strip_prefix("GRANT ") else {
        return Vec::new();
    };
    let grantable = grant.ends_with(" WITH GRANT OPTION") || grant.ends_with(" WITH ADMIN OPTION");
    // The privileges (or roles): top-level commas, up to " ON " or " TO ".
    let mut parts = Vec::new();
    let (mut depth, mut quote, mut start) = (0usize, None::<u8>, 0usize);
    let mut end = None;
    let bytes = body.as_bytes();
    for i in 0..bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                b'`' | b'\'' => quote = Some(c),
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                b',' if depth == 0 => {
                    parts.push(body[start..i].trim().to_string());
                    start = i + 1;
                }
                b' ' if depth == 0
                    && (bytes[i..].starts_with(b" ON ") || bytes[i..].starts_with(b" TO ")) =>
                {
                    parts.push(body[start..i].trim().to_string());
                    end = Some(i);
                    break;
                }
                _ => {}
            },
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    let rest = &body[end..];
    if rest.starts_with(" TO ") {
        return parts
            .iter()
            .map(|role| GrantedPrivilege {
                target: PrivilegeTarget {
                    scope: Some(Role),
                    name: listed_account(role),
                    ..Default::default()
                },
                privilege: "MEMBER".into(),
                grantable,
                denied: false,
            })
            .collect();
    }
    let Some(level) = rest.strip_prefix(" ON ") else {
        return Vec::new();
    };
    let (scope, level) = if let Some(level) = level.strip_prefix("FUNCTION ") {
        (Function, level)
    } else if let Some(level) = level.strip_prefix("PROCEDURE ") {
        (Procedure, level)
    } else {
        (Table, level)
    };
    let target = if level.starts_with("*.*") {
        PrivilegeTarget {
            scope: Some(Global),
            ..Default::default()
        }
    } else {
        let Some((database, after)) = backquoted(level) else {
            return Vec::new();
        };
        let Some(after) = after.strip_prefix('.') else {
            return Vec::new();
        };
        if after.starts_with('*') {
            PrivilegeTarget {
                scope: Some(Database),
                database,
                ..Default::default()
            }
        } else {
            let Some((name, _)) = backquoted(after) else {
                return Vec::new();
            };
            PrivilegeTarget {
                scope: Some(scope),
                database,
                name,
                ..Default::default()
            }
        }
    };
    parts
        .into_iter()
        .filter(|p| p != "USAGE" && p != "PROXY")
        .map(|privilege| GrantedPrivilege {
            target: target.clone(),
            privilege,
            grantable,
            denied: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_mysql::MysqlDialect;
    use spelaion_driver_postgres::PostgresDialect;
    use spelaion_driver_sqlserver::SqlServerDialect;

    fn change(
        kind: PrincipalKind,
        grantee: &str,
        scope: PrivilegeScope,
        names: (&str, &str, &str),
        privileges: &[&str],
    ) -> PrivilegeChange {
        PrivilegeChange {
            grantee: PrincipalTarget {
                kind,
                name: grantee.into(),
            },
            target: PrivilegeTarget {
                scope: Some(scope),
                database: names.0.into(),
                schema: names.1.into(),
                name: names.2.into(),
            },
            privileges: privileges.iter().map(|p| (*p).to_string()).collect(),
            revoke: false,
            grant_option: false,
        }
    }

    fn sql(dialect: &dyn Dialect, change: &PrivilegeChange) -> String {
        privilege_plan(dialect, change).unwrap().script()
    }

    #[test]
    fn each_engine_writes_its_grants_and_revokes() {
        use PrincipalKind::{Login, Role, User};
        use PrivilegeScope::*;
        let pg = PostgresDialect;
        let mut c = change(
            Role,
            "app",
            Table,
            ("", "sales", "Orders"),
            &["select", "INSERT"],
        );
        assert_eq!(
            sql(&pg, &c),
            "GRANT SELECT, INSERT ON TABLE sales.\"Orders\" TO app;\n"
        );
        c.grant_option = true;
        assert_eq!(
            sql(&pg, &c),
            "GRANT SELECT, INSERT ON TABLE sales.\"Orders\" TO app WITH GRANT OPTION;\n"
        );
        c.revoke = true;
        assert_eq!(
            sql(&pg, &c),
            "REVOKE GRANT OPTION FOR SELECT, INSERT ON TABLE sales.\"Orders\" FROM app CASCADE;\n"
        );
        assert_eq!(
            sql(
                &pg,
                &change(
                    Role,
                    "app",
                    AllTablesInSchema,
                    ("", "sales", ""),
                    &["SELECT"]
                )
            ),
            "GRANT SELECT ON ALL TABLES IN SCHEMA sales TO app;\n"
        );
        assert_eq!(
            sql(
                &pg,
                &change(Role, "app", Database, ("shop", "", ""), &["CONNECT"])
            ),
            "GRANT CONNECT ON DATABASE shop TO app;\n"
        );
        let mut member = change(Role, "app", PrivilegeScope::Role, ("", "", "readers"), &[]);
        member.grant_option = true;
        assert_eq!(
            sql(&pg, &member),
            "GRANT readers TO app WITH ADMIN OPTION;\n"
        );

        let my = MysqlDialect;
        let account = "'app'@'%'";
        assert_eq!(
            sql(
                &my,
                &change(User, account, Global, ("", "", ""), &["PROCESS"])
            ),
            "GRANT PROCESS ON *.* TO `app`@`%`;\n"
        );
        assert_eq!(
            sql(
                &my,
                &change(
                    User,
                    account,
                    Database,
                    ("shop", "", ""),
                    &["SELECT", "SELECT"]
                )
            ),
            "GRANT SELECT ON `shop`.* TO `app`@`%`;\n"
        );
        let mut revoke = change(User, account, Procedure, ("shop", "", "p"), &["EXECUTE"]);
        revoke.revoke = true;
        assert_eq!(
            sql(&my, &revoke),
            "REVOKE EXECUTE ON PROCEDURE `shop`.`p` FROM `app`@`%`;\n"
        );
        assert_eq!(
            sql(
                &my,
                &change(
                    User,
                    account,
                    PrivilegeScope::Role,
                    ("", "", "'r'@'%'"),
                    &[]
                )
            ),
            "GRANT `r`@`%` TO `app`@`%`;\n"
        );

        let ms = SqlServerDialect;
        assert_eq!(
            sql(
                &ms,
                &change(Login, "app", Global, ("", "", ""), &["VIEW SERVER STATE"])
            ),
            "EXEC master.sys.sp_executesql N'GRANT VIEW SERVER STATE TO app';\n"
        );
        assert_eq!(
            sql(
                &ms,
                &change(
                    Login,
                    "app",
                    PrivilegeScope::Role,
                    ("", "", "dbcreator"),
                    &[]
                )
            ),
            "ALTER SERVER ROLE dbcreator ADD MEMBER app;\n"
        );
        assert_eq!(
            sql(
                &ms,
                &change(User, "app", Schema, ("", "sales", ""), &["SELECT"])
            ),
            "GRANT SELECT ON SCHEMA::sales TO app;\n"
        );
        let mut revoke = change(User, "app", Table, ("", "dbo", "t"), &["DELETE"]);
        revoke.revoke = true;
        assert_eq!(
            sql(&ms, &revoke),
            "REVOKE DELETE ON OBJECT::dbo.t FROM app CASCADE;\n"
        );
        assert_eq!(
            sql(
                &ms,
                &change(
                    User,
                    "app",
                    PrivilegeScope::Role,
                    ("", "", "db_datareader"),
                    &[]
                )
            ),
            "ALTER ROLE db_datareader ADD MEMBER app;\n"
        );
    }

    #[test]
    fn only_the_scope_s_own_privileges_and_names_are_written() {
        use PrincipalKind::{Login, Role, User};
        use PrivilegeScope::*;
        let refused =
            |dialect: &dyn Dialect, c: PrivilegeChange| privilege_plan(dialect, &c).is_err();
        // Typed SQL never becomes a privilege.
        assert!(refused(
            &PostgresDialect,
            change(
                Role,
                "app",
                Table,
                ("", "s", "t"),
                &["SELECT ON t TO x; DROP TABLE t; --"]
            )
        ));
        assert!(refused(
            &PostgresDialect,
            change(Role, "app", Table, ("", "s", "t"), &["CONNECT"])
        ));
        assert!(refused(
            &PostgresDialect,
            change(Role, "app", Table, ("", "s", "t"), &[])
        ));
        assert!(refused(
            &PostgresDialect,
            change(Role, "app", Global, ("", "", ""), &["SELECT"])
        ));
        assert!(refused(
            &PostgresDialect,
            change(Role, "app", Table, ("", "", "t"), &["SELECT"])
        ));
        // A SQL Server login holds server permissions, a user the database's.
        assert!(refused(
            &SqlServerDialect,
            change(Login, "app", Table, ("", "dbo", "t"), &["SELECT"])
        ));
        assert!(refused(
            &SqlServerDialect,
            change(User, "app", Global, ("", "", ""), &["CONNECT SQL"])
        ));
        // Names are quoted, so they cannot add statements.
        let quoted = sql(
            &PostgresDialect,
            &change(
                Role,
                "a\"; DROP ROLE x; --",
                Table,
                ("", "s", "t"),
                &["SELECT"],
            ),
        );
        assert_eq!(
            quoted,
            "GRANT SELECT ON TABLE s.t TO \"a\"\"; DROP ROLE x; --\";\n"
        );
        assert_eq!(PostgresDialect.split_statements(&quoted).len(), 1);
        let quoted = sql(
            &MysqlDialect,
            &change(
                User,
                "'a'@'%'",
                Table,
                ("d`; DROP TABLE t; --", "", "t"),
                &["SELECT"],
            ),
        );
        assert_eq!(MysqlDialect.split_statements(&quoted).len(), 1);
    }

    #[test]
    fn show_grants_lines_become_privileges_and_never_read_the_account() {
        use PrivilegeScope::*;
        let parsed = parse_mysql_grant(
            "GRANT SELECT, INSERT, UPDATE (`a`, `b`) ON `shop`.`orders` TO `app`@`%` WITH GRANT OPTION",
        );
        assert_eq!(
            parsed
                .iter()
                .map(|p| p.privilege.as_str())
                .collect::<Vec<_>>(),
            ["SELECT", "INSERT", "UPDATE (`a`, `b`)"]
        );
        assert!(parsed.iter().all(|p| p.grantable));
        assert_eq!(parsed[0].target.scope, Some(Table));
        assert_eq!(parsed[0].target.database, "shop");
        assert_eq!(parsed[0].target.name, "orders");
        let global =
            parse_mysql_grant("GRANT USAGE ON *.* TO `app`@`%` IDENTIFIED BY PASSWORD '*ABC'");
        assert!(global.is_empty(), "{global:?}");
        let global = parse_mysql_grant("GRANT PROCESS, RELOAD ON *.* TO `app`@`%`");
        assert_eq!(global[0].target.scope, Some(Global));
        assert_eq!(global.len(), 2);
        let db = parse_mysql_grant("GRANT ALL PRIVILEGES ON `we``ird`.* TO `app`@`%`");
        assert_eq!(db[0].target.scope, Some(Database));
        assert_eq!(db[0].target.database, "we`ird");
        assert_eq!(db[0].privilege, "ALL PRIVILEGES");
        let routine = parse_mysql_grant("GRANT EXECUTE ON PROCEDURE `shop`.`p` TO `app`@`%`");
        assert_eq!(routine[0].target.scope, Some(Procedure));
        let roles = parse_mysql_grant("GRANT `reader`@`%`,`writer`@`%` TO `app`@`%`");
        assert_eq!(
            roles
                .iter()
                .map(|r| r.target.name.as_str())
                .collect::<Vec<_>>(),
            ["'reader'@'%'", "'writer'@'%'"]
        );
        let maria = parse_mysql_grant("GRANT `reader` TO `app`@`%` WITH ADMIN OPTION");
        assert_eq!(maria[0].target.name, "reader");
        assert!(maria[0].grantable);
        for line in ["REVOKE x", "GRANT", "GRANT SELECT"] {
            assert!(parse_mysql_grant(line).is_empty(), "{line}");
        }
    }
}
