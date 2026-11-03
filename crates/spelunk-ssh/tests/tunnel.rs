//! Tunnels against the bastions in `testing/docker` (ADR-012).
//!
//! They need `SPELUNK_TEST_SSH=127.0.0.1:2221` and the keys from `testing/docker/gen-keys.sh`:
//! `docker compose -f testing/docker/compose.yaml up -d --build --wait`.
//! Chain: host → bastion1 (127.0.0.1:2221) → bastion2:22 → postgres:5432.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use spelunk_driver_api::{
    ConnectParams, Connection, Driver, DriverError, ExecOptions, Secret, TlsMode,
};
use spelunk_driver_postgres::PostgresDriver;
use spelunk_ssh::{
    AuthMethod, HopSpec, HostInfo, Interaction, KnownHosts, NonInteractive, SecretRequest,
    SshError, Target, Tunnel, TunnelSpec, TunnelState,
};

fn bastion() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn key(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testing/docker/keys")
        .join(name)
}

/// Our own known_hosts in a temporary directory, without reading the user's.
struct TempKnownHosts(PathBuf);

impl TempKnownHosts {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("spelunk-ssh-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn known(&self) -> KnownHosts {
        KnownHosts {
            team_file: None,
            user_file: None,
            app_file: self.0.join("known_hosts"),
        }
    }

    /// With the team's file next to ours (ADR-022).
    fn with_team(&self, content: &str) -> KnownHosts {
        let team = self.0.join("team_known_hosts");
        std::fs::write(&team, content).unwrap();
        KnownHosts {
            team_file: Some(team),
            ..self.known()
        }
    }
}

impl Drop for TempKnownHosts {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Test interface: accepts (or not) new hosts and answers questions from a queue.
#[derive(Default)]
struct TestUi {
    accept_hosts: bool,
    hosts_asked: AtomicUsize,
    answers: Mutex<VecDeque<Option<Secret>>>,
    questions: Mutex<Vec<SecretRequest>>,
}

impl TestUi {
    fn accepting() -> Arc<Self> {
        Arc::new(Self {
            accept_hosts: true,
            ..Default::default()
        })
    }

    fn answering(answers: Vec<Option<&str>>) -> Arc<Self> {
        Arc::new(Self {
            accept_hosts: true,
            answers: Mutex::new(answers.into_iter().map(|a| a.map(Secret::new)).collect()),
            ..Default::default()
        })
    }
}

#[async_trait]
impl Interaction for TestUi {
    async fn confirm_unknown_host(&self, host: &HostInfo) -> bool {
        assert!(host.fingerprint.starts_with("SHA256:"), "{host:?}");
        self.hosts_asked.fetch_add(1, Ordering::SeqCst);
        self.accept_hosts
    }

    async fn ask_secret(&self, request: &SecretRequest) -> Option<Secret> {
        self.questions.lock().unwrap().push(request.clone());
        self.answers.lock().unwrap().pop_front().flatten()
    }
}

fn hop(host: &str, port: u16, identity: &str) -> HopSpec {
    HopSpec {
        host: host.into(),
        port,
        user: "spelunk".into(),
        auth: vec![AuthMethod::Key],
        identity_file: Some(key(identity)),
        password: None,
        command: None,
    }
}

fn two_hops(host: &str, port: u16) -> TunnelSpec {
    TunnelSpec {
        hops: vec![
            hop(host, port, "id_ed25519"),
            hop("bastion2", 22, "id_ed25519"),
        ],
    }
}

fn postgres() -> Target {
    Target {
        host: "postgres".into(),
        port: 5432,
    }
}

async fn pg_through(tunnel: &Tunnel) -> Box<dyn Connection> {
    let params = ConnectParams {
        host: "127.0.0.1".into(),
        port: tunnel.local_addr().port(),
        user: "spelunk".into(),
        password: Some(Secret::new("spelunk")),
        database: "spelunk".into(),
        tls: TlsMode::Disable,
        ..Default::default()
    };
    PostgresDriver
        .connect(&params)
        .await
        .expect("PostgreSQL through the tunnel")
}

async fn scalar(conn: &dyn Connection, sql: &str) -> Result<String, DriverError> {
    use spelunk_driver_api::{ExecEvent, Value};
    let events: Vec<ExecEvent> = conn
        .execute(sql, &ExecOptions::default())
        .await?
        .try_collect()
        .await?;
    for event in events {
        if let ExecEvent::Batch(batch) = event {
            let logical = spelunk_driver_api::ColumnMeta::from_field(batch.schema().field(0))
                .unwrap()
                .logical;
            return Ok(Value::from_array(batch.column(0).as_ref(), 0, logical)
                .unwrap()
                .to_string());
        }
    }
    Err(DriverError::Other("no rows".into()))
}

#[tokio::test]
async fn two_hops_reach_postgres_and_remember_hosts() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("two-hops");
    let ui = TestUi::accepting();

