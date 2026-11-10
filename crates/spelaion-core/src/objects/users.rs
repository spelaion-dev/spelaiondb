//! Accounts of the server (ADR-048): create a role, user or login, change its password, lock or
//! unlock it, drop it. The form says what to do; the core writes the statements with the
//! engine's syntax, shows them with the password masked, and runs them only as shown.
//!
//! The password is a secret. The script that is shown, compared and returned never holds it:
//! its literal is `'***'`. The real statement is written only on Apply, from a [`Secret`], run
//! as a generated statement (never recorded in the history), and every error it causes is
//! scrubbed of the password before it leaves this module.

use spelaion_config::Safeguards;
use spelaion_driver_api::{Dialect, Secret, Value};

use super::alter::{AlterationPlan, Step, run_plan};
use super::ddl::mysql_string;
use super::{ObjectError, nstring};
use crate::generate::scalar_metadata;
use crate::session::Session;

/// What replaces the password in every script the core shows.
const MASK: &str = "***";

/// An account's kind, as each engine has them: PostgreSQL roles (that may log in); MySQL and
/// MariaDB users (`'name'@'host'`) and roles; SQL Server server logins, and the users and roles
/// of the connection's database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    Role,
    User,
    Login,
}

/// An account the explorer lists, by its kind and its name as the explorer shows it (a MySQL
/// user as `'name'@'host'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalTarget {
    pub kind: PrincipalKind,
    pub name: String,
}

/// A new account. Fields an engine or kind does not have are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewPrincipal {
    pub kind: Option<PrincipalKind>,
    pub name: String,
    /// MySQL and MariaDB users: the host it may connect from (`%` when empty).
    pub host: String,
    /// Whether a password is set. Its value never travels with the form: it is sent on Apply.
    pub with_password: bool,
    /// PostgreSQL: `LOGIN` (a user is a role that may log in).
    pub can_login: bool,
    /// PostgreSQL: `SUPERUSER`, `CREATEDB`, `CREATEROLE`.
    pub superuser: bool,
    pub create_db: bool,
    pub create_role: bool,
    /// SQL Server logins: `DEFAULT_DATABASE` (none when empty).
    pub default_database: String,
    /// SQL Server logins: `CHECK_POLICY = OFF` when false.
    pub check_policy: bool,
    /// SQL Server users: the login it is for (`WITHOUT LOGIN` when empty).
    pub login: String,
    /// SQL Server users: `DEFAULT_SCHEMA` (none when empty).
    pub default_schema: String,
}

/// What to do with an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalChange {
    Create(NewPrincipal),
    SetPassword(PrincipalTarget),
    Lock(PrincipalTarget),
    Unlock(PrincipalTarget),
    Drop(PrincipalTarget),
}

impl PrincipalChange {
    /// Whether Apply needs the password.
    pub fn needs_password(&self) -> bool {
        match self {
            Self::Create(new) => new.with_password,
            Self::SetPassword(_) => true,
            Self::Lock(_) | Self::Unlock(_) | Self::Drop(_) => false,
        }
    }
}

/// Which kinds of account each action has on an engine: the app offers only these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrincipalActions {
    pub create: Vec<PrincipalKind>,
    pub password: Vec<PrincipalKind>,
    pub lock: Vec<PrincipalKind>,
    pub drop: Vec<PrincipalKind>,
    /// Kinds that can be granted privileges (`privileges.rs`).
    pub privileges: Vec<PrincipalKind>,
}

pub fn principal_actions(dialect: &dyn Dialect) -> PrincipalActions {
    use PrincipalKind::{Login, Role, User};
    match dialect.name() {
        "postgres" => PrincipalActions {
            create: vec![Role],
            password: vec![Role],
            lock: vec![Role],
            drop: vec![Role],
            privileges: vec![Role],
        },
        "mysql" => PrincipalActions {
            create: vec![User, Role],
            password: vec![User],
            lock: vec![User],
            drop: vec![User, Role],
            privileges: vec![User, Role],
        },
        "sqlserver" => PrincipalActions {
            create: vec![Login, User, Role],
            password: vec![Login],
            lock: vec![Login],
            drop: vec![Login, User, Role],
            privileges: vec![Login, User, Role],
        },
        _ => PrincipalActions::default(),
    }
}

