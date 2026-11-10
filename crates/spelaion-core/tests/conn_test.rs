//! The connection test (ADR-032): each step's outcome, its failure kind and hint, Stop, the
//! connect timeout, and that no secret reaches the log or the diagnostics.
//!
//! The local cases always run. The ones through the bastions need `SPELAION_TEST_SSH` (and
//! `SPELAION_TEST_PG`, `SPELAION_TEST_SOCKS` where they say so; see `testing/docker`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use spelaion_config::{
    ConnectionId, ResolveError, ResolvedConnection, Resolver, Source, SystemBackend, TrustStore,
    load,
};
use spelaion_core::{
    Engines, FailureKind, StepKind, StepStatus, TestOptions, TestReport, TestScope, TestStep,
    test_connection,
};
use spelaion_driver_api::Secret;
use spelaion_ssh::{HostInfo, Interaction, KnownHosts, SecretRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Temporary, trusted repository with `.spelaion/` and a user directory.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-conn-test-{name}-{}", std::process::id()));
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

    fn resolve(&self, id: &str) -> Result<ResolvedConnection, ResolveError> {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let id = ConnectionId {
            source: Source::Workspace,
            path: id.into(),
        };
        Resolver::new(&config, &trust, &SystemBackend).resolve(&id)
    }

    fn known_hosts(&self) -> KnownHosts {
        KnownHosts {
            team_file: None,
            user_file: None,
            app_file: self.base.join("user/known_hosts"),
        }
    }

    fn options(&self, scope: TestScope, ui: Arc<dyn Interaction>) -> TestOptions {
        TestOptions {
            scope,
            known_hosts: self.known_hosts(),
            interaction: ui,
            password: None,
            name: String::new(),
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Accepts unknown hosts, answers questions from a list, and counts host questions.
#[derive(Default)]
struct TestUi {
    answers: Mutex<Vec<String>>,
    hosts_asked: Mutex<usize>,
}

impl TestUi {
    fn answering(answers: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.iter().rev().map(|a| a.to_string()).collect()),
            ..Default::default()
        })
    }
}

#[async_trait]
impl Interaction for TestUi {
    async fn confirm_unknown_host(&self, _host: &HostInfo) -> bool {
        *self.hosts_asked.lock().unwrap() += 1;
        true
    }

    async fn ask_secret(&self, _request: &SecretRequest) -> Option<Secret> {
        self.answers.lock().unwrap().pop().map(Secret::new)
    }
}

/// Every snapshot the observer received.
#[derive(Default)]
struct Seen(Mutex<Vec<Vec<TestStep>>>);

impl Seen {
    fn observer(&self) -> impl Fn(&[TestStep]) + Send + Sync + '_ {
        move |steps: &[TestStep]| self.0.lock().unwrap().push(steps.to_vec())
    }

    /// Everything the observer was ever shown, as text.
    fn text(&self) -> String {
        format!("{:?}", self.0.lock().unwrap())
    }
}

async fn run(
    resolved: Result<ResolvedConnection, ResolveError>,
    options: TestOptions,
    seen: &Seen,
) -> TestReport {
    test_connection(
        resolved,
        &Engines::builtin(),
        options,
        &seen.observer(),
        std::future::pending(),
    )
    .await
}

fn statuses(report: &TestReport) -> Vec<StepStatus> {
    report.steps.iter().map(|s| s.status).collect()
}

/// The failed step's position and failure kind.
fn failure(report: &TestReport) -> (usize, FailureKind) {
    let (index, step) = report
        .failed()
        .unwrap_or_else(|| panic!("no step failed:\n{}", report.diagnostics));
    (index, step.failure.as_ref().unwrap().kind)
}

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

/// Path as a TOML literal (single quotes: no escapes, works for Windows paths).
fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// A port nothing listens on, like a stopped server's.
fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Accepts connections and never answers, like a server behind a firewall that drops what
/// it sends. Keeps them, to see whether the client closed them.
struct Silent {
    port: u16,
    accepted: Arc<tokio::sync::Mutex<Vec<tokio::net::TcpStream>>>,
}

impl Silent {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let keep = accepted.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                keep.lock().await.push(stream);
            }
        });
        Self { port, accepted }
    }

    /// Whether every connection it accepted was closed by the client within `within`.
    async fn all_closed(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        let mut streams = self.accepted.lock().await;
        assert!(!streams.is_empty(), "nothing connected");
        for stream in streams.iter_mut() {
            let mut buf = [0u8; 256];
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(left, stream.read(&mut buf)).await {
                    Ok(Ok(0)) | Ok(Err(_)) => break,
                    Ok(Ok(_)) => continue,
                    Err(_) => return false,
                }
            }
        }
        true
    }
}

