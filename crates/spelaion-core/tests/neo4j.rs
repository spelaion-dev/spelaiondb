//! Neo4j, served only by its plugin, through the core (ADR-056): a script of Cypher is split
//! by the core and each statement sent as a request in the session's database; nodes and
//! relationships come back as JSON cells; an error stops at its statement; the guard reads the
//! clauses; the explorer's labels and relationship types open with a bounded statement; there is
//! no read-only mode nor agents; and connections go through the two SSH bastions and through the
//! SOCKS5 proxy alone, both opened by the core (a plugin opens no tunnel nor proxy).
//!
//! They need `SPELAION_TEST_NEO4J` (`testing/docker`, 127.0.0.1:17474, user `neo4j`, password
//! `spelaion-test`) and the plugin: the pinned build (`spelaion-fetch-plugins`) or one built on
//! this machine (`SPELAION_DEV_PLUGIN_NEO4J`). The tunnel test also needs `SPELAION_TEST_SSH`,
//! the proxy test `SPELAION_TEST_SOCKS`. Without them the tests are skipped, which does not count
//! as tested.

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
use spelaion_driver_api::{ObjectKind, Secret, Value};
use spelaion_ssh::KnownHosts;

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_NEO4J") else {
        eprintln!("SPELAION_TEST_NEO4J not set: test skipped");
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
                    team_files: Vec::new(),
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
        "schema_version = 1\nengine = \"neo4j\"\nhost = \"{host}\"\nport = {port}\nuser = \"neo4j\"\npassword = {{ secret = \"cmd:echo spelaion-test\" }}\n{extra}\n[tls]\nmode = \"disable\"\n"
    )
}

/// A connection straight to the test server.
async fn direct(label: &str) -> Option<(Workspace, OpenConnection)> {
    let (host, port) = server()?;
    let ws = Workspace::new(label);
    ws.write(
        ".spelaion/connections/graph.toml",
        &connection(&host, port, ""),
    );
    let open = ws.open("graph").await;
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

/// A script of Cypher is split by the core and each statement runs in the session's database;
/// nodes and relationships are JSON cells.
#[tokio::test]
async fn a_cypher_script_runs_statement_by_statement() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((_ws, open)) = direct("neo4j-script").await else {
        return;
    };
    let tag = format!("core{}", std::process::id());
    let script = format!(
        "// The demo's graph\n\
         MATCH (n:SpelaionCore {{tag: '{tag}'}}) DETACH DELETE n;\n\
         CREATE (:SpelaionCore {{tag: '{tag}', name: 'Ana; María'}})-[:SPELAION_KNOWS {{since: 2020}}]->(:SpelaionCore {{tag: '{tag}', name: 'Luis'}});\n\
         MATCH (a:SpelaionCore {{tag: '{tag}'}})-[r:SPELAION_KNOWS]->(b) RETURN a, r, b.name AS name;\n\
         MATCH (n:SpelaionCore {{tag: '{tag}'}}) RETURN count(n) AS n\n"
    );
    let ran = run(&open, &script).await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    let pair = ran.results[ran.results.len() - 2];
    let node: serde_json::Value = serde_json::from_str(&cell(&open, pair, 0, 0)).unwrap();
    assert_eq!(node["labels"], serde_json::json!(["SpelaionCore"]));
    assert_eq!(node["properties"]["name"], "Ana; María");
    let rel: serde_json::Value = serde_json::from_str(&cell(&open, pair, 0, 1)).unwrap();
    assert_eq!(rel["type"], "SPELAION_KNOWS");
    assert_eq!(cell(&open, pair, 0, 2), "Luis");
    assert_eq!(cell(&open, *ran.results.last().unwrap(), 0, 0), "2");
    assert_eq!(
        spelaion_core::active_schema(&open.session).await,
        Ok(Some("neo4j".into()))
    );
    run(
        &open,
        &format!("MATCH (n:SpelaionCore {{tag: '{tag}'}}) DETACH DELETE n"),
    )
    .await;
    open.close().await;
}

/// The server's error stops the script at its statement, and the session goes on.
#[tokio::test]
async fn an_error_stops_at_its_statement() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((_ws, open)) = direct("neo4j-error").await else {
        return;
    };
    let ran = run(&open, "RETURN 1 AS one;\nRETRUN 2;\nRETURN 3 AS three").await;
    assert_eq!(ran.results.len(), 1, "{ran:?}");
    let (index, message) = ran.errors.first().expect("an error").clone();
    assert_eq!(index, 1);
    assert!(message.contains("RETRUN"), "{message}");
    let ran = run(&open, "RETURN 'still here' AS s").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "still here");
    open.close().await;
}

