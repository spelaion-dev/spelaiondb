//! A PostgreSQL server's databases (ADR-074, SDB-489), through the pinned plugin and the core's
//! session: `list_databases` holds the databases the user may open, never a template, never
//! one that refuses connections, and not one whose `CONNECT` was taken from the user. A
//! session still never changes database (ADR-074 point 4), and nothing in the app uses the
//! list yet. Needs `SPELAION_TEST_PG=host:port`; without it the test is skipped, which does
//! not count as tested.

use crate::engines;

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, Secret};

/// This test's own objects, apart from the plugin repository's test of the same.
const NO_CONNECT: &str = "core_dblist_no_connect";
const CLOSED: &str = "core_dblist_closed";
const ROLE: &str = "core_dblist_role";

async fn session(host: &str, port: u16, user: &str, password: &str) -> Session {
    let driver = engines::postgres();
    let conn = driver
        .connect(&ConnectParams {
            host: host.into(),
            port,
            user: user.into(),
            password: Some(Secret::new(password)),
            database: "spelaion".into(),
            ..Default::default()
        })
        .await
        .unwrap_or_else(|e| panic!("connecting as {user}: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

async fn run(session: &Session, sql: &str) {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    for event in &events {
        if let CoreEvent::StatementFailed { error, .. } = event {
            panic!("{sql}\n→ {}", error.message)
        }
    }
}

async fn clean(admin: &Session) {
    run(admin, &format!("DROP DATABASE IF EXISTS {NO_CONNECT}")).await;
    run(admin, &format!("DROP DATABASE IF EXISTS {CLOSED}")).await;
    run(admin, &format!("DROP ROLE IF EXISTS {ROLE}")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_databases_are_the_ones_the_user_may_open() {
    let _turn = crate::serial::file_turn(module_path!());
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let port: u16 = port.parse().expect("port");
    let admin = session(host, port, "spelaion", "spelaion").await;
    clean(&admin).await;
    run(&admin, &format!("CREATE DATABASE {NO_CONNECT}")).await;
    run(
        &admin,
        &format!("REVOKE CONNECT ON DATABASE {NO_CONNECT} FROM PUBLIC"),
    )
    .await;
    run(&admin, &format!("CREATE DATABASE {CLOSED}")).await;
    run(
        &admin,
        &format!("ALTER DATABASE {CLOSED} ALLOW_CONNECTIONS false"),
    )
    .await;
    run(
        &admin,
        &format!("CREATE ROLE {ROLE} LOGIN PASSWORD 'dblist'"),
    )
    .await;

    let user = session(host, port, ROLE, "dblist").await;
    let listed = user.connection().list_databases().await;
    let everything = admin.connection().list_databases().await;
    let switched = admin.connection().select_database("postgres").await;
    drop(user);
    clean(&admin).await;
    let listed = listed.expect("the user's databases");
    let everything = everything.expect("the superuser's databases");

    for name in ["spelaion", "spelaion_gis", "postgres"] {
        assert!(listed.iter().any(|d| d == name), "{name} in {listed:?}");
    }
    for name in ["template0", "template1", NO_CONNECT, CLOSED] {
        assert!(
            !listed.iter().any(|d| d == name),
            "{name} not in {listed:?}"
        );
    }
    // A superuser has CONNECT everywhere; templates and a closed database are still left out.
    assert!(everything.iter().any(|d| d == NO_CONNECT), "{everything:?}");
    for name in ["template0", "template1", CLOSED] {
        assert!(
            !everything.iter().any(|d| d == name),
            "{name} not in {everything:?}"
        );
    }
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "ordered by name");
    assert!(
        switched.is_err(),
        "a PostgreSQL session never changes database"
    );
}
