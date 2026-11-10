//! Server observation against every engine that has it: PostgreSQL, MySQL, MariaDB and SQL
//! Server with their `SPELAION_TEST_*` variables (without them they are skipped, which does
//! not count as tested); table statistics also on SQLite and DuckDB.
//!
//! - Two sessions, one waiting for the other's row lock: the lock view shows the blocking
//!   pair, the waiting and the granted locks, and the blocking tree with the holder on top.
//!   Ending the holder through the process monitor's checks lets the waiter finish.
//! - The settings viewer reads the server's settings (and MySQL's status counters).
//! - Table statistics: sizes and a row estimate for a table, nothing for a view.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, LockSnapshot, ProcessTarget, ResultStore, Session, SettingKind, control_process,
    list_locks, list_processes, server_settings, table_stats,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

async fn session(driver: &dyn Driver, params: &ConnectParams) -> Session {
    let conn = driver
        .connect(params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
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

/// Runs each statement on its own and panics on a failure.
async fn run(label: &str, session: &Session, statements: &[&str]) {
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            if let CoreEvent::StatementFailed { error, .. } = event {
                panic!("{label}: {statement}\n→ {}", error.message)
            }
        }
    }
}

/// Runs a statement whose failure does not matter (dropping what may not exist).
async fn try_run(session: &Session, statement: &str) {
    let _: Vec<CoreEvent> = session.run_script(statement).collect().await;
}

struct Engine {
    label: &'static str,
    driver: Box<dyn Driver>,
    /// The two sessions that block each other.
    user: ConnectParams,
    /// The monitor: a login that may see every session's locks.
    monitor: ConnectParams,
    table: &'static str,
    begin: &'static str,
}

