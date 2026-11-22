//! MySQL's cleartext password plugin (SDB-396): the core lets a driver answer it only over TLS
//! that checks the server's certificate or through an SSH tunnel, and these tests try every
//! other way to get the password sent in clear. A driver that records what it is given stands
//! in for MySQL's, so a refused connection is seen to never reach the driver at all. The tunnel
//! case also needs `SPELAION_TEST_SSH` (the Docker bastions); everything else needs nothing
//! external.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use spelaion_config::{
    ConnectionId, ResolvedConnection, Resolver, Source, SystemBackend, TrustStore, engine_options,
    load,
};
use spelaion_core::{
    Engines, OpenError, OpenOptions, ResultStore, TunnelPool, cleartext, open_connection,
};
use spelaion_driver_api::{ConnectParams, Connection, Driver, DriverError, Result, Secret};
use spelaion_ssh::KnownHosts;

/// MySQL's place, taken by a driver that only remembers what it was asked to connect with.
struct Recorder {
    engine: &'static str,
    seen: Arc<Mutex<Vec<ConnectParams>>>,
}

#[async_trait]
impl Driver for Recorder {
    fn engine(&self) -> &str {
        self.engine
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        self.seen.lock().unwrap().push(params.clone());
        Err(DriverError::Connection("recorded".into()))
    }
}

struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-cleartext-{name}-{}", std::process::id()));
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

    fn resolve(&self, id: &str) -> ResolvedConnection {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving")
    }

    /// Opens `id` with the recorder as its engine's driver: the error, and what the driver
    /// was given (nothing when the core refused first).
    async fn open(&self, id: &str, engine: &'static str) -> (OpenError, Vec<ConnectParams>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let engines = Engines::builtin().with_driver(Arc::new(Recorder {
            engine,
            seen: seen.clone(),
        }));
        let opened = open_connection(
            self.resolve(id),
            &engines,
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: self.base.join("user/known_hosts"),
                },
                interaction: Arc::new(AcceptHosts),
                password: Some(Secret::new("not-to-be-sent")),
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        )
        .await;
        let Err(error) = opened else {
            panic!("the recorder never connects");
        };
        let seen = seen.lock().unwrap().clone();
        (error, seen)
    }
}

/// The Docker bastions' keys are new to every test's empty known_hosts.
struct AcceptHosts;

#[async_trait]
impl spelaion_ssh::Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        true
    }
    async fn ask_secret(&self, _request: &spelaion_ssh::SecretRequest) -> Option<Secret> {
        None
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

const CLEAR: &str = "schema_version = 1\nengine = \"mysql\"\nhost = \"db.internal\"\nuser = \"app\"\n\
                     password = { secret = \"prompt\" }\ncleartext_password = true\n";

fn tls(mode: &str) -> String {
    format!("{CLEAR}[tls]\nmode = \"{mode}\"\n")
}

fn asks_cleartext(params: &ConnectParams) -> bool {
    params
        .options
        .get(engine_options::CLEARTEXT_PASSWORD)
        .map(String::as_str)
        == Some("on")
}

#[tokio::test]
async fn without_a_checked_certificate_the_driver_is_never_called() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("tls");
    // No [tls] table at all is `prefer`: encrypted if the server wants, checked never.
    ws.write(".spelaion/connections/default.toml", CLEAR);
    for mode in ["disable", "prefer", "require"] {
        ws.write(&format!(".spelaion/connections/{mode}.toml"), &tls(mode));
    }
    for id in ["default", "disable", "prefer", "require"] {
        let (error, seen) = ws.open(id, "mysql").await;
        assert!(matches!(error, OpenError::Cleartext(_)), "{id}: {error}");
        assert!(
            error.to_string().contains("verify-ca or verify-full"),
            "{error}"
        );
        assert!(seen.is_empty(), "{id}: the driver was called");
    }
    // MariaDB, the same.
    ws.write(
        ".spelaion/connections/maria.toml",
        &tls("require").replace("\"mysql\"", "\"mariadb\""),
    );
    let (error, seen) = ws.open("maria", "mariadb").await;
    assert!(matches!(error, OpenError::Cleartext(_)) && seen.is_empty());
}

