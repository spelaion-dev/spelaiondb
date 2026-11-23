//! Google Cloud Spanner, served only by its plugin, through the core (SDB-352): a script is
//! split by the core's GoogleSQL dialect and run statement by statement (DDL through the admin
//! API, data changes and queries), a transaction is followed from the plugin's session, an
//! error stops at its statement, the explorer's default schema is `default` and names in it are
//! written unqualified, the guard asks before what loses data, there is no read-only mode nor
//! agents, and connections go through the two SSH bastions and through the SOCKS5 proxy alone,
//! both opened by the core (a plugin opens no tunnel nor proxy).
//!
//! They need `SPELAION_TEST_SPANNER` (`testing/docker`, Google's emulator on 127.0.0.1:19021; the
//! tests create its instance `test` and database `core` in project `spelaion`) and the plugin: the
//! pinned build (`spelaion-fetch-plugins`) or one built on this machine
//! (`SPELAION_DEV_PLUGIN_SPANNER`). The tunnel test also needs `SPELAION_TEST_SSH`, the proxy test
//! `SPELAION_TEST_SOCKS`. Without them the tests are skipped, which does not count as tested.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::{
    AgentView, CoreEvent, Engines, OpenConnection, OpenOptions, ResultId, ResultStore, Risk,
    TunnelPool, agent_view, open_connection, qualified_name, script_risks,
};
use spelaion_driver_api::{Secret, TransactionState, Value};
use spelaion_ssh::KnownHosts;

/// Spanner runs one schema change at a time: the tests that change the schema take turns.
static SCHEMA: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_SPANNER") else {
        eprintln!("SPELAION_TEST_SPANNER not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let port = port.parse().expect("a port");
    ensure_database(host, port);
    Some((host.to_string(), port))
}

