//! A relation's properties, partitions and dependencies (SDB-393) on every engine the details
//! are shown for: SQLite and DuckDB always; PostgreSQL, MySQL, MariaDB and SQL Server with their
//! `SPELAION_TEST_*` variables (without them they are skipped, which does not count as
//! tested).
//!
//! SQLite's are read here (its driver is compiled in); every other engine's come from its
//! plugin's own repository, and each plugin's tests check them against its engine. What this
//! file checks for those is the core's side: a plugin that answers them is read, and a plugin
//! of an older protocol is not asked and fails nothing, its engine telling none of them (no
//! partitions, dependencies it cannot tell, never an empty list in their place).

mod engines;
mod support;

use std::path::Path;
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, DependencyKind, Driver, Secret, TlsMode};

struct Case {
    label: &'static str,
    driver: Arc<dyn Driver>,
    params: ConnectParams,
    schema: &'static str,
}

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

async fn cases() -> Vec<Case> {
    let dir = std::env::temp_dir().join(format!("spelaion-details-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let duck = dir.join("details.duckdb");
    let _ = std::fs::remove_file(&duck);
    engines::create_database(spelaion_config::Engine::Duckdb, &duck).await;
    let sqlite = dir.join("details.sqlite");
    std::fs::write(&sqlite, b"").unwrap();
    let file = |path: &Path| ConnectParams {
        database: path.to_string_lossy().into_owned(),
        ..Default::default()
    };
    let mut out = vec![
        Case {
            label: "sqlite",
            driver: engines::sqlite(),
            params: file(&sqlite),
            schema: "main",
        },
        Case {
            label: "duckdb",
            driver: engines::duckdb(),
            params: file(&duck),
            schema: "main",
        },
    ];
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        out.push(Case {
            label: "postgres",
            driver: engines::postgres(),
            params: login(&host, port, "spelaion", "spelaion", "spelaion"),
            schema: "public",
        });
    }
    for (label, var, driver) in [
        ("mysql", "SPELAION_TEST_MYSQL", engines::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", engines::mariadb()),
    ] {
        if let Some((host, port)) = server(var) {
            out.push(Case {
                label,
                driver,
                params: login(&host, port, "spelaion", "spelaion", "spelaion"),
                schema: "spelaion",
            });
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        out.push(Case {
            label: "sqlserver",
            driver: engines::sqlserver(),
            params: ConnectParams {
                tls: TlsMode::Require,
                ..login(&host, port, "sa", "Spelaion-test-1", "tempdb")
            },
            schema: "dbo",
        });
    }
    out
}

async fn run(label: &str, session: &Session, sql: &str) {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    for event in &events {
        if let CoreEvent::StatementFailed { error, .. } = event {
            panic!("{label}: {sql}\n→ {}", error.message);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn details_are_read_or_said_unknown_on_every_engine() {
    let pid = std::process::id();
    for case in cases().await {
        let label = case.label;
        let conn = case
            .driver
            .connect(&case.params)
            .await
            .unwrap_or_else(|e| panic!("{label}: connecting: {e}"));
        let session = Session::new(
            conn,
            spelaion_dialect::for_engine(case.driver.engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        let (table, view) = (format!("details_t_{pid}"), format!("details_v_{pid}"));
        let drops = [
            format!("drop view if exists {view}"),
            format!("drop table if exists {table}"),
        ];
        run(label, &session, &drops[0]).await;
        run(label, &session, &drops[1]).await;
        // The servers are shared: what a failed run leaves must not reach the next one.
        support::cleaning_up(
            details_on(label, &session, case.schema, &table, &view),
            |panicked| support::run_cleanup(label, &session, &drops, panicked),
        )
        .await;
    }
}

async fn details_on(label: &str, session: &Session, schema: &str, table: &str, view: &str) {
    run(
        label,
        session,
        &format!("create table {table} (id int not null primary key, note varchar(20))"),
    )
    .await;
    run(
        label,
        session,
        &format!("create view {view} as select id, note from {table}"),
    )
    .await;
    let connection = session.connection();

    // Properties are what the engine keeps; none is a valid answer.
    let properties = connection
        .object_properties(schema, table)
        .await
        .unwrap_or_else(|e| panic!("{label}: properties: {e}"));
    assert!(
        properties.iter().all(|p| !p.name.is_empty()),
        "{label}: {properties:?}"
    );
    // A table that is not partitioned has no partitions, whatever the engine tells.
    assert_eq!(
        connection
            .table_partitions(schema, table)
            .await
            .unwrap_or_else(|e| panic!("{label}: partitions: {e}")),
        None,
        "{label}"
    );
    // Dependencies are either told, with the view among what uses the table, or unknown.
    let dependencies = connection
        .object_dependencies(schema, table)
        .await
        .unwrap_or_else(|e| panic!("{label}: dependencies: {e}"));
    match (&dependencies, label) {
        // The view is among what uses the table, unless the engine says it does not record
        // which views read a table (DuckDB binds a view when it runs).
        (Some(told), _) => assert!(
            told.untracked.contains(&DependencyKind::View)
                || told
                    .used_by
                    .iter()
                    .any(|d| d.kind == DependencyKind::View && d.name.eq_ignore_ascii_case(view)),
            "{label}: {told:?}"
        ),
        (None, "sqlite") => panic!("SQLite tells what it records"),
        (None, _) => eprintln!("{label}: its plugin does not tell dependencies yet"),
    }
}