    let tunnel = Tunnel::open(
        &two_hops(&host, port),
        postgres(),
        &known.known(),
        ui.clone(),
    )
    .await
    .unwrap();
    assert_eq!(*tunnel.state().borrow(), TunnelState::Up);
    assert_eq!(
        ui.hosts_asked.load(Ordering::SeqCst),
        2,
        "should ask about both bastions"
    );

    // Several connections at once through the same tunnel.
    let a = pg_through(&tunnel).await;
    let b = pg_through(&tunnel).await;
    assert_eq!(
        scalar(&*a, "select count(*) from big").await.unwrap(),
        "1000000"
    );
    assert_eq!(scalar(&*b, "select 2").await.unwrap(), "2");

    // Cancellation opens another connection, which also goes through the tunnel.
    let a: Arc<dyn Connection> = Arc::from(a);
    let cancel = a.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    assert!(matches!(
        scalar(&*a, "select pg_sleep(30)::text").await,
        Err(DriverError::Cancelled)
    ));
    canceller.await.unwrap().unwrap();
    tunnel.close().await;

    // The second time the fingerprints are already stored: nothing is asked.
    let again = Tunnel::open(
        &two_hops(&host, port),
        postgres(),
        &known.known(),
        ui.clone(),
    )
    .await
    .unwrap();
    assert_eq!(ui.hosts_asked.load(Ordering::SeqCst), 2);
    again.close().await;
}

#[tokio::test]
async fn unknown_hosts_are_rejected_without_interaction() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("unknown");
    match Tunnel::open(
        &two_hops(&host, port),
        postgres(),
        &known.known(),
        Arc::new(NonInteractive),
    )
    .await
    {
        Err(SshError::HostKeyRejected { hop: 0, .. }) => {}
        other => panic!("expected HostKeyRejected at hop 1: {:?}", other.err()),
    }
}

#[tokio::test]
async fn a_changed_host_key_blocks_the_tunnel() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("changed");
    // Another ed25519 key registered for the bastion: simulates an impersonated server.
    let other_key = std::fs::read_to_string(key("id_ed25519_pass.pub")).unwrap();
    let mut parts = other_key.split_whitespace();
    let line = format!(
        "[{host}]:{port} {} {}\n",
        parts.next().unwrap(),
        parts.next().unwrap()
    );
    std::fs::write(&known.known().app_file, line).unwrap();

    let ui = TestUi::accepting();
    match Tunnel::open(
        &two_hops(&host, port),
        postgres(),
        &known.known(),
        ui.clone(),
    )
    .await
    {
        Err(SshError::HostKeyChanged {
            hop: 0, line: 1, ..
        }) => {}
        other => panic!("expected HostKeyChanged: {:?}", other.err()),
    }
    assert_eq!(
        ui.hosts_asked.load(Ordering::SeqCst),
        0,
        "a changed key is never asked about: it is blocked"
    );
}

#[tokio::test]
async fn encrypted_keys_ask_for_the_passphrase() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("passphrase");
    let spec = TunnelSpec {
        hops: vec![hop(&host, port, "id_ed25519_pass")],
    };

    let ui = TestUi::answering(vec![Some("mal"), Some("spelunk-test")]);
    let tunnel = Tunnel::open(
        &spec,
        Target {
            host: "bastion2".into(),
            port: 22,
        },
        &known.known(),
        ui.clone(),
    )
    .await
    .unwrap();
    let questions = ui.questions.lock().unwrap().clone();
    assert!(
        matches!(
            questions.as_slice(),
            [
                SecretRequest::KeyPassphrase { retry: false, .. },
                SecretRequest::KeyPassphrase { retry: true, .. }
            ]
        ),
        "{questions:?}"
    );
    tunnel.close().await;

    // Cancelling the question cancels the opening.
    let ui = TestUi::answering(vec![None]);
    let result = Tunnel::open(
        &spec,
        Target {
            host: "bastion2".into(),
            port: 22,
        },
        &known.known(),
        ui,
    )
    .await;
    assert!(
        matches!(result, Err(SshError::Cancelled)),
        "{:?}",
        result.err()
    );
}