/// One plain HTTP request to the emulator's admin API; its status.
fn http(host: &str, port: u16, method: &str, path: &str, body: &str) -> u16 {
    let mut stream = std::net::TcpStream::connect((host, port)).expect("the emulator answers");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut answer = String::new();
    let _ = stream.read_to_string(&mut answer);
    answer
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// The instance and the database the tests use, created once (an existing one is fine).
fn ensure_database(host: &str, port: u16) {
    let base = "/v1/projects/spelaion";
    if http(
        host,
        port,
        "GET",
        &format!("{base}/instances/test/databases/core"),
        "",
    ) == 200
    {
        return;
    }
    http(
        host,
        port,
        "POST",
        &format!("{base}/instances"),
        r#"{"instanceId":"test","instance":{"config":"projects/spelaion/instanceConfigs/emulator-config","displayName":"test","nodeCount":1}}"#,
    );
    http(
        host,
        port,
        "POST",
        &format!("{base}/instances/test/databases"),
        r#"{"createStatement":"CREATE DATABASE core"}"#,
    );
    for _ in 0..50 {
        if http(
            host,
            port,
            "GET",
            &format!("{base}/instances/test/databases/core"),
            "",
        ) == 200
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the emulator did not create the test database");
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
        "schema_version = 1\nengine = \"spanner\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion/test/core\"\n{extra}\n[tls]\nmode = \"disable\"\n"
    )
}

/// A connection straight to the emulator.
async fn direct(label: &str) -> Option<(Workspace, OpenConnection)> {
    let (host, port) = server()?;
    let ws = Workspace::new(label);
    ws.write(
        ".spelaion/connections/cloud.toml",
        &connection(&host, port, ""),
    );
    let open = ws.open("cloud").await;
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

/// A script split by the core: strings (triple-quoted too), quoted names and comments hide
/// their semicolons; DDL, data changes and queries each go where they belong; a transaction
/// across statements is followed, and one the core opens in manual commit too.
#[tokio::test]
async fn a_script_runs_statement_by_statement_with_its_transaction() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((_ws, open)) = direct("spanner-script").await else {
        return;
    };
    let _turn = SCHEMA.lock().await;
    let table = unique("Core");
    let script = format!(
        "# The demo's table; with a semicolon in a comment\n\
         CREATE TABLE {table} (id INT64 NOT NULL, s STRING(MAX)) PRIMARY KEY (id);\n\
         INSERT INTO `{table}` (id, s) VALUES (1, 'a;b'), (2, '''it's;\nmultiline''');\n\
         -- another; comment\n\
         SELECT s FROM {table} ORDER BY id;\n\
         SELECT count(*) FROM {table}"
    );
    let ran = run(&open, &script).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    let rows = ran.results[ran.results.len() - 2];
    assert_eq!(cell(&open, rows, 0, 0), "a;b");
    assert_eq!(cell(&open, rows, 1, 0), "it's;\nmultiline");
    assert_eq!(cell(&open, *ran.results.last().unwrap(), 0, 0), "2");

    let connection = open.session.connection();
    let status = open
        .session
        .follow_transactions(&Safeguards::default())
        .await;
    assert!(status.supported);
    let ran = run(
        &open,
        &format!("BEGIN; INSERT INTO {table} (id, s) VALUES (3, 'tx')"),
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
    open.session.set_auto_commit(false).await.unwrap();
    let ran = run(
        &open,
        &format!("INSERT INTO {table} (id, s) VALUES (4, 'kept')"),
    )
    .await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(
        connection.transaction_state().await.unwrap(),
        TransactionState::Open
    );
    open.session.commit().await.unwrap();
    open.session.set_auto_commit(true).await.unwrap();
    let ran = run(&open, &format!("SELECT count(*) FROM {table}")).await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "3");

    let ran = run(&open, &format!("DROP TABLE {table}")).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    open.close().await;
}

/// The server's error stops the script at its statement, and the session goes on.
#[tokio::test]
async fn an_error_stops_at_its_statement() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((_ws, open)) = direct("spanner-error").await else {
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

/// The default schema is `default` in the explorer and in the editor, and the names the core
/// writes for it are unqualified.
#[tokio::test]
async fn the_default_schema_is_named_default_and_left_out_of_names() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((_ws, open)) = direct("spanner-explorer").await else {
        return;
    };
    let _turn = SCHEMA.lock().await;
    let table = unique("Browse");
    let ran = run(
        &open,
        &format!("CREATE TABLE {table} (id INT64 NOT NULL) PRIMARY KEY (id)"),
    )
    .await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    let connection = open.session.connection();
    let schemas = connection.list_schemas().await.unwrap();
    assert!(
        schemas.iter().any(|s| s.name == "default" && !s.system),
        "{schemas:?}"
    );
    let objects = connection.list_objects("default").await.unwrap();
    assert!(objects.iter().any(|o| o.name == table), "{objects:?}");
    assert_eq!(
        spelaion_core::active_schema(&open.session).await,
        Ok(Some("default".into()))
    );
    let dialect = spelaion_dialect::for_engine("spanner").unwrap();
    let name = qualified_name(dialect.as_ref(), "default", std::slice::from_ref(&table));
    assert_eq!(name, format!("`{table}`"));
    let ran = run(&open, &format!("SELECT count(*) FROM {name}")).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(
        qualified_name(dialect.as_ref(), "other", &["t".to_string()]),
        "`other`.`t`"
    );
    run(&open, &format!("DROP TABLE {table}")).await;
    open.close().await;
}

#[test]
fn the_guard_asks_before_what_spanner_would_lose() {
    let _turn = crate::serial::file_turn(module_path!());
    let dialect = spelaion_dialect::for_engine("spanner").expect("GoogleSQL");
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
    // Spanner itself asks for a WHERE; the guard still reads one written without it.
    assert_eq!(risk("DELETE FROM t", &plain), [Risk::DeleteAll]);
    assert_eq!(risk("UPDATE t SET a = 1", &plain), [Risk::UpdateAll]);
    assert_eq!(risk("DROP TABLE t", &plain), [Risk::Drop]);
    for quiet in [
        "SELECT 1",
        "DELETE FROM t WHERE id = 1",
        "INSERT INTO t (id) VALUES (1)",
    ] {
        assert!(risk(quiet, &plain).is_empty(), "{quiet}");
    }
    assert_eq!(
        risk("INSERT INTO t (id) VALUES (1)", &production),
        [Risk::DataChange]
    );
    assert!(risk("SELECT 1", &production).is_empty());
}

#[test]
fn spanner_has_no_read_only_mode_nor_agents() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("spanner-no-barrier");
    ws.write(
        ".spelaion/connections/ro.toml",
        &connection("db", 9020, "read_only = true\n"),
    );
    let problems: Vec<String> = ws.config().problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("Cloud Spanner has no read-only mode")),
        "{problems:?}"
    );

    let ws = Workspace::new("spanner-no-agents");
    ws.write(
        ".spelaion/connections/cloud.toml",
        &connection("db", 9020, "\n[mcp]\nexposed = true\nwrites = \"allow\"\n"),
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
async fn spanner_through_two_bastions() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("spanner-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/cloud.toml",
        &connection("spanner", 9020, "tunnel = \"bastions\"\n"),
    );
    let open = ws.open("cloud").await;
    let ran = run(&open, "SELECT 'behind two bastions'").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "behind two bastions");
    open.close().await;
}

/// Through the SOCKS5 proxy alone (ADR-027): the core opens it and gives the plugin its local
/// end; the plugin reads no proxy from the environment, so it never goes around it.
#[tokio::test]
async fn spanner_through_a_socks_proxy() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SOCKS") else {
        eprintln!("SPELAION_TEST_SOCKS not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("spanner-proxy");
    ws.write(
        ".spelaion/tunnels/socks.toml",
        &format!(
            "schema_version = 1\n\n[proxy]\ntype = \"socks5\"\nhost = \"{host}\"\nport = {port}\nuser = \"proxyuser\"\npassword = {{ secret = \"cmd:echo proxy-pass\" }}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/cloud.toml",
        &connection("spanner", 9020, "tunnel = \"socks\"\n"),
    );
    let open = ws.open("cloud").await;
    let ran = run(&open, "SELECT 'through the proxy'").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "through the proxy");
    open.close().await;
}
