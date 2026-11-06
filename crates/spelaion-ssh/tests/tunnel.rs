//! Tunnels against the bastions in `testing/docker` (ADR-012).
//!
//! They need `SPELAION_TEST_SSH=127.0.0.1:2221` and the keys from `testing/docker/gen-keys.sh`:
//! `docker compose -f testing/docker/compose.yaml up -d --build --wait`.
//! Chain: host → bastion1 (127.0.0.1:2221) → bastion2:22 → postgres:5432.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use spelaion_driver_api::{
    ConnectParams, Connection, Driver, DriverError, ExecOptions, Secret, TlsMode,
};
use spelaion_driver_postgres::PostgresDriver;
use spelaion_ssh::{
    AuthMethod, HopSpec, HostInfo, Interaction, KnownHosts, LocalPort, NonInteractive, ProxyKind,
    ProxySpec, SecretRequest, SshError, Target, Tunnel, TunnelOptions, TunnelSpec, TunnelState,
};

fn bastion() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
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
        let dir = std::env::temp_dir().join(format!("spelaion-ssh-{name}-{}", std::process::id()));
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
        user: "spelaion".into(),
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
        ..Default::default()
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
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: "spelaion".into(),
        tls: TlsMode::Disable,
        ..Default::default()
    };
    PostgresDriver
        .connect(&params)
        .await
        .expect("PostgreSQL through the tunnel")
}

async fn scalar(conn: &dyn Connection, sql: &str) -> Result<String, DriverError> {
    use spelaion_driver_api::{ExecEvent, Value};
    let events: Vec<ExecEvent> = conn
        .execute(sql, &ExecOptions::default())
        .await?
        .try_collect()
        .await?;
    for event in events {
        if let ExecEvent::Batch(batch) = event {
            let logical = spelaion_driver_api::ColumnMeta::from_field(batch.schema().field(0))
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
async fn host_pins_cover_different_algorithms_and_all_approved_keys() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let spec = TunnelSpec {
        hops: vec![hop(&host, port, "id_ed25519")],
        ..Default::default()
    };
    let learned = TempKnownHosts::new("algorithms-learned");
    let tunnel = Tunnel::connect(&spec, &learned.known(), TestUi::accepting())
        .await
        .unwrap();
    tunnel.close().await;
    let correct = std::fs::read_to_string(&learned.known().app_file).unwrap();
    let other = std::fs::read_to_string(key("id_ed25519.pub")).unwrap();
    let other = other
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    let known = TempKnownHosts::new("algorithms-team");
    let ecdsa = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBHwf2HMM5TRXvo2SQJjsNkiDD5KqiiNjrGVv3UUh+mMT5RHxiRtOnlqvjhQtBq0VpmpCV/PwUdhOig4vkbqAcEc=";
    let team = format!("[{host}]:{port} {ecdsa}\n");
    let known_files = KnownHosts {
        user_file: Some(learned.known().app_file),
        ..known.with_team(&team)
    };
    let ui = TestUi::accepting();
    match Tunnel::connect(&spec, &known_files, ui.clone()).await {
        Err(SshError::HostKeyChanged {
            hop: 0, line: 1, ..
        }) => {}
        other => panic!(
            "a key of another algorithm must be blocked: {:?}",
            other.map(|_| ())
        ),
    }
    assert_eq!(ui.hosts_asked.load(Ordering::SeqCst), 0);
    assert!(!known_files.app_file.exists());

    let team = format!("[{host}]:{port} {other}\n{correct}");
    let tunnel = Tunnel::connect(&spec, &known.with_team(&team), Arc::new(NeverAsked))
        .await
        .unwrap();
    tunnel.close().await;
}

#[tokio::test]
async fn encrypted_keys_ask_for_the_passphrase() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("passphrase");
    let spec = TunnelSpec {
        hops: vec![hop(&host, port, "id_ed25519_pass")],
        ..Default::default()
    };

    let ui = TestUi::answering(vec![Some("mal"), Some("spelaion-test")]);
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
        "ssh -i \"{}\" -o StrictHostKeyChecking=no -o UserKnownHostsFile=\"{}\" -o IdentitiesOnly=yes -o BatchMode=yes -p {port} -W %h:%p spelaion@{host}",
        key("id_ed25519").display(),
        ssh_known.display()
    );
    let mut first = hop("bastion2", 22, "id_ed25519");
    first.command = Some(command);
    let spec = TunnelSpec {
        hops: vec![first],
        ..Default::default()
    };
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
/// Only with `SPELAION_TEST_SSH_AGENT=1`. It also checks that `identity_file` filters the
/// agent's keys: if others were loaded, they are not tried.
#[tokio::test]
async fn agent_auth_uses_only_the_configured_identity() {
    let Some((host, port)) = bastion() else {
        return;
    };
    if std::env::var("SPELAION_TEST_SSH_AGENT").is_err() {
        eprintln!("SPELAION_TEST_SSH_AGENT not set: test skipped");
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

/// A TCP relay to the bastion that can be frozen: both sockets stay open but no byte goes
/// through any more, as when a NAT or firewall forgets an idle session without telling either
/// side. Tunnels open through it with its address as their first hop.
struct Relay {
    port: u16,
    frozen: Arc<std::sync::atomic::AtomicBool>,
}

impl Relay {
    async fn to(host: &str, port: u16) -> Relay {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap().port();
        let frozen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let upstream = format!("{host}:{port}");
        let flag = frozen.clone();
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = tokio::net::TcpStream::connect(&upstream).await else {
                    continue;
                };
                let (client_read, client_write) = client.into_split();
                let (server_read, server_write) = server.into_split();
                tokio::spawn(pipe(client_read, server_write, flag.clone()));
                tokio::spawn(pipe(server_read, client_write, flag.clone()));
            }
        });
        Relay {
            port: local,
            frozen,
        }
    }

    fn freeze(&self) {
        self.frozen.store(true, Ordering::SeqCst);
    }
}

