//! MySQL and MariaDB under the session's own `sql_mode` (SDB-262): `NO_BACKSLASH_ESCAPES`
//! makes `\` an ordinary character in a string, and `ANSI_QUOTES` makes `"…"` a name. The
//! editor's checks read a statement as the default mode does and as the session's mode does,
//! and decide from both: the guard asks if either reading is risky, and a read-only
//! connection refuses unless both are reads. The SQL the core writes means the same in the
//! session's mode. Needs `SPELAION_TEST_MYSQL` and `SPELAION_TEST_MARIADB` (testing/docker).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    CoreEvent, NewColumn, NewTable, PrincipalKind, PrincipalTarget, PrivilegeChange,
    PrivilegeScope, PrivilegeTarget, ResultStore, Risk, Session, apply_create_table,
    apply_privilege_change, check_read_only, check_script, create_table_plan, principal_privileges,
    privilege_plan,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, Value};
use spelaion_driver_mysql::MysqlDriver;

/// MySQL and MariaDB as root (accounts and grants), with their labels.
async fn servers() -> Vec<(&'static str, Session)> {
    let mut out = Vec::new();
    for (label, var, driver) in [
        ("mysql", "SPELAION_TEST_MYSQL", MysqlDriver::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", MysqlDriver::mariadb()),
    ] {
        let Ok(addr) = std::env::var(var) else {
            eprintln!("{var} not set: skipped");
            continue;
        };
        let (host, port) = addr.rsplit_once(':').expect("host:port");
        let params = ConnectParams {
            host: host.into(),
            port: port.parse().expect("port"),
            user: "root".into(),
            password: Some(Secret::new("spelaion-root")),
            database: "spelaion".into(),
            ..Default::default()
        };
        let conn = driver.connect(&params).await.expect("connecting");
        out.push((
            label,
            Session::new(conn, driver.dialect(), Arc::new(ResultStore::new())),
        ));
    }
    out
}

/// Runs a script; panics on a failure unless `tolerate`. The first cell of its last result.
async fn run(label: &str, session: &Session, script: &str, tolerate: bool) -> Option<String> {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    let mut cell = None;
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } if !tolerate => {
                panic!("{label}: {script}\n→ {}", error.message)
            }
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(*result).unwrap();
                let stored = stored.read().unwrap();
                cell = stored.value_at(0, 0).map(|v| match v {
                    Value::Null => "NULL".to_string(),
                    other => other.to_string(),
                });
            }
            _ => {}
        }
    }
    cell
}

const MODES: [&str; 3] = ["", "NO_BACKSLASH_ESCAPES", "ANSI_QUOTES"];

