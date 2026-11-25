//! InfluxDB 3 through the core: a `flightsql` connection whose `database` the Flight SQL plugin
//! sends as a header on every call (its 0.1.1), which is how InfluxDB chooses the database. A
//! script runs as the editor runs it, the explorer shows the database's tables, and agents' reads
//! are not run (no read barrier of ours, as for every Flight SQL server).
//!
//! Needs `SPELAION_TEST_INFLUXDB` (127.0.0.1:18181, `testing/docker`, no authentication; its HTTP
//! API and Flight SQL share the port) and the plugin: the pinned build or one built on this
//! machine (`SPELAION_DEV_PLUGIN_FLIGHTSQL`). Without them the tests are skipped, which does not
//! count as tested.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadError, ReadLimits, ResultStore,
    TunnelPool, open_connection,
};
use spelaion_ssh::KnownHosts;

/// The tests' own database, made by InfluxDB on its first write.
const DATABASE: &str = "spelaion_core_influx";

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_INFLUXDB") else {
        eprintln!("SPELAION_TEST_INFLUXDB not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("a port")))
}

/// Writes line protocol to the tests' database through InfluxDB's HTTP API (the same port).
fn write_lines(host: &str, port: u16, lines: &str) {
    let mut stream = TcpStream::connect((host, port)).expect("InfluxDB's HTTP API");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let request = format!(
        "POST /api/v3/write_lp?db={DATABASE} HTTP/1.1\r\nHost: {host}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{lines}",
        lines.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let status = answer.lines().next().unwrap_or_default();
    assert!(status.contains(" 20"), "the write was refused: {answer}");
}

/// A temporary, trusted repository with one InfluxDB connection, opened as the app opens it.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(label: &str, host: &str, port: u16) -> Self {
        let base = std::env::temp_dir().join(format!("spelaion-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        std::fs::write(
            base.join("repo/.spelaion/connections/metrics.toml"),
            format!(
                "schema_version = 1\nengine = \"flightsql\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"{DATABASE}\"\n\n[tls]\nmode = \"disable\"\n"
            ),
        )
        .unwrap();
        Self { base }
    }

    async fn open(&self) -> OpenConnection {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: "metrics".into(),
            })
            .expect("resolving");
        let options = OpenOptions {
            tunnels: Arc::new(TunnelPool::new()),
            known_hosts: KnownHosts {
                team_files: Vec::new(),
                user_file: None,
                app_file: self.base.join("user/known_hosts"),
            },
            interaction: Arc::new(NoDialogs),
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

struct NoDialogs;

#[async_trait::async_trait]
impl spelaion_ssh::Interaction for NoDialogs {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        false
    }
    async fn ask_secret(
        &self,
        _request: &spelaion_ssh::SecretRequest,
    ) -> Option<spelaion_driver_api::Secret> {
        None
    }
}

/// The first value of the last result of a script run as the editor runs it, as text.
async fn value(open: &OpenConnection, sql: &str) -> String {
    let mut events = open.session.run_script(sql);
    let mut last = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => last = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {}", error.message),
            _ => {}
        }
    }
    let id = last.unwrap_or_else(|| panic!("{sql}: no result"));
    let stored = open.session.results().get(id).expect("the result");
    let cell = stored.read().unwrap().value_at(0, 0).expect("a cell");
    cell.to_string()
}

#[tokio::test]
async fn a_flightsql_connection_reads_its_influxdb_database() {
    let Some((host, port)) = server() else { return };
    write_lines(
        &host,
        port,
        "cpu,host=alpha usage=12.5 1700000000000000000\ncpu,host=beta usage=40 1700000001000000000\n",
    );
    let ws = Workspace::new("influxdb", &host, port);
    let open = ws.open().await;

    // Two statements, split by the core and each sent alone with the database header.
    let script = "SELECT count(*) FROM cpu WHERE time <= '2023-11-14T22:13:21Z';\n\
                  SELECT host FROM cpu WHERE time <= '2023-11-14T22:13:21Z' ORDER BY usage DESC LIMIT 1;";
    assert_eq!(value(&open, script).await, "beta");

    // The explorer: InfluxDB's tables are in schema `iox` of its one catalog, `public`.
    let connection = open.session.connection();
    let schemas = connection.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "iox"), "{schemas:?}");
    let objects = connection.list_objects("iox").await.unwrap();
    assert!(objects.iter().any(|o| o.name == "cpu"), "{objects:?}");

    // No barrier of ours: agents' reads are not run here.
    let refused = open
        .session
        .run_read_only("SELECT 1", &ReadLimits::default())
        .await;
    assert!(
        matches!(refused, Err(ReadError::Unsupported(_))),
        "{refused:?}"
    );
    open.close().await;
}