/// Copies until either side closes; frozen, it reads and drops, so neither side sees an error.
async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    frozen: Arc<std::sync::atomic::AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    while let Ok(n) = from.read(&mut buf).await {
        if n == 0 {
            break;
        }
        if !frozen.load(Ordering::SeqCst) && to.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
}

async fn down_within(tunnel: &Tunnel, limit: Duration) -> Option<String> {
    let mut state = tunnel.state();
    let down = tokio::time::timeout(
        limit,
        state.wait_for(|s| matches!(s, TunnelState::Down { .. })),
    )
    .await;
    match down {
        Ok(Ok(state)) => match &*state {
            TunnelState::Down { reason } => Some(reason.clone()),
            TunnelState::Up => None,
        },
        Ok(Err(_)) | Err(_) => None,
    }
}

#[tokio::test]
async fn keepalives_notice_a_session_a_firewall_dropped() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("keepalive");
    let relay = Relay::to(&host, port).await;
    let mut spec = two_hops("127.0.0.1", relay.port);
    spec.options = TunnelOptions {
        keepalive_interval: Some(Duration::from_secs(1)),
        keepalive_count: 2,
        ..Default::default()
    };
    let tunnel = Tunnel::open(&spec, postgres(), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    let conn = pg_through(&tunnel).await;
    assert_eq!(scalar(&*conn, "select 1").await.unwrap(), "1");

    relay.freeze();
    let reason = down_within(&tunnel, Duration::from_secs(15))
        .await
        .expect("the keep-alives must take the tunnel down");
    assert!(reason.contains("hop "), "{reason}");
    // Down means down: the driver sees the error at once instead of hanging.
    let failed = tokio::time::timeout(Duration::from_secs(10), scalar(&*conn, "select 1"))
        .await
        .expect("a query on a dead tunnel must fail, not hang");
    assert!(failed.is_err());
}