fn postgres_connection(host: &str, port: u16, extra: &str) -> String {
    format!(
        "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\n{extra}"
    )
}

// ------------------------------------------------------------------------------- local

#[tokio::test]
async fn a_local_file_is_read_and_opened() {
    let ws = Workspace::new("sqlite");
    let file = ws.base.join("data.sqlite");
    spelaion_core::create_file_database(spelaion_config::Engine::Sqlite, &file).unwrap();
    ws.write(
        ".spelaion/connections/local.toml",
        &format!(
            "schema_version = 1\nengine = \"sqlite\"\ndatabase = {}\n",
            toml_path(&file)
        ),
    );
    let seen = Seen::default();
    let report = run(
        ws.resolve("local"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &seen,
    )
    .await;
    assert!(report.ok, "{}", report.diagnostics);
    assert!(matches!(report.steps[0].kind, StepKind::Resolve));
    assert!(matches!(
        report.steps[1].kind,
        StepKind::Database { file: true, .. }
    ));
    assert!(report.server_version.is_some());
    assert!(
        report.diagnostics.contains("Result: OK"),
        "{}",
        report.diagnostics
    );
    // The observer saw the plan before anything ran.
    let first = seen.0.lock().unwrap()[0].clone();
    assert!(first.iter().all(|s| s.status == StepStatus::Pending));
}

#[tokio::test]
async fn a_configuration_error_is_the_first_step() {
    let ws = Workspace::new("resolve");
    ws.write(
        ".spelaion/connections/bad.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"db\"\ndatabase = \"d\"\nuser = \"u\"\npassword = { secret = \"env:SPELAION_TEST_SURELY_UNSET_VARIABLE\" }\n",
    );
    let report = run(
        ws.resolve("bad"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert_eq!(failure(&report), (0, FailureKind::Config));
    assert_eq!(report.steps.len(), 1);
    assert!(
        report.diagnostics.contains("Hint:"),
        "{}",
        report.diagnostics
    );
}

#[tokio::test]
async fn a_stopped_server_fails_at_reaching_it() {
    let ws = Workspace::new("direct-closed");
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("127.0.0.1", closed_port(), ""),
    );
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert_eq!(failure(&report), (1, FailureKind::HostUnreachable));
    assert!(matches!(report.steps[1].kind, StepKind::Target { .. }));
    assert_eq!(
        statuses(&report),
        [StepStatus::Ok, StepStatus::Failed, StepStatus::Skipped]
    );
}

#[tokio::test]
async fn the_connect_timeout_is_the_connections() {
    let silent = Silent::start().await;
    let ws = Workspace::new("db-timeout");
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("127.0.0.1", silent.port, "connect_timeout = 1\n"),
    );
    let started = Instant::now();
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    // The address answers; the database never does.
    assert_eq!(failure(&report), (2, FailureKind::Timeout));
    assert!(
        report.diagnostics.contains("did not answer in 1 s"),
        "{}",
        report.diagnostics
    );
}

#[tokio::test]
async fn network_only_stops_before_logging_in() {
    let silent = Silent::start().await;
    let ws = Workspace::new("network-only");
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("127.0.0.1", silent.port, ""),
    );
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Network, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert!(report.ok, "{}", report.diagnostics);
    assert_eq!(report.steps.len(), 2);
    assert!(
        report.diagnostics.contains("network only"),
        "{}",
        report.diagnostics
    );
}