/// How the password literal is written.
#[derive(Clone, Copy)]
enum Password<'a> {
    /// The script that is shown: `'***'`.
    Masked,
    /// The statement that runs. `backslashes`: the engine reads a backslash in a string as an
    /// escape (MySQL's default `sql_mode`), so it is doubled.
    Real { text: &'a str, backslashes: bool },
}

impl Password<'_> {
    fn literal(self, engine: &str) -> String {
        let (text, backslashes) = match self {
            Self::Masked => (MASK, false),
            Self::Real { text, backslashes } => (text, backslashes),
        };
        match engine {
            "sqlserver" => nstring(text),
            "mysql" if backslashes => mysql_string(text),
            _ => quoted(text),
        }
    }
}

/// The script a change runs, with the password masked: what the dialog shows, and what Apply
/// must be sent back. Nothing runs.
pub fn principal_plan(
    dialect: &dyn Dialect,
    change: &PrincipalChange,
) -> Result<AlterationPlan, ObjectError> {
    plan(dialect, change, Password::Masked)
}

fn plan(
    dialect: &dyn Dialect,
    change: &PrincipalChange,
    password: Password<'_>,
) -> Result<AlterationPlan, ObjectError> {
    let statement = statement(dialect, change, password)?;
    Ok(AlterationPlan {
        before: Vec::new(),
        steps: vec![Step::Run(statement)],
        after: Vec::new(),
        begin: None,
        note: None,
        rebuild: false,
    })
}

/// A name typed in the form: trimmed, not empty, without control characters.
fn checked_name(name: &str) -> Result<&str, ObjectError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ObjectError::NoName);
    }
    if name.chars().any(char::is_control) {
        return Err(ObjectError::Unsupported(
            "a name cannot hold control characters".into(),
        ));
    }
    Ok(name)
}

/// `'name'@'host'` as the explorer shows a MySQL account (its driver's `quote_account`): each
/// part quoted, with `\\` and `\'` inside. `None` when it is not one.
pub(crate) fn parse_account(text: &str) -> Option<(String, String)> {
    fn part(text: &str) -> Option<(String, &str)> {
        let mut chars = text.char_indices();
        if chars.next()?.1 != '\'' {
            return None;
        }
        let mut out = String::new();
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => out.push(chars.next()?.1),
                '\'' => return Some((out, &text[i + 1..])),
                c => out.push(c),
            }
        }
        None
    }
    let (user, rest) = part(text)?;
    let (host, rest) = part(rest.strip_prefix('@')?)?;
    rest.is_empty().then_some((user, host))
}

/// The account part of a MySQL statement: `` `name`@`host` ``, or a role's `` `name` `` (a
/// MariaDB role has no host; the explorer lists it by its bare name).
fn mysql_account(dialect: &dyn Dialect, target: &PrincipalTarget) -> Result<String, ObjectError> {
    match parse_account(&target.name) {
        Some((user, host)) => Ok(format!(
            "{}@{}",
            dialect.quote_identifier(&user),
            dialect.quote_identifier(&host)
        )),
        None if target.kind == PrincipalKind::Role => {
            Ok(dialect.quote_identifier(checked_name(&target.name)?))
        }
        None => Err(ObjectError::Unsupported(format!(
            "{} is not a 'user'@'host' account",
            target.name
        ))),
    }
}

fn unsupported(dialect: &dyn Dialect, what: &str) -> ObjectError {
    ObjectError::Unsupported(format!("{} cannot {what}", engine_label(dialect.name())))
}

fn engine_label(engine: &str) -> &'static str {
    match engine {
        "postgres" => "PostgreSQL",
        "mysql" => "MySQL and MariaDB",
        "sqlserver" => "SQL Server",
        "sqlite" => "SQLite",
        "duckdb" => "DuckDB",
        _ => "this engine",
    }
}