#[tokio::test]
async fn without_keepalives_a_dropped_session_goes_unnoticed() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("no-keepalive");
    let relay = Relay::to(&host, port).await;
    let mut spec = two_hops("127.0.0.1", relay.port);
    spec.options = TunnelOptions {
        keepalive_interval: None,
        keepalive_count: 2,
        ..Default::default()
    };
    let tunnel = Tunnel::open(&spec, postgres(), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    relay.freeze();
    // What the setting is for: with them off, nothing finds out.
    assert_eq!(down_within(&tunnel, Duration::from_secs(5)).await, None);
    tunnel.close().await;
}

#[tokio::test]
async fn a_server_that_never_answers_times_out_as_configured() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("connect-timeout");
    // Accepts the connection but nothing ever comes back, like a firewall that drops packets.
    let relay = Relay::to(&host, port).await;
    relay.freeze();
    let mut spec = two_hops("127.0.0.1", relay.port);
    spec.options.connect_timeout = Duration::from_secs(1);
    let started = std::time::Instant::now();
    let result = Tunnel::connect(&spec, &known.known(), TestUi::accepting()).await;
    let elapsed = started.elapsed();
    match result {
        Err(SshError::Connect {
            hop: 0, message, ..
        }) => {
            assert!(message.contains("no response in 1 s"), "{message}");
        }
        other => panic!("expected a timeout at hop 1: {:?}", other.err()),
    }
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

/// Three consecutive free local ports, held open until dropped.
fn free_ports() -> (u16, Vec<std::net::TcpListener>) {
    for base in (41_000u16..60_000).step_by(37) {
        let held: Vec<_> = (base..base + 3)
            .map_while(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok())
            .collect();
        if held.len() == 3 {
            return (base, held);
        }
    }
    panic!("no three consecutive free ports");
}

#[tokio::test]
async fn a_fixed_local_port_is_used_shared_and_refused_when_taken() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("fixed-port");
    let tunnel = Tunnel::connect(&two_hops(&host, port), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    let (base, held) = free_ports();
    drop(held);
    let addr = tunnel
        .forward_on(postgres(), LocalPort::Fixed(base))
        .await
        .unwrap();
    assert_eq!(addr.to_string(), format!("127.0.0.1:{base}"));
    let conn = pg_through(&tunnel).await;
    assert_eq!(scalar(&*conn, "select 1").await.unwrap(), "1");
    // Asking again, for that port or any, shares the forward.
    assert_eq!(
        tunnel
            .forward_on(postgres(), LocalPort::Fixed(base))
            .await
            .unwrap(),
        addr
    );
    assert_eq!(tunnel.forward(postgres()).await.unwrap(), addr);
    // Another fixed port for the same target is a second forward.
    let second = tunnel
        .forward_on(postgres(), LocalPort::Fixed(base + 1))
        .await
        .unwrap();
    assert_eq!(second.port(), base + 1);

    // A port another program holds is an error that names it.
    let taken = std::net::TcpListener::bind(("127.0.0.1", base + 2)).unwrap();
    let target = Target {
        host: "bastion2".into(),
        port: 22,
    };
    match tunnel.forward_on(target, LocalPort::Fixed(base + 2)).await {
        Err(SshError::Other(message)) => {
            assert!(
                message.contains(&format!("local port {}", base + 2)),
                "{message}"
            );
        }
        other => panic!("a taken port must be refused: {other:?}"),
    }
    drop(taken);
    drop(conn);
    tunnel.close().await;
    // Closing frees the fixed port, so reconnecting can take it again.
    let mut again = None;
    for _ in 0..50 {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", base)).await {
            again = Some(listener);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(again.is_some(), "port {base} was not freed");
}

#[tokio::test]
async fn concurrent_fixed_port_requests_share_one_forward() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("concurrent-fixed");
    let tunnel = Tunnel::connect(&two_hops(&host, port), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    let (base, held) = free_ports();
    drop(held);
    let (one, two) = tokio::join!(
        tunnel.forward_on(postgres(), LocalPort::Fixed(base)),
        tunnel.forward_on(postgres(), LocalPort::Fixed(base)),
    );
    assert_eq!(one.unwrap(), two.unwrap());
    tunnel.close().await;
}

#[tokio::test]
async fn forwards_take_the_first_free_port_of_a_range() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let known = TempKnownHosts::new("port-range");
    let tunnel = Tunnel::connect(&two_hops(&host, port), &known.known(), TestUi::accepting())
        .await
        .unwrap();
    let (base, mut held) = free_ports();
    // The first port is busy: the next one is taken.
    held.truncate(1);
    let range = LocalPort::Range {
        min: base,
        max: base + 1,
    };
    let addr = tunnel.forward_on(postgres(), range).await.unwrap();
    assert_eq!(addr.port(), base + 1);
    let conn = pg_through(&tunnel).await;
    assert_eq!(scalar(&*conn, "select 1").await.unwrap(), "1");
    // The range is full now: another target has nowhere to go.
    let target = Target {
        host: "bastion2".into(),
        port: 22,
    };
    match tunnel.forward_on(target, range).await {
        Err(SshError::Other(message)) => {
            assert!(message.contains("no free local port"), "{message}")
        }
        other => panic!("a full range must be refused: {other:?}"),
    }
    tunnel.close().await;
    drop(held);
}