#[tokio::test]
async fn stop_ends_the_running_step_and_closes_a_half_open_tunnel() {
    let silent = Silent::start().await;
    let ws = Workspace::new("stop");
    let identity = toml_path(&keys().join("id_ed25519"));
    // A bastion that accepts the connection and never answers: the hop waits.
    ws.write(
        ".spelaion/tunnels/silent.toml",
        &format!(
            "schema_version = 1\nconnect_timeout = 120\n\n[[hops]]\nhost = \"127.0.0.1\"\nport = {}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n",
            silent.port
        ),
    );
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("postgres", 5432, "tunnel = \"silent\"\n"),
    );
    let running = tokio::sync::Notify::new();
    let observer = |steps: &[TestStep]| {
        if steps
            .iter()
            .any(|s| matches!(s.kind, StepKind::Hop { .. }) && s.status == StepStatus::Running)
        {
            running.notify_one();
        }
    };
    // Stopped once the hop is running and its connection is open, halfway through.
    let stop = async {
        running.notified().await;
        while silent.accepted.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let started = Instant::now();
    let report = test_connection(
        ws.resolve("db"),
        &Engines::builtin(),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &observer,
        stop,
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(report.stopped && !report.ok);
    assert_eq!(
        statuses(&report),
        [
            StepStatus::Ok,
            StepStatus::Stopped,
            StepStatus::Skipped,
            StepStatus::Skipped
        ]
    );
    assert!(report.diagnostics.contains("Result: stopped"));
    assert!(
        silent.all_closed(Duration::from_secs(5)).await,
        "the stopped hop's connection was left open"
    );
}

#[tokio::test]
async fn stop_during_the_login_closes_its_connection() {
    let silent = Silent::start().await;
    let ws = Workspace::new("stop-login");
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("127.0.0.1", silent.port, "connect_timeout = 600\n"),
    );
    let accepted = silent.accepted.clone();
    // Reaching the address opens and closes one connection; the login opens the second.
    let stop = async {
        while accepted.lock().await.len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let report = test_connection(
        ws.resolve("db"),
        &Engines::builtin(),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &|_: &[TestStep]| {},
        stop,
    )
    .await;
    assert!(report.stopped, "{}", report.diagnostics);
    assert_eq!(report.steps[2].status, StepStatus::Stopped);
    assert!(
        silent.all_closed(Duration::from_secs(5)).await,
        "the stopped login's connection was left open"
    );
}

/// A system keychain for the resolver, without touching the real one: `entries` by account,
/// or no keychain at all.
struct FakeKeychain {
    entries: Vec<(String, String)>,
    available: bool,
}

impl spelaion_config::SecretBackend for FakeKeychain {
    fn keychain_get(&self, account: &str) -> Result<Option<String>, String> {
        if !self.available {
            return Err(spelaion_config::KEYCHAIN_UNAVAILABLE.into());
        }
        Ok(self
            .entries
            .iter()
            .find(|(a, _)| a == account)
            .map(|(_, secret)| secret.clone()))
    }
    fn keychain_set(&self, _: &str, _: &str) -> Result<(), String> {
        Err("read-only in this test".into())
    }
    fn keychain_delete(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn run(&self, _: &str, _: &[&str]) -> Result<String, String> {
        Err("nothing runs in this test".into())
    }
    fn run_shell(&self, _: &str) -> Result<String, String> {
        Err("nothing runs in this test".into())
    }
    fn keychain_available(&self) -> bool {
        self.available
    }
}

impl Workspace {
    fn resolve_with(
        &self,
        id: &str,
        backend: &dyn spelaion_config::SecretBackend,
    ) -> Result<ResolvedConnection, ResolveError> {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let id = ConnectionId {
            source: Source::Workspace,
            path: id.into(),
        };
        Resolver::new(&config, &trust, backend).resolve(&id)
    }
}

/// A `keychain` connection with nothing stored fails at logging in with its own kind and
/// hint, not as a connection that asks every time; `prompt`, and `keychain` on a system
/// without one, still ask every time. A password the keychain holds for another entry never
/// reaches the log or the diagnostics.
#[tokio::test]
async fn a_keychain_connection_with_nothing_stored_says_so() {
    let server = Silent::start().await;
    let ws = Workspace::new("keychain-empty");
    let connection = |secret: &str| {
        format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"127.0.0.1\"\nport = {}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"{secret}\" }}\n",
            server.port
        )
    };
    ws.write(".spelaion/connections/db.toml", &connection("keychain"));
    let planted = "planted-keychain-secret-4321";
    let keychain = FakeKeychain {
        entries: vec![("another/connection/password".into(), planted.into())],
        available: true,
    };
    let seen = Seen::default();
    let report = run(
        ws.resolve_with("db", &keychain),
        ws.options(TestScope::Full, TestUi::answering(&[planted])),
        &seen,
    )
    .await;
    assert_eq!(
        failure(&report),
        (2, FailureKind::KeychainEmpty),
        "{}",
        report.diagnostics
    );
    assert!(
        report
            .diagnostics
            .contains(FailureKind::KeychainEmpty.hint()),
        "{}",
        report.diagnostics
    );
    assert!(
        report.diagnostics.contains("no password stored")
            && !report
                .diagnostics
                .contains(FailureKind::PasswordRequired.hint()),
        "{}",
        report.diagnostics
    );
    assert_no_leak(&report, &seen, &[planted]);

    // No keychain on this system: the password is asked every time.
    let none = FakeKeychain {
        entries: vec![],
        available: false,
    };
    let report = run(
        ws.resolve_with("db", &none),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert_eq!(failure(&report), (2, FailureKind::PasswordRequired));

    ws.write(".spelaion/connections/db.toml", &connection("prompt"));
    let report = run(
        ws.resolve_with("db", &keychain),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert_eq!(failure(&report), (2, FailureKind::PasswordRequired));
}

// --------------------------------------------------------------------------- redaction

/// Everything that went out must be free of `secrets`.
fn assert_no_leak(report: &TestReport, seen: &Seen, secrets: &[&str]) {
    let shown = format!("{}\n{report:?}\n{}", report.diagnostics, seen.text());
    for secret in secrets {
        assert!(
            !shown.contains(secret),
            "{secret:?} leaked into the log or the diagnostics:\n{}",
            report.diagnostics
        );
    }
}

/// A hop command whose error output repeats the secrets it was given: the hop password from a
/// `cmd:` reference, the database password, a `password=` value, a URL with credentials and a
/// token. None may reach the log or the diagnostics.
#[tokio::test]
async fn secrets_in_error_output_are_redacted() {
    let ws = Workspace::new("leak-command");
    let noise = "hop-secret-1234 db-secret-5678 password=kv-secret-9 http://bob:url-secret-0@proxy.example:3128/ token=tok-secret-7";
    // Written into a TOML literal string ('…'): no single quotes in either shell's command.
    let command = if cfg!(windows) {
        format!("echo {noise} 1>&2 & exit 1")
    } else {
        format!("echo \"{noise}\" 1>&2; exit 1")
    };
    assert!(!command.contains('\''));
    ws.write(
        ".spelaion/tunnels/leaky.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"bastion.example\"\nuser = \"spelaion\"\nauth = [\"password\"]\npassword = {{ secret = \"cmd:echo hop-secret-1234\" }}\ncommand = '{command}'\n"
        ),
    );
    ws.write(
        ".spelaion/connections/db.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = { secret = \"cmd:echo db-secret-5678\" }\ntunnel = \"leaky\"\n",
    );
    let seen = Seen::default();
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &seen,
    )
    .await;
    // The hop failed, and its message still says what the command printed, minus secrets.
    // The planted output did reach the error ("proxy.example" is only in it), so what
    // follows tests the redaction, not a message the output never got into.
    assert_eq!(failure(&report), (1, FailureKind::HostUnreachable));
    assert!(
        report.diagnostics.contains("the command said:")
            && report.diagnostics.contains("proxy.example"),
        "{}",
        report.diagnostics
    );
    assert_no_leak(
        &report,
        &seen,
        &[
            "hop-secret-1234",
            "db-secret-5678",
            "kv-secret-9",
            "url-secret-0",
            "tok-secret-7",
        ],
    );
}

/// The smallest base64 decoder, for the fake proxy below.
fn base64_decode(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let clean: Vec<u8> = text.bytes().filter(|c| *c != b'=').collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= u32::from(value(*c)) << (18 - 6 * i);
        }
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    out
}

