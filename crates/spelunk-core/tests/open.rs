//! End to end: TOML file → resolution → tunnel → driver → query.
//!
//! SQLite always runs. PostgreSQL needs `SPELUNK_TEST_PG` and the tunnels need
//! `SPELUNK_TEST_SSH` (see `testing/docker`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelunk_config::{
    ConnectionId, HopDraft, PasswordChoice, Resolver, Source, SshAuth, SystemBackend, TrustStore,
    TunnelDraft, load,
};
use spelunk_core::{
    CoreEvent, Engines, OpenConnection, OpenError, OpenOptions, ResultStore, TunnelPool,
    open_connection, test_tunnel,
};
use spelunk_driver_api::{Secret, Value};
use spelunk_ssh::{HostInfo, Interaction, KnownHosts, NonInteractive, SecretRequest, TunnelState};

/// Temporary, trusted repository with `.spelunk/` and a user directory.
struct Workspace {
    base: PathBuf,
    tunnels: Arc<TunnelPool>,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("spelunk-open-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelunk")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        TrustStore::load(&base.join("user"))
            .unwrap()
            .trust(&base.join("repo"))
            .unwrap();
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
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let trust = TrustStore::load(&self.base.join("user")).unwrap();
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
                user_file: None,
                app_file: self.base.join("user/known_hosts"),
            },
            interaction: ui,
            password,
        };
        open_connection(
            resolved,
            &Engines::builtin(),
            options,
            Arc::new(ResultStore::new()),
        )
        .await
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
        ".spelunk/connections/local.toml",
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

#[tokio::test]
async fn postgres_direct_with_a_command_secret() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_PG") else {
        eprintln!("SPELUNK_TEST_PG not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("pg-direct");
    // The secret comes from a real shell command (cmd /C on Windows, sh -c on Unix).
    ws.write(
        ".spelunk/connections/pg.toml",
        &format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = {{ secret = \"cmd:echo spelunk\" }}\n\n[tls]\nmode = \"require\"\n"
        ),
    );
    let conn = ws.open("pg", None, Arc::new(NonInteractive)).await.unwrap();
    assert_eq!(first_value(&conn, "select current_user").await, "spelunk");
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
        ".spelunk/connections/pg.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"127.0.0.1\"\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = { secret = \"prompt\" }\n",
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
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelunk/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelunk/connections/prod/orders.toml",
        &format!(
            "schema_version = 1\nengine = \"postgres\"\nenv = \"prod\"\nhost = \"postgres\"\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {}\n",
            toml_path(&keys().join("ca.crt"))
        ),
    );

    let conn = ws
        .open(
            "prod/orders",
            Some(Secret::new("spelunk")),
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
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("shared");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelunk/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    for (id, database) in [("app", "spelunk"), ("admin", "postgres")] {
        ws.write(
            &format!(".spelunk/connections/{id}.toml"),
            &format!(
                "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"{database}\"\nuser = \"spelunk\"\npassword = {{ secret = \"cmd:echo spelunk\" }}\ntunnel = \"bastions\"\n"
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
        "spelunk"
    );
    assert_eq!(
        first_value(&tab2, "select current_database()").await,
        "spelunk"
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

/// A tunnel typed in the editor (not saved) is tested by opening and closing its hops. A
/// wrong user fails at the hop that rejects it, and nothing is written to disk.
#[tokio::test]
async fn unsaved_tunnels_can_be_tested_hop_by_hop() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("tunnel-draft");
    let config = load(Some(&ws.base.join("repo")), &ws.base.join("user"));
    let trust = TrustStore::load(&ws.base.join("user")).unwrap();
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
            hop(host, Some(port.parse().unwrap()), "spelunk"),
            hop("bastion2", None, "spelunk"),
        ],
        kubernetes: None,
    };
    let known_hosts = KnownHosts {
        user_file: None,
        app_file: ws.base.join("user/known_hosts"),
    };
    let resolved = Resolver::new(&config, &trust, &SystemBackend)
        .resolve_tunnel_draft(&draft)
        .unwrap();
    test_tunnel(&resolved, vec![], &known_hosts, Arc::new(AcceptHosts))
        .await
        .unwrap();

    draft.hops[1].user = "nobody".into();
    let resolved = Resolver::new(&config, &trust, &SystemBackend)
        .resolve_tunnel_draft(&draft)
        .unwrap();
    match test_tunnel(&resolved, vec![], &known_hosts, Arc::new(AcceptHosts)).await {
        Err(OpenError::Tunnel(spelunk_ssh::SshError::Auth { hop: 1, .. })) => {}
        other => panic!("expected an authentication error at hop 2: {other:?}"),
    }
    assert!(
        !ws.base.join("repo/.spelunk/tunnels").exists(),
        "testing writes nothing"
    );
}

/// MySQL behind the two bastions with verify-full: the driver connects to the tunnel's local
/// end but checks the certificate against the server's real name ("mysql").
#[tokio::test]
async fn mysql_through_two_bastions_with_verify_full() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    if std::env::var("SPELUNK_TEST_MYSQL").is_err() {
        eprintln!("SPELUNK_TEST_MYSQL not set: test skipped");
        return;
    }
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("mysql-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelunk/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelunk/connections/shop.toml",
        &format!(
            "schema_version = 1\nengine = \"mysql\"\nhost = \"mysql\"\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {}\n",
            toml_path(&keys().join("ca.crt"))
        ),
    );
    let conn = ws
        .open("shop", Some(Secret::new("spelunk")), Arc::new(AcceptHosts))
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