fn kind_label(kind: PrincipalKind) -> &'static str {
    match kind {
        PrincipalKind::Role => "role",
        PrincipalKind::User => "user",
        PrincipalKind::Login => "login",
    }
}

/// The one statement of a change.
fn statement(
    dialect: &dyn Dialect,
    change: &PrincipalChange,
    password: Password<'_>,
) -> Result<String, ObjectError> {
    let engine = dialect.name();
    let actions = principal_actions(dialect);
    let offered = |kinds: &[PrincipalKind], kind: PrincipalKind, what: &str| {
        if kinds.contains(&kind) {
            Ok(())
        } else {
            Err(unsupported(
                dialect,
                &format!("{what} a {}", kind_label(kind)),
            ))
        }
    };
    let q = |name: &str| dialect.quote_identifier(name);
    let pass = password.literal(engine);
    match change {
        PrincipalChange::Create(new) => {
            let kind = new
                .kind
                .ok_or_else(|| ObjectError::Unsupported("choose what to create".into()))?;
            offered(&actions.create, kind, "create")?;
            let name = checked_name(&new.name)?;
            match (engine, kind) {
                ("postgres", _) => {
                    let mut options = Vec::new();
                    if new.can_login {
                        options.push("LOGIN".to_string());
                    }
                    if new.superuser {
                        options.push("SUPERUSER".into());
                    }
                    if new.create_db {
                        options.push("CREATEDB".into());
                    }
                    if new.create_role {
                        options.push("CREATEROLE".into());
                    }
                    if new.with_password {
                        options.push(format!("PASSWORD {pass}"));
                    }
                    let mut sql = format!("CREATE ROLE {}", q(name));
                    if !options.is_empty() {
                        sql.push_str(" WITH ");
                        sql.push_str(&options.join(" "));
                    }
                    Ok(sql)
                }
                ("mysql", PrincipalKind::User) => {
                    let host = match new.host.trim() {
                        "" => "%",
                        host => host,
                    };
                    if host.chars().any(char::is_control) {
                        return Err(ObjectError::Unsupported(
                            "a host cannot hold control characters".into(),
                        ));
                    }
                    let mut sql = format!("CREATE USER {}@{}", q(name), q(host));
                    if new.with_password {
                        sql.push_str(&format!(" IDENTIFIED BY {pass}"));
                    }
                    Ok(sql)
                }
                ("mysql", _) => Ok(format!("CREATE ROLE {}", q(name))),
                ("sqlserver", PrincipalKind::Login) => {
                    if !new.with_password {
                        return Err(ObjectError::Unsupported(
                            "a SQL Server login needs a password".into(),
                        ));
                    }
                    let mut sql = format!("CREATE LOGIN {} WITH PASSWORD = {pass}", q(name));
                    let database = new.default_database.trim();
                    if !database.is_empty() {
                        sql.push_str(&format!(
                            ", DEFAULT_DATABASE = {}",
                            q(checked_name(database)?)
                        ));
                    }
                    if !new.check_policy {
                        sql.push_str(", CHECK_POLICY = OFF");
                    }
                    Ok(sql)
                }
                ("sqlserver", PrincipalKind::User) => {
                    let mut sql = format!("CREATE USER {}", q(name));
                    let login = new.login.trim();
                    if login.is_empty() {
                        sql.push_str(" WITHOUT LOGIN");
                    } else {
                        sql.push_str(&format!(" FOR LOGIN {}", q(checked_name(login)?)));
                    }
                    let schema = new.default_schema.trim();
                    if !schema.is_empty() {
                        sql.push_str(&format!(
                            " WITH DEFAULT_SCHEMA = {}",
                            q(checked_name(schema)?)
                        ));
                    }
                    Ok(sql)
                }
                ("sqlserver", PrincipalKind::Role) => Ok(format!("CREATE ROLE {}", q(name))),
                _ => Err(unsupported(dialect, "create accounts")),
            }
        }
        PrincipalChange::SetPassword(target) => {
            offered(&actions.password, target.kind, "set the password of")?;
            match engine {
                "postgres" => Ok(format!(
                    "ALTER ROLE {} WITH PASSWORD {pass}",
                    q(checked_name(&target.name)?)
                )),
                "mysql" => Ok(format!(
                    "ALTER USER {} IDENTIFIED BY {pass}",
                    mysql_account(dialect, target)?
                )),
                "sqlserver" => Ok(format!(
                    "ALTER LOGIN {} WITH PASSWORD = {pass}",
                    q(checked_name(&target.name)?)
                )),
                _ => Err(unsupported(dialect, "set passwords")),
            }
        }
        PrincipalChange::Lock(target) | PrincipalChange::Unlock(target) => {
            let lock = matches!(change, PrincipalChange::Lock(_));
            offered(
                &actions.lock,
                target.kind,
                if lock { "lock" } else { "unlock" },
            )?;
            match engine {
                "postgres" => Ok(format!(
                    "ALTER ROLE {} WITH {}",
                    q(checked_name(&target.name)?),
                    if lock { "NOLOGIN" } else { "LOGIN" }
                )),
                "mysql" => Ok(format!(
                    "ALTER USER {} ACCOUNT {}",
                    mysql_account(dialect, target)?,
                    if lock { "LOCK" } else { "UNLOCK" }
                )),
                "sqlserver" => Ok(format!(
                    "ALTER LOGIN {} {}",
                    q(checked_name(&target.name)?),
                    if lock { "DISABLE" } else { "ENABLE" }
                )),
                _ => Err(unsupported(dialect, "lock accounts")),
            }
        }
        PrincipalChange::Drop(target) => {
            offered(&actions.drop, target.kind, "drop")?;
            match (engine, target.kind) {
                ("postgres", _) => Ok(format!("DROP ROLE {}", q(checked_name(&target.name)?))),
                ("mysql", PrincipalKind::Role) if parse_account(&target.name).is_none() => {
                    Ok(format!("DROP ROLE {}", mysql_account(dialect, target)?))
                }
                ("mysql", _) => Ok(format!("DROP USER {}", mysql_account(dialect, target)?)),
                ("sqlserver", kind) => Ok(format!(
                    "DROP {} {}",
                    match kind {
                        PrincipalKind::Login => "LOGIN",
                        PrincipalKind::User => "USER",
                        PrincipalKind::Role => "ROLE",
                    },
                    q(checked_name(&target.name)?)
                )),
                _ => Err(unsupported(dialect, "drop accounts")),
            }
        }
    }
}

