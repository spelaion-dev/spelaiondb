//! Accounts (ADR-048) on every engine that has them: PostgreSQL, MySQL, MariaDB and SQL Server
//! with their `SPELAION_TEST_*` variables (without them they are skipped, which does not count
//! as tested). Each account is created, logged in with its password, given a new one, locked,
//! unlocked and dropped; and the password is looked for, and never found, in the history, the
//! audit, the store's file and the errors the core returned.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    CoreEvent, NewPrincipal, ObjectError, PrincipalChange, PrincipalKind, PrincipalTarget,
    QueryHistory, ResultStore, Session, apply_principal_change, principal_actions,
    principal_locked, principal_plan,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};
use spelaion_store::{AuditFilter, Store};

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// An engine with an administrator's login, and how a new account logs in.
struct Engine {
    label: &'static str,
    driver: Box<dyn Driver>,
    admin: ConnectParams,
    /// The database a new account connects to.
    database: &'static str,
}

impl Engine {
    async fn session(&self) -> Session {
        session(self.driver.as_ref(), &self.admin).await
    }

    fn params(&self, user: &str, password: &str) -> ConnectParams {
        ConnectParams {
            user: user.into(),
            password: Some(Secret::new(password)),
            database: self.database.into(),
            ..self.admin.clone()
        }
    }