/// A hostile HTTP proxy that refuses every CONNECT and puts the credentials it received
/// (both encoded and decoded) into its answer, which ends up in the error message.
async fn echoing_proxy() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let request = String::from_utf8_lossy(&request).to_string();
                let token = request
                    .lines()
                    .find_map(|l| l.strip_prefix("Proxy-Authorization: Basic "))
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let decoded = String::from_utf8_lossy(&base64_decode(&token)).to_string();
                let answer =
                    format!("HTTP/1.1 403 Basic {token} {decoded} password={decoded}\r\n\r\n");
                let _ = stream.write_all(answer.as_bytes()).await;
            });
        }
    });
    port
}

/// A proxy password answered in the dialog during the test is redacted like a stored one,
/// even when a hostile proxy repeats it back.
#[tokio::test]
async fn secrets_answered_in_dialogs_are_redacted() {
    let port = echoing_proxy().await;
    let ws = Workspace::new("leak-dialog");
    ws.write(
        ".spelaion/tunnels/proxy.toml",
        &format!(
            "schema_version = 1\n\n[proxy]\ntype = \"http\"\nhost = \"127.0.0.1\"\nport = {port}\nuser = \"proxyuser\"\npassword = {{ secret = \"prompt\" }}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("postgres", 5432, "tunnel = \"proxy\"\n"),
    );
    let seen = Seen::default();
    let answered = "answered-proxy-secret";
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[answered])),
        &seen,
    )
    .await;
    // Proxy fine on its own (HTTP checks credentials on CONNECT), refused at the target.
    assert_eq!(failure(&report), (2, FailureKind::HostUnreachable));
    assert!(report.diagnostics.contains("403"), "{}", report.diagnostics);
    let encoded = "cHJveHl1c2VyOmFuc3dlcmVkLXByb3h5LXNlY3JldA";
    assert_no_leak(&report, &seen, &[answered, encoded]);
}