/// Whether the session's engine reads a backslash in a string literal as an escape: MySQL's
/// default `sql_mode` does, PostgreSQL's `standard_conforming_strings` does not.
async fn backslash_escapes(session: &Session) -> Result<bool, ObjectError> {
    let engine = session.dialect().name();
    let sql = match engine {
        "mysql" => "SELECT @@SESSION.sql_mode",
        "postgres" => "SELECT current_setting('standard_conforming_strings')",
        _ => return Ok(false),
    };
    let setting = match scalar_metadata(session, sql).await {
        Ok(Some(Value::Text(text))) => text,
        Ok(_) => return Ok(false),
        Err(message) => {
            return Err(ObjectError::Failed {
                index: 0,
                total: 1,
                message,
            });
        }
    };
    Ok(if engine == "mysql" {
        !setting
            .to_ascii_uppercase()
            .contains("NO_BACKSLASH_ESCAPES")
    } else {
        setting.eq_ignore_ascii_case("off")
    })
}

/// A standard SQL string literal: quotes doubled.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `message` with every spelling of the password the statement held replaced by `***`, then the
/// audit's own masking.
fn scrub(message: &str, password: Option<&Secret>) -> String {
    let mut out = message.to_string();
    if let Some(password) = password.map(Secret::expose).filter(|p| !p.is_empty()) {
        let doubled = password.replace('\'', "''");
        let escaped = password.replace('\\', "\\\\").replace('\'', "''");
        let mut spellings = vec![escaped, doubled, password.to_string()];
        // The longest first, so a shorter spelling inside a longer one leaves nothing behind.
        spellings.sort_by_key(|s| std::cmp::Reverse(s.len()));
        for spelling in spellings {
            out = out.replace(&spelling, MASK);
        }
    }
    crate::audit::redact_secrets(&out)
}

