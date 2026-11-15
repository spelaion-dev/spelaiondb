//! Oracle Database, served only by its plugin on Oracle's thin driver (ADR-059), through the
//! core: scripts split as SQL*Plus reads them (PL/SQL blocks and `/`), the transaction read from
//! the driver, what Oracle does not offer yet (read-only connections and agents), and a
//! connection through the two SSH bastions with TLS checked against the database's name.
//!
//! They need `SPELAION_TEST_ORACLE` (`testing/docker`, 127.0.0.1:11521) and the plugin: the
//! pinned build (`spelaion-fetch-plugins`) or one built on this machine
//! (`SPELAION_DEV_PLUGIN_ORACLE`). The tunnel test also needs `SPELAION_TEST_SSH`. Without them
//! the tests are skipped, which does not count as tested.

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
    let Ok(addr) = std::env::var("SPELAION_TEST_ORACLE") else {
        eprintln!("SPELAION_TEST_ORACLE not set: test skipped");
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
                password: Some(Secret::new("spelaion")),
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
        "schema_version = 1\nengine = \"oracle\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"FREEPDB1\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\n{extra}"
    )
}

/// Runs a script; the first failure fails the test.
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

/// The first cell of the last result of a script, as text.
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

/// A script as SQL*Plus reads it: statements end at `;` (which is not sent), PL/SQL blocks keep
/// theirs until a `/` line, and each runs on the server as one statement.
#[tokio::test]
async fn scripts_with_plsql_blocks_run_as_sqlplus_reads_them() {
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("oracle-scripts");
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(&host, port, ""),
    );
    let open = ws.open("ora").await;
    let table = format!("spelaion_script_{}", std::process::id());
    let events = script(
        &open,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (id number(9) primary key, note varchar2(40));\n\
             begin\n  for i in 1 .. 3 loop\n    insert into {table} values (i, q'[it's; row ]' || i);\n  end loop;\n  commit;\nend;\n/\n\
             select count(*) from {table}"
        ),
    )
    .await;
    let finished = events
        .iter()
        .filter(|e| matches!(e, CoreEvent::StatementFinished { .. }))
        .count();
    assert_eq!(finished, 4, "{events:?}");
    assert_eq!(
        value(&open, &format!("select count(*) from {table}")).await,
        "3"
    );
    assert_eq!(
        value(&open, &format!("select note from {table} where id = 2")).await,
        "it's; row 2"
    );
    script(&open, &format!("drop table {table} purge")).await;
    open.close().await;
}

/// The transaction state comes from the driver after every statement (ADR-031): an `INSERT`
/// opens one, `ROLLBACK` ends it, and closing then needs no confirmation.
#[tokio::test]
async fn the_transaction_is_read_from_the_driver() {
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("oracle-transactions");
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(&host, port, ""),
    );
    let open = ws.open("ora").await;
    let table = format!("spelaion_tx_{}", std::process::id());
    script(
        &open,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (id number(9))"
        ),
    )
    .await;
    script(&open, &format!("insert into {table} values (1)")).await;
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
        "0"
    );
    script(&open, &format!("drop table {table} purge")).await;
    open.close().await;
}

/// ADR-059, point 2: read-only connections and agents wait for a barrier that stands up to
/// tests that try to get around it, so neither the configuration nor `[mcp]` turns them on.
#[test]
fn oracle_has_no_read_only_mode_nor_agents_yet() {
    let ws = Workspace::new("oracle-no-barrier");
    ws.write(
        ".spelaion/connections/ro.toml",
        &connection("db", 1521, "read_only = true\n"),
    );
    let problems: Vec<String> = ws.config().problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("Oracle has no read-only mode")),
        "{problems:?}"
    );

    let ws = Workspace::new("oracle-no-agents");
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection("db", 1521, "\n[mcp]\nexposed = true\nwrites = \"allow\"\n"),
    );
    let config = ws.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    for all_read_only in [false, true] {
        assert_eq!(
            agent_view(&config.connections[0], true, all_read_only),
            AgentView::EngineWithoutBarrier
        );
    }
}

/// Through the two bastions of `testing/docker`, with TLS: the plugin connects to the tunnel's
/// local end (127.0.0.1) and checks the certificate against the database's name behind the
/// bastions, `oracle-tls`, signed by the test authority (ADR-059, ADR-041: a plugin opens no
/// tunnel).
#[tokio::test]
async fn oracle_through_two_bastions_with_tls_verified_by_name() {
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("oracle-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    let ca = toml_path(&keys().join("ca.crt"));
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(
            "oracle-tls",
            2484,
            &format!("tunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {ca}\n"),
        ),
    );
    let open = ws.open("ora").await;
    assert_eq!(value(&open, "select user from dual").await, "SPELAION");
    assert_eq!(
        value(
            &open,
            "select count(*) from dual connect by level <= 100000"
        )
        .await,
        "100000"
    );
    let schemas = open.session.connection().list_schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "SPELAION"));
    open.close().await;
}
