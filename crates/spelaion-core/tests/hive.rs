//! HiveServer2's engines, one plugin (SDB-176), through the core: Apache Hive, opened from a
//! connection file directly and through the two SSH bastions; the guard on what Hive replaces
//! without deleting; and what they do not get: agents' reads, as SpelaionDB has no read barrier
//! of its own for them. Spark's Thrift server, Kyuubi and Impala are tested in the plugin's
//! repository.
//!
//! They need `SPELAION_TEST_HIVE` (127.0.0.1:11010, `testing/docker`) and the plugin: the pinned
//! build or one built on this machine (`SPELAION_DEV_PLUGIN_HIVE`). The tunnel test also needs
//! `SPELAION_TEST_SSH`. Without them the tests are skipped, which does not count as tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadError, ReadLimits, ResultStore,
    TunnelPool, open_connection, script_risks,
};
use spelaion_driver_api::Secret;
use spelaion_ssh::KnownHosts;

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_HIVE") else {
        eprintln!("SPELAION_TEST_HIVE not set: test skipped");
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
            password: Some(Secret::new("spelaion")),
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

fn connection(engine: &str, host: &str, port: u16, tunnel: Option<&str>) -> String {
    let tunnel = tunnel
        .map(|t| format!("tunnel = \"{t}\"\n"))
        .unwrap_or_default();
    format!(
        "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\n{tunnel}\n[tls]\nmode = \"disable\"\n"
    )
}

/// A script of several statements runs as the editor runs it: split by the core with Hive's
/// rules (a `;` inside a string or a backquoted name does not end a statement), each statement
/// sent alone, its result read back as Arrow.
#[tokio::test]
async fn hive_runs_scripts_and_browses() {
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("hive-script");
    ws.write(
        ".spelaion/connections/warehouse.toml",
        &connection("hive", &host, port, None),
    );
    let open = ws.open("warehouse").await;
    let db = format!("spelaion_core_{}", std::process::id());
    let script = format!(
        "create database {db};\n\
         create table {db}.`odd;name` (id int, label string);\n\
         insert into {db}.`odd;name` values (1, 'a;b'), (2, 'it\\'s');\n\
         select count(*), max(label) from {db}.`odd;name`;"
    );
    assert_eq!(value(&open, &script).await, "2");
    let objects = open.session.connection().list_objects(&db).await.unwrap();
    assert!(objects.iter().any(|o| o.name == "odd;name"), "{objects:?}");
    let _ = value(&open, &format!("drop database {db} cascade; select 1")).await;

    // No barrier of ours: agents' reads are not run here, whatever the statement.
    let refused = open
        .session
        .run_read_only("select 1", &ReadLimits::default())
        .await;
    assert!(
        matches!(refused, Err(ReadError::Unsupported(_))),
        "{refused:?}"
    );
    // What replaces a table's rows asks first, read with this session's dialect.
    let risks = script_risks(
        open.session.dialect().as_ref(),
        "insert into t values (1); insert overwrite table t select 1",
        &Default::default(),
    );
    assert_eq!(risks.len(), 1, "{risks:?}");
    assert_eq!(risks[0].index, 1);
    open.close().await;
}

/// Through the two bastions of `testing/docker`: the plugin's socket goes to the tunnel's local
/// end, whose far end is `hive:10000`, a name only the bastions resolve.
#[tokio::test]
async fn hive_through_two_bastions() {
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("hive-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/warehouse.toml",
        &connection("hive", "hive", 10000, Some("bastions")),
    );
    let open = ws.open("warehouse").await;
    assert_eq!(value(&open, "select 40 + 2").await, "42");
    assert!(
        open.session
            .connection()
            .list_schemas()
            .await
            .unwrap()
            .iter()
            .any(|s| s.name == "default")
    );
    open.close().await;
}
