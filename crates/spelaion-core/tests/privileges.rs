//! Privileges (ADR-048) on PostgreSQL, MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*`
//! variables (without them they are skipped, which does not count as tested). Each grant (on
//! objects, some columns, or PostgreSQL's default privileges of objects created later) is
//! applied as shown, then tried by logging in as the account; what the account has is read back
//! from the catalog, and each revoke takes the access away again.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    CoreEvent, GrantedPrivilege, NewPrincipal, PrincipalChange, PrincipalKind, PrincipalTarget,
    PrivilegeChange, PrivilegeScope, PrivilegeTarget, ResultStore, Session, apply_principal_change,
    apply_privilege_change, principal_plan, principal_privileges, privilege_plan,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

const PASSWORD: &str = "Priv'Test;Pass-1";

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn login(host: &str, port: u16, user: &str, password: &str, database: &str) -> ConnectParams {
    ConnectParams {
        host: host.into(),
        port,
        user: user.into(),
        password: Some(Secret::new(password)),
        database: database.into(),
        ..Default::default()
    }
}

async fn session(driver: &dyn Driver, params: &ConnectParams) -> Session {
    let conn = driver
        .connect(params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

/// Runs statements written by the test; `tolerate` ignores failures (cleanups).
async fn run(session: &Session, statements: &[&str], tolerate: bool) {
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in events {
            match event {
                CoreEvent::StatementFailed { error, .. } if !tolerate => {
                    panic!("{statement}\n→ {}", error.message)
                }
                CoreEvent::ResultStarted { result, .. } => {
                    session.results().remove(result);
                }
                _ => {}
            }
        }
    }
}

/// Whether `params` can log in and run `sql` without an error.
async fn can_run(driver: &dyn Driver, params: &ConnectParams, sql: &str) -> bool {
    let Ok(conn) = driver.connect(params).await else {
        return false;
    };
    let session = Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()));
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    !events
        .iter()
        .any(|e| matches!(e, CoreEvent::StatementFailed { .. }))
}

async fn account(session: &Session, change: PrincipalChange, password: Option<&str>) {
    let shown = principal_plan(session.dialect().as_ref(), &change)
        .unwrap()
        .script();
    let secret = password.map(Secret::new);
    apply_principal_change(
        session,
        &change,
        secret.as_ref(),
        &shown,
        false,
        &Safeguards::default(),
        true,
    )
    .await
    .unwrap_or_else(|e| panic!("{change:?}: {e}"));
}

fn target(scope: PrivilegeScope, database: &str, schema: &str, name: &str) -> PrivilegeTarget {
    PrivilegeTarget {
        scope: Some(scope),
        database: database.into(),
        schema: schema.into(),
        name: name.into(),
        columns: Vec::new(),
    }
}

/// A column privilege's target: some columns of a table.
fn columns(database: &str, schema: &str, table: &str, columns: &[&str]) -> PrivilegeTarget {
    PrivilegeTarget {
        columns: columns.iter().map(|c| (*c).to_string()).collect(),
        ..target(PrivilegeScope::Column, database, schema, table)
    }
}

/// Whether the account has `privilege` on exactly these columns of `table`.
fn has_columns(granted: &[GrantedPrivilege], table: &str, privilege: &str, cols: &[&str]) -> bool {
    granted.iter().any(|g| {
        g.target.scope == Some(PrivilegeScope::Column)
            && g.target.name == table
            && g.privilege.eq_ignore_ascii_case(privilege)
            && g.target.columns == cols
            && !g.denied
    })
}

/// Grants (or revokes) as shown.
async fn privilege(
    session: &Session,
    grantee: &PrincipalTarget,
    on: PrivilegeTarget,
    privileges: &[&str],
    revoke: bool,
) {
    let change = PrivilegeChange {
        grantee: grantee.clone(),
        target: on,
        privileges: privileges.iter().map(|p| (*p).to_string()).collect(),
        revoke,
        grant_option: false,
    };
    let shown = privilege_plan(session.dialect().as_ref(), &change)
        .unwrap()
        .script();
    apply_privilege_change(
        session,
        &change,
        &shown,
        false,
        &Safeguards::default(),
        false,
    )
    .await
    .unwrap_or_else(|e| panic!("{change:?}: {e}"));
}