fn scrubbed(error: ObjectError, password: Option<&Secret>) -> ObjectError {
    match error {
        ObjectError::Failed {
            index,
            total,
            message,
        } => ObjectError::Failed {
            index,
            total,
            message: scrub(&message, password),
        },
        ObjectError::Unsupported(message) => ObjectError::Unsupported(scrub(&message, password)),
        ObjectError::Gone(name) => ObjectError::Gone(scrub(&name, password)),
        other @ (ObjectError::NoName
        | ObjectError::NoColumns
        | ObjectError::SameName
        | ObjectError::ReadOnly
        | ObjectError::NeedsConfirmation
        | ObjectError::Changed
        | ObjectError::TransactionOpen) => other,
    }
}

/// Runs an account change, never on a read-only connection, and only as the script the user was
/// shown (with the password masked). A drop runs only `confirmed` (the dialog is the
/// confirmation), and so do statements the connection's type asks about. The password, when the
/// change sets one, is `password`: it is written into the statement that runs and nowhere else.
/// Returns how many statements ran.
#[allow(clippy::too_many_arguments)]
pub async fn apply_principal_change(
    session: &Session,
    change: &PrincipalChange,
    password: Option<&Secret>,
    shown: &str,
    read_only: bool,
    safeguards: &Safeguards,
    confirmed: bool,
) -> Result<usize, ObjectError> {
    run_change(
        session, change, password, shown, read_only, safeguards, confirmed,
    )
    .await
    .map_err(|error| scrubbed(error, password))
}

async fn run_change(
    session: &Session,
    change: &PrincipalChange,
    password: Option<&Secret>,
    shown: &str,
    read_only: bool,
    safeguards: &Safeguards,
    confirmed: bool,
) -> Result<usize, ObjectError> {
    if read_only {
        return Err(ObjectError::ReadOnly);
    }
    let dialect = session.dialect();
    let masked = principal_plan(dialect.as_ref(), change)?;
    if masked.script() != shown {
        return Err(ObjectError::Changed);
    }
    let needs_confirmation = matches!(change, PrincipalChange::Drop(_))
        || !masked.risks(dialect.as_ref(), safeguards).is_empty();
    if needs_confirmation && !confirmed {
        return Err(ObjectError::NeedsConfirmation);
    }
    let text = match (change.needs_password(), password) {
        (false, _) => None,
        (true, None) => return Err(ObjectError::Unsupported("type the password".into())),
        (true, Some(secret)) => Some(secret.expose()),
    };
    let real = match text {
        None => masked,
        Some(text) => {
            if text.is_empty() {
                return Err(ObjectError::Unsupported("type the password".into()));
            }
            if text.contains('\0') {
                return Err(ObjectError::Unsupported(
                    "a password cannot hold a NUL character".into(),
                ));
            }
            let engine = dialect.name();
            let escapes = backslash_escapes(session).await?;
            // MySQL with NO_BACKSLASH_ESCAPES, PostgreSQL with standard_conforming_strings
            // off: the core's splitter would read the literal otherwise than the engine.
            let disagree = match engine {
                "mysql" => !escapes,
                "postgres" => escapes,
                _ => false,
            };
            if disagree && text.contains('\\') {
                return Err(ObjectError::Unsupported(
                    "this server's settings read a backslash in a string differently: choose a password without one".into(),
                ));
            }
            plan(
                dialect.as_ref(),
                change,
                Password::Real {
                    text,
                    backslashes: engine == "mysql" && escapes,
                },
            )?
        }
    };
    // The real statement must be one statement, as the core's splitter reads it.
    for statement in real.statements() {
        let spans = dialect.split_statements(statement);
        if spans.len() != 1 || spans[0].text(statement).trim() != statement.trim() {
            return Err(ObjectError::Unsupported(
                "the password cannot be written as one string literal".into(),
            ));
        }
    }
    run_plan(session, &real).await
}