#[tokio::test]
async fn auth_failures_say_which_hop_and_what_was_tried() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("auth");
    let mut spec = two_hops(&host, port);
    spec.hops[1].user = "nobody".into();
    spec.hops[1].auth = vec![AuthMethod::Key, AuthMethod::Password];
    spec.hops[1].password = Some(Secret::new("x"));
    match Tunnel::open(&spec, postgres(), &known.known(), TestUi::accepting()).await {
        Err(SshError::Auth {
            hop: 1,
            user,
            tried,
            ..
        }) => {
            assert_eq!(user, "nobody");
            assert_eq!(tried, ["key", "password"]);
        }
        other => panic!(
            "expected an authentication error at hop 2: {:?}",
            other.err()
        ),
    }
}

#[tokio::test]
async fn unreachable_hops_are_reported() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("unreachable");
    let mut spec = two_hops(&host, port);
    spec.hops[1].host = "does-not-exist".into();
    match Tunnel::open(&spec, postgres(), &known.known(), TestUi::accepting()).await {
        Err(SshError::Connect { hop: 1, host, .. }) => assert_eq!(host, "does-not-exist"),
        other => panic!("expected a connection error at hop 2: {:?}", other.err()),
    }
}

#[tokio::test]
async fn closing_marks_down_and_frees_the_port() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("close");
    let tunnel = Tunnel::open(
        &two_hops(&host, port),
        postgres(),
        &known.known(),
        TestUi::accepting(),
    )
    .await
    .unwrap();
    let addr = tunnel.local_addr();
    let mut state = tunnel.state();
    tunnel.close().await;
    assert!(matches!(
        &*state.borrow_and_update(),
        TunnelState::Down { .. }
    ));
    // The local port stops accepting connections.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn first_hop_can_be_an_external_command() {
    let Some((host, port)) = bastion() else {
        return;
    };
    if std::process::Command::new("ssh")
        .arg("-V")
        .output()
        .is_err()
    {
        eprintln!("no ssh client in PATH: test skipped");
        return;
    }
    let known = TempKnownHosts::new("command");
    // Temporary known_hosts for the system ssh. Do not use NUL: Windows OpenSSH ends up
    // creating a file with that reserved name in the current directory.
    let ssh_known = known.0.join("ssh_known_hosts");
    // The system ssh acts as ProxyCommand up to bastion2; russh speaks SSH with bastion2.
    let command = format!(
        "ssh -i \"{}\" -o StrictHostKeyChecking=no -o UserKnownHostsFile=\"{}\" -o IdentitiesOnly=yes -o BatchMode=yes -p {port} -W %h:%p spelunk@{host}",
        key("id_ed25519").display(),
        ssh_known.display()
    );
    let mut first = hop("bastion2", 22, "id_ed25519");
    first.command = Some(command);
    let spec = TunnelSpec { hops: vec![first] };
    let tunnel = Tunnel::open(&spec, postgres(), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    let conn = pg_through(&tunnel).await;
    assert_eq!(scalar(&*conn, "select 1").await.unwrap(), "1");
    tunnel.close().await;
}

#[tokio::test]
async fn only_the_first_hop_may_use_a_command() {
    let known = TempKnownHosts::new("command-rule");
    let mut spec = two_hops("127.0.0.1", 1);
    spec.hops[1].command = Some("ssh -W %h:%p x".into());
    let result = Tunnel::open(&spec, postgres(), &known.known(), Arc::new(NonInteractive)).await;
    assert!(matches!(result, Err(SshError::Other(m)) if m.contains("first hop")));
}