fn has(granted: &[GrantedPrivilege], scope: PrivilegeScope, name: &str, privilege: &str) -> bool {
    granted.iter().any(|g| {
        g.target.scope == Some(scope)
            && (g.target.name == name
                || g.target.schema == name
                || g.target.database == name
                || name.is_empty())
            && g.privilege.eq_ignore_ascii_case(privilege)
            && !g.denied
    })
}

#[tokio::test]
async fn postgres_grants_on_a_database_a_schema_a_table_columns_a_role_and_later_objects() {
    let Some((host, port)) = server("SPELAION_TEST_PG") else {
        return;
    };
    let driver = spelaion_driver_postgres::PostgresDriver;
    let admin = session(
        &driver,
        &login(&host, port, "spelaion", "spelaion", "spelaion"),
    )
    .await;
    let cleanup = [
        "DROP SCHEMA IF EXISTS priv_s CASCADE",
        // Default privileges left by a failed run keep the role from being dropped.
        "DROP OWNED BY priv_acct",
        "DROP ROLE IF EXISTS priv_acct",
        "DROP ROLE IF EXISTS priv_reader",
    ];
    run(&admin, &cleanup, true).await;
    run(
        &admin,
        &[
            "CREATE SCHEMA priv_s",
            "CREATE TABLE priv_s.t (id int, secret int)",
            "INSERT INTO priv_s.t VALUES (1, 2)",
        ],
        false,
    )
    .await;
    for (name, login_role) in [("priv_acct", true), ("priv_reader", false)] {
        account(
            &admin,
            PrincipalChange::Create(NewPrincipal {
                kind: Some(PrincipalKind::Role),
                name: name.into(),
                with_password: login_role,
                can_login: login_role,
                ..Default::default()
            }),
            login_role.then_some(PASSWORD),
        )
        .await;
    }
    let acct = PrincipalTarget {
        kind: PrincipalKind::Role,
        name: "priv_acct".into(),
    };
    let reader = PrincipalTarget {
        kind: PrincipalKind::Role,
        name: "priv_reader".into(),
    };
    let as_acct = login(&host, port, "priv_acct", PASSWORD, "spelaion");
    let select = "SELECT id FROM priv_s.t";
    assert!(
        !can_run(&driver, &as_acct, select).await,
        "nothing granted yet"
    );

    privilege(
        &admin,
        &acct,
        target(PrivilegeScope::Database, "spelaion", "", ""),
        &["CONNECT"],
        false,
    )
    .await;
    privilege(
        &admin,
        &acct,
        target(PrivilegeScope::Schema, "", "priv_s", ""),
        &["USAGE"],
        false,
    )
    .await;
    privilege(
        &admin,
        &acct,
        target(PrivilegeScope::Table, "", "priv_s", "t"),
        &["SELECT"],
        false,
    )
    .await;
    assert!(can_run(&driver, &as_acct, select).await, "granted");
    let granted = principal_privileges(&admin, &acct).await.unwrap();
    assert!(
        has(&granted, PrivilegeScope::Database, "spelaion", "CONNECT"),
        "{granted:?}"
    );
    assert!(
        has(&granted, PrivilegeScope::Schema, "priv_s", "USAGE"),
        "{granted:?}"
    );
    assert!(
        has(&granted, PrivilegeScope::Table, "t", "SELECT"),
        "{granted:?}"
    );

    privilege(
        &admin,
        &acct,
        target(PrivilegeScope::Table, "", "priv_s", "t"),
        &["SELECT"],
        true,
    )
    .await;
    assert!(!can_run(&driver, &as_acct, select).await, "revoked");
    let granted = principal_privileges(&admin, &acct).await.unwrap();
    assert!(
        !has(&granted, PrivilegeScope::Table, "t", "SELECT"),
        "{granted:?}"
    );

    // Through a role: every table of the schema to the role, the role to the account.
    privilege(
        &admin,
        &reader,
        target(PrivilegeScope::Schema, "", "priv_s", ""),
        &["USAGE"],
        false,
    )
    .await;
    privilege(
        &admin,
        &reader,
        target(PrivilegeScope::AllTablesInSchema, "", "priv_s", ""),
        &["SELECT"],
        false,
    )
    .await;
    privilege(
        &admin,
        &acct,
        target(PrivilegeScope::Role, "", "", "priv_reader"),
        &[],
        false,
    )
    .await;
    assert!(can_run(&driver, &as_acct, select).await, "through the role");
    let granted = principal_privileges(&admin, &acct).await.unwrap();
    assert!(
        has(&granted, PrivilegeScope::Role, "priv_reader", "MEMBER"),
        "{granted:?}"
    );
    privilege(
        &admin,
        &acct,
        target(PrivilegeScope::Role, "", "", "priv_reader"),
        &[],
        true,
    )
    .await;
    assert!(!can_run(&driver, &as_acct, select).await, "role revoked");

    // Some columns: `id`, not `secret`.
    let on_id = columns("", "priv_s", "t", &["id"]);
    privilege(&admin, &acct, on_id.clone(), &["SELECT", "UPDATE"], false).await;
    assert!(can_run(&driver, &as_acct, select).await, "column");
    assert!(
        can_run(&driver, &as_acct, "UPDATE priv_s.t SET id = 1").await,
        "column update"
    );
    assert!(
        !can_run(&driver, &as_acct, "SELECT secret FROM priv_s.t").await,
        "another column"
    );
    let granted = principal_privileges(&admin, &acct).await.unwrap();
    assert!(has_columns(&granted, "t", "SELECT", &["id"]), "{granted:?}");
    assert!(has_columns(&granted, "t", "UPDATE", &["id"]), "{granted:?}");
    privilege(&admin, &acct, on_id, &["SELECT", "UPDATE"], true).await;
    assert!(!can_run(&driver, &as_acct, select).await, "column revoked");
    let granted = principal_privileges(&admin, &acct).await.unwrap();
    assert!(
        !granted
            .iter()
            .any(|g| g.target.scope == Some(PrivilegeScope::Column)),
        "{granted:?}"
    );

    // Default privileges: tables the administrator creates later in the schema, then in every
    // schema; each revoked as "What it has" lists it.
    for schema in ["priv_s", ""] {
        privilege(
            &admin,
            &acct,
            target(PrivilegeScope::DefaultTables, "", schema, ""),
            &["SELECT"],
            false,
        )
        .await;
        let table = format!("priv_s.later_{}", schema.len());
        run(&admin, &[&format!("CREATE TABLE {table} (id int)")], false).await;
        let read_later = format!("SELECT id FROM {table}");
        assert!(
            can_run(&driver, &as_acct, &read_later).await,
            "default privileges in '{schema}'"
        );
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        let row = granted
            .iter()
            .find(|g| g.target.scope == Some(PrivilegeScope::DefaultTables))
            .unwrap_or_else(|| panic!("{granted:?}"));
        assert_eq!(row.target.schema, schema, "{granted:?}");
        assert_eq!(row.target.name, "spelaion", "the role that creates them");
        assert!(row.privilege.eq_ignore_ascii_case("SELECT"), "{row:?}");
        // As listed: FOR ROLE spelaion.
        privilege(&admin, &acct, row.target.clone(), &["SELECT"], true).await;
        let after = format!("priv_s.after_{}", schema.len());
        run(&admin, &[&format!("CREATE TABLE {after} (id int)")], false).await;
        assert!(
            !can_run(&driver, &as_acct, &format!("SELECT id FROM {after}")).await,
            "default privileges revoked in '{schema}'"
        );
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            !granted
                .iter()
                .any(|g| g.target.scope == Some(PrivilegeScope::DefaultTables)),
            "{granted:?}"
        );
    }
    // Sequences and functions created later, naming the role that creates them. Each new
    // object's own privileges then name the account (PUBLIC runs every function anyway).
    for (scope, privilege_name, create, object, name) in [
        (
            PrivilegeScope::DefaultSequences,
            "USAGE",
            "CREATE SEQUENCE priv_s.later_seq",
            PrivilegeScope::Sequence,
            "later_seq",
        ),
        (
            PrivilegeScope::DefaultFunctions,
            "EXECUTE",
            "CREATE FUNCTION priv_s.later_fn() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            PrivilegeScope::Function,
            "later_fn",
        ),
    ] {
        let on = target(scope, "", "priv_s", "spelaion");
        privilege(&admin, &acct, on.clone(), &[privilege_name], false).await;
        run(&admin, &[create], false).await;
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            has(&granted, scope, "priv_s", privilege_name),
            "{granted:?}"
        );
        assert!(
            has(&granted, object, name, privilege_name),
            "{scope:?}: {granted:?}"
        );
        privilege(&admin, &acct, on, &[privilege_name], true).await;
    }
    assert!(
        can_run(&driver, &as_acct, "SELECT nextval('priv_s.later_seq')").await,
        "the sequence created while the default applied"
    );

    run(
        &admin,
        &[
            "DROP SCHEMA IF EXISTS priv_s CASCADE",
            "REVOKE ALL ON DATABASE spelaion FROM priv_acct",
        ],
        true,
    )
    .await;
    run(&admin, &cleanup, true).await;
}