/// Holder takes a row lock in an open transaction, waiter asks for the same row; the monitor
/// must show the pair. Then the holder is ended the way the lock view does it (the process
/// monitor's `control_process`), and the waiter goes on.
async fn blocking_pair(engine: Engine) {
    let Engine {
        label,
        driver,
        user,
        monitor,
        table,
        begin,
    } = engine;
    let holder = session(driver.as_ref(), &user).await;
    let waiter = session(driver.as_ref(), &user).await;
    let monitor = session(driver.as_ref(), &monitor).await;
    try_run(&holder, &format!("DROP TABLE {table}")).await;
    run(
        label,
        &holder,
        &[
            &format!("CREATE TABLE {table} (id INT PRIMARY KEY, v INT)"),
            &format!("INSERT INTO {table} (id, v) VALUES (1, 1), (2, 2)"),
        ],
    )
    .await;
    let holder_id = list_processes(&holder).await.unwrap().own_id;
    let waiter_id = list_processes(&waiter).await.unwrap().own_id;
    run(
        label,
        &holder,
        &[begin, &format!("UPDATE {table} SET v = 10 WHERE id = 1")],
    )
    .await;
    let update = waiter.run_script(&format!("UPDATE {table} SET v = 20 WHERE id = 1"));
    let waiting = tokio::spawn(async move { update.collect::<Vec<CoreEvent>>().await });

    // The waiter needs a moment to reach the lock.
    let started = Instant::now();
    let snapshot: LockSnapshot = loop {
        let snapshot = list_locks(&monitor)
            .await
            .unwrap_or_else(|e| panic!("{label}: listing locks: {e}"));
        if snapshot
            .waits
            .iter()
            .any(|w| w.waiting == waiter_id && w.blocking == holder_id)
        {
            break snapshot;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{label}: {waiter_id} never showed waiting for {holder_id}: {:?}",
            snapshot.waits
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(
        snapshot.locks.iter().all(|l| l.session != snapshot.own_id),
        "{label}: the monitor's own locks are left out"
    );
    assert!(
        snapshot
            .locks
            .iter()
            .any(|l| l.session == waiter_id && !l.granted),
        "{label}: a waiting lock of {waiter_id}: {:?}",
        snapshot.locks
    );
    assert!(
        snapshot
            .locks
            .iter()
            .any(|l| l.session == holder_id && l.granted && l.object.contains(table)),
        "{label}: a granted lock of {holder_id} on {table}: {:?}",
        snapshot.locks
    );
    let waited = snapshot
        .locks
        .iter()
        .find(|l| l.session == waiter_id && !l.granted)
        .unwrap();
    assert!(
        waited.seconds.is_some_and(|s| (0..600).contains(&s)),
        "{label}: how long it waits: {waited:?}"
    );
    let head = snapshot
        .tree
        .iter()
        .position(|n| n.session == holder_id && n.depth == 0)
        .unwrap_or_else(|| panic!("{label}: {holder_id} heads the tree: {:?}", snapshot.tree));
    assert!(
        snapshot.tree[head + 1..]
            .iter()
            .take_while(|n| n.depth > 0)
            .any(|n| n.session == waiter_id && n.depth == 1 && !n.cycle),
        "{label}: {waiter_id} under {holder_id}: {:?}",
        snapshot.tree
    );

    // End the holder from the monitor, through the process monitor's checks.
    let process = list_processes(&monitor)
        .await
        .unwrap()
        .processes
        .into_iter()
        .find(|p| p.id == holder_id)
        .unwrap_or_else(|| panic!("{label}: the monitor sees {holder_id}"));
    let target = ProcessTarget {
        id: process.id,
        user: process.user,
        host: process.host,
        command: process.command,
        sql: process.sql,
        identity: process.identity,
    };
    control_process(&monitor, &target, true, false, true)
        .await
        .unwrap_or_else(|e| panic!("{label}: ending {holder_id}: {e}"));
    let events = tokio::time::timeout(Duration::from_secs(20), waiting)
        .await
        .unwrap_or_else(|_| panic!("{label}: the waiter goes on once the holder ends"))
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
        "{label}: the waiting update succeeds"
    );
    // MariaDB's lock tables are a cache InnoDB refreshes at most every 100 ms.
    let started = Instant::now();
    loop {
        let after = list_locks(&monitor).await.unwrap();
        if !after
            .waits
            .iter()
            .any(|w| w.waiting == waiter_id || w.blocking == holder_id)
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{label}: nothing waits any more: {:?}",
            after.waits
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    settings(label, &monitor).await;
    stats(label, &waiter, table).await;
    try_run(&waiter, &format!("DROP TABLE {table}")).await;
}

async fn settings(label: &str, monitor: &Session) {
    let settings = server_settings(monitor)
        .await
        .unwrap_or_else(|e| panic!("{label}: settings: {e}"));
    let find = |name: &str, kind: SettingKind| {
        settings
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(name) && s.kind == kind)
            .unwrap_or_else(|| panic!("{label}: {name} ({kind:?}) is listed"))
    };
    match label {
        "postgres" => {
            let max = find("max_connections", SettingKind::Setting);
            assert!(max.value.parse::<u32>().is_ok(), "{max:?}");
            assert!(max.description.is_some() && max.category.is_some());
            // As SHOW writes it, with its unit: `128MB`.
            let buffers = find("shared_buffers", SettingKind::Setting);
            assert!(
                buffers.value.ends_with("MB")
                    || buffers.value.ends_with("GB")
                    || buffers.value.ends_with("kB"),
                "{buffers:?}"
            );
            assert!(!buffers.value.contains(' '), "{buffers:?}");
        }
        "mysql" | "mariadb" => {
            find("max_connections", SettingKind::Variable);
            let uptime = find("Uptime", SettingKind::Status);
            assert!(uptime.value.parse::<u64>().is_ok(), "{uptime:?}");
        }
        "sqlserver" => {
            let memory = find("max server memory (MB)", SettingKind::Setting);
            assert!(memory.description.is_some());
            let version = find("ProductVersion", SettingKind::Property);
            assert!(version.value.starts_with("16."), "{version:?}");
        }
        other => panic!("{other}"),
    }
}

async fn stats(label: &str, session: &Session, table: &str) {
    let schema = match label {
        "postgres" => "public",
        "sqlserver" => "dbo",
        _ => "spelaion",
    };
    if label == "postgres" {
        run(label, session, &[&format!("ANALYZE {table}")]).await;
    }
    let stats = table_stats(session, schema, table)
        .await
        .unwrap_or_else(|e| panic!("{label}: table stats: {e}"))
        .unwrap_or_else(|| panic!("{label}: {table} has statistics"));
    assert!(
        stats.total_bytes.is_some_and(|b| b > 0),
        "{label}: {stats:?}"
    );
    assert!(
        stats.table_bytes.is_some_and(|b| b > 0),
        "{label}: {stats:?}"
    );
    // The primary key is the table itself in InnoDB and SQL Server's clustered index.
    assert!(
        stats.index_bytes.is_some_and(|b| b >= 0),
        "{label}: {stats:?}"
    );
    assert!(
        stats.total_bytes >= stats.table_bytes,
        "{label}: the total includes the table: {stats:?}"
    );
    match label {
        "postgres" | "sqlserver" => assert_eq!(stats.row_estimate, Some(2), "{label}"),
        // InnoDB's estimate comes from sampled pages and is cached.
        _ => assert!(stats.row_estimate.is_some(), "{label}: {stats:?}"),
    }
    let view = format!("{table}_v");
    try_run(session, &format!("DROP VIEW {view}")).await;
    run(
        label,
        session,
        &[&format!("CREATE VIEW {view} AS SELECT id FROM {table}")],
    )
    .await;
    assert_eq!(
        table_stats(session, schema, &view).await.unwrap(),
        None,
        "{label}: a view has no size of its own"
    );
    assert_eq!(
        table_stats(session, schema, "no ' such table")
            .await
            .unwrap(),
        None,
        "{label}: an unknown table"
    );
    run(label, session, &[&format!("DROP VIEW {view}")]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_shows_who_blocks_whom() {
    let Some((host, port)) = server("SPELAION_TEST_PG") else {
        return;
    };
    let user = login(&host, port, "spelaion", "spelaion", "spelaion");
    blocking_pair(Engine {
        label: "postgres",
        driver: Box::new(spelaion_driver_postgres::PostgresDriver),
        monitor: user.clone(),
        user,
        table: "obs_locks",
        begin: "BEGIN",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mysql_shows_who_blocks_whom() {
    let Some((host, port)) = server("SPELAION_TEST_MYSQL") else {
        return;
    };
    // Locks need PROCESS and performance_schema: the test user has neither.
    let user = login(&host, port, "spelaion", "spelaion", "spelaion");
    let unprivileged = session(&spelaion_driver_mysql::MysqlDriver::mysql(), &user).await;
    let refused = list_locks(&unprivileged).await.err().unwrap_or_default();
    assert!(
        refused.to_lowercase().contains("denied"),
        "the server's refusal is shown: {refused}"
    );
    blocking_pair(Engine {
        label: "mysql",
        driver: Box::new(spelaion_driver_mysql::MysqlDriver::mysql()),
        monitor: login(&host, port, "root", "spelaion-root", "spelaion"),
        user,
        table: "obs_locks",
        begin: "START TRANSACTION",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mariadb_shows_who_blocks_whom() {
    let Some((host, port)) = server("SPELAION_TEST_MARIADB") else {
        return;
    };
    blocking_pair(Engine {
        label: "mariadb",
        driver: Box::new(spelaion_driver_mysql::MysqlDriver::mariadb()),
        user: login(&host, port, "spelaion", "spelaion", "spelaion"),
        monitor: login(&host, port, "root", "spelaion-root", "spelaion"),
        table: "obs_locks",
        begin: "START TRANSACTION",
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlserver_shows_who_blocks_whom() {
    let Some((host, port)) = server("SPELAION_TEST_MSSQL") else {
        return;
    };
    let user = ConnectParams {
        tls: TlsMode::Require,
        ..login(&host, port, "sa", "Spelaion-test-1", "tempdb")
    };
    blocking_pair(Engine {
        label: "sqlserver",
        driver: Box::new(spelaion_driver_sqlserver::SqlServerDriver),
        monitor: user.clone(),
        user,
        table: "obs_locks",
        begin: "BEGIN TRANSACTION",
    })
    .await;
}

#[tokio::test]
async fn embedded_engines_have_row_estimates_or_nothing() {
    let memory = ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let duckdb = session(&spelaion_driver_duckdb::DuckdbDriver::default(), &memory).await;
    run(
        "duckdb",
        &duckdb,
        &[
            "CREATE TABLE stats_t (id INT PRIMARY KEY)",
            "INSERT INTO stats_t VALUES (1), (2), (3)",
        ],
    )
    .await;
    let stats = table_stats(&duckdb, "main", "stats_t")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.row_estimate, Some(3));
    assert_eq!(stats.total_bytes, None);
    assert!(list_locks(&duckdb).await.is_err());
    assert!(server_settings(&duckdb).await.is_err());

    let sqlite = session(&spelaion_driver_sqlite::SqliteDriver::default(), &memory).await;
    run("sqlite", &sqlite, &["CREATE TABLE stats_t (id INTEGER)"]).await;
    assert_eq!(table_stats(&sqlite, "main", "stats_t").await.unwrap(), None);
}
