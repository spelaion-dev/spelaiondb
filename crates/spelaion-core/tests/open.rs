//! End to end: TOML file → resolution → tunnel → driver → query.
//!
//! SQLite always runs. PostgreSQL needs `SPELAION_TEST_PG` and the tunnels need
//! `SPELAION_TEST_SSH` (see `testing/docker`).

mod engines;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Engine, HopDraft, PasswordChoice, Resolver, Safeguards, Source, SshAuth,
    SystemBackend, TrustStore, TunnelDraft, load,
};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenError, OpenOptions, ResultStore, RunRefused,
    TunnelPool, check_run, open_connection, test_tunnel,
};
use spelaion_driver_api::{ConnectParams, Connection, Driver, ExecOptions, Secret, Value};
use spelaion_ssh::{HostInfo, Interaction, KnownHosts, NonInteractive, SecretRequest, TunnelState};

/// Temporary, trusted repository with `.spelaion/` and a user directory.
struct Workspace {
    base: PathBuf,
    tunnels: Arc<TunnelPool>,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-open-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self {
            base,
            tunnels: Arc::new(TunnelPool::new()),
        }
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.base.join("repo").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    async fn open(
        &self,
        id: &str,
        password: Option<Secret>,
        ui: Arc<dyn Interaction>,
    ) -> Result<OpenConnection, OpenError> {
        self.open_with(id, password, ui, &Engines::builtin()).await
    }

    async fn open_with(
        &self,
        id: &str,
        password: Option<Secret>,
        ui: Arc<dyn Interaction>,
        engines: &Engines,
    ) -> Result<OpenConnection, OpenError> {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        // The repository is trusted as the test wrote it (ADR-022 pins its content).
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let id = ConnectionId {
            source: Source::Workspace,
            path: id.into(),
        };
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&id)
            .expect("resolving");
        let options = OpenOptions {
            tunnels: self.tunnels.clone(),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: self.base.join("user/known_hosts"),
            },
            interaction: ui,
            password,
            read_only_session: false,
        };
        open_connection(resolved, engines, options, Arc::new(ResultStore::new())).await
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct AcceptHosts;

#[async_trait]
impl Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &HostInfo) -> bool {
        true
    }

    async fn ask_secret(&self, _request: &SecretRequest) -> Option<Secret> {
        None
    }
}

/// First column of the first row of the script's last query.
async fn first_value(conn: &OpenConnection, script: &str) -> String {
    let events: Vec<CoreEvent> = conn.session.run_script(script).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{script}: {error:?}");
    }
    let result = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("no result");
    let stored = conn.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    let logical = stored.schema().columns[0].logical;
    Value::from_array(batch.column(0).as_ref(), 0, logical)
        .unwrap()
        .to_string()
}

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

/// Path as a TOML literal (single quotes: no escapes, works for Windows paths).
fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

#[tokio::test]
async fn sqlite_from_config() {
    let ws = Workspace::new("sqlite");
    ws.write("data/app.db", "");
    ws.write(
        ".spelaion/connections/local.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"data/app.db\"\n",
    );
    let conn = ws
        .open("local", None, Arc::new(NonInteractive))
        .await
        .unwrap();
    assert!(conn.tunnel_state().is_none());
    assert_eq!(
        first_value(
            &conn,
            "create table t (a int); insert into t values (41), (1); select sum(a) from t"
        )
        .await,
        "42"
    );
}

/// SQLite's own driver behind another value: what a driver that is not compiled in looks like
/// to the core.
struct Stand(Arc<dyn Driver>);

#[async_trait]
impl Driver for Stand {
    fn engine(&self) -> &str {
        self.0.engine()
    }

    async fn connect(
        &self,
        params: &ConnectParams,
    ) -> spelaion_driver_api::Result<Box<dyn Connection>> {
        self.0.connect(params).await
    }
}