// ------------------------------------------------------------------- Docker: the bastions

fn bastion() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    Some((host.to_string(), port.parse().unwrap()))
}

/// bastion1 (from the host) then bastion2, by key, unless `second` says otherwise.
fn two_hops(host: &str, port: u16, second: &str) -> String {
    let identity = toml_path(&keys().join("id_ed25519"));
    format!(
        "schema_version = 1\nconnect_timeout = 10\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\n{second}"
    )
}

fn key_auth() -> String {
    format!(
        "auth = [\"key\"]\nidentity_file = {}\n",
        toml_path(&keys().join("id_ed25519"))
    )
}

#[tokio::test]
async fn a_wrong_password_on_hop_2_is_hop_2s_ssh_authentication() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let ws = Workspace::new("hop2-password");
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &two_hops(
            &host,
            port,
            "auth = [\"password\"]\npassword = { secret = \"cmd:echo wrong-hop2-password\" }\n",
        ),
    );
    // The test database's password is its user name, which redaction would hide too.
    ws.write(
        ".spelaion/connections/db.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = { secret = \"cmd:echo unused-db-password\" }\ntunnel = \"bastions\"\n",
    );
    let seen = Seen::default();
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &seen,
    )
    .await;
    assert_eq!(
        failure(&report),
        (2, FailureKind::SshAuth),
        "{}",
        report.diagnostics
    );
    assert!(matches!(report.steps[2].kind, StepKind::Hop { hop: 1, .. }));
    assert_eq!(
        statuses(&report),
        [
            StepStatus::Ok,
            StepStatus::Ok,
            StepStatus::Failed,
            StepStatus::Skipped,
            StepStatus::Skipped
        ]
    );
    assert!(
        report
            .diagnostics
            .contains("3. [FAILED] SSH hop 2: spelaion@bastion2:22"),
        "{}",
        report.diagnostics
    );
    assert!(report.diagnostics.contains(FailureKind::SshAuth.hint()));
    assert_no_leak(&report, &seen, &["wrong-hop2-password"]);
}

#[tokio::test]
async fn a_stopped_bastion_is_its_hops_unreachable_host() {
    let Some((host, port)) = bastion() else {
        return;
    };
    // The first bastion stopped: nothing listens where it should.
    let ws = Workspace::new("bastion-stopped");
    ws.write(
        ".spelaion/tunnels/first.toml",
        &two_hops("127.0.0.1", closed_port(), &key_auth()),
    );
    // The second one stopped: bastion1 cannot open a channel to it.
    ws.write(
        ".spelaion/tunnels/second.toml",
        &two_hops(&host, port, &format!("port = 2299\n{}", key_auth())),
    );
    for (tunnel, step) in [("first", 1), ("second", 2)] {
        ws.write(
            &format!(".spelaion/connections/{tunnel}.toml"),
            &postgres_connection("postgres", 5432, &format!("tunnel = \"{tunnel}\"\n")),
        );
        let report = run(
            ws.resolve(tunnel),
            ws.options(TestScope::Full, TestUi::answering(&[])),
            &Seen::default(),
        )
        .await;
        assert_eq!(
            failure(&report),
            (step, FailureKind::HostUnreachable),
            "{tunnel}: {}",
            report.diagnostics
        );
        assert!(
            report
                .diagnostics
                .contains(FailureKind::HostUnreachable.hint())
        );
    }
}

#[tokio::test]
async fn a_wrong_database_name_fails_at_logging_in() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let ws = Workspace::new("wrong-db");
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &two_hops(&host, port, &key_auth()),
    );
    ws.write(
        ".spelaion/connections/db.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"no_such_database\"\nuser = \"spelaion\"\npassword = { secret = \"cmd:echo spelaion\" }\ntunnel = \"bastions\"\n",
    );
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert_eq!(
        failure(&report),
        (4, FailureKind::DbNotFound),
        "{}",
        report.diagnostics
    );
    assert_eq!(
        statuses(&report)[..4],
        [
            StepStatus::Ok,
            StepStatus::Ok,
            StepStatus::Ok,
            StepStatus::Ok
        ]
    );
    assert!(report.diagnostics.contains(FailureKind::DbNotFound.hint()));
}