#[tokio::test]
async fn mysql_and_mariadb_grants_globally_on_a_database_a_table_columns_and_a_role() {
    for (label, var) in [
        ("mysql", "SPELAION_TEST_MYSQL"),
        ("mariadb", "SPELAION_TEST_MARIADB"),
    ] {
        let Some((host, port)) = server(var) else {
            continue;
        };
        let driver = if label == "mysql" {
            spelaion_driver_mysql::MysqlDriver::mysql()
        } else {
            spelaion_driver_mysql::MysqlDriver::mariadb()
        };
        let admin = session(
            &driver,
            &login(&host, port, "root", "spelaion-root", "spelaion"),
        )
        .await;
        let cleanup = [
            "DROP USER IF EXISTS 'priv_acct'@'%'",
            "DROP ROLE IF EXISTS priv_reader",
            "DROP TABLE IF EXISTS spelaion.priv_t",
        ];
        run(&admin, &cleanup, true).await;
        run(
            &admin,
            &[
                "CREATE TABLE spelaion.priv_t (id int, secret int)",
                "INSERT INTO spelaion.priv_t VALUES (1, 2)",
            ],
            false,
        )
        .await;
        account(
            &admin,
            PrincipalChange::Create(NewPrincipal {
                kind: Some(PrincipalKind::User),
                name: "priv_acct".into(),
                with_password: true,
                ..Default::default()
            }),
            Some(PASSWORD),
        )
        .await;
        let acct = PrincipalTarget {
            kind: PrincipalKind::User,
            name: "'priv_acct'@'%'".into(),
        };
        let as_acct = login(&host, port, "priv_acct", PASSWORD, "");
        let select = "SELECT id FROM spelaion.priv_t";
        assert!(
            !can_run(&driver, &as_acct, select).await,
            "{label}: nothing yet"
        );

        let table = target(PrivilegeScope::Table, "spelaion", "", "priv_t");
        privilege(&admin, &acct, table.clone(), &["SELECT"], false).await;
        assert!(can_run(&driver, &as_acct, select).await, "{label}: table");
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            has(&granted, PrivilegeScope::Table, "priv_t", "SELECT"),
            "{label}: {granted:?}"
        );
        privilege(&admin, &acct, table, &["SELECT"], true).await;
        assert!(
            !can_run(&driver, &as_acct, select).await,
            "{label}: table revoked"
        );

        // Some columns: `id`, not `secret`.
        let on_id = columns("spelaion", "", "priv_t", &["id"]);
        privilege(&admin, &acct, on_id.clone(), &["SELECT", "UPDATE"], false).await;
        assert!(can_run(&driver, &as_acct, select).await, "{label}: column");
        assert!(
            can_run(&driver, &as_acct, "UPDATE spelaion.priv_t SET id = 1").await,
            "{label}: column update"
        );
        assert!(
            !can_run(&driver, &as_acct, "SELECT secret FROM spelaion.priv_t").await,
            "{label}: another column"
        );
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            has_columns(&granted, "priv_t", "SELECT", &["id"]),
            "{label}: {granted:?}"
        );
        assert!(
            has_columns(&granted, "priv_t", "UPDATE", &["id"]),
            "{label}: {granted:?}"
        );
        privilege(&admin, &acct, on_id, &["SELECT", "UPDATE"], true).await;
        assert!(
            !can_run(&driver, &as_acct, select).await,
            "{label}: column revoked"
        );
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            !granted
                .iter()
                .any(|g| g.target.scope == Some(PrivilegeScope::Column)),
            "{label}: {granted:?}"
        );

        let database = target(PrivilegeScope::Database, "spelaion", "", "");
        privilege(&admin, &acct, database.clone(), &["SELECT"], false).await;
        assert!(
            can_run(&driver, &as_acct, select).await,
            "{label}: database"
        );
        privilege(&admin, &acct, database, &["SELECT"], true).await;

        let global = target(PrivilegeScope::Global, "", "", "");
        privilege(&admin, &acct, global.clone(), &["PROCESS"], false).await;
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            has(&granted, PrivilegeScope::Global, "", "PROCESS"),
            "{label}: {granted:?}"
        );
        assert!(
            !has(&granted, PrivilegeScope::Database, "spelaion", "SELECT"),
            "{label}: {granted:?}"
        );
        privilege(&admin, &acct, global, &["PROCESS"], true).await;

        // A role: MySQL lists it as an account, MariaDB by its name.
        account(
            &admin,
            PrincipalChange::Create(NewPrincipal {
                kind: Some(PrincipalKind::Role),
                name: "priv_reader".into(),
                ..Default::default()
            }),
            None,
        )
        .await;
        let role_name = if label == "mysql" {
            "'priv_reader'@'%'"
        } else {
            "priv_reader"
        };
        let reader = PrincipalTarget {
            kind: PrincipalKind::Role,
            name: role_name.into(),
        };
        privilege(
            &admin,
            &reader,
            target(PrivilegeScope::Table, "spelaion", "", "priv_t"),
            &["SELECT"],
            false,
        )
        .await;
        privilege(
            &admin,
            &acct,
            target(PrivilegeScope::Role, "", "", role_name),
            &[],
            false,
        )
        .await;
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            has(&granted, PrivilegeScope::Role, role_name, "MEMBER"),
            "{label}: {granted:?}"
        );
        let role_granted = principal_privileges(&admin, &reader).await.unwrap();
        assert!(
            has(&role_granted, PrivilegeScope::Table, "priv_t", "SELECT"),
            "{label}: {role_granted:?}"
        );
        privilege(
            &admin,
            &acct,
            target(PrivilegeScope::Role, "", "", role_name),
            &[],
            true,
        )
        .await;
        let granted = principal_privileges(&admin, &acct).await.unwrap();
        assert!(
            !has(&granted, PrivilegeScope::Role, role_name, "MEMBER"),
            "{label}: {granted:?}"
        );

        run(&admin, &cleanup, true).await;
    }
}