/// ADR-041: a driver is the wire, not the rules. One put in place of an engine's brings no
/// dialect: the session splits, classifies and guards with the core's dialect for that engine.
#[tokio::test]
async fn a_driver_put_in_place_brings_only_the_wire() {
    let ws = Workspace::new("stand-in");
    ws.write("data/app.db", "");
    ws.write(
        ".spelaion/connections/local.toml",
        "schema_version = 1
engine = \"sqlite\"
database = \"data/app.db\"
",
    );
    let sqlite = Engines::builtin()
        .get(Engine::Sqlite)
        .expect("SQLite is built in");
    let engines = Engines::builtin().with_driver(Arc::new(Stand(sqlite)));
    let conn = ws
        .open_with("local", None, Arc::new(NonInteractive), &engines)
        .await
        .unwrap();

    let dialect = conn.session.dialect();
    assert_eq!(dialect.name(), "sqlite");
    assert_eq!(dialect.quote_identifier("a\"b"), "\"a\"\"b\"");

    // Split by the core: three statements run.
    let script = "create table t (a int); insert into t values (41), (1); select sum(a) from t";
    let events: Vec<CoreEvent> = conn.session.run_script(script).collect().await;
    let finished = events
        .iter()
        .filter(|e| matches!(e, CoreEvent::StatementFinished { .. }))
        .count();
    assert_eq!(finished, 3, "{events:?}");

    // Guarded by the core: the `DROP` behind the `select` still asks.
    let refused = check_run(
        &conn.session,
        "select 1; drop table t",
        &Safeguards::default(),
        false,
    );
    assert!(
        matches!(refused, Err(RunRefused::NeedsConfirmation(_))),
        "{refused:?}"
    );
    assert_eq!(first_value(&conn, "select sum(a) from t").await, "42");
}

#[tokio::test]
async fn duckdb_from_config() {
    let ws = Workspace::new("duckdb");
    std::fs::create_dir_all(ws.base.join("repo/data")).unwrap();
    engines::create_database(
        spelaion_config::Engine::Duckdb,
        &ws.base.join("repo/data/app.duckdb"),
    )
    .await;
    ws.write(
        ".spelaion/connections/local.toml",
        "schema_version = 1\nengine = \"duckdb\"\ndatabase = \"data/app.duckdb\"\n",
    );
    let conn = ws
        .open("local", None, Arc::new(NonInteractive))
        .await
        .unwrap();
    assert!(conn.tunnel_state().is_none());
    assert_eq!(
        first_value(
            &conn,
            "create table t (a int); insert into t values (41), (1); select sum(a) from t"
        )
        .await,
        "42"
    );
    // A second connection to the same file (another tab) shares it.
    let other = ws
        .open("local", None, Arc::new(NonInteractive))
        .await
        .unwrap();
    assert_eq!(first_value(&other, "select count(*) from t").await, "2");
}

#[tokio::test]
async fn postgres_direct_with_a_command_secret() {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("pg-direct");
    // The secret comes from a real shell command (cmd /C on Windows, sh -c on Unix).
    ws.write(
        ".spelaion/connections/pg.toml",
        &format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\n\n[tls]\nmode = \"require\"\n"
        ),
    );
    let conn = ws.open("pg", None, Arc::new(NonInteractive)).await.unwrap();
    assert_eq!(first_value(&conn, "select current_user").await, "spelaion");
    assert_eq!(
        first_value(
            &conn,
            "select ssl from pg_stat_ssl where pid = pg_backend_pid()"
        )
        .await,
        "true"
    );
}

#[tokio::test]
async fn prompted_passwords_must_be_supplied() {
    let ws = Workspace::new("prompt");
    ws.write(
        ".spelaion/connections/pg.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"127.0.0.1\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = { secret = \"prompt\" }\n",
    );
    match ws.open("pg", None, Arc::new(NonInteractive)).await {
        Err(OpenError::PasswordRequired { save_to: None }) => {}
        Err(other) => panic!("expected PasswordRequired: {other}"),
        Ok(_) => panic!("should not connect without a password"),
    }
}

/// The typical company setup: two bastions and TLS verify-full against the server's real
/// name, even though the driver connects to the tunnel's local end.
#[tokio::test]
async fn postgres_through_two_bastions_with_verify_full() {
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
        ".spelaion/connections/prod/orders.toml",
        &format!(
            "schema_version = 1\nengine = \"postgres\"\nenv = \"prod\"\nhost = \"postgres\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {}\n",
            toml_path(&keys().join("ca.crt"))
        ),
    );

    let conn = ws
        .open(
            "prod/orders",
            Some(Secret::new("spelaion")),
            Arc::new(AcceptHosts),
        )
        .await
        .unwrap();
    assert!(conn.resolved.is_production());
    let mut state = conn.tunnel_state().expect("goes through a tunnel");
    assert_eq!(*state.borrow(), TunnelState::Up);
    assert_eq!(
        first_value(
            &conn,
            "select ssl from pg_stat_ssl where pid = pg_backend_pid()"
        )
        .await,
        "true"
    );
    assert_eq!(
        first_value(&conn, "select count(*) from big").await,
        "1000000"
    );

    conn.close().await;
    assert!(matches!(
        &*state.borrow_and_update(),
        TunnelState::Down { .. }
    ));
}

