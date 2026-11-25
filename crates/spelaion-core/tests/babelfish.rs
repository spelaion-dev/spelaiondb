//! Babelfish for PostgreSQL through the SQL Server plugin (SDB-313): an ordinary SQL Server
//! connection that the core tells apart from `@@VERSION`. Its transactions hold, but SQL
//! Server's barrier (a transaction always rolled back, ADR-020) does not: its T-SQL reaches
//! PostgreSQL's own functions, some of whose effects outlive the rollback. So read-only
//! connections, agents' sessions and the reads that rely on that barrier are refused, saying
//! why. Against the image built in `testing/docker/babelfish` (`SPELAION_TEST_BABELFISH`);
//! without it the tests are skipped, which does not count as tested.

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Engine, LocalPort, Password, ResolvedConnection, Source};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadError, ReadLimits, ResultStore,
    ServerFamily, Session, TunnelPool, open_connection,
};
use spelaion_driver_api::{ConnectParams, Secret, TlsMode};
use spelaion_ssh::{KnownHosts, NonInteractive};
use std::sync::Arc;
use std::time::Duration;

fn resolved(read_only: bool) -> Option<ResolvedConnection> {
    let Ok(address) = std::env::var("SPELAION_TEST_BABELFISH") else {
        eprintln!("SPELAION_TEST_BABELFISH not set: Babelfish not tested");
        return None;
    };
    let (host, port) = address.rsplit_once(':').unwrap();
    Some(ResolvedConnection {
        id: ConnectionId {
            source: Source::User,
            path: "babelfish.toml".into(),
        },
        name: "Babelfish test".into(),
        engine: Engine::Sqlserver,
        env: None,
        kind: None,
        params: ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "babelfish_user".into(),
            database: "master".into(),
            tls: TlsMode::Require,
            read_only,
            ..Default::default()
        },
        password: Password::Value(Secret::new("Spelaion-test-1")),
        tunnel: None,
        kubernetes: None,
        sqlite3: None,
        file_folder: None,
        databases: None,
        other_databases: spelaion_config::OtherDatabases::None,
        local_port: LocalPort::Any,
        keepalive: None,
        statement_timeout: Some(Duration::from_secs(20)),
        session: Default::default(),
        mcp: Default::default(),
        masking: Default::default(),
    })
}

async fn open(
    resolved: ResolvedConnection,
    read_only_session: bool,
) -> Result<OpenConnection, String> {
    open_connection(
        resolved,
        &Engines::builtin(),
        OpenOptions {
            tunnels: Arc::new(TunnelPool::new()),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: std::env::temp_dir().join("unused-babelfish-known-hosts"),
            },
            interaction: Arc::new(NonInteractive),
            password: None,
            read_only_session,
        },
        Arc::new(ResultStore::new()),
    )
    .await
    .map_err(|e| e.to_string())
}

/// The first value of each row of `sql`'s results.
async fn run(session: &Session, sql: &str) -> Vec<String> {
    let mut values = Vec::new();
    for event in session.run_script(sql).collect::<Vec<_>>().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {error:?}"),
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(result).unwrap();
                let stored = stored.read().unwrap();
                for row in 0..stored.rows() {
                    values.push(stored.value_at(row, 0).unwrap().to_string());
                }
                drop(stored);
                session.results().remove(result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::ResultEnded { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    values
}

#[tokio::test]
async fn babelfish_opens_as_sql_server_and_is_told_apart() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(connection) = resolved(false) else {
        return;
    };
    let open = open(connection, false).await.unwrap();
    let session = &open.session;
    assert_eq!(session.server_family(), Some(&ServerFamily::Babelfish));
    assert_eq!(
        run(session, "SELECT 1 AS a; SELECT 2 AS b").await,
        ["1", "2"]
    );
    // Its transactions hold: a table edit or an import may rely on them.
    assert_eq!(session.atomic_writes_refusal(), None);
    // What relies on SQL Server's barrier is refused before anything is sent: streaming
    // exports, column statistics, the data search, and an agent's read.
    let refusal = session.read_only_transaction_refusal().unwrap();
    assert!(refusal.contains("PostgreSQL's own functions"), "{refusal}");
    let read = session
        .run_read_only(
            "SELECT 1",
            &ReadLimits {
                max_rows: 10,
                timeout: Duration::from_secs(10),
            },
        )
        .await;
    assert!(
        matches!(&read, Err(ReadError::Unsupported(reason)) if reason.contains("Babelfish")),
        "{read:?}"
    );
    open.close().await;
}

#[tokio::test]
async fn read_only_connections_and_agents_sessions_are_refused_on_babelfish() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(read_only) = resolved(true) else {
        return;
    };
    let error = open(read_only, false).await.err().expect("refused");
    assert!(error.contains("not available on Babelfish"), "{error}");
    // An agent's session (a session that only reads) is refused when it opens.
    let error = open(resolved(false).unwrap(), true)
        .await
        .err()
        .expect("refused");
    assert!(error.contains("Babelfish"), "{error}");
}

/// Why SQL Server's barrier does not hold on Babelfish: a T-SQL `SELECT` inside a transaction
/// that is rolled back takes a PostgreSQL advisory lock that stays after the rollback.
#[tokio::test]
async fn a_select_in_a_rolled_back_transaction_leaves_a_lock_behind_on_babelfish() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(connection) = resolved(false) else {
        return;
    };
    let open = open(connection, false).await.unwrap();
    let session = &open.session;
    run(
        session,
        "BEGIN TRANSACTION; \
         SELECT CAST(pg_catalog.pg_try_advisory_lock(31313) AS int); \
         ROLLBACK",
    )
    .await;
    let held = run(
        session,
        "SELECT CAST(COUNT(*) AS int) FROM pg_catalog.pg_locks \
         WHERE locktype = 'advisory' AND objid = 31313 AND pid = pg_catalog.pg_backend_pid()",
    )
    .await;
    assert_eq!(held, ["1"], "the lock outlived the rollback");
    open.close().await;
}
