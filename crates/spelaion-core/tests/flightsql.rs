//! The Flight SQL engines, one plugin (ADR-041), through the core: GizmoSQL as `gizmosql` and as
//! the generic `flightsql`, opened from a connection file directly and through the two SSH
//! bastions; and what they do not get: agents' reads, as SpelaionDB has no read barrier of its
//! own for them. Dremio is tested in the plugin's repository.
//!
//! They need `SPELAION_TEST_GIZMOSQL` (127.0.0.1:31337, `testing/docker`) and the plugin: the
//! pinned build or one built on this machine (`SPELAION_DEV_PLUGIN_GIZMOSQL`,
//! `SPELAION_DEV_PLUGIN_FLIGHTSQL`). The tunnel test also needs `SPELAION_TEST_SSH`. Without them
//! the tests are skipped, which does not count as tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadError, ReadLimits, ResultStore,
    TunnelPool, open_connection,
};
use spelaion_driver_api::Secret;
use spelaion_ssh::KnownHosts;

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_GIZMOSQL") else {
        eprintln!("SPELAION_TEST_GIZMOSQL not set: test skipped");
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

/// A script of several statements runs as the editor runs it: split by the core, each statement
/// sent alone, results as Arrow straight from the server.
#[tokio::test]
async fn gizmosql_runs_scripts_and_browses_as_named_and_as_generic() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    for engine in ["gizmosql", "flightsql"] {
        let ws = Workspace::new(&format!("{engine}-script"));
        ws.write(
            ".spelaion/connections/lake.toml",
            &connection(engine, &host, port, None),
        );
        let open = ws.open("lake").await;
        let table = format!("lake_{engine}_{}", std::process::id());
        let script = format!(
            "CREATE OR REPLACE TABLE {table} AS SELECT range AS id, 'r' || range AS name FROM range(1000);\n\
             SELECT count(*), max(name) FROM {table} WHERE name LIKE 'r9%';"
        );
        assert_eq!(value(&open, &script).await, "111", "{engine}");
        let objects = open
            .session
            .connection()
            .list_objects("main")
            .await
            .unwrap();
        assert!(objects.iter().any(|o| o.name == table), "{engine}");
        let _ = value(&open, &format!("DROP TABLE {table}; SELECT 1")).await;

        // No barrier of ours: agents' reads are not run here, whatever the statement.
        let refused = open
            .session
            .run_read_only("SELECT 1", &ReadLimits::default())
            .await;
        assert!(
            matches!(refused, Err(ReadError::Unsupported(_))),
            "{engine}: {refused:?}"
        );
        open.close().await;
    }
}

/// Through the two bastions of `testing/docker`: the plugin's gRPC goes to the tunnel's local
/// end, whose far end is `gizmosql:31337`, a name only the bastions resolve.
#[tokio::test]
async fn gizmosql_through_two_bastions() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("gizmosql-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/lake.toml",
        &connection("gizmosql", "gizmosql", 31337, Some("bastions")),
    );
    let open = ws.open("lake").await;
    assert_eq!(
        value(&open, "SELECT count(*) FROM range(250000)").await,
        "250000"
    );
    assert!(
        !open
            .session
            .connection()
            .list_schemas()
            .await
            .unwrap()
            .is_empty()
    );
    open.close().await;
}