/// ADR-004: a shared tunnel. Two tabs against the same database and another database behind
/// the same bastion use a single SSH chain, which closes when the last connection closes.
#[tokio::test]
async fn connections_share_one_tunnel() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("shared");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    for (id, database) in [("app", "spelaion"), ("admin", "postgres")] {
        ws.write(
            &format!(".spelaion/connections/{id}.toml"),
            &format!(
                "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"{database}\"\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\ntunnel = \"bastions\"\n"
            ),
        );
    }
    let ui = Arc::new(AcceptHosts);
    let tab1 = ws.open("app", None, ui.clone()).await.unwrap();
    let tab2 = ws.open("app", None, ui.clone()).await.unwrap();
    let admin = ws.open("admin", None, ui.clone()).await.unwrap();
    assert_eq!(
        ws.tunnels.active().await,
        1,
        "all three connections should share the tunnel"
    );
    assert_eq!(
        first_value(&tab1, "select current_database()").await,
        "spelaion"
    );
    assert_eq!(
        first_value(&tab2, "select current_database()").await,
        "spelaion"
    );
    assert_eq!(
        first_value(&admin, "select current_database()").await,
        "postgres"
    );

    let state = tab1.tunnel_state().unwrap();
    tab1.close().await;
    admin.close().await;
    assert_eq!(*state.borrow(), TunnelState::Up, "tab2 still uses it");
    assert_eq!(first_value(&tab2, "select 1").await, "1");
    tab2.close().await;
    assert!(matches!(&*state.borrow(), TunnelState::Down { .. }));
    assert_eq!(ws.tunnels.active().await, 0);
}

/// Connections from configuration through each test proxy (ADR-027): the proxy alone, and
/// the proxy in front of an SSH hop the host cannot reach. The proxy's password comes from a
/// secret reference, as any other.
#[tokio::test]
async fn connections_go_through_a_proxy_from_their_tunnel_file() {
    let proxies: Vec<(&str, String)> = [
        ("socks5", "SPELAION_TEST_SOCKS"),
        ("http", "SPELAION_TEST_HTTP_PROXY"),
    ]
    .into_iter()
    .filter_map(|(kind, var)| match std::env::var(var) {
        Ok(addr) => Some((kind, addr)),
        Err(_) => {
            eprintln!("{var} not set: skipped");
            None
        }
    })
    .collect();
    for (kind, addr) in proxies {
        let (host, port) = addr.rsplit_once(':').unwrap();
        let ws = Workspace::new(&format!("proxy-{kind}"));
        let proxy = format!(
            "[proxy]\ntype = \"{kind}\"\nhost = \"{host}\"\nport = {port}\nuser = \"proxyuser\"\npassword = {{ secret = \"cmd:echo proxy-pass\" }}\n"
        );
        ws.write(
            ".spelaion/tunnels/alone.toml",
            &format!("schema_version = 1\n\n{proxy}"),
        );
        let identity = toml_path(&keys().join("id_ed25519"));
        ws.write(
            ".spelaion/tunnels/hop.toml",
            &format!(
                "schema_version = 1\n\n{proxy}\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
            ),
        );
        for tunnel in ["alone", "hop"] {
            ws.write(
                &format!(".spelaion/connections/{tunnel}.toml"),
                &format!(
                    "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\ntunnel = \"{tunnel}\"\n"
                ),
            );
        }
        let ui = Arc::new(AcceptHosts);
        let alone = ws.open("alone", None, ui.clone()).await.unwrap();
        assert_eq!(first_value(&alone, "select 1").await, "1", "{kind}");
        if std::env::var("SPELAION_TEST_SSH").is_ok() {
            let hop = ws.open("hop", None, ui.clone()).await.unwrap();
            assert_eq!(first_value(&hop, "select 2").await, "2", "{kind}");
            hop.close().await;
        }
        alone.close().await;
    }
}

