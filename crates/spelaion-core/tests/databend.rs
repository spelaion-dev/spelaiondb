//! Databend, served only by its plugin, through the core (SDB-351): a script is split by the
//! core's own dialect and run statement by statement, a transaction is read from the server, a
//! session's setup holds, an error stops at its statement, the explorer lists databases and
//! tables, the guard asks before what loses data, there is no read-only mode nor agents
//! (Databend enforces none, SDB-350), and connections go through the two SSH bastions and
//! through the SOCKS5 proxy alone, both opened by the core (a plugin opens no tunnel nor proxy).
//!
//! They need `SPELAION_TEST_DATABEND` (`testing/docker`, 127.0.0.1:18320, user `spelaion`,
//! password `spelaion-test`) and the plugin: the pinned build (`spelaion-fetch-plugins`) or one
//! built on this machine (`SPELAION_DEV_PLUGIN_DATABEND`). The tunnel test also needs
//! `SPELAION_TEST_SSH`, the proxy test `SPELAION_TEST_SOCKS`. Without them the tests are
//! skipped, which does not count as tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::{
    AgentView, CoreEvent, Engines, OpenConnection, OpenOptions, ResultId, ResultStore, Risk,
    TunnelPool, agent_view, open_connection, script_risks,
};
use spelaion_driver_api::{ObjectKind, Secret, TransactionState, Value};
use spelaion_ssh::KnownHosts;

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_DATABEND") else {
        eprintln!("SPELAION_TEST_DATABEND not set: test skipped");
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
        "schema_version = 1\nengine = \"databend\"\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion-test\" }}\n{extra}\n[tls]\nmode = \"disable\"\n"
    )
}

/// A connection straight to the test server.
async fn direct(label: &str, extra: &str) -> Option<(Workspace, OpenConnection)> {
    let (host, port) = server()?;
    let ws = Workspace::new(label);
    ws.write(
        ".spelaion/connections/warehouse.toml",
        &connection(&host, port, extra),
    );
    let open = ws.open("warehouse").await;
    Some((ws, open))
}

/// What a script did: each result, and each failed statement's index and message.
#[derive(Default, Debug)]
struct Ran {
    results: Vec<ResultId>,
    errors: Vec<(usize, String)>,
}

async fn run(open: &OpenConnection, script: &str) -> Ran {
    let mut ran = Ran::default();
    let mut events = open.session.run_script(script);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => ran.results.push(result),
            CoreEvent::StatementFailed { index, error } => ran.errors.push((index, error.message)),
            _ => {}
        }
    }
    ran
}

/// The cell at `row`, `column` of a stored result, as text.
fn cell(open: &OpenConnection, result: ResultId, row: usize, column: usize) -> String {
    let stored = open.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    stored
        .value_at(row, column)
        .map_or_else(|| "NULL".into(), |v: Value| v.to_string())
}

fn unique(prefix: &str) -> String {
    format!("{prefix}_{}", std::process::id())
}

/// A script split by the core: strings, quoted names, comments and a `$$` body hide their
/// semicolons; a transaction across statements is read from the server.
#[tokio::test]
async fn a_script_runs_statement_by_statement_with_its_transaction() {
    let Some((_ws, open)) = direct("databend-script", "").await else {
        return;
    };
    let db = unique("sdb351_core");
    let script = format!(
        "-- The demo's warehouse; with a semicolon in a comment\n\
         CREATE OR REPLACE DATABASE {db};\n\
         USE {db};\n\
         CREATE TABLE \"we;ird\" (id INT, s STRING, ts TIMESTAMP);\n\
         INSERT INTO \"we;ird\" VALUES (1, 'a;b', '2024-02-29 10:20:30'), (2, 'it''s', NULL);\n\
         /* a block; comment */ SELECT s, ts FROM \"we;ird\" ORDER BY id;\n\
         SELECT count(*) FROM \"we;ird\""
    );
    let ran = run(&open, &script).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    let rows = ran.results[ran.results.len() - 2];
    assert_eq!(cell(&open, rows, 0, 0), "a;b");
    assert_eq!(cell(&open, rows, 0, 1), "2024-02-29 10:20:30");
    assert_eq!(cell(&open, rows, 1, 0), "it's");
    assert_eq!(cell(&open, *ran.results.last().unwrap(), 0, 0), "2");
    assert_eq!(
        spelaion_core::active_schema(&open.session).await,
        Ok(Some(db.clone()))
    );

    // An editor session follows its transaction (ADR-031): one written in the script…
    let connection = open.session.connection();
    let status = open
        .session
        .follow_transactions(&Safeguards::default())
        .await;
    assert!(status.supported);
    let ran = run(
        &open,
        "BEGIN; INSERT INTO \"we;ird\" VALUES (3, 'tx', NULL)",
    )
    .await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(
        connection.transaction_state().await.unwrap(),
        TransactionState::Open
    );
    assert!(open.session.close_needs_confirmation().await);
    open.session.rollback().await.unwrap();
    assert_eq!(
        connection.transaction_state().await.unwrap(),
        TransactionState::Idle
    );
    // …and one the core opens in manual commit, with Databend's own `BEGIN`.
    open.session.set_auto_commit(false).await.unwrap();
    let ran = run(&open, "INSERT INTO \"we;ird\" VALUES (4, 'kept', NULL)").await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(
        connection.transaction_state().await.unwrap(),
        TransactionState::Open
    );
    open.session.commit().await.unwrap();
    open.session.set_auto_commit(true).await.unwrap();
    let ran = run(&open, "SELECT count(*) FROM \"we;ird\"").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "3");

    // The schema chosen above the editor is the session's database.
    spelaion_core::set_active_schema(&open.session, "default")
        .await
        .unwrap();
    assert_eq!(
        spelaion_core::active_schema(&open.session).await,
        Ok(Some("default".into()))
    );

    run(&open, &format!("DROP DATABASE {db}")).await;
    open.close().await;
}

