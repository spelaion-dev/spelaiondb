//! libSQL (`sqld`, Turso), served only by its plugin (ADR-058), through the core: SQLite's
//! scripts and types, the transaction read from the server, what libSQL does not offer yet
//! (read-only connections and agents), and connections through the two SSH bastions and through
//! the SOCKS5 proxy alone, both opened by the core (a plugin opens no tunnel nor proxy).
//!
//! They need `SPELAION_TEST_LIBSQL` (`testing/docker`, 127.0.0.1:18095) and the plugin: the
//! pinned build (`spelaion-fetch-plugins`) or one built on this machine
//! (`SPELAION_DEV_PLUGIN_LIBSQL`). The tunnel test also needs `SPELAION_TEST_SSH`, the proxy test
//! `SPELAION_TEST_SOCKS`. Without them the tests are skipped, which does not count as tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    AgentView, CoreEvent, Engines, OpenConnection, OpenOptions, ResultStore, TunnelPool,
    agent_view, open_connection,
};
use spelaion_driver_api::{Secret, TransactionState, Value};
use spelaion_ssh::KnownHosts;

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_LIBSQL") else {
        eprintln!("SPELAION_TEST_LIBSQL not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("a port")))
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

    fn config(&self) -> spelaion_config::Config {
        load(Some(&self.base.join("repo")), &self.base.join("user"))
    }

    async fn open(&self, id: &str) -> OpenConnection {
        let config = self.config();
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving");
        open_connection(
            resolved,
            &Engines::builtin(),
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: self.base.join("user/known_hosts"),
                },
                interaction: Arc::new(AcceptHosts),
                password: None,
                read_only_session: false,
            },
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
    async fn ask_secret(&self, _request: &spelaion_ssh::SecretRequest) -> Option<Secret> {
        None
    }
}

fn toml_path(path: &Path) -> String {
    format!("{:?}", path.display().to_string().replace('\\', "/"))
}

fn keys() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

fn connection(host: &str, port: u16, extra: &str) -> String {
    format!(
        "schema_version = 1\nengine = \"libsql\"\nhost = \"{host}\"\nport = {port}\n{extra}\n[tls]\nmode = \"disable\"\n"
    )
}

async fn script(open: &OpenConnection, sql: &str) -> Vec<CoreEvent> {
    let events: Vec<CoreEvent> = open.session.run_script(sql).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{sql}\n→ {error:?}");
    }
    events
}

async fn value(open: &OpenConnection, sql: &str) -> String {
    let events = script(open, sql).await;
    let result = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result");
    let stored = open.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    stored
        .value_at(0, 0)
        .map_or_else(|| "NULL".into(), |v: Value| v.to_string())
}

/// A script split as SQLite's is, a transaction across statements, and its state read from
/// the server (ADR-031).
#[tokio::test]
async fn scripts_and_transactions_run_as_on_sqlite() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("libsql-scripts");
    ws.write(
        ".spelaion/connections/edge.toml",
        &connection(&host, port, ""),
    );
    let open = ws.open("edge").await;
    let table = format!("spelaion_script_{}", std::process::id());
    script(
        &open,
        &format!(
            "drop table if exists {table}; create table {table} (id integer primary key, note text); \
             insert into {table} (note) values ('a;b'), ('it''s')"
        ),
    )
    .await;
    assert_eq!(
        value(&open, &format!("select count(*) from {table}")).await,
        "2"
    );
    script(&open, "begin").await;
    script(&open, &format!("delete from {table} where id = 1")).await;
    assert_eq!(
        open.session.connection().transaction_state().await.unwrap(),
        TransactionState::Open
    );
    script(&open, "rollback").await;
    assert_eq!(
        open.session.connection().transaction_state().await.unwrap(),
        TransactionState::Idle
    );
    assert_eq!(
        value(&open, &format!("select count(*) from {table}")).await,
        "2"
    );
    script(&open, &format!("drop table {table}")).await;
    open.close().await;
}

/// ADR-058, point 4: no session barrier, so neither a read-only connection nor agents.
#[test]
fn libsql_has_no_read_only_mode_nor_agents_yet() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("libsql-no-barrier");
    ws.write(
        ".spelaion/connections/ro.toml",
        &connection("db", 8080, "read_only = true\n"),
    );
    let problems: Vec<String> = ws.config().problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("libSQL has no read-only mode")),
        "{problems:?}"
    );

    let ws = Workspace::new("libsql-no-agents");
    ws.write(
        ".spelaion/connections/edge.toml",
        &connection("db", 8080, "\n[mcp]\nexposed = true\nwrites = \"allow\"\n"),
    );
    let config = ws.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(
        agent_view(&config.connections[0], true, false),
        AgentView::EngineWithoutBarrier
    );
}

/// Through the two bastions of `testing/docker`: the plugin connects to the tunnel's local end.
#[tokio::test]
async fn libsql_through_two_bastions() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("libsql-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/edge.toml",
        &connection("libsql", 8080, "tunnel = \"bastions\"\n"),
    );
    let open = ws.open("edge").await;
    assert_eq!(
        value(&open, "select 'behind two bastions'").await,
        "behind two bastions"
    );
    let schemas = open.session.connection().list_schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "main"));
    open.close().await;
}

/// Through the SOCKS5 proxy alone (ADR-027): the core opens it and gives the plugin its local
/// end; the plugin reads no proxy from the environment, so it never goes around it.
#[tokio::test]
async fn libsql_through_a_socks_proxy() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SOCKS") else {
        eprintln!("SPELAION_TEST_SOCKS not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("libsql-proxy");
    ws.write(
        ".spelaion/tunnels/socks.toml",
        &format!(
            "schema_version = 1\n\n[proxy]\ntype = \"socks5\"\nhost = \"{host}\"\nport = {port}\nuser = \"proxyuser\"\npassword = {{ secret = \"cmd:echo proxy-pass\" }}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/edge.toml",
        &connection("libsql", 8080, "tunnel = \"socks\"\n"),
    );
    let open = ws.open("edge").await;
    assert_eq!(
        value(&open, "select 'through the proxy'").await,
        "through the proxy"
    );
    open.close().await;
}