/// Whether an account is locked now (PostgreSQL: cannot log in; MySQL and MariaDB: `ACCOUNT
/// LOCK`; SQL Server: disabled), for the lock dialog to offer the other state. `None` when the
/// engine or kind has no lock, or the session cannot read it.
pub async fn principal_locked(
    session: &Session,
    target: &PrincipalTarget,
) -> Result<Option<bool>, ObjectError> {
    let dialect = session.dialect();
    if !principal_actions(dialect.as_ref())
        .lock
        .contains(&target.kind)
    {
        return Ok(None);
    }
    let sql = match dialect.name() {
        "postgres" => format!(
            "SELECT CASE WHEN rolcanlogin THEN 0 ELSE 1 END FROM pg_roles WHERE rolname = {}",
            quoted(&target.name)
        ),
        "mysql" => {
            let Some((user, host)) = parse_account(&target.name) else {
                return Ok(None);
            };
            // MariaDB keeps it in `global_priv`'s JSON, MySQL in `mysql.user`.
            let (user, host) = (mysql_string(&user), mysql_string(&host));
            if session_is_mariadb(session).await {
                format!(
                    "SELECT CASE WHEN JSON_VALUE(Priv, '$.account_locked') IN ('1', 'true') THEN 1 ELSE 0 END \
                     FROM mysql.global_priv WHERE User = {user} AND Host = {host}"
                )
            } else {
                format!(
                    "SELECT CASE WHEN account_locked = 'Y' THEN 1 ELSE 0 END \
                     FROM mysql.user WHERE User = {user} AND Host = {host}"
                )
            }
        }
        "sqlserver" => format!(
            "SELECT CAST(is_disabled AS int) FROM sys.server_principals WHERE name = {}",
            nstring(&target.name)
        ),
        _ => return Ok(None),
    };
    match scalar_metadata(session, &sql).await {
        Ok(Some(Value::Int(n))) => Ok(Some(n != 0)),
        Ok(Some(Value::Decimal(text) | Value::Text(text))) => Ok(Some(text.trim() != "0")),
        Ok(None) => Err(ObjectError::Gone(target.name.clone())),
        // Without the privilege to read the catalog the state is unknown, not an error.
        Ok(_) | Err(_) => Ok(None),
    }
}