/// Sets the session's mode to the server's own plus `extra`; returns the server's own.
async fn set_mode(label: &str, session: &Session, base: &str, extra: &str) {
    let mode = [base, extra]
        .iter()
        .filter(|m| !m.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(",");
    // The statement names sql_mode: the session reads it again afterwards.
    run(
        label,
        session,
        &format!("SET SESSION sql_mode = '{mode}'"),
        false,
    )
    .await;
}

async fn server_mode(label: &str, session: &Session) -> String {
    let mode = run(label, session, "SELECT @@SESSION.sql_mode", false)
        .await
        .unwrap_or_default();
    if mode == "NULL" { String::new() } else { mode }
}

/// A write whose WHERE a quote hides from one reading: the default mode reads
/// `'x\' -- '` as one string followed by the WHERE; under NO_BACKSLASH_ESCAPES the string ends
/// at `\'`, the rest of the line is a comment, and every row is updated. The same with
/// double quotes, which ANSI_QUOTES (a name) and NO_BACKSLASH_ESCAPES (a string without
/// escapes) both end at `\"`.
const HIDDEN_WHERE: &str = r"update t set a = 'x\' -- ' where id = 1";
const HIDDEN_WHERE_DOUBLE: &str = r#"update t set a = "x\" -- " where id = 1"#;
/// A read for the default mode (one string); a call that reaches outside the database for
/// a mode that ends the string at the backslash.
const HIDDEN_CALL: &str = r"select 'a\', load_file('/etc/passwd') -- '";
const HIDDEN_CALL_DOUBLE: &str = r#"select "a\", load_file('/etc/passwd') -- ""#;

#[tokio::test]
async fn the_editors_checks_read_a_statement_as_the_default_and_the_sessions_mode_do() {
    for (label, session) in servers().await {
        let base = server_mode(label, &session).await;
        for extra in MODES {
            set_mode(label, &session, &base, extra).await;
            let case = format!(
                "{label} ({})",
                if extra.is_empty() { "default" } else { extra }
            );
            let dialect = session.dialect();

            // What the server itself makes of the quote: the string ends where the mode says.
            let read = run(&case, &session, r"select 'x\' -- '", true).await;
            let expected = if extra == "NO_BACKSLASH_ESCAPES" {
                r"x\"
            } else {
                "x' -- "
            };
            assert_eq!(read.as_deref(), Some(expected), "{case}");

            let risk = |sql: &str| {
                check_script(dialect.as_ref(), sql, &Safeguards::default(), false)
                    .err()
                    .map(|e| e.risks[0].risk)
            };
            let single_hides = extra == "NO_BACKSLASH_ESCAPES";
            let double_hides = !extra.is_empty();
            assert_eq!(
                risk(HIDDEN_WHERE),
                single_hides.then_some(Risk::UpdateAll),
                "{case}: {HIDDEN_WHERE}"
            );
            assert_eq!(
                risk(HIDDEN_WHERE_DOUBLE),
                double_hides.then_some(Risk::UpdateAll),
                "{case}: {HIDDEN_WHERE_DOUBLE}"
            );
            // Never less than the default reading: a risk it finds is still found.
            assert_eq!(risk("delete from t"), Some(Risk::DeleteAll), "{case}");
            assert_eq!(risk(r"update t set a = 'x\'' where id = 1"), None, "{case}");

            assert_eq!(
                check_read_only(dialect.as_ref(), HIDDEN_CALL).is_err(),
                single_hides,
                "{case}: {HIDDEN_CALL}"
            );
            assert_eq!(
                check_read_only(dialect.as_ref(), HIDDEN_CALL_DOUBLE).is_err(),
                double_hides,
                "{case}: {HIDDEN_CALL_DOUBLE}"
            );
            assert!(
                check_read_only(dialect.as_ref(), "select 1").is_ok(),
                "{case}"
            );
            assert!(
                check_read_only(dialect.as_ref(), "select load_file('/etc/passwd')").is_err(),
                "{case}"
            );
        }
        set_mode(label, &session, &base, "").await;
    }
}

/// A column comment with a backslash is stored as typed, whatever the session's mode.
#[tokio::test]
async fn a_comment_the_core_writes_keeps_its_backslash_in_every_mode() {
    for (label, session) in servers().await {
        let base = server_mode(label, &session).await;
        for extra in MODES {
            set_mode(label, &session, &base, extra).await;
            let case = format!(
                "{label} ({})",
                if extra.is_empty() { "default" } else { extra }
            );
            run(
                &case,
                &session,
                "DROP TABLE IF EXISTS sdb262_comment",
                false,
            )
            .await;
            let table = NewTable {
                schema: "spelaion".into(),
                name: "sdb262_comment".into(),
                columns: vec![NewColumn {
                    name: "path".into(),
                    data_type: "varchar(40)".into(),
                    nullable: true,
                    default: None,
                    comment: Some(r"C:\dir and it's".into()),
                }],
                primary_key: Vec::new(),
                foreign_keys: Vec::new(),
                indexes: Vec::new(),
            };
            let shown = create_table_plan(session.dialect().as_ref(), &table)
                .unwrap()
                .script();
            apply_create_table(
                &session,
                &table,
                &shown,
                false,
                &Safeguards::default(),
                true,
            )
            .await
            .unwrap_or_else(|e| panic!("{case}: {shown}\n→ {e}"));
            let comment = run(
                &case,
                &session,
                "SELECT column_comment FROM information_schema.columns \
                 WHERE table_schema = 'spelaion' AND table_name = 'sdb262_comment'",
                false,
            )
            .await;
            assert_eq!(
                comment.as_deref(),
                Some(r"C:\dir and it's"),
                "{case}: {shown}"
            );
            run(&case, &session, "DROP TABLE sdb262_comment", false).await;
        }
        set_mode(label, &session, &base, "").await;
    }
}

/// A grant for an account whose name and host hold a quote and a backslash applies, and is
/// read back, in every mode.
#[tokio::test]
async fn grants_for_an_account_named_with_a_quote_and_a_backslash_apply_in_every_mode() {
    for (label, session) in servers().await {
        let base = server_mode(label, &session).await;
        for extra in MODES {
            set_mode(label, &session, &base, "").await;
            run(
                label,
                &session,
                r"DROP USER IF EXISTS `o'ne\il`@`h'st`",
                false,
            )
            .await;
            run(
                label,
                &session,
                r"CREATE USER `o'ne\il`@`h'st` IDENTIFIED BY 'Sdb262-pass'",
                false,
            )
            .await;
            set_mode(label, &session, &base, extra).await;
            let case = format!(
                "{label} ({})",
                if extra.is_empty() { "default" } else { extra }
            );
            // As the explorer names it (the driver's `quote_account`).
            let grantee = PrincipalTarget {
                kind: PrincipalKind::User,
                name: r"'o\'ne\\il'@'h\'st'".into(),
            };
            let change = PrivilegeChange {
                grantee: grantee.clone(),
                target: PrivilegeTarget {
                    scope: Some(PrivilegeScope::Database),
                    database: "spelaion".into(),
                    ..Default::default()
                },
                privileges: vec!["SELECT".into()],
                revoke: false,
                grant_option: false,
            };
            let shown = privilege_plan(session.dialect().as_ref(), &change)
                .unwrap()
                .script();
            apply_privilege_change(
                &session,
                &change,
                &shown,
                false,
                &Safeguards::default(),
                true,
            )
            .await
            .unwrap_or_else(|e| panic!("{case}: {shown}\n→ {e}"));
            let granted = principal_privileges(&session, &grantee)
                .await
                .unwrap_or_else(|e| panic!("{case}: {e}"));
            assert!(
                granted
                    .iter()
                    .any(|g| g.privilege.eq_ignore_ascii_case("SELECT")
                        && g.target.database == "spelaion"),
                "{case}: {granted:?}"
            );
            set_mode(label, &session, &base, "").await;
            run(label, &session, r"DROP USER `o'ne\il`@`h'st`", false).await;
        }
    }
}