/// The server's error stops the script at its statement, and the session goes on.
#[tokio::test]
async fn an_error_stops_at_its_statement() {
    let Some((_ws, open)) = direct("databend-error", "").await else {
        return;
    };
    let ran = run(
        &open,
        "SELECT 1 AS one; SELECT * FROM not_there; SELECT 3 AS three",
    )
    .await;
    assert_eq!(ran.results.len(), 1, "{ran:?}");
    let (index, message) = ran.errors.first().expect("an error").clone();
    assert_eq!(index, 1);
    assert!(message.contains("not_there"), "{message}");
    let ran = run(&open, "SELECT 'still here'").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "still here");
    open.close().await;
}

/// The connection's `[session]` setup runs when the session opens, and holds.
#[tokio::test]
async fn the_session_setup_holds() {
    let Some((_ws, open)) = direct(
        "databend-setup",
        "\n[session]\nsetup = \"SET timezone = 'Asia/Tokyo';\"\n",
    )
    .await
    else {
        return;
    };
    let ran = run(&open, "SELECT timezone()").await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(cell(&open, ran.results[0], 0, 0), "Asia/Tokyo");
    open.close().await;
}

#[tokio::test]
async fn the_explorer_lists_databases_tables_and_views() {
    let Some((_ws, open)) = direct("databend-explorer", "").await else {
        return;
    };
    let db = unique("sdb351_browse");
    let ran = run(
        &open,
        &format!(
            "CREATE OR REPLACE DATABASE {db}; CREATE TABLE {db}.t (id INT); \
             CREATE VIEW {db}.v AS SELECT id FROM {db}.t"
        ),
    )
    .await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    let connection = open.session.connection();
    let schemas = connection.list_schemas().await.unwrap();
    assert!(
        schemas.iter().any(|s| s.name == db && !s.system),
        "{schemas:?}"
    );
    let objects = connection.list_objects(&db).await.unwrap();
    let kinds: Vec<(&str, ObjectKind)> =
        objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(kinds, [("t", ObjectKind::Table), ("v", ObjectKind::View)]);
    run(&open, &format!("DROP DATABASE {db}")).await;
    open.close().await;
}

#[test]
fn the_guard_asks_before_what_databend_would_lose() {
    let dialect = spelaion_dialect::for_engine("databend").expect("Databend's dialect");
    let plain = Safeguards::default();
    let production = Safeguards {
        confirm_data_changes: true,
        ..Safeguards::default()
    };
    let risk = |script: &str, safeguards: &Safeguards| -> Vec<Risk> {
        script_risks(dialect.as_ref(), script, safeguards)
            .into_iter()
            .map(|r| r.risk)
            .collect()
    };
    assert_eq!(risk("DELETE FROM t", &plain), [Risk::DeleteAll]);
    assert_eq!(risk("UPDATE t SET a = 1", &plain), [Risk::UpdateAll]);
    assert_eq!(risk("DROP TABLE t", &plain), [Risk::Drop]);
    assert_eq!(risk("DROP DATABASE d", &plain), [Risk::Drop]);
    assert!(!risk("TRUNCATE TABLE t", &plain).is_empty());
    for quiet in [
        "SELECT 1",
        "DELETE FROM t WHERE id = 1",
        "INSERT INTO t VALUES (1)",
    ] {
        assert!(risk(quiet, &plain).is_empty(), "{quiet}");
    }
    assert_eq!(
        risk("INSERT INTO t VALUES (1)", &production),
        [Risk::DataChange]
    );
    assert!(risk("SELECT 1", &production).is_empty());
}

#[test]
fn databend_has_no_read_only_mode_nor_agents() {
    let ws = Workspace::new("databend-no-barrier");
    ws.write(
        ".spelaion/connections/ro.toml",
        &connection("db", 8000, "read_only = true\n"),
    );
    let problems: Vec<String> = ws.config().problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("Databend has no read-only mode")),
        "{problems:?}"
    );

    let ws = Workspace::new("databend-no-agents");
    ws.write(
        ".spelaion/connections/warehouse.toml",
        &connection("db", 8000, "\n[mcp]\nexposed = true\nwrites = \"allow\"\n"),
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
async fn databend_through_two_bastions() {
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("databend-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/warehouse.toml",
        &connection("databend", 8000, "tunnel = \"bastions\"\n"),
    );
    let open = ws.open("warehouse").await;
    let ran = run(&open, "SELECT 'behind two bastions'").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "behind two bastions");
    open.close().await;
}

/// Through the SOCKS5 proxy alone (ADR-027): the core opens it and gives the plugin its local
/// end; the plugin reads no proxy from the environment, so it never goes around it.
#[tokio::test]
async fn databend_through_a_socks_proxy() {
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SOCKS") else {
        eprintln!("SPELAION_TEST_SOCKS not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("databend-proxy");
    ws.write(
        ".spelaion/tunnels/socks.toml",
        &format!(
            "schema_version = 1\n\n[proxy]\ntype = \"socks5\"\nhost = \"{host}\"\nport = {port}\nuser = \"proxyuser\"\npassword = {{ secret = \"cmd:echo proxy-pass\" }}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/warehouse.toml",
        &connection("databend", 8000, "tunnel = \"socks\"\n"),
    );
    let open = ws.open("warehouse").await;
    let ran = run(&open, "SELECT 'through the proxy'").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "through the proxy");
    open.close().await;
}