/// Two consecutive free local ports that the operating system never hands out on its own.
///
/// A port from its ephemeral range can be checked free here and taken before the tunnel binds
/// it: every outgoing connection (this test's own SSH and database connections included) and
/// every listener on port 0 that the tests running alongside make gets one. That is how the test
/// once failed on Linux, where the range is 32768–60999 (SDB-128). So the ports come from below
/// every system's default range (Linux 32768, Windows and macOS 49152), and on Linux also from
/// outside the range this machine is configured with.
fn free_port_pair() -> u16 {
    let ephemeral = ephemeral_ports();
    (20_000u16..32_000)
        .step_by(41)
        .find(|p| {
            !ephemeral.contains(p)
                && !ephemeral.contains(&(*p + 1))
                && std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok()
                && std::net::TcpListener::bind(("127.0.0.1", *p + 1)).is_ok()
        })
        .expect("two free ports outside the system's ephemeral range")
}

/// The ports the system gives to outgoing connections and to listeners on port 0.
fn ephemeral_ports() -> std::ops::RangeInclusive<u16> {
    if cfg!(target_os = "linux")
        && let Ok(range) = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        && let [Ok(low), Ok(high)] = range
            .split_whitespace()
            .map(str::parse::<u16>)
            .collect::<Vec<_>>()[..]
    {
        return low..=high;
    }
    if cfg!(target_os = "linux") {
        32_768..=60_999
    } else {
        49_152..=65_535
    }
}

/// A connection's `local_port` and the personal `[tunnels]` range decide where the tunnel's
/// local end listens; the tunnel's network settings are used as written.
#[tokio::test]
async fn local_ports_follow_the_connection_and_the_personal_range() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("local-ports");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\nkeepalive_interval = 5\nkeepalive_count = 2\nconnect_timeout = 10\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    let fixed = free_port_pair();
    let ranged = fixed + 1;
    let connection = |database: &str, extra: &str| {
        format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"{database}\"\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\ntunnel = \"bastions\"\n{extra}"
        )
    };
    ws.write(
        ".spelaion/connections/fixed.toml",
        &connection("spelaion", &format!("local_port = {fixed}\n")),
    );
    ws.write(
        ".spelaion/connections/free.toml",
        &connection("postgres", ""),
    );
    std::fs::write(
        ws.base.join("user/user.toml"),
        format!("schema_version = 1\n\n[tunnels]\nport_min = {ranged}\nport_max = {ranged}\n"),
    )
    .unwrap();

    let ui = Arc::new(AcceptHosts);
    let pinned = ws.open("fixed", None, ui.clone()).await.unwrap();
    assert_eq!(
        first_value(&pinned, "select current_database()").await,
        "spelaion"
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", fixed)).is_err(),
        "the tunnel should listen on local_port {fixed}"
    );
    // Without local_port: a port from the personal range, on the same tunnel.
    let free = ws.open("free", None, ui.clone()).await.unwrap();
    assert_eq!(
        first_value(&free, "select current_database()").await,
        "postgres"
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", ranged)).is_err(),
        "the tunnel should listen on {ranged}, the only port of the range"
    );
    assert_eq!(ws.tunnels.active().await, 1);
    pinned.close().await;
    free.close().await;
}

/// A tunnel typed in the editor (not saved) is tested by opening and closing its hops. A
/// wrong user fails at the hop that rejects it, and nothing is written to disk.
#[tokio::test]
async fn unsaved_tunnels_can_be_tested_hop_by_hop() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("tunnel-draft");
    let config = load(Some(&ws.base.join("repo")), &ws.base.join("user"));
    let mut trust = TrustStore::load(&ws.base.join("user")).unwrap();
    trust.trust(&config).unwrap();
    let identity = keys().join("id_ed25519").display().to_string();
    let hop = |host: &str, port: Option<u16>, user: &str| HopDraft {
        host: host.into(),
        port,
        user: user.into(),
        auth: vec![SshAuth::Key],
        identity_file: identity.clone(),
        password: PasswordChoice::None,
    };
    let mut draft = TunnelDraft {
        source: Source::Workspace,
        name: "Bastions".into(),
        hops: vec![
            hop(host, Some(port.parse().unwrap()), "spelaion"),
            hop("bastion2", None, "spelaion"),
        ],
        options: Default::default(),
        proxy: None,
        kubernetes: None,
        ssm: None,
    };
    let known_hosts = KnownHosts {
        team_file: None,
        user_file: None,
        app_file: ws.base.join("user/known_hosts"),
    };
    let resolved = Resolver::new(&config, &trust, &SystemBackend)
        .resolve_tunnel_draft(&draft)
        .unwrap();
    test_tunnel(
        &resolved,
        Default::default(),
        &known_hosts,
        Arc::new(AcceptHosts),
    )
    .await
    .unwrap();

    draft.hops[1].user = "nobody".into();
    let resolved = Resolver::new(&config, &trust, &SystemBackend)
        .resolve_tunnel_draft(&draft)
        .unwrap();
    match test_tunnel(
        &resolved,
        Default::default(),
        &known_hosts,
        Arc::new(AcceptHosts),
    )
    .await
    {
        Err(OpenError::Tunnel(spelaion_ssh::SshError::Auth { hop: 1, .. })) => {}
        other => panic!("expected an authentication error at hop 2: {other:?}"),
    }
    assert!(
        !ws.base.join("repo/.spelaion/tunnels").exists(),
        "testing writes nothing"
    );
}

