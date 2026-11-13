//! The server's own output reaches a run as notices (ADR-035): PostgreSQL's notices, SQL
//! Server's `PRINT` and low-severity `RAISERROR`, MySQL's and MariaDB's warnings and notes.
//! SQLite and DuckDB have none. Needs `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`,
//! `SPELAION_TEST_MARIADB` and `SPELAION_TEST_MSSQL` (skipped without them, which does not count
//! as tested).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};
use spelaion_driver_mysql::MysqlDriver;
use spelaion_driver_postgres::PostgresDriver;
use spelaion_driver_sqlserver::SqlServerDriver;

fn server(var: &str, user: &str, password: &str) -> Option<ConnectParams> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: user.into(),
        password: Some(Secret::new(password)),
        database: "spelaion".into(),
        ..Default::default()
    })
}

async fn connect(driver: &dyn Driver, params: &ConnectParams) -> Session {
    Session::new(
        driver.connect(params).await.expect("connecting"),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

async fn servers() -> Vec<(&'static str, Session)> {
    let mut out = Vec::new();
    if let Some(params) = server("SPELAION_TEST_PG", "spelaion", "spelaion") {
        out.push(("postgres", connect(&PostgresDriver, &params).await));
    }
    if let Some(params) = server("SPELAION_TEST_MYSQL", "spelaion", "spelaion") {
        out.push(("mysql", connect(&MysqlDriver::mysql(), &params).await));
    }
    if let Some(params) = server("SPELAION_TEST_MARIADB", "spelaion", "spelaion") {
        out.push(("mariadb", connect(&MysqlDriver::mariadb(), &params).await));
    }
    if let Some(mut params) = server("SPELAION_TEST_MSSQL", "sa", "Spelaion-test-1") {
        params.tls = TlsMode::Require;
        params.database = "master".into();
        out.push(("sqlserver", connect(&SqlServerDriver, &params).await));
    }
    out
}

/// The notices of a run ("SEVERITY: message") and the indexes of its failed statements.
async fn run(session: &Session, sql: &str) -> (Vec<String>, Vec<usize>) {
    let mut notices = Vec::new();
    let mut failed = Vec::new();
    let mut events = session.run_script(sql);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::Notice { notice, .. } => {
                notices.push(format!("{}: {}", notice.severity, notice.message));
            }
            CoreEvent::StatementFailed { index, .. } => failed.push(index),
            _ => {}
        }
    }
    (notices, failed)
}

#[tokio::test]
async fn server_messages_reach_the_run_on_every_engine_that_sends_them() {
    for (name, session) in servers().await {
        let (script, expected): (&str, &[&str]) = match name {
            "postgres" => (
                "do $$ begin raise notice 'hello from plpgsql'; end $$",
                &["NOTICE: hello from plpgsql"],
            ),
            "mysql" | "mariadb" => (
                "drop table if exists spelaion_no_such_table;\nselect cast('abc' as signed)",
                &[
                    "NOTE: Unknown table",
                    "(1051)",
                    "WARNING: Truncated incorrect INTEGER value",
                    "(1292)",
                ],
            ),
            "sqlserver" => (
                "print 'hello from print';\nraiserror('low severity', 10, 1)",
                &["INFO: hello from print", "INFO: low severity"],
            ),
            other => panic!("no messages to check on {other}"),
        };
        let (notices, failed) = run(&session, script).await;
        assert!(failed.is_empty(), "{name}: {notices:?}");
        for text in expected {
            assert!(
                notices.iter().any(|n| n.contains(text)),
                "{name}: no {text:?} in {notices:?}"
            );
        }

        // A statement without messages costs nothing and says nothing.
        let (notices, _) = run(&session, "select 1").await;
        assert!(notices.is_empty(), "{name}: {notices:?}");
    }
}

#[tokio::test]
async fn sql_server_messages_before_an_error_are_kept() {
    for (name, session) in servers().await {
        if name != "sqlserver" {
            continue;
        }
        let (notices, failed) = run(
            &session,
            "print 'before the error'\nraiserror('boom', 16, 1)",
        )
        .await;
        assert_eq!(failed, [0]);
        assert!(
            notices.iter().any(|n| n == "INFO: before the error"),
            "{notices:?}"
        );
        // Messages of one batch never leak into the next.
        let (notices, _) = run(&session, "select 1").await;
        assert!(notices.is_empty(), "{notices:?}");
    }
}