#[tokio::test]
async fn sql_server_grants_on_the_server_a_database_a_schema_an_object_columns_and_roles() {
    let Some((host, port)) = server("SPELAION_TEST_MSSQL") else {
        return;
    };
    let driver = spelaion_driver_sqlserver::SqlServerDriver;
    let tls = |params: ConnectParams| ConnectParams {
        tls: TlsMode::Require,
        ..params
    };
    let admin = session(
        &driver,
        &tls(login(&host, port, "sa", "Spelaion-test-1", "tempdb")),
    )
    .await;
    let cleanup = [
        "DROP TABLE IF EXISTS priv_s.t",
        "DROP SCHEMA IF EXISTS priv_s",
        "DROP USER IF EXISTS priv_user",
        "IF SUSER_ID(N'priv_login') IS NOT NULL DROP LOGIN priv_login",
    ];
    run(&admin, &cleanup, true).await;
    account(
        &admin,
        PrincipalChange::Create(NewPrincipal {
            kind: Some(PrincipalKind::Login),
            name: "priv_login".into(),
            with_password: true,
            check_policy: false,
            ..Default::default()
        }),
        Some(PASSWORD),
    )
    .await;
    account(
        &admin,
        PrincipalChange::Create(NewPrincipal {
            kind: Some(PrincipalKind::User),
            name: "priv_user".into(),
            login: "priv_login".into(),
            ..Default::default()
        }),
        None,
    )
    .await;
    run(
        &admin,
        &[
            "CREATE SCHEMA priv_s",
            "CREATE TABLE priv_s.t (id int, secret int)",
            "INSERT INTO priv_s.t VALUES (1, 2)",
        ],
        false,
    )
    .await;
    let login_target = PrincipalTarget {
        kind: PrincipalKind::Login,
        name: "priv_login".into(),
    };
    let user = PrincipalTarget {
        kind: PrincipalKind::User,
        name: "priv_user".into(),
    };
    let as_login = tls(login(&host, port, "priv_login", PASSWORD, "tempdb"));
    let select = "SELECT id FROM priv_s.t";
    assert!(!can_run(&driver, &as_login, select).await, "nothing yet");

    // The server: a permission and a server role, on the login.
    privilege(
        &admin,
        &login_target,
        target(PrivilegeScope::Global, "", "", ""),
        &["VIEW SERVER STATE"],
        false,
    )
    .await;
    privilege(
        &admin,
        &login_target,
        target(PrivilegeScope::Role, "", "", "dbcreator"),
        &[],
        false,
    )
    .await;
    let granted = principal_privileges(&admin, &login_target).await.unwrap();
    assert!(
        has(&granted, PrivilegeScope::Global, "", "VIEW SERVER STATE"),
        "{granted:?}"
    );
    assert!(
        has(&granted, PrivilegeScope::Role, "dbcreator", "MEMBER"),
        "{granted:?}"
    );
    privilege(
        &admin,
        &login_target,
        target(PrivilegeScope::Global, "", "", ""),
        &["VIEW SERVER STATE"],
        true,
    )
    .await;
    privilege(
        &admin,
        &login_target,
        target(PrivilegeScope::Role, "", "", "dbcreator"),
        &[],
        true,
    )
    .await;
    let granted = principal_privileges(&admin, &login_target).await.unwrap();
    assert!(
        !has(&granted, PrivilegeScope::Role, "dbcreator", "MEMBER"),
        "{granted:?}"
    );

    // The database's user: a schema, then one object, then a database role.
    let schema = target(PrivilegeScope::Schema, "", "priv_s", "");
    privilege(&admin, &user, schema.clone(), &["SELECT"], false).await;
    assert!(can_run(&driver, &as_login, select).await, "schema");
    let granted = principal_privileges(&admin, &user).await.unwrap();
    assert!(
        has(&granted, PrivilegeScope::Schema, "priv_s", "SELECT"),
        "{granted:?}"
    );
    privilege(&admin, &user, schema, &["SELECT"], true).await;
    assert!(!can_run(&driver, &as_login, select).await, "schema revoked");

    let table = target(PrivilegeScope::Table, "", "priv_s", "t");
    privilege(&admin, &user, table.clone(), &["SELECT"], false).await;
    assert!(can_run(&driver, &as_login, select).await, "table");
    let granted = principal_privileges(&admin, &user).await.unwrap();
    assert!(
        has(&granted, PrivilegeScope::Table, "t", "SELECT"),
        "{granted:?}"
    );
    privilege(&admin, &user, table, &["SELECT"], true).await;
    assert!(!can_run(&driver, &as_login, select).await, "table revoked");

    // Some columns: `id`, not `secret`.
    let on_id = columns("", "priv_s", "t", &["id"]);
    privilege(&admin, &user, on_id.clone(), &["SELECT", "UPDATE"], false).await;
    assert!(can_run(&driver, &as_login, select).await, "column");
    assert!(
        can_run(&driver, &as_login, "UPDATE priv_s.t SET id = 1").await,
        "column update"
    );
    assert!(
        !can_run(&driver, &as_login, "SELECT secret FROM priv_s.t").await,
        "another column"
    );
    let granted = principal_privileges(&admin, &user).await.unwrap();
    assert!(has_columns(&granted, "t", "SELECT", &["id"]), "{granted:?}");
    assert!(has_columns(&granted, "t", "UPDATE", &["id"]), "{granted:?}");
    privilege(&admin, &user, on_id, &["SELECT", "UPDATE"], true).await;
    assert!(!can_run(&driver, &as_login, select).await, "column revoked");
    let granted = principal_privileges(&admin, &user).await.unwrap();
    assert!(
        !granted
            .iter()
            .any(|g| g.target.scope == Some(PrivilegeScope::Column)),
        "{granted:?}"
    );

    privilege(
        &admin,
        &user,
        target(PrivilegeScope::Database, "", "", ""),
        &["CREATE TABLE"],
        false,
    )
    .await;
    privilege(
        &admin,
        &user,
        target(PrivilegeScope::Role, "", "", "db_datareader"),
        &[],
        false,
    )
    .await;
    assert!(can_run(&driver, &as_login, select).await, "db_datareader");
    let granted = principal_privileges(&admin, &user).await.unwrap();
    assert!(
        has(&granted, PrivilegeScope::Database, "", "CREATE TABLE"),
        "{granted:?}"
    );
    assert!(
        has(&granted, PrivilegeScope::Role, "db_datareader", "MEMBER"),
        "{granted:?}"
    );
    privilege(
        &admin,
        &user,
        target(PrivilegeScope::Role, "", "", "db_datareader"),
        &[],
        true,
    )
    .await;
    assert!(!can_run(&driver, &as_login, select).await, "role revoked");

    run(&admin, &cleanup, true).await;
}