/// MySQL behind the two bastions with verify-full: the driver connects to the tunnel's local
/// end but checks the certificate against the server's real name ("mysql").
#[tokio::test]
async fn mysql_through_two_bastions_with_verify_full() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    if std::env::var("SPELAION_TEST_MYSQL").is_err() {
        eprintln!("SPELAION_TEST_MYSQL not set: test skipped");
        return;
    }
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("mysql-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/shop.toml",
        &format!(
            "schema_version = 1\nengine = \"mysql\"\nhost = \"mysql\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {}\n",
            toml_path(&keys().join("ca.crt"))
        ),
    );
    let conn = ws
        .open("shop", Some(Secret::new("spelaion")), Arc::new(AcceptHosts))
        .await
        .unwrap();
    assert_eq!(
        first_value(
            &conn,
            "select variable_value <> '' from performance_schema.session_status where variable_name = 'Ssl_cipher'"
        )
        .await,
        "1"
    );
    conn.close().await;
}

/// The tunnel to the Docker server that holds a SQLite file (ADR-018): bastion1, then
/// `last_hop` (`sqlitehost` runs commands; `bastion2` only forwards).
fn sqlite_server(ws: &Workspace, addr: &str, last_hop: &str) {
    let (host, port) = addr.rsplit_once(':').unwrap();
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/server.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"{last_hop}\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
}

/// Every event of a script, and the error of its failed statement if any.
async fn script(
    conn: &OpenConnection,
    sql: &str,
) -> (Vec<CoreEvent>, Option<spelaion_core::StatementError>) {
    let events: Vec<CoreEvent> = conn.session.run_script(sql).collect().await;
    let error = events.iter().find_map(|e| match e {
        CoreEvent::StatementFailed { error, .. } => Some(error.clone()),
        _ => None,
    });
    (events, error)
}

fn first_result(events: &[CoreEvent]) -> spelaion_core::ResultId {
    events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result")
}