/// The explorer lists the databases, and in each its labels and relationship types; opening
/// one runs the dialect's bounded statement.
#[tokio::test]
async fn the_explorer_browses_labels_and_relationship_types() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((_ws, open)) = direct("neo4j-explorer").await else {
        return;
    };
    let tag = format!("browse{}", std::process::id());
    run(
        &open,
        &format!("CREATE (:SpelaionBrowse {{tag: '{tag}'}})-[:SPELAION_LINK]->(:SpelaionBrowse {{tag: '{tag}'}})"),
    )
    .await;
    let connection = open.session.connection();
    let schemas = connection.list_schemas().await.unwrap();
    assert!(
        schemas.iter().any(|s| s.name == "neo4j" && !s.system),
        "{schemas:?}"
    );
    let objects = connection.list_objects("neo4j").await.unwrap();
    let dialect = spelaion_dialect::for_engine("neo4j").unwrap();
    for (name, kind) in [
        ("SpelaionBrowse", ObjectKind::NodeLabel),
        ("SPELAION_LINK", ObjectKind::RelationshipType),
    ] {
        let object = objects
            .iter()
            .find(|o| o.name == name && o.kind == kind)
            .unwrap_or_else(|| panic!("{name} not listed: {objects:?}"));
        let statement = dialect.browse_statement(object).expect("a statement");
        let ran = run(&open, &statement).await;
        assert!(ran.errors.is_empty(), "{statement}: {:?}", ran.errors);
        let first: serde_json::Value =
            serde_json::from_str(&cell(&open, ran.results[0], 0, 0)).unwrap();
        assert!(first.get("elementId").is_some(), "{first}");
    }
    run(
        &open,
        &format!("MATCH (n:SpelaionBrowse {{tag: '{tag}'}}) DETACH DELETE n"),
    )
    .await;
    open.close().await;
}

#[test]
fn the_guard_asks_before_what_neo4j_would_lose() {
    let _turn = crate::serial::file_turn(module_path!());
    let dialect = spelaion_dialect::for_engine("neo4j").expect("Cypher");
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
    assert_eq!(risk("MATCH (n) DETACH DELETE n", &plain), [Risk::DeleteAll]);
    assert_eq!(risk("DROP INDEX person_name", &plain), [Risk::Drop]);
    // A write, a read and a procedure ask nothing by default…
    for quiet in ["CREATE (:P)", "MATCH (n) RETURN n", "CALL db.labels()"] {
        assert!(risk(quiet, &plain).is_empty(), "{quiet}");
    }
    // …and on Production a write and a procedure ask, never a read.
    for asks in ["CREATE (:P)", "MATCH (n) SET n.x = 1", "CALL apoc.do.it()"] {
        assert_eq!(risk(asks, &production), [Risk::DataChange], "{asks}");
    }
    for read in ["MATCH (n) RETURN n", "SHOW DATABASES", "RETURN 1"] {
        assert!(risk(read, &production).is_empty(), "{read}");
    }
}

#[test]
fn neo4j_has_no_read_only_mode_nor_agents_yet() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("neo4j-no-barrier");
    ws.write(
        ".spelaion/connections/ro.toml",
        &connection("db", 7474, "read_only = true\n"),
    );
    let problems: Vec<String> = ws.config().problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("Neo4j has no read-only mode")),
        "{problems:?}"
    );

    let ws = Workspace::new("neo4j-no-agents");
    ws.write(
        ".spelaion/connections/graph.toml",
        &connection("db", 7474, "\n[mcp]\nexposed = true\nwrites = \"allow\"\n"),
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
async fn neo4j_through_two_bastions() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("neo4j-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/graph.toml",
        &connection("neo4j", 7474, "tunnel = \"bastions\"\n"),
    );
    let open = ws.open("graph").await;
    let ran = run(&open, "RETURN 'behind two bastions' AS s").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "behind two bastions");
    open.close().await;
}

/// Through the SOCKS5 proxy alone (ADR-027): the core opens it and gives the plugin its local
/// end; the plugin reads no proxy from the environment, so it never goes around it.
#[tokio::test]
async fn neo4j_through_a_socks_proxy() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SOCKS") else {
        eprintln!("SPELAION_TEST_SOCKS not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("neo4j-proxy");
    ws.write(
        ".spelaion/tunnels/socks.toml",
        &format!(
            "schema_version = 1\n\n[proxy]\ntype = \"socks5\"\nhost = \"{host}\"\nport = {port}\nuser = \"proxyuser\"\npassword = {{ secret = \"cmd:echo proxy-pass\" }}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/graph.toml",
        &connection("neo4j", 7474, "tunnel = \"socks\"\n"),
    );
    let open = ws.open("graph").await;
    let ran = run(&open, "RETURN 'through the proxy' AS s").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), "through the proxy");
    open.close().await;
}
