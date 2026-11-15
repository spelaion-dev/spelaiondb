//! Trino and Presto, one plugin for both, through the core: agents' reads and their
//! engine-enforced barrier (ADR-020: a read-only transaction), streaming exports (ADR-034), a
//! read-only connection (ADR-053) and a connection through the two SSH bastions. Every test runs
//! on each server it has.
//!
//! They need `SPELAION_TEST_TRINO` (127.0.0.1:18080) and `SPELAION_TEST_PRESTO`
//! (127.0.0.1:18081) from `testing/docker`, and the plugin: the pinned build or one built on this
//! machine (`SPELAION_DEV_PLUGIN_TRINO`, `SPELAION_DEV_PLUGIN_PRESTO`). The tunnel test also
//! needs `SPELAION_TEST_SSH`. Without them the tests are skipped, which does not count as tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::{
    CoreEvent, CsvOptions, DEFAULT_BATCH_ROWS, Engines, ExportCancel, ExportError, ExportFormat,
    OpenConnection, OpenOptions, ReadError, ReadLimits, ResultStore, Session, StreamExport,
    TextEncoding, TunnelPool, WriteError, WriteLimits, export_query, open_connection,
};
use spelaion_driver_api::{
    ConnectParams, Driver, DriverError, ExecOptions, TlsMode, TransactionState, Value,
};
use spelaion_ssh::KnownHosts;

mod engines;
mod support;
use support::{cleaning_up, run_cleanup};

/// The servers there are: (engine, driver, host, port).
fn servers() -> Vec<(&'static str, Arc<dyn Driver>, String, u16)> {
    let mut out = Vec::new();
    for (variable, engine) in [
        ("SPELAION_TEST_TRINO", "trino"),
        ("SPELAION_TEST_PRESTO", "presto"),
    ] {
        match std::env::var(variable) {
            Ok(addr) => {
                let (host, port) = addr.rsplit_once(':').expect("host:port");
                let driver = if engine == "trino" {
                    engines::trino()
                } else {
                    engines::presto()
                };
                out.push((
                    engine,
                    driver,
                    host.to_string(),
                    port.parse().expect("a port"),
                ));
            }
            Err(_) => eprintln!("{variable} not set: {engine} skipped"),
        }
    }
    out
}