#[tokio::test]
async fn sqlite_on_a_server_through_two_hops() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let ws = Workspace::new("sqlite-remote");
    sqlite_server(&ws, &addr, "sqlitehost");
    ws.write(
        ".spelaion/connections/shop.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelaion/shop.db\"\ntunnel = \"server\"\n",
    );
    let conn = ws
        .open("shop", None, Arc::new(AcceptHosts))
        .await
        .expect("opening the remote file");
    let version = conn.session.connection().server_version().await.unwrap();
    assert!(version.starts_with("3."), "{version}");

    // Values keep their storage class: integers, text, reals and blobs.
    let (events, error) = script(
        &conn,
        "select id, name, price, photo from product order by id",
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    let stored = conn.session.results().get(first_result(&events)).unwrap();
    {
        use spelaion_driver_api::LogicalType;
        let stored = stored.read().unwrap();
        let logical: Vec<_> = stored.schema().columns.iter().map(|c| c.logical).collect();
        assert_eq!(
            logical,
            [
                LogicalType::Int,
                LogicalType::Text,
                LogicalType::Float,
                LogicalType::Bytes
            ]
        );
        let batch = &stored.window(0, 3)[0];
        let cell = |c: usize, r: usize| {
            Value::from_array(
                batch.column(c).as_ref(),
                r,
                stored.schema().columns[c].logical,
            )
            .unwrap()
            .to_string()
        };
        assert_eq!(
            (cell(1, 0), cell(2, 0), cell(3, 0)),
            ("Lamp".into(), "19.5".into(), "\\x00ff".into())
        );
        assert_eq!(cell(1, 2), "it's a chair");
    }

    // The explorer reads the server's catalog.
    let objects = conn
        .session
        .connection()
        .list_objects("main")
        .await
        .unwrap();
    assert!(objects.iter().any(|o| o.name == "product"), "{objects:?}");
    let product = conn
        .session
        .connection()
        .describe_table("main", "product")
        .await
        .unwrap();
    assert_eq!(product.primary_key, ["id"]);

    // Writes, row counts, a value with a line break and a transaction across statements.
    let (events, error) = script(
        &conn,
        "create table if not exists remote_t (id integer primary key, note text);\n\
         delete from remote_t;\n\
         insert into remote_t values (1, 'a'), (2, 'it''s\nmultiline');\n\
         begin; delete from remote_t; rollback",
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    assert!(events.iter().any(|e| matches!(
        e,
        CoreEvent::StatementFinished {
            index: 2,
            rows_affected: Some(2),
            ..
        }
    )));
    assert_eq!(
        first_value(&conn, "select note from remote_t where id = 2").await,
        "it's\nmultiline"
    );

    // The column catalog of metadata search, through the server's sqlite3 (ADR-043), and no
    // data search there: nothing but SQL text reaches that shell, so no value can be bound.
    let columns = conn
        .session
        .connection()
        .list_columns("main")
        .await
        .unwrap();
    let remote_columns: Vec<&str> = columns
        .iter()
        .filter(|c| c.table == "remote_t")
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(remote_columns, ["id", "note"]);
    assert!(!conn.session.connection().capabilities().bound_params);

    // Editing a page of the table, in one transaction.
    let table = conn
        .session
        .connection()
        .describe_table("main", "remote_t")
        .await
        .unwrap();
    let page = spelaion_core::TablePage {
        schema: "main".into(),
        name: "remote_t".into(),
        filter: String::new(),
        order_by: "id".into(),
        limit: 10,
        offset: 0,
    };
    let sql = spelaion_core::table_page_sql(conn.session.dialect().as_ref(), &page).unwrap();
    let (events, _) = script(&conn, &sql).await;
    let stored = conn.session.results().get(first_result(&events)).unwrap();
    let edit_target = conn.session.edit_target().await.unwrap();
    let statements = spelaion_core::edit_statements(
        &edit_target,
        &table,
        &stored.read().unwrap(),
        &[spelaion_core::RowChange::Update {
            row: 0,
            values: vec![(1, spelaion_core::CellInput::Text("edited".into()))],
        }],
    )
    .unwrap();
    assert_eq!(
        spelaion_core::apply_edits(&conn.session, &statements).await,
        Ok(1)
    );
    assert_eq!(
        first_value(&conn, "select note from remote_t where id = 1").await,
        "edited"
    );

    // Triggers, and their definitions, from the server's catalog (ADR-024).
    let (_, error) = script(
        &conn,
        "create trigger if not exists remote_t_note after update on remote_t begin select 1; end",
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    let triggers = conn
        .session
        .connection()
        .list_schema_objects("main")
        .await
        .unwrap();
    let trigger = triggers
        .iter()
        .find(|o| o.name == "remote_t_note")
        .unwrap_or_else(|| panic!("{triggers:?}"));
    assert_eq!(trigger.detail.as_deref(), Some("remote_t"));
    // The keys that point to a table, from the server's catalog: written without its column,
    // a key points to the primary key.
    let (_, error) = script(
        &conn,
        "create table if not exists child_of_remote (id integer primary key, \
         parent integer references remote_t)",
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    let referencing = conn
        .session
        .connection()
        .list_referencing_keys("main", "remote_t")
        .await
        .unwrap();
    assert_eq!(
        referencing,
        [spelaion_driver_api::ReferencingKey {
            schema: "main".into(),
            table: "child_of_remote".into(),
            name: "fk_0".into(),
            columns: vec!["parent".into()],
            referenced_columns: vec!["id".into()],
        }]
    );
    // The explorer's filter goes into the server's catalog query too (ADR-025).
    let only_remote = spelaion_driver_api::NameFilter {
        include: vec!["REMOTE_*".into()],
        exclude: vec![],
    };
    let narrowed: Vec<String> = conn
        .session
        .connection()
        .list_objects_matching("main", &only_remote)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.name)
        .collect();
    assert_eq!(narrowed, ["remote_t"]);
    assert_eq!(
        conn.session
            .connection()
            .object_definition(trigger)
            .await
            .unwrap(),
        "CREATE TRIGGER remote_t_note after update on remote_t begin select 1; end;"
    );

    // The shell never runs commands of its own, nor a statement that would swallow the rest.
    let (_, error) = script(&conn, ".shell echo pwned").await;
    assert!(error.unwrap().message.contains("not run on the server"));
    let (_, error) = script(&conn, "select 'open").await;
    assert!(error.unwrap().message.contains("not complete"));

    // Cancelling interrupts the statement and keeps the session.
    let cancel = conn.session.connection().cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    let (_, error) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        script(
            &conn,
            "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c",
        ),
    )
    .await
    .expect("the cancel was lost");
    canceller.await.unwrap().unwrap();
    assert!(error.as_ref().is_some_and(|e| e.cancelled), "{error:?}");
    // sqlite3 ends when interrupted: the session says so, and a new one goes on.
    let (_, error) = script(&conn, "select 1").await;
    let message = error.unwrap().message;
    assert!(message.contains("reconnect"), "{message}");
    let conn = ws.open("shop", None, Arc::new(AcceptHosts)).await.unwrap();
    assert_eq!(
        first_value(&conn, "select count(*) from remote_t").await,
        "2"
    );
    let (_, error) = script(&conn, "drop table remote_t").await;
    assert!(error.is_none(), "{error:?}");

    // Safe mode: what touches the server's files ends sqlite3, and the session says so.
    let (_, error) = script(&conn, "select writefile('/tmp/x', 1)").await;
    let message = error.unwrap().message;
    assert!(message.contains("safe mode"), "{message}");
    let (_, error) = script(&conn, "select 1").await;
    assert!(error.is_some(), "the session ended with sqlite3");
}

/// Scripts written for the `sqlite3` command line put its commands after a comment header:
/// the shell skips a line that is only a comment and would then run the command, changing
/// how it writes every later result. They are refused, and the session reads as before.
#[tokio::test]
async fn sqlite_on_a_server_never_runs_a_command_after_a_comment() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let ws = Workspace::new("sqlite-remote-dot");
    sqlite_server(&ws, &addr, "sqlitehost");
    ws.write(
        ".spelaion/connections/shop.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelaion/shop.db\"\ntunnel = \"server\"\n",
    );
    let conn = ws
        .open("shop", None, Arc::new(AcceptHosts))
        .await
        .expect("opening the remote file");
    for refused in [
        "-- show everything\n.headers off",
        "/* c */\n.mode list",
        "-- a\n-- b\n.changes off",
        "# x\nselect 1",
        "-- a\n-- b\n.quit",
    ] {
        let (_, error) = script(&conn, refused).await;
        let message = error.map(|e| e.message).unwrap_or_default();
        assert!(
            message.contains("not run on the server"),
            "{refused:?}: {message}"
        );
    }
    // A line of a value that starts with a dot is the value's, byte for byte.
    assert_eq!(
        first_value(&conn, "-- a comment first\nselect 'a\n.headers off\n#b'").await,
        "a\n.headers off\n#b"
    );
    let (events, error) = script(&conn, "select id, name from product order by id").await;
    assert!(error.is_none(), "{error:?}");
    let stored = conn.session.results().get(first_result(&events)).unwrap();
    let stored = stored.read().unwrap();
    let names: Vec<&str> = stored
        .schema()
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["id", "name"]);
    let batch = &stored.window(0, 1)[0];
    let cell = |c: usize| {
        Value::from_array(
            batch.column(c).as_ref(),
            0,
            stored.schema().columns[c].logical,
        )
        .unwrap()
        .to_string()
    };
    assert_eq!((cell(0), cell(1)), ("1".to_string(), "Lamp".to_string()));
}