/// A real SSH agent with the test key loaded (`ssh-add testing/docker/keys/id_ed25519`).
/// Only with `SPELUNK_TEST_SSH_AGENT=1`. It also checks that `identity_file` filters the
/// agent's keys: if others were loaded, they are not tried.
#[tokio::test]
async fn agent_auth_uses_only_the_configured_identity() {
    let Some((host, port)) = bastion() else {
        return;
    };
    if std::env::var("SPELUNK_TEST_SSH_AGENT").is_err() {
        eprintln!("SPELUNK_TEST_SSH_AGENT not set: test skipped");
        return;
    }
    let known = TempKnownHosts::new("agent");
    let mut spec = two_hops(&host, port);
    for hop in &mut spec.hops {
        hop.auth = vec![AuthMethod::Agent];
    }
    let tunnel = Tunnel::open(&spec, postgres(), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    let conn = pg_through(&tunnel).await;
    assert_eq!(scalar(&*conn, "select 1").await.unwrap(), "1");
    tunnel.close().await;
}

/// Several targets over the same chain of sessions, and the same target reuses its port.
#[tokio::test]
async fn one_chain_serves_several_targets() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("targets");
    let tunnel = Tunnel::connect(&two_hops(&host, port), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    let pg = tunnel.forward(postgres()).await.unwrap();
    let ssh = tunnel
        .forward(Target {
            host: "localhost".into(),
            port: 22,
        })
        .await
        .unwrap();
    assert_ne!(pg, ssh);
    assert_eq!(
        tunnel.forward(postgres()).await.unwrap(),
        pg,
        "same target, same port"
    );
    assert_eq!(tunnel.local_addr(), pg, "local_addr is the first forward");

    // The second target is bastion2's sshd: it greets with "SSH-2.0".
    use tokio::io::AsyncReadExt;
    let mut stream = tokio::net::TcpStream::connect(ssh).await.unwrap();
    let mut banner = [0u8; 7];
    stream.read_exact(&mut banner).await.unwrap();
    assert_eq!(&banner, b"SSH-2.0");

    let conn = pg_through(&tunnel).await;
    assert_eq!(scalar(&*conn, "select 3").await.unwrap(), "3");
    tunnel.close().await;
    assert!(
        tunnel.forward(postgres()).await.is_err(),
        "a closed tunnel does not forward"
    );
}

/// A UI that must not be asked about any host.
struct NeverAsked;

#[async_trait]
impl Interaction for NeverAsked {
    async fn confirm_unknown_host(&self, host: &HostInfo) -> bool {
        panic!("the team's known_hosts should have verified {host:?}");
    }

    async fn ask_secret(&self, _: &SecretRequest) -> Option<Secret> {
        None
    }
}

#[tokio::test]
async fn the_teams_known_hosts_verifies_the_first_connection_and_blocks_a_different_key() {
    let Some((host, port)) = bastion() else {
        return;
    };
    // Someone on the team learnt both bastions' keys; that file is what the repository holds.
    let learnt = TempKnownHosts::new("team-learn");
    let tunnel = Tunnel::connect(&two_hops(&host, port), &learnt.known(), TestUi::accepting())
        .await
        .unwrap();
    tunnel.close().await;
    let team = std::fs::read_to_string(learnt.0.join("known_hosts")).unwrap();
    assert_eq!(
        team.lines().filter(|l| !l.trim().is_empty()).count(),
        2,
        "{team}"
    );

    // A newcomer with nothing of their own: verified by the team's file, nothing asked.
    let newcomer = TempKnownHosts::new("team-newcomer");
    let tunnel = Tunnel::connect(
        &two_hops(&host, port),
        &newcomer.with_team(&team),
        Arc::new(NeverAsked),
    )
    .await
    .unwrap();
    tunnel.close().await;
    assert!(
        !newcomer.0.join("known_hosts").exists(),
        "nothing was learnt"
    );

    // The team's file says another key for the first bastion: blocked, even though the
    // user's own file has the right one, and the error names the team's file.
    let first = team
        .lines()
        .find(|l| l.contains(&format!(":{port}")))
        .unwrap();
    let mut fields: Vec<&str> = first.split_whitespace().collect();
    assert_eq!(fields[1], "ssh-ed25519", "{first}");
    let other = std::fs::read_to_string(key("id_ed25519.pub")).unwrap();
    let other_key = other.split_whitespace().nth(1).unwrap().to_string();
    fields[2] = &other_key;
    let tampered = team.replace(first, &fields.join(" "));
    let known = KnownHosts {
        user_file: Some(learnt.0.join("known_hosts")),
        ..newcomer.with_team(&tampered)
    };
    match Tunnel::connect(&two_hops(&host, port), &known, Arc::new(NeverAsked)).await {
        Err(SshError::HostKeyChanged { hop, file, .. }) => {
            assert_eq!(hop, 0);
            assert_eq!(file, newcomer.0.join("team_known_hosts"));
        }
        other => panic!("expected a blocking key change: {:?}", other.map(|_| ())),
    }
}