#[tokio::test]
async fn a_wrong_database_password_is_database_authentication() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let ws = Workspace::new("wrong-db-password");
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &two_hops(&host, port, &key_auth()),
    );
    ws.write(
        ".spelaion/connections/db.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = { secret = \"cmd:echo wrong-db-password\" }\ntunnel = \"bastions\"\n",
    );
    let seen = Seen::default();
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &seen,
    )
    .await;
    assert_eq!(
        failure(&report),
        (4, FailureKind::DbAuth),
        "{}",
        report.diagnostics
    );
    assert_no_leak(&report, &seen, &["wrong-db-password"]);
}

#[tokio::test]
async fn every_step_succeeds_through_two_bastions() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let ws = Workspace::new("all-ok");
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &two_hops(&host, port, &key_auth()),
    );
    // The connection's fixed local port is taken (by an open tab, say): the test uses its own.
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken_port = taken.local_addr().unwrap().port();
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection(
            "postgres",
            5432,
            &format!("tunnel = \"bastions\"\nlocal_port = {taken_port}\n"),
        ),
    );
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert!(report.ok, "{}", report.diagnostics);
    assert!(!report.server_version.unwrap().is_empty());
    assert!(report.steps.iter().all(|s| s.elapsed.is_some()));
    // Network only: the same steps without the login.
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Network, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert!(report.ok, "{}", report.diagnostics);
    assert_eq!(report.steps.len(), 4);
}

#[tokio::test]
async fn a_changed_host_key_blocks_and_is_never_asked_about() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let ws = Workspace::new("host-key-changed");
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &two_hops(&host, port, &key_auth()),
    );
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("postgres", 5432, "tunnel = \"bastions\"\n"),
    );
    // Another key stored for bastion1: what an impersonated server looks like.
    let other = std::fs::read_to_string(keys().join("id_ed25519_pass.pub")).unwrap();
    let mut parts = other.split_whitespace();
    let known = ws.known_hosts();
    std::fs::write(
        &known.app_file,
        format!(
            "[{host}]:{port} {} {}\n",
            parts.next().unwrap(),
            parts.next().unwrap()
        ),
    )
    .unwrap();
    let ui = TestUi::answering(&[]);
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, ui.clone()),
        &Seen::default(),
    )
    .await;
    assert_eq!(
        failure(&report),
        (1, FailureKind::HostKeyChanged),
        "{}",
        report.diagnostics
    );
    assert_eq!(
        *ui.hosts_asked.lock().unwrap(),
        0,
        "a changed key is never asked about"
    );
    assert!(report.diagnostics.contains("known_hosts"));
}

#[tokio::test]
async fn a_wrong_proxy_password_is_the_proxys_authentication() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SOCKS") else {
        eprintln!("SPELAION_TEST_SOCKS not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("proxy-password");
    ws.write(
        ".spelaion/tunnels/proxy.toml",
        &format!(
            "schema_version = 1\n\n[proxy]\ntype = \"socks5\"\nhost = \"{host}\"\nport = {port}\nuser = \"proxyuser\"\npassword = {{ secret = \"cmd:echo wrong-proxy-password\" }}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection("postgres", 5432, "tunnel = \"proxy\"\n"),
    );
    let seen = Seen::default();
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &seen,
    )
    .await;
    assert_eq!(
        failure(&report),
        (1, FailureKind::ProxyAuth),
        "{}",
        report.diagnostics
    );
    assert!(matches!(report.steps[1].kind, StepKind::Proxy { .. }));
    assert_no_leak(&report, &seen, &["wrong-proxy-password"]);
}

#[tokio::test]
async fn a_certificate_the_client_cannot_verify_is_tls() {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("tls");
    // The test server's certificate is signed by the test authority, unknown to the system.
    ws.write(
        ".spelaion/connections/db.toml",
        &postgres_connection(
            host,
            port.parse().unwrap(),
            "\n[tls]\nmode = \"verify-full\"\n",
        ),
    );
    let report = run(
        ws.resolve("db"),
        ws.options(TestScope::Full, TestUi::answering(&[])),
        &Seen::default(),
    )
    .await;
    assert_eq!(
        failure(&report),
        (2, FailureKind::Tls),
        "{}",
        report.diagnostics
    );
}