    /// Whether `user` can log in with `password` and run a query.
    async fn logs_in(&self, user: &str, password: &str) -> bool {
        let Ok(conn) = self.driver.connect(&self.params(user, password)).await else {
            return false;
        };
        let session = Session::new(
            conn,
            spelaion_dialect::for_engine(self.driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        let events: Vec<CoreEvent> = session.run_script("SELECT 1").collect().await;
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    }
}

async fn session(driver: &dyn Driver, params: &ConnectParams) -> Session {
    let conn = driver
        .connect(params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

fn engines() -> Vec<Engine> {
    let login =
        |host: String, port: u16, user: &str, password: &str, database: &str| ConnectParams {
            host,
            port,
            user: user.into(),
            password: Some(Secret::new(password)),
            database: database.into(),
            ..Default::default()
        };
    let mut out = Vec::new();
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        out.push(Engine {
            label: "postgres",
            driver: Box::new(spelaion_driver_postgres::PostgresDriver),
            admin: login(host, port, "spelaion", "spelaion", "spelaion"),
            database: "spelaion",
        });
    }
    for (label, var) in [
        ("mysql", "SPELAION_TEST_MYSQL"),
        ("mariadb", "SPELAION_TEST_MARIADB"),
    ] {
        if let Some((host, port)) = server(var) {
            let driver = if label == "mysql" {
                spelaion_driver_mysql::MysqlDriver::mysql()
            } else {
                spelaion_driver_mysql::MysqlDriver::mariadb()
            };
            out.push(Engine {
                label,
                driver: Box::new(driver),
                admin: login(host, port, "root", "spelaion-root", "spelaion"),
                database: "",
            });
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        out.push(Engine {
            label: "sqlserver",
            driver: Box::new(spelaion_driver_sqlserver::SqlServerDriver),
            admin: ConnectParams {
                tls: TlsMode::Require,
                ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
            },
            database: "master",
        });
    }
    out
}

/// Runs statements written by the test, ignoring failures (cleanups of what may not exist).
async fn tolerate(session: &Session, statements: &[String]) {
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in events {
            if let CoreEvent::ResultStarted { result, .. } = event {
                session.results().remove(result);
            }
        }
    }
}

/// The kind that logs in on each engine, and its target as the explorer lists it.
fn login_kind(label: &str) -> PrincipalKind {
    match label {
        "postgres" => PrincipalKind::Role,
        "sqlserver" => PrincipalKind::Login,
        _ => PrincipalKind::User,
    }
}

fn target(label: &str, name: &str) -> PrincipalTarget {
    PrincipalTarget {
        kind: login_kind(label),
        name: match label {
            "mysql" | "mariadb" => format!("'{name}'@'%'"),
            _ => name.into(),
        },
    }
}

fn new_login(label: &str, name: &str) -> NewPrincipal {
    NewPrincipal {
        kind: Some(login_kind(label)),
        name: name.into(),
        with_password: true,
        can_login: true,
        check_policy: false,
        ..Default::default()
    }
}

fn drop_statement(label: &str, name: &str) -> String {
    match label {
        "postgres" => format!("DROP ROLE IF EXISTS {name}"),
        "sqlserver" => format!("IF SUSER_ID(N'{name}') IS NOT NULL DROP LOGIN {name}"),
        _ => format!("DROP USER IF EXISTS '{name}'@'%'"),
    }
}

/// Shows the change's script, then applies it as shown.
async fn apply(
    session: &Session,
    change: &PrincipalChange,
    password: Option<&str>,
) -> Result<usize, ObjectError> {
    let shown = principal_plan(session.dialect().as_ref(), change)?.script();
    let secret = password.map(Secret::new);
    apply_principal_change(
        session,
        change,
        secret.as_ref(),
        &shown,
        false,
        &Safeguards::default(),
        true,
    )
    .await
}

// Each has a quote, a `;` and a backslash, and passes SQL Server's complexity rules.
const FIRST: &str = "Fir'st;Pass\\word-1";
const SECOND: &str = "Sec'ond;Pass\\word-2";

#[tokio::test]
async fn accounts_are_created_logged_in_given_a_new_password_locked_and_dropped() {
    for engine in engines() {
        let label = engine.label;
        let admin = engine.session().await;
        let name = format!("spelaion_acct_{label}");
        tolerate(&admin, &[drop_statement(label, &name)]).await;

        let target = target(label, &name);
        apply(
            &admin,
            &PrincipalChange::Create(new_login(label, &name)),
            Some(FIRST),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: create: {e}"));
        assert!(engine.logs_in(&name, FIRST).await, "{label}: first login");
        assert_eq!(
            principal_locked(&admin, &target).await.unwrap(),
            Some(false),
            "{label}"
        );

        apply(
            &admin,
            &PrincipalChange::SetPassword(target.clone()),
            Some(SECOND),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: password: {e}"));
        assert!(!engine.logs_in(&name, FIRST).await, "{label}: old password");
        assert!(engine.logs_in(&name, SECOND).await, "{label}: new password");

        apply(&admin, &PrincipalChange::Lock(target.clone()), None)
            .await
            .unwrap_or_else(|e| panic!("{label}: lock: {e}"));
        assert!(!engine.logs_in(&name, SECOND).await, "{label}: locked");
        assert_eq!(
            principal_locked(&admin, &target).await.unwrap(),
            Some(true),
            "{label}"
        );
        apply(&admin, &PrincipalChange::Unlock(target.clone()), None)
            .await
            .unwrap_or_else(|e| panic!("{label}: unlock: {e}"));
        assert!(engine.logs_in(&name, SECOND).await, "{label}: unlocked");

        // A drop runs only confirmed.
        let drop = PrincipalChange::Drop(target.clone());
        let shown = principal_plan(admin.dialect().as_ref(), &drop)
            .unwrap()
            .script();
        assert_eq!(
            apply_principal_change(
                &admin,
                &drop,
                None,
                &shown,
                false,
                &Safeguards::default(),
                false
            )
            .await,
            Err(ObjectError::NeedsConfirmation),
            "{label}"
        );
        apply(&admin, &drop, None)
            .await
            .unwrap_or_else(|e| panic!("{label}: drop: {e}"));
        assert!(!engine.logs_in(&name, SECOND).await, "{label}: dropped");
        assert!(
            matches!(
                principal_locked(&admin, &target).await,
                Err(ObjectError::Gone(_))
            ),
            "{label}"
        );
    }
}

#[tokio::test]
async fn roles_and_database_users_are_created_and_dropped() {
    for engine in engines() {
        let label = engine.label;
        let admin = engine.session().await;
        let actions = principal_actions(admin.dialect().as_ref());
        let role = format!("spelaion_role_{label}");
        let cleanup = match label {
            "postgres" => format!("DROP ROLE IF EXISTS {role}"),
            "sqlserver" => format!("DROP ROLE IF EXISTS {role}"),
            _ => format!("DROP ROLE IF EXISTS {role}"),
        };
        tolerate(&admin, &[cleanup]).await;
        assert!(actions.create.contains(&PrincipalKind::Role), "{label}");
        apply(
            &admin,
            &PrincipalChange::Create(NewPrincipal {
                kind: Some(PrincipalKind::Role),
                name: role.clone(),
                ..Default::default()
            }),
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: create role: {e}"));
        // MySQL lists a role as an account; MariaDB and the others by its name.
        let listed = if label == "mysql" {
            format!("'{role}'@'%'")
        } else {
            role.clone()
        };
        apply(
            &admin,
            &PrincipalChange::Drop(PrincipalTarget {
                kind: PrincipalKind::Role,
                name: listed,
            }),
            None,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: drop role: {e}"));

        if label == "sqlserver" {
            // A user of the database for a login, and one without a login.
            let login = "spelaion_user_login";
            tolerate(
                &admin,
                &[
                    "DROP USER IF EXISTS spelaion_user_a".into(),
                    "DROP USER IF EXISTS spelaion_user_b".into(),
                    drop_statement(label, login),
                ],
            )
            .await;
            apply(
                &admin,
                &PrincipalChange::Create(new_login(label, login)),
                Some(FIRST),
            )
            .await
            .unwrap();
            for (name, login) in [("spelaion_user_a", login), ("spelaion_user_b", "")] {
                apply(
                    &admin,
                    &PrincipalChange::Create(NewPrincipal {
                        kind: Some(PrincipalKind::User),
                        name: name.into(),
                        login: login.into(),
                        default_schema: "dbo".into(),
                        ..Default::default()
                    }),
                    None,
                )
                .await
                .unwrap_or_else(|e| panic!("create user {name}: {e}"));
                apply(
                    &admin,
                    &PrincipalChange::Drop(PrincipalTarget {
                        kind: PrincipalKind::User,
                        name: name.into(),
                    }),
                    None,
                )
                .await
                .unwrap_or_else(|e| panic!("drop user {name}: {e}"));
            }
            tolerate(&admin, &[drop_statement(label, login)]).await;
        }
    }
}

fn temp_store(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "spelaion-users-{name}-{}-{nonce}.db",
        std::process::id()
    ))
}

/// Every byte of the store's database and its write-ahead log, as text where it is text.
fn store_bytes(path: &Path) -> String {
    let mut text = String::new();
    for file in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
    ] {
        if let Ok(bytes) = std::fs::read(&file) {
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
    }
    text
}

/// The history recorded the session's `SELECT 1` but nothing with "Leak" (every test password
/// holds it), and neither do the audit or the store's file on disk.
async fn assert_nothing_leaked(history: QueryHistory, path: &Path, label: &str, secrets: &[&str]) {
    let recorded = history
        .list("/repo".into(), "".into(), 100, 0)
        .await
        .unwrap();
    assert!(
        recorded.iter().any(|e| e.statement == "SELECT 1"),
        "{label}: the history records this session"
    );
    for entry in &recorded {
        assert!(
            !entry.statement.contains("Leak")
                && !entry.error.as_deref().unwrap_or("").contains("Leak"),
            "{label}: {entry:?}"
        );
    }
    drop(history);
    let store = Store::open(path).unwrap();
    for record in store
        .audit_events(&AuditFilter::default(), 1000, 0)
        .unwrap()
    {
        assert!(
            !record.entry.statement.contains("Leak")
                && !record
                    .entry
                    .detail
                    .as_deref()
                    .unwrap_or("")
                    .contains("Leak"),
            "{label}: {record:?}"
        );
    }
    drop(store);
    let bytes = store_bytes(path);
    for secret in secrets.iter().copied().chain(["Leak'me", "Leak''me"]) {
        assert!(!bytes.contains(secret), "{label}: {secret} in the store");
    }
    assert!(bytes.contains("SELECT 1"), "{label}: the store was read");
}

/// SQL Server's contained database users (`CREATE USER … WITH PASSWORD`): created in a
/// database with `CONTAINMENT = PARTIAL`, logged in to it with their own password, given a new
/// one and dropped; the passwords never reach the history, the audit, the store or an error.
#[tokio::test]
async fn a_contained_database_user_logs_in_with_its_own_password_and_it_never_leaks() {
    let Some(engine) = engines().into_iter().find(|e| e.label == "sqlserver") else {
        return;
    };
    let label = "sqlserver contained";
    let database = "spelaion_contained";
    tolerate(
        &engine.session().await,
        &[
            "EXEC sp_configure 'contained database authentication', 1".into(),
            "RECONFIGURE".into(),
            format!(
                "IF DB_ID(N'{database}') IS NULL CREATE DATABASE {database} CONTAINMENT = PARTIAL"
            ),
        ],
    )
    .await;
    let contained = Engine {
        label: "sqlserver",
        driver: Box::new(spelaion_driver_sqlserver::SqlServerDriver),
        admin: ConnectParams {
            database: database.into(),
            ..engine.admin.clone()
        },
        database,
    };
    let admin = contained.session().await;
    let name = "spelaion_contained_user";
    tolerate(&admin, &[format!("DROP USER IF EXISTS {name}")]).await;
    let path = temp_store("contained");
    let history = QueryHistory::open(path.clone()).unwrap();
    let events: Vec<CoreEvent> = history
        .run_script(&admin, "SELECT 1", "/repo".into(), "shop".into())
        .collect()
        .await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
        "{label}"
    );

    let first = "Leak'me;Contained\\1-x";
    let second = "Leak'me;Contained\\2-y";
    let user = PrincipalTarget {
        kind: PrincipalKind::User,
        name: name.into(),
    };
    assert!(
        principal_actions(admin.dialect().as_ref())
            .password
            .contains(&PrincipalKind::User)
    );
    let create = PrincipalChange::Create(NewPrincipal {
        kind: Some(PrincipalKind::User),
        name: name.into(),
        with_password: true,
        default_schema: "dbo".into(),
        ..Default::default()
    });
    let shown = principal_plan(admin.dialect().as_ref(), &create)
        .unwrap()
        .script();
    assert!(shown.contains("PASSWORD = N'***'"), "{shown}");
    apply(&admin, &create, Some(first))
        .await
        .unwrap_or_else(|e| panic!("{label}: create: {e}"));
    assert!(contained.logs_in(name, first).await, "{label}: first login");
    // Its password is the database's, not a server login's.
    assert!(!engine.logs_in(name, first).await, "{label}: not a login");

    apply(
        &admin,
        &PrincipalChange::SetPassword(user.clone()),
        Some(second),
    )
    .await
    .unwrap_or_else(|e| panic!("{label}: password: {e}"));
    assert!(
        !contained.logs_in(name, first).await,
        "{label}: old password"
    );
    assert!(
        contained.logs_in(name, second).await,
        "{label}: new password"
    );

    // A user that is not there fails, and the error does not carry the password.
    let error = apply(
        &admin,
        &PrincipalChange::SetPassword(PrincipalTarget {
            kind: PrincipalKind::User,
            name: "spelaion_missing_contained".into(),
        }),
        Some(second),
    )
    .await
    .expect_err("no such user");
    assert!(!error.to_string().contains("Leak"), "{label}: {error}");
    assert_nothing_leaked(history, &path, label, &[first, second]).await;

    apply(&admin, &PrincipalChange::Drop(user), None)
        .await
        .unwrap_or_else(|e| panic!("{label}: drop: {e}"));
    assert!(!contained.logs_in(name, second).await, "{label}: dropped");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_new_password_never_reaches_the_history_the_audit_or_an_error() {
    // MySQL and MariaDB also under NO_BACKSLASH_ESCAPES, where the password's literal is
    // written another way (SDB-262).
    let runs = engines().into_iter().flat_map(|engine| {
        let modes: &[bool] = match engine.label {
            "mysql" | "mariadb" => &[false, true],
            _ => &[false],
        };
        let engine = std::rc::Rc::new(engine);
        modes
            .iter()
            .map(move |no_backslash_escapes| (engine.clone(), *no_backslash_escapes))
            .collect::<Vec<_>>()
    });
    for (engine, no_backslash_escapes) in runs {
        let label = engine.label;
        let admin = engine.session().await;
        let mode = if no_backslash_escapes {
            "nbe"
        } else {
            "default"
        };
        if no_backslash_escapes {
            tolerate(
                &admin,
                &[
                    "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',NO_BACKSLASH_ESCAPES')"
                        .to_string(),
                ],
            )
            .await;
        }
        let path = temp_store(&format!("{label}_{mode}"));
        let history = QueryHistory::open(path.clone()).unwrap();
        let name = format!("spelaion_secret_{label}_{mode}");
        tolerate(&admin, &[drop_statement(label, &name)]).await;

        // The history records what the user runs on this session…
        let events: Vec<CoreEvent> = history
            .run_script(&admin, "SELECT 1", "/repo".into(), "shop".into())
            .collect()
            .await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
            "{label}"
        );
        // …but not an account's statements, nor their passwords.
        let first = "Leak'me;First\\1-x";
        let second = "Leak'me;Second\\2-y";
        let create = PrincipalChange::Create(new_login(label, &name));
        let shown = principal_plan(admin.dialect().as_ref(), &create)
            .unwrap()
            .script();
        assert!(!shown.contains("Leak"), "{label}: {shown}");
        apply(&admin, &create, Some(first))
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        apply(
            &admin,
            &PrincipalChange::SetPassword(target(label, &name)),
            Some(second),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));

        // An account that is not there fails, and the error does not carry the password.
        let missing = format!("spelaion_missing_{label}");
        let error = apply(
            &admin,
            &PrincipalChange::SetPassword(target(label, &missing)),
            Some(second),
        )
        .await
        .expect_err("no such account");
        let message = error.to_string();
        assert!(!message.contains("Leak"), "{label}: {message}");
        // Apply without the password fails before anything runs.
        let error = apply(&admin, &create, None).await.expect_err("no password");
        assert!(!error.to_string().contains("Leak"), "{label}");

        assert_nothing_leaked(history, &path, label, &[first, second]).await;

        assert!(engine.logs_in(&name, second).await, "{label}");
        tolerate(&admin, &[drop_statement(label, &name)]).await;
        let _ = std::fs::remove_file(&path);
    }
}

/// MySQL and MariaDB under NO_BACKSLASH_ESCAPES: the session's splitter reads strings as the
/// server does (SDB-161), so a password with a backslash is written as that mode reads it, and
/// the account logs in with it (SDB-262); the account's lock state is read for a name with a
/// backslash too. PostgreSQL with standard_conforming_strings off: its splitter does not follow
/// that setting, so such a password is still refused before anything runs.
#[tokio::test]
async fn a_backslash_in_a_password_is_written_as_the_sessions_mode_reads_it() {
    for engine in engines() {
        let label = engine.label;
        let setting = match label {
            "mysql" | "mariadb" => {
                "SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, ',NO_BACKSLASH_ESCAPES')"
            }
            "postgres" => "SET standard_conforming_strings = off",
            _ => continue,
        };
        let admin = engine.session().await;
        let name = format!("spelaion_slash_{label}");
        tolerate(&admin, &[drop_statement(label, &name), setting.into()]).await;
        let create = PrincipalChange::Create(new_login(label, &name));
        if label != "postgres" {
            apply(&admin, &create, Some(FIRST))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(engine.logs_in(&name, FIRST).await, "{label}: {FIRST}");
            assert!(
                !engine.logs_in(&name, "Fir'st;Pass\\\\word-1").await,
                "{label}"
            );
            // A user named with a backslash: its lock state is still read.
            let slash = format!("spelaion\\sl_{label}");
            tolerate(&admin, &[format!("DROP USER IF EXISTS `{slash}`@`%`")]).await;
            apply(
                &admin,
                &PrincipalChange::Create(new_login(label, &slash)),
                Some(SECOND),
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            let slash_target = PrincipalTarget {
                kind: PrincipalKind::User,
                name: format!("'{}'@'%'", slash.replace('\\', "\\\\")),
            };
            assert_eq!(
                spelaion_core::principal_locked(&admin, &slash_target)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {e}")),
                Some(false),
                "{label}"
            );
            tolerate(
                &admin,
                &[
                    drop_statement(label, &name),
                    format!("DROP USER IF EXISTS `{slash}`@`%`"),
                ],
            )
            .await;
            continue;
        }
        assert!(
            matches!(
                apply(&admin, &create, Some(FIRST)).await,
                Err(ObjectError::Unsupported(_))
            ),
            "{label}"
        );
        assert!(!engine.logs_in(&name, FIRST).await, "{label}: nothing ran");
        // Without a backslash it is written as usual, quotes doubled.
        apply(&admin, &create, Some("No'slash;here-1"))
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert!(engine.logs_in(&name, "No'slash;here-1").await, "{label}");
        tolerate(&admin, &[drop_statement(label, &name)]).await;
    }
}