async fn session_is_mariadb(session: &Session) -> bool {
    matches!(
        scalar_metadata(session, "SELECT VERSION()").await,
        Ok(Some(Value::Text(version))) if version.to_ascii_lowercase().contains("mariadb")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_mysql::MysqlDialect;
    use spelaion_driver_postgres::PostgresDialect;
    use spelaion_driver_sqlite::SqliteDialect;
    use spelaion_driver_sqlserver::SqlServerDialect;

    fn target(kind: PrincipalKind, name: &str) -> PrincipalTarget {
        PrincipalTarget {
            kind,
            name: name.into(),
        }
    }

    fn script(dialect: &dyn Dialect, change: PrincipalChange) -> String {
        principal_plan(dialect, &change).unwrap().script()
    }

    #[test]
    fn each_engine_writes_its_own_statements_with_the_password_masked() {
        use PrincipalKind::{Login, Role, User};
        let pg = PostgresDialect;
        assert_eq!(
            script(
                &pg,
                PrincipalChange::Create(NewPrincipal {
                    kind: Some(Role),
                    name: "App User".into(),
                    with_password: true,
                    can_login: true,
                    create_db: true,
                    ..Default::default()
                })
            ),
            "CREATE ROLE \"App User\" WITH LOGIN CREATEDB PASSWORD '***';\n"
        );
        assert_eq!(
            script(
                &pg,
                PrincipalChange::Create(NewPrincipal {
                    kind: Some(Role),
                    name: "readers".into(),
                    ..Default::default()
                })
            ),
            "CREATE ROLE readers;\n"
        );
        assert_eq!(
            script(&pg, PrincipalChange::Lock(target(Role, "app"))),
            "ALTER ROLE app WITH NOLOGIN;\n"
        );
        let my = MysqlDialect;
        assert_eq!(
            script(
                &my,
                PrincipalChange::Create(NewPrincipal {
                    kind: Some(User),
                    name: "app".into(),
                    host: " ".into(),
                    with_password: true,
                    ..Default::default()
                })
            ),
            "CREATE USER `app`@`%` IDENTIFIED BY '***';\n"
        );
        assert_eq!(
            script(
                &my,
                PrincipalChange::SetPassword(target(User, "'o\\'neil'@'10.0.%'"))
            ),
            "ALTER USER `o'neil`@`10.0.%` IDENTIFIED BY '***';\n"
        );
        assert_eq!(
            script(&my, PrincipalChange::Drop(target(Role, "reporting"))),
            "DROP ROLE `reporting`;\n"
        );
        // A MySQL role is listed as an account: dropped as one.
        assert_eq!(
            script(&my, PrincipalChange::Drop(target(Role, "'r'@'%'"))),
            "DROP USER `r`@`%`;\n"
        );
        let ms = SqlServerDialect;
        assert_eq!(
            script(
                &ms,
                PrincipalChange::Create(NewPrincipal {
                    kind: Some(Login),
                    name: "app".into(),
                    with_password: true,
                    default_database: "shop".into(),
                    check_policy: false,
                    ..Default::default()
                })
            ),
            "CREATE LOGIN app WITH PASSWORD = N'***', DEFAULT_DATABASE = shop, CHECK_POLICY = OFF;\n"
        );
        assert_eq!(
            script(
                &ms,
                PrincipalChange::Create(NewPrincipal {
                    kind: Some(User),
                    name: "app".into(),
                    login: "app".into(),
                    default_schema: "sales".into(),
                    ..Default::default()
                })
            ),
            "CREATE USER app FOR LOGIN app WITH DEFAULT_SCHEMA = sales;\n"
        );
        assert_eq!(
            script(&ms, PrincipalChange::Unlock(target(Login, "a]b"))),
            "ALTER LOGIN [a]]b] ENABLE;\n"
        );
    }

    #[test]
    fn what_an_engine_or_kind_cannot_do_is_refused() {
        use PrincipalKind::{Login, Role, User};
        let refused = |dialect: &dyn Dialect, change: PrincipalChange| {
            principal_plan(dialect, &change).is_err()
        };
        assert!(refused(
            &SqliteDialect,
            PrincipalChange::Drop(target(Role, "x"))
        ));
        assert!(refused(
            &PostgresDialect,
            PrincipalChange::Drop(target(Login, "x"))
        ));
        assert!(refused(
            &MysqlDialect,
            PrincipalChange::SetPassword(target(Role, "r"))
        ));
        assert!(refused(
            &MysqlDialect,
            PrincipalChange::Lock(target(User, "not an account"))
        ));
        assert!(refused(
            &SqlServerDialect,
            PrincipalChange::Create(NewPrincipal {
                kind: Some(Login),
                name: "nopassword".into(),
                ..Default::default()
            })
        ));
        for name in ["", "  ", "a\nb"] {
            assert!(refused(
                &PostgresDialect,
                PrincipalChange::Create(NewPrincipal {
                    kind: Some(Role),
                    name: name.into(),
                    ..Default::default()
                })
            ));
        }
    }

    #[test]
    fn names_are_quoted_so_they_cannot_add_statements() {
        let sql = script(
            &PostgresDialect,
            PrincipalChange::Drop(target(PrincipalKind::Role, "x\"; DROP TABLE t; --")),
        );
        assert_eq!(sql, "DROP ROLE \"x\"\"; DROP TABLE t; --\";\n");
        let sql = script(
            &MysqlDialect,
            PrincipalChange::Create(NewPrincipal {
                kind: Some(PrincipalKind::User),
                name: "a`; DROP TABLE t; --".into(),
                host: "h`x".into(),
                ..Default::default()
            }),
        );
        assert_eq!(sql, "CREATE USER `a``; DROP TABLE t; --`@`h``x`;\n");
        for (dialect, sql) in [
            (
                &PostgresDialect as &dyn Dialect,
                "DROP ROLE \"x\"\"; DROP TABLE t; --\"",
            ),
            (&MysqlDialect, "CREATE USER `a``; DROP TABLE t; --`@`h``x`"),
        ] {
            assert_eq!(dialect.split_statements(sql).len(), 1);
        }
    }

    #[test]
    fn real_passwords_are_one_literal_on_every_engine() {
        let nasty = "it's; a\\' DROP ROLE x; --";
        let change = PrincipalChange::SetPassword(target(PrincipalKind::Role, "app"));
        let pg = plan(
            &PostgresDialect,
            &change,
            Password::Real {
                text: nasty,
                backslashes: false,
            },
        )
        .unwrap();
        let sql = pg.statements()[0];
        assert_eq!(
            sql,
            "ALTER ROLE app WITH PASSWORD 'it''s; a\\'' DROP ROLE x; --'"
        );
        assert_eq!(PostgresDialect.split_statements(sql).len(), 1);
        let change = PrincipalChange::SetPassword(target(PrincipalKind::User, "'app'@'%'"));
        let my = plan(
            &MysqlDialect,
            &change,
            Password::Real {
                text: nasty,
                backslashes: true,
            },
        )
        .unwrap();
        let sql = my.statements()[0];
        assert_eq!(
            sql,
            "ALTER USER `app`@`%` IDENTIFIED BY 'it''s; a\\\\'' DROP ROLE x; --'"
        );
        assert_eq!(MysqlDialect.split_statements(sql).len(), 1);
        let change = PrincipalChange::SetPassword(target(PrincipalKind::Login, "app"));
        let ms = plan(
            &SqlServerDialect,
            &change,
            Password::Real {
                text: nasty,
                backslashes: false,
            },
        )
        .unwrap();
        let sql = ms.statements()[0];
        assert_eq!(
            sql,
            "ALTER LOGIN app WITH PASSWORD = N'it''s; a\\'' DROP ROLE x; --'"
        );
        assert_eq!(SqlServerDialect.split_statements(sql).len(), 1);
        // The masked script never holds it.
        for dialect in [
            &PostgresDialect as &dyn Dialect,
            &MysqlDialect,
            &SqlServerDialect,
        ] {
            let kind = principal_actions(dialect).password[0];
            let name = if dialect.name() == "mysql" {
                "'app'@'%'"
            } else {
                "app"
            };
            let shown = script(dialect, PrincipalChange::SetPassword(target(kind, name)));
            assert!(shown.contains("'***'"), "{shown}");
            assert!(!shown.contains("it''s"), "{shown}");
        }
    }

    #[test]
    fn errors_lose_every_spelling_of_the_password() {
        let secret = Secret::new("p'a\\ss");
        for message in [
            "near 'p''a\\\\ss' at line 1",
            "near 'p''a\\ss'",
            "password p'a\\ss is too weak",
        ] {
            let scrubbed = scrub(message, Some(&secret));
            assert!(!scrubbed.contains("a\\ss"), "{scrubbed}");
            assert!(!scrubbed.contains("p'a"), "{scrubbed}");
        }
        assert_eq!(scrub("PASSWORD = N'other'", None), "PASSWORD = N'***'");
    }

    #[test]
    fn accounts_parse_back_from_the_explorer_s_names() {
        assert_eq!(parse_account("'app'@'%'"), Some(("app".into(), "%".into())));
        assert_eq!(
            parse_account("'o\\'ne\\\\il'@'local host'"),
            Some(("o'ne\\il".into(), "local host".into()))
        );
        for text in ["app", "'app'", "'app'@", "'app'@'%'x", "'a@'b'"] {
            assert_eq!(parse_account(text), None, "{text}");
        }
    }
}