#[tokio::test]
async fn sqlite_on_a_server_says_what_is_wrong() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let ws = Workspace::new("sqlite-remote-errors");
    sqlite_server(&ws, &addr, "sqlitehost");
    ws.write(
        ".spelaion/connections/missing.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelaion/missing.db\"\ntunnel = \"server\"\n",
    );
    ws.write(
        ".spelaion/connections/readonly.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelaion/other.db\"\ntunnel = \"server\"\nread_only = true\n",
    );
    match ws.open("missing", None, Arc::new(AcceptHosts)).await {
        Err(e) => assert!(
            e.to_string().contains("does not exist on the server"),
            "{e}"
        ),
        Ok(_) => panic!("a missing file must not open (nor be created)"),
    }
    let conn = ws
        .open("readonly", None, Arc::new(AcceptHosts))
        .await
        .unwrap();
    // The core refuses the write before it reaches the server (ADR-053)…
    let (_, error) = script(&conn, "insert into product (name) values ('x')").await;
    let message = error.unwrap().message;
    assert!(message.contains("the connection is read-only"), "{message}");
    // …and sent around the core, the server's `sqlite3 -readonly` refuses it too.
    let mut failure = None;
    match conn
        .session
        .connection()
        .execute(
            "insert into product (name) values ('x')",
            &ExecOptions::default(),
        )
        .await
    {
        Err(e) => failure = Some(e.to_string()),
        Ok(mut events) => {
            while let Some(event) = events.next().await {
                if let Err(e) = event {
                    failure = Some(e.to_string());
                    break;
                }
            }
        }
    }
    let message = failure.expect("the server's sqlite3 let a write through");
    assert!(message.to_lowercase().contains("readonly"), "{message}");

    // A last hop that only forwards ports (bastion2 runs no commands).
    let ws = Workspace::new("sqlite-remote-noshell");
    sqlite_server(&ws, &addr, "bastion2");
    ws.write(
        ".spelaion/connections/shop.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelaion/shop.db\"\ntunnel = \"server\"\n",
    );
    match ws.open("shop", None, Arc::new(AcceptHosts)).await {
        Err(e) => {
            let message = e.to_string();
            assert!(
                message.contains("sqlite3 on the server ended") || message.contains("refused"),
                "{message}"
            )
        }
        Ok(_) => panic!("a bastion that runs no commands cannot open a file"),
    }
}