#[tokio::test]
async fn a_checked_certificate_lets_the_driver_answer_the_plugin() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("verified");
    for mode in ["verify-ca", "verify-full"] {
        ws.write(&format!(".spelaion/connections/{mode}.toml"), &tls(mode));
        let (error, seen) = ws.open(mode, "mysql").await;
        assert!(matches!(error, OpenError::Driver(_)), "{mode}: {error}");
        assert_eq!(seen.len(), 1, "{mode}");
        assert!(asks_cleartext(&seen[0]), "{mode}");
    }
    // Without the setting, the driver is never told to answer it, whatever the route.
    ws.write(
        ".spelaion/connections/plain.toml",
        &tls("require").replace("cleartext_password = true\n", ""),
    );
    let (_, seen) = ws.open("plain", "mysql").await;
    assert_eq!(seen.len(), 1);
    assert!(seen[0].options.is_empty(), "{:?}", seen[0].options);
}

#[tokio::test]
async fn a_proxy_ssm_or_kubernetes_route_is_not_a_tunnel_and_is_refused_before_it_starts() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("routes");
    // A tunnel file with no hops: the proxy (or SSM) reaches the database itself. Nothing is
    // listening on these addresses, so a refusal that came after starting them would be a
    // tunnel error, not this one.
    ws.write(
        ".spelaion/tunnels/proxy.toml",
        "schema_version = 1\n[proxy]\ntype = \"socks5\"\nhost = \"127.0.0.1\"\nport = 9\n",
    );
    ws.write(
        ".spelaion/tunnels/ssm.toml",
        "schema_version = 1\n[ssm]\ntarget = \"i-0123456789abcdef0\"\naws = \"/nonexistent/aws\"\n",
    );
    ws.write(
        ".spelaion/connections/proxy.toml",
        &format!("{CLEAR}tunnel = \"proxy\"\n[tls]\nmode = \"require\"\n"),
    );
    ws.write(
        ".spelaion/connections/ssm.toml",
        &format!("{CLEAR}tunnel = \"ssm\"\n[tls]\nmode = \"require\"\n"),
    );
    ws.write(
        ".spelaion/connections/kube.toml",
        &format!(
            "{}[kubernetes]\nresource = \"svc/mysql\"\nkubectl = \"/nonexistent/kubectl\"\n",
            CLEAR.replace("host = \"db.internal\"\n", "")
        ),
    );
    for id in ["proxy", "ssm", "kube"] {
        let (error, seen) = ws.open(id, "mysql").await;
        assert!(matches!(error, OpenError::Cleartext(_)), "{id}: {error}");
        assert!(seen.is_empty(), "{id}");
    }
}

#[test]
fn the_option_is_found_in_what_the_driver_receives_whatever_its_case_or_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("forged");
    ws.write(".spelaion/connections/clear.toml", &tls("require"));
    ws.write(
        ".spelaion/connections/pg.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\n[tls]\nmode = \"verify-full\"\n",
    );
    let clear = ws.resolve("clear");
    let mut params = clear.connect_params(None);
    assert!(cleartext::refusal(&clear, &params).is_some());
    // Without the option there is nothing to refuse.
    params.options.clear();
    assert_eq!(cleartext::refusal(&clear, &params), None);
    // Spelled another way, or with another value: still the option.
    for key in [
        "CLEARTEXT_PASSWORD",
        " cleartext_password ",
        "Cleartext_Password",
    ] {
        let mut params = clear.connect_params(None);
        params.options.clear();
        params.options.insert(key.into(), "off".into());
        assert!(cleartext::refusal(&clear, &params).is_some(), "{key:?}");
    }
    // Another engine never answers it, even over a checked certificate.
    let pg = ws.resolve("pg");
    let mut params = pg.connect_params(None);
    params
        .options
        .insert(engine_options::CLEARTEXT_PASSWORD.into(), "on".into());
    let refusal = cleartext::refusal(&pg, &params).expect("refused");
    assert!(refusal.contains("postgres"), "{refusal}");
}

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

fn toml_path(path: &Path) -> String {
    format!("{:?}", path.to_string_lossy().replace('\\', "/"))
}

/// Through the two Docker bastions the driver is reached, told to answer the plugin, at the
/// tunnel's local end.
#[tokio::test]
async fn an_ssh_tunnel_lets_the_driver_answer_the_plugin() {
    let _turn = crate::serial::file_turn(module_path!());
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/tunnel.toml",
        &format!(
            "{}tunnel = \"bastions\"\n[tls]\nmode = \"disable\"\n",
            CLEAR.replace("db.internal", "mysql")
        ),
    );
    let (error, seen) = ws.open("tunnel", "mysql").await;
    assert!(matches!(error, OpenError::Driver(_)), "{error}");
    assert_eq!(seen.len(), 1);
    assert!(asks_cleartext(&seen[0]));
    assert!(seen[0].connect_addr.is_some(), "the tunnel's local end");
}