fn proxy_from(var: &str, kind: ProxyKind) -> Option<ProxySpec> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some(ProxySpec {
        kind,
        host: host.into(),
        port: port.parse().expect("port"),
        user: Some("proxyuser".into()),
        password: Some(Secret::new("proxy-pass")),
    })
}

fn proxies() -> Vec<ProxySpec> {
    [
        proxy_from("SPELAION_TEST_SOCKS", ProxyKind::Socks5),
        proxy_from("SPELAION_TEST_HTTP_PROXY", ProxyKind::Http),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[tokio::test]
async fn a_proxy_alone_reaches_the_database_by_name() {
    for proxy in proxies() {
        let known = TempKnownHosts::new("proxy-only");
        let spec = TunnelSpec {
            proxy: Some(proxy.clone()),
            ..Default::default()
        };
        // `postgres` only resolves on the proxy's side.
        let tunnel = Tunnel::open(&spec, postgres(), &known.known(), Arc::new(NeverAsked))
            .await
            .unwrap_or_else(|e| panic!("{:?}: {e}", proxy.kind));
        let conn = pg_through(&tunnel).await;
        assert_eq!(
            scalar(&*conn, "select 1").await.unwrap(),
            "1",
            "{:?}",
            proxy.kind
        );
        // Several connections share it, each through its own proxy connection.
        let second = pg_through(&tunnel).await;
        assert_eq!(scalar(&*second, "select 2").await.unwrap(), "2");
        // Nothing to run commands on.
        assert!(tunnel.exec("true").await.is_err());
        tunnel.close().await;
    }
}

#[tokio::test]
async fn the_first_ssh_hop_goes_through_the_proxy() {
    if bastion().is_none() {
        return;
    }
    for proxy in proxies() {
        let known = TempKnownHosts::new("proxy-hop");
        // bastion2 is not reachable from the host: only through the proxy.
        let spec = TunnelSpec {
            hops: vec![hop("bastion2", 22, "id_ed25519")],
            proxy: Some(proxy.clone()),
            ..Default::default()
        };
        let tunnel = Tunnel::open(&spec, postgres(), &known.known(), TestUi::accepting())
            .await
            .unwrap_or_else(|e| panic!("{:?}: {e}", proxy.kind));
        let conn = pg_through(&tunnel).await;
        assert_eq!(
            scalar(&*conn, "select 1").await.unwrap(),
            "1",
            "{:?}",
            proxy.kind
        );
        tunnel.close().await;
    }
}

#[tokio::test]
async fn a_wrong_proxy_password_is_reported_as_such() {
    for mut proxy in proxies() {
        proxy.password = Some(Secret::new("wrong"));
        let known = TempKnownHosts::new("proxy-wrong");
        // SOCKS5 refuses it when the tunnel opens; HTTP on the first CONNECT (the hop's).
        let spec = TunnelSpec {
            hops: vec![hop("bastion2", 22, "id_ed25519")],
            proxy: Some(proxy.clone()),
            ..Default::default()
        };
        match Tunnel::connect(&spec, &known.known(), TestUi::accepting()).await {
            Err(SshError::Proxy { message, .. }) => {
                assert!(
                    message.contains("rejected the username or password"),
                    "{message}"
                );
            }
            other => panic!(
                "{:?}: expected a proxy error: {:?}",
                proxy.kind,
                other.err()
            ),
        }
        let alone = TunnelSpec {
            proxy: Some(proxy.clone()),
            ..Default::default()
        };
        if proxy.kind == ProxyKind::Socks5 {
            assert!(matches!(
                Tunnel::connect(&alone, &known.known(), TestUi::accepting()).await,
                Err(SshError::Proxy { .. })
            ));
        } else {
            let tunnel = Tunnel::connect(&alone, &known.known(), TestUi::accepting())
                .await
                .unwrap();
            match tunnel.forward(postgres()).await {
                Err(SshError::Proxy { message, .. }) => assert!(
                    message.contains("rejected the username or password"),
                    "{message}"
                ),
                other => panic!("expected an HTTP proxy authentication error: {other:?}"),
            }
        }
    }
}

struct SlowProxyPassword;

#[async_trait]
impl Interaction for SlowProxyPassword {
    async fn confirm_unknown_host(&self, _: &HostInfo) -> bool {
        true
    }
    async fn ask_secret(&self, _: &SecretRequest) -> Option<Secret> {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        Some(Secret::new("proxy-pass"))
    }
}

#[tokio::test]
async fn proxy_password_prompt_is_not_subject_to_connect_timeout() {
    let Some(mut proxy) = proxy_from("SPELAION_TEST_SOCKS", ProxyKind::Socks5) else {
        return;
    };
    proxy.password = None;
    let known = TempKnownHosts::new("slow-proxy-password");
    let spec = TunnelSpec {
        hops: vec![hop("bastion2", 22, "id_ed25519")],
        proxy: Some(proxy),
        options: TunnelOptions {
            connect_timeout: Duration::from_secs(1),
            ..Default::default()
        },
    };
    let tunnel = Tunnel::connect(&spec, &known.known(), Arc::new(SlowProxyPassword))
        .await
        .unwrap();
    tunnel.close().await;
}

#[tokio::test]
async fn a_missing_proxy_password_is_asked_once() {
    for mut proxy in proxies() {
        proxy.password = None;
        let known = TempKnownHosts::new("proxy-ask");
        let ui = TestUi::answering(vec![Some("proxy-pass")]);
        let spec = TunnelSpec {
            proxy: Some(proxy.clone()),
            ..Default::default()
        };
        let tunnel = Tunnel::open(&spec, postgres(), &known.known(), ui.clone())
            .await
            .unwrap();
        let conn = pg_through(&tunnel).await;
        assert_eq!(scalar(&*conn, "select 1").await.unwrap(), "1");
        let _again = pg_through(&tunnel).await;
        assert_eq!(
            *ui.questions.lock().unwrap(),
            vec![SecretRequest::ProxyPassword {
                user: "proxyuser".into(),
                host: proxy.host.clone()
            }],
            "{:?}",
            proxy.kind
        );
        tunnel.close().await;
    }
}

#[tokio::test]
async fn a_proxy_and_a_command_hop_do_not_mix() {
    let known = TempKnownHosts::new("proxy-command");
    let mut first = hop("bastion2", 22, "id_ed25519");
    first.command = Some("ssh -W %h:%p jump".into());
    let spec = TunnelSpec {
        hops: vec![first],
        proxy: Some(ProxySpec {
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 1,
            user: None,
            password: None,
        }),
        ..Default::default()
    };
    assert!(matches!(
        Tunnel::connect(&spec, &known.known(), Arc::new(NonInteractive)).await,
        Err(SshError::Other(m)) if m.contains("proxy")
    ));
}