/// A newcomer who must not be asked about any host: the team's file answers for them.
struct RefuseHosts;

#[async_trait]
impl Interaction for RefuseHosts {
    async fn confirm_unknown_host(&self, host: &HostInfo) -> bool {
        panic!("the team's known_hosts should have verified {host:?}");
    }

    async fn ask_secret(&self, _request: &SecretRequest) -> Option<Secret> {
        None
    }
}

/// ADR-022: `.spelaion/known_hosts` in the repository verifies a newcomer's first connection.
#[tokio::test]
async fn the_teams_known_hosts_verifies_a_newcomers_first_connection() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let identity = toml_path(&keys().join("id_ed25519"));
    let setup = |ws: &Workspace| {
        ws.write(
            ".spelaion/tunnels/bastions.toml",
            &format!(
                "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
            ),
        );
        ws.write(
            ".spelaion/connections/orders.toml",
            "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = { secret = \"prompt\" }\ntunnel = \"bastions\"\n",
        );
    };

    // Someone on the team connects once and commits the keys they checked.
    let veteran = Workspace::new("team-hosts-veteran");
    setup(&veteran);
    let conn = veteran
        .open(
            "orders",
            Some(Secret::new("spelaion")),
            Arc::new(AcceptHosts),
        )
        .await
        .unwrap();
    conn.close().await;
    let learnt = std::fs::read_to_string(veteran.base.join("user/known_hosts")).unwrap();

    // A newcomer clones the repository with that file and has nothing of their own.
    let newcomer = Workspace::new("team-hosts-newcomer");
    setup(&newcomer);
    newcomer.write(".spelaion/known_hosts", &learnt);
    let conn = newcomer
        .open(
            "orders",
            Some(Secret::new("spelaion")),
            Arc::new(RefuseHosts),
        )
        .await
        .unwrap();
    assert_eq!(
        conn.resolved.tunnel.as_ref().unwrap().known_hosts,
        Some(newcomer.base.join("repo/.spelaion/known_hosts"))
    );
    assert_eq!(first_value(&conn, "select 1").await, "1");
    conn.close().await;
    assert!(
        !newcomer.base.join("user/known_hosts").exists(),
        "nothing was asked or learnt"
    );
}