async fn session(driver: &Arc<dyn Driver>, host: &str, port: u16, database: &str) -> Session {
    let params = ConnectParams {
        host: host.into(),
        port,
        user: "spelaion".into(),
        database: database.into(),
        tls: TlsMode::Disable,
        ..Default::default()
    };
    Session::new(
        driver.connect(&params).await.expect("connects"),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

fn limits(max_rows: usize) -> ReadLimits {
    ReadLimits {
        max_rows,
        timeout: Duration::from_secs(20),
    }
}

/// Straight to the driver: what no check of the core sees.
async fn raw(session: &Session, sql: &str) -> Result<(), DriverError> {
    let mut events = session
        .connection()
        .execute(sql, &ExecOptions::default())
        .await?;
    while let Some(event) = events.next().await {
        event?;
    }
    Ok(())
}

async fn must(session: &Session, sql: &str) {
    raw(session, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rejected<T: std::fmt::Debug>(result: Result<T, ReadError>, what: &str) {
    assert!(
        matches!(result, Err(ReadError::Rejected(_))),
        "{what}: {result:?}"
    );
}

const SLOW: &str = "SELECT count(*) FROM UNNEST(sequence(1, 10000)) a(x) \
                    CROSS JOIN UNNEST(sequence(1, 10000)) b(y) CROSS JOIN UNNEST(sequence(1, 10000)) c(z)";

/// Agents read inside a read-only transaction the server enforces, always rolled back: what
/// gets past the classifier is refused by the server, and nothing is left open after an error
/// or a timeout.
#[tokio::test]
async fn agents_read_in_a_read_only_transaction_the_server_enforces() {
    for (engine, driver, host, port) in servers() {
        let session = session(&driver, &host, port, "memory.default").await;
        let cleanup = self::session(&driver, &host, port, "memory.default").await;
        let t = format!("agent_read_{engine}_{}", std::process::id());
        let drops = [format!("DROP TABLE IF EXISTS memory.default.{t}")];
        let body = async {
            let _ = raw(&session, &format!("DROP TABLE IF EXISTS {t}")).await;
            must(
                &session,
                &format!("CREATE TABLE {t} AS SELECT * FROM (VALUES 1, 2, 3) v(id)"),
            )
            .await;

            let started = Instant::now();
            let r = session
                .run_read_only("SELECT * FROM tpch.tiny.lineitem", &limits(5))
                .await
                .unwrap();
            assert_eq!((r.rows.len(), r.truncated), (5, true), "{engine}");
            assert!(started.elapsed() < Duration::from_secs(15));
            assert_eq!(
                session.connection().transaction_state().await.unwrap(),
                TransactionState::Idle,
                "{engine}: rolled back"
            );
            let r = session
                .run_read_only(&format!("SELECT count(*) FROM {t}"), &limits(5))
                .await
                .unwrap();
            assert_eq!(r.rows, [[Value::Int(3)]], "{engine}");

            for sql in [
                format!("INSERT INTO {t} VALUES (4)"),
                format!("DELETE FROM {t}"),
                format!("DROP TABLE {t}"),
                format!("CREATE TABLE {t}_copy AS SELECT * FROM {t}"),
                format!("SELECT 1; SELECT * FROM {t}"),
                "SELECT * FROM TABLE(memory.system.query(query => 'DELETE FROM t'))".into(),
                "CALL system.runtime.kill_query(query_id => 'x')".into(),
            ] {
                rejected(
                    session.run_read_only(&sql, &limits(5)).await,
                    &format!("{engine}: {sql}"),
                );
            }

            // The barrier is the server's: inside the same read-only transaction a write sent
            // straight to it is refused there.
            must(&session, "START TRANSACTION READ ONLY").await;
            match raw(&session, &format!("INSERT INTO {t} VALUES (9)")).await {
                Err(DriverError::Database { code, .. }) => {
                    assert_eq!(code.as_deref(), Some("READ_ONLY_VIOLATION"), "{engine}");
                }
                other => panic!("{engine}: {other:?}"),
            }
            must(&session, "ROLLBACK").await;

            // A failed read and a timeout leave no transaction open.
            assert!(matches!(
                session.run_read_only("SELECT 1 / 0", &limits(5)).await,
                Err(ReadError::Database(_))
            ));
            assert_eq!(
                session.connection().transaction_state().await.unwrap(),
                TransactionState::Idle
            );
            let started = Instant::now();
            let slow = session
                .run_read_only(
                    SLOW,
                    &ReadLimits {
                        max_rows: 1,
                        timeout: Duration::from_secs(2),
                    },
                )
                .await;
            assert!(
                matches!(
                    slow,
                    Err(ReadError::Timeout(_)) | Err(ReadError::Database(DriverError::Cancelled))
                ),
                "{engine}: {slow:?}"
            );
            assert!(started.elapsed() < Duration::from_secs(20), "{engine}");
            assert_eq!(
                session.connection().transaction_state().await.unwrap(),
                TransactionState::Idle
            );
            let r = session
                .run_read_only("SELECT 42", &limits(5))
                .await
                .unwrap();
            assert_eq!(r.rows, [[Value::Int(42)]]);

            // Agents' writes run in a transaction of their own, which these engines' connectors
            // cannot all give: refused.
            assert!(matches!(
                session
                    .run_write(
                        &format!("INSERT INTO {t} VALUES (6)"),
                        &WriteLimits::default()
                    )
                    .await,
                Err(WriteError::Unsupported(_))
            ));
        };
        cleaning_up(body, |panicked| {
            run_cleanup(engine, &cleanup, &drops, panicked)
        })
        .await;
    }
}

fn csv() -> StreamExport {
    StreamExport {
        format: ExportFormat::Csv,
        header: true,
        csv: CsvOptions {
            delimiter: ';',
            null_text: "NULL".into(),
            escape_formulas: true,
        },
        encoding: TextEncoding::Utf8,
        columns: None,
        batch_size: DEFAULT_BATCH_ROWS,
    }
}

/// A query streams to a file inside a read-only transaction of the export's own session; writes
/// never reach the server through the export path.
#[tokio::test]
async fn exports_stream_rows_in_a_read_only_transaction() {
    for (engine, driver, host, port) in servers() {
        let session = session(&driver, &host, port, "tpch.tiny").await;
        let dir =
            std::env::temp_dir().join(format!("spelaion-{engine}-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("orders.csv");
        let summary = export_query(
            &session,
            "SELECT orderkey, comment FROM orders ORDER BY orderkey",
            &Safeguards::default(),
            false,
            &path,
            &csv(),
            &ExportCancel::new(),
            &|_| {},
        )
        .await
        .unwrap();
        assert_eq!(summary.rows, 15_000, "{engine}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("orderkey;comment\r\n1;"), "{engine}");
        assert_eq!(
            session.connection().transaction_state().await.unwrap(),
            TransactionState::Idle,
            "{engine}: rolled back"
        );
        for sql in [
            "INSERT INTO t VALUES (1)",
            "SELECT 1; SELECT 2",
            "DELETE FROM orders",
        ] {
            let result = export_query(
                &session,
                sql,
                &Safeguards::default(),
                false,
                &dir.join("refused.csv"),
                &csv(),
                &ExportCancel::new(),
                &|_| {},
            )
            .await;
            assert!(
                matches!(result, Err(ExportError::NotARead(_))),
                "{engine}: {sql}: {result:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// A temporary, trusted repository whose connections a test writes.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir().join(format!("spelaion-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.base.join("repo").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    async fn open(&self, id: &str) -> OpenConnection {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving");
        let options = OpenOptions {
            tunnels: Arc::new(TunnelPool::new()),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: self.base.join("user/known_hosts"),
            },
            interaction: Arc::new(AcceptHosts),
            password: None,
            read_only_session: false,
        };
        open_connection(
            resolved,
            &Engines::builtin(),
            options,
            Arc::new(ResultStore::new()),
        )
        .await
        .expect("opens")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct AcceptHosts;

#[async_trait::async_trait]
impl spelaion_ssh::Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        true
    }
    async fn ask_secret(
        &self,
        _request: &spelaion_ssh::SecretRequest,
    ) -> Option<spelaion_driver_api::Secret> {
        None
    }
}

fn toml_path(path: &Path) -> String {
    format!("{:?}", path.display().to_string().replace('\\', "/"))
}

fn keys() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

/// The first value of a statement run as the editor runs it, as text.
async fn value(session: &Session, sql: &str) -> String {
    let mut events = session.run_script(sql);
    let mut last = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => last = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {}", error.message),
            _ => {}
        }
    }
    let id = last.unwrap_or_else(|| panic!("{sql}: no result"));
    let stored = session.results().get(id).expect("the result");
    let cell = stored.read().unwrap().value_at(0, 0).expect("a cell");
    cell.to_string()
}

/// A read-only connection (ADR-053) runs reads and refuses writes before they reach the
/// server (these engines have no session-wide read-only mode to set).
#[tokio::test]
async fn a_read_only_connection_refuses_writes() {
    for (engine, _, host, port) in servers() {
        let ws = Workspace::new(&format!("{engine}-read-only"));
        ws.write(
            ".spelaion/connections/ro.toml",
            &format!(
                "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"memory.default\"\nuser = \"spelaion\"\nread_only = true\n\n[tls]\nmode = \"disable\"\n"
            ),
        );
        let open = ws.open("ro").await;
        assert_eq!(value(&open.session, "SELECT 1 + 1").await, "2", "{engine}");
        let mut events = open.session.run_script("CREATE TABLE nope (a integer)");
        let mut refused = false;
        while let Some(event) = events.next().await {
            refused |= matches!(event, CoreEvent::StatementFailed { .. });
        }
        assert!(refused, "{engine}");
        open.close().await;
    }
}

/// Through the two bastions of `testing/docker`: the plugin connects to the tunnel's local end
/// and follows each `nextUri` there, whatever host the server names in it.
#[tokio::test]
async fn trino_and_presto_through_two_bastions() {
    let servers = servers();
    if servers.is_empty() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    for (engine, ..) in servers {
        let ws = Workspace::new(&format!("{engine}-tunnel"));
        let identity = toml_path(&keys().join("id_ed25519"));
        ws.write(
            ".spelaion/tunnels/bastions.toml",
            &format!(
                "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
            ),
        );
        ws.write(
            ".spelaion/connections/warehouse.toml",
            &format!(
                "schema_version = 1\nengine = \"{engine}\"\nhost = \"{engine}\"\ndatabase = \"tpch.tiny\"\nuser = \"spelaion\"\ntunnel = \"bastions\"\n\n[tls]\nmode = \"disable\"\n"
            ),
        );
        let open = ws.open("warehouse").await;
        // Several pages, each followed through the tunnel.
        assert_eq!(
            value(&open.session, "SELECT count(*) FROM lineitem").await,
            "60175",
            "{engine}"
        );
        let catalogs = open.session.connection().list_databases().await.unwrap();
        assert!(
            catalogs.contains(&"tpch".to_string()),
            "{engine}: {catalogs:?}"
        );
        open.close().await;
    }
}
