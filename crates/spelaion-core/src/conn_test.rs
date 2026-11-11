//! The connection test (ADR-032): opens a connection one step at a time (reading the
//! configuration, the proxy, each SSH hop, the Kubernetes forward, reaching the database's
//! address, logging in) and says which step failed, what kind of failure it was and what to
//! try. A test opens its own tunnel, never a shared one, closes everything it opened when it
//! ends or is stopped, and retries nothing.
//!
//! Every text it hands out (step labels, error messages, the diagnostics to copy) passes
//! through a [`Redactor`]: the secrets it knows (resolved passwords, the ones typed in the
//! form, the ones answered in a dialog) and anything shaped like a credential become `***`.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use spelaion_config::{
    Engine, Password, ProxyType, ResolveError, ResolvedConnection, ResolvedTunnel, TunnelPasswords,
};
use spelaion_driver_api::{DriverError, Secret};
use spelaion_ssh::{
    HostInfo, Interaction, KnownHosts, ProxyFailure, SecretRequest, SshError, SsmFailure, Tunnel,
    TunnelStage,
};

use crate::Engines;
use crate::kube::{KubeError, KubeTunnel};
use crate::open::{
    OpenError, connect_driver, remote_sqlite, target, team_known_hosts, tunnel_spec,
    typed_tunnel_spec,
};

/// How far a connection test goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestScope {
    /// Every step, logging in to the database at the end.
    Full,
    /// The network only: up to reaching the database's address, without logging in.
    Network,
}

/// One step of a connection test, with what it is about. Labels never hold secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    /// Reading the connection, its network profile, variables and secret references.
    Resolve,
    /// The network profile's proxy (ADR-027).
    Proxy {
        /// `socks5` or `http`.
        proxy_type: String,
        host: String,
        port: u16,
    },
    /// The network profile's AWS SSM session (ADR-050).
    Ssm {
        target: String,
        region: Option<String>,
    },
    /// An SSH hop (0-based `hop`).
    Hop {
        hop: usize,
        user: String,
        host: String,
        port: u16,
        /// Reached through an external command (`ProxyCommand`).
        command: bool,
    },
    /// `kubectl port-forward` (ADR-015).
    Kubernetes {
        resource: String,
        context: Option<String>,
        namespace: Option<String>,
    },
    /// Reaching the database's address (from the last hop, through the proxy, or directly),
    /// without sending it anything.
    Target { host: String, port: u16 },
    /// Logging in to the database, or opening its file.
    Database {
        engine: String,
        database: String,
        user: String,
        file: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Ok,
    Failed,
    /// Not run: an earlier step failed.
    Skipped,
    /// It was running when the test was stopped.
    Stopped,
}

/// What kind of failure a step had, decided here and nowhere else; each has a hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The configuration or a secret could not be read.
    Config,
    /// The connection asks for its password and none was typed.
    PasswordRequired,
    /// The password is read from the system keychain, which has none stored for this
    /// connection, and none was typed.
    KeychainEmpty,
    /// A question (password, passphrase, fingerprint) was dismissed.
    Cancelled,
    ProxyAuth,
    SshAuth,
    HostUnreachable,
    /// A blocking error: the server's key is not the stored one.
    HostKeyChanged,
    /// An unknown server whose fingerprint was not accepted.
    HostKeyRejected,
    DbAuth,
    DbNotFound,
    Tls,
    Timeout,
    Kubernetes,
    /// AWS SSM could not start: a missing AWS CLI or plugin, credentials, the managed node.
    Ssm,
    Other,
}

impl FailureKind {
    /// Short name, as the diagnostics show it.
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::Config => "configuration",
            FailureKind::PasswordRequired => "password required",
            FailureKind::KeychainEmpty => "no password stored",
            FailureKind::Cancelled => "cancelled",
            FailureKind::ProxyAuth => "proxy authentication",
            FailureKind::SshAuth => "SSH authentication",
            FailureKind::HostUnreachable => "host unreachable",
            FailureKind::HostKeyChanged => "host key changed",
            FailureKind::HostKeyRejected => "host key not accepted",
            FailureKind::DbAuth => "database authentication",
            FailureKind::DbNotFound => "database not found",
            FailureKind::Tls => "TLS",
            FailureKind::Timeout => "timeout",
            FailureKind::Kubernetes => "Kubernetes",
            FailureKind::Ssm => "AWS SSM",
            FailureKind::Other => "error",
        }
    }

    /// What to try. A changed host key is explained, never offered to be accepted.
    pub fn hint(self) -> &'static str {
        match self {
            FailureKind::Config => {
                "The connection or its network profile could not be read: fix what the message names (a variable, a secret reference, a file path)."
            }
            FailureKind::PasswordRequired => {
                "This connection asks for its password every time: type it in the form to test the login."
            }
            FailureKind::KeychainEmpty => {
                "No password is stored in the system keychain for this connection. Type it to test the login; saving the connection stores it."
            }
            FailureKind::Cancelled => {
                "A question (a password, a passphrase or a server fingerprint) was dismissed, so the test stopped there. Test again and answer it."
            }
            FailureKind::ProxyAuth => {
                "The proxy refused the username or the password. Check them in the proxy section of the network profile."
            }
            FailureKind::SshAuth => {
                "This SSH server refused every method tried: check the user, the key (its public key must be in the server's authorized_keys), the password or the agent. Some servers refuse passwords altogether."
            }
            FailureKind::HostUnreachable => {
                "Nothing answered at this address: check the host name and port, that the server is running, and that this step's network (VPN, firewall, the previous hop) can reach it."
            }
            FailureKind::HostKeyChanged => {
                "This server's key is not the one stored for it. That happens when a server is reinstalled, and also when someone intercepts the connection. Ask its administrator for its new fingerprint; only once it is confirmed, remove the old line from the known_hosts file the message names. SpelaionDB will not connect to it until then."
            }
            FailureKind::HostKeyRejected => {
                "The server's fingerprint was not accepted. Compare it with the one its administrator gives you, then test again."
            }
            FailureKind::DbAuth => {
                "The database refused the user or the password. Check both, and that this user may log in from where the connection arrives (the last hop, with a tunnel)."
            }
            FailureKind::DbNotFound => {
                "The server answered, but the database (or file) does not exist or this user cannot open it: check its name."
            }
            FailureKind::Tls => {
                "The encrypted connection failed: check the SSL/TLS mode, the CA certificate, and that the host name matches the server's certificate (through a tunnel the real server name is still checked)."
            }
            FailureKind::Timeout => {
                "No answer in time: the server may be down, or a firewall may be dropping packets. Check the address, or raise the connect timeout if the network is only slow."
            }
            FailureKind::Kubernetes => {
                "kubectl could not forward the port: check the context, namespace, resource and port in the Kubernetes tab, and that kubectl works from a terminal."
            }
            FailureKind::Ssm => {
                "AWS SSM could not start the session: check that the AWS CLI v2 and the Session Manager plugin are installed (the message names what is missing), that you are signed in (aws sso login for SSO profiles), and that the target is a managed node you may start sessions on."
            }
            FailureKind::Other => {
                "See the message. Copy the diagnostics to share them: secrets are left out."
            }
        }
    }
}

/// Why a step failed. The message is already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFailure {
    pub kind: FailureKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestStep {
    pub kind: StepKind,
    pub status: StepStatus,
    pub failure: Option<StepFailure>,
    /// How long it took (or ran before failing or being stopped).
    pub elapsed: Option<Duration>,
}

impl TestStep {
    fn pending(kind: StepKind) -> Self {
        Self {
            kind,
            status: StepStatus::Pending,
            failure: None,
            elapsed: None,
        }
    }

    /// One line in English, as the diagnostics show it.
    pub fn title(&self) -> String {
        match &self.kind {
            StepKind::Resolve => "Read the configuration and secrets".into(),
            StepKind::Proxy {
                proxy_type,
                host,
                port,
            } => format!("Proxy ({}) {host}:{port}", proxy_type.to_uppercase()),
            StepKind::Hop {
                hop,
                user,
                host,
                port,
                command,
            } => format!(
                "SSH hop {}: {user}@{host}:{port}{}",
                hop + 1,
                if *command { " (through a command)" } else { "" }
            ),
            StepKind::Ssm { target, region } => match region {
                Some(region) => format!("AWS SSM session through {target} ({region})"),
                None => format!("AWS SSM session through {target}"),
            },
            StepKind::Kubernetes {
                resource,
                context,
                namespace,
            } => {
                let mut title = format!("Kubernetes port-forward to {resource}");
                if let Some(namespace) = namespace {
                    title.push_str(&format!(" in {namespace}"));
                }
                if let Some(context) = context {
                    title.push_str(&format!(" (context {context})"));
                }
                title
            }
            StepKind::Target { host, port } => format!("Reach {host}:{port}"),
            StepKind::Database {
                engine,
                database,
                user,
                file,
            } => {
                if *file {
                    format!("Open the {engine} file {database}")
                } else if database.is_empty() {
                    format!("Log in to {engine} as {user}")
                } else {
                    format!("Log in to {engine} database {database} as {user}")
                }
            }
        }
    }
}

/// The end of a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestReport {
    pub scope: TestScope,
    /// Every step reached `Ok`.
    pub ok: bool,
    pub stopped: bool,
    pub server_version: Option<String>,
    pub steps: Vec<TestStep>,
    /// The text "Copy diagnostics" copies: built here, redacted here.
    pub diagnostics: String,
}

impl TestReport {
    /// The step that failed, if one did.
    pub fn failed(&self) -> Option<(usize, &TestStep)> {
        self.steps
            .iter()
            .enumerate()
            .find(|(_, s)| s.status == StepStatus::Failed)
    }
}

/// What a connection test needs besides the resolved configuration.
pub struct TestOptions {
    pub scope: TestScope,
    pub known_hosts: KnownHosts,
    /// Dialogs (unknown host, passphrase, 2FA). Its answers are redacted from the log.
    pub interaction: Arc<dyn Interaction>,
    /// The password typed in the form, for a connection that asks for it.
    pub password: Option<Secret>,
    /// The connection's name, for the diagnostics' header.
    pub name: String,
}

/// Receives every step, already redacted, whenever one changes.
pub type StepObserver<'a> = &'a (dyn Fn(&[TestStep]) + Send + Sync);

type Failure = (FailureKind, String);

/// Tests a connection step by step. `resolved` is the outcome of resolving it: an error
/// there is the first step's. Ends when every step has run, one has failed, or `stop`
/// completes; whatever was opened is closed before it returns.
pub async fn test_connection(
    resolved: Result<ResolvedConnection, ResolveError>,
    engines: &Engines,
    options: TestOptions,
    observer: StepObserver<'_>,
    stop: impl Future<Output = ()> + Send,
) -> TestReport {
    let redactor = Arc::new(Redactor::new());
    if let Some(password) = &options.password {
        redactor.add(password.expose());
    }
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            let run = Run::new(vec![StepKind::Resolve], redactor, observer);
            run.start(0);
            run.fail((FailureKind::Config, error.to_string()));
            return run.report(options.scope, &options.name, None, false);
        }
    };
    learn_connection_secrets(&redactor, &resolved);
    let plan = Plan::of_connection(&resolved, options.scope);
    let run = Run::new(plan.steps.clone(), redactor.clone(), observer);
    let slots = Slots::default();
    let interaction: Arc<dyn Interaction> = Arc::new(Recording {
        inner: options.interaction.clone(),
        redactor,
    });
    let work = connection_steps(
        &run,
        &slots,
        &plan,
        &resolved,
        engines,
        &options,
        interaction,
    );
    let (server_version, stopped) = drive(&run, &slots, work, stop).await;
    let name = if options.name.trim().is_empty() {
        resolved.name.clone()
    } else {
        options.name.clone()
    };
    run.report(options.scope, &name, server_version, stopped)
}

/// Tests a network profile (a tunnel file) on its own: the proxy and every hop, without a
/// database. Passwords typed in its editor are used where the file has none, and redacted.
pub async fn test_network_profile(
    resolved: Result<ResolvedTunnel, ResolveError>,
    passwords: TunnelPasswords,
    known_hosts: &KnownHosts,
    interaction: Arc<dyn Interaction>,
    observer: StepObserver<'_>,
    stop: impl Future<Output = ()> + Send,
) -> TestReport {
    let redactor = Arc::new(Redactor::new());
    for secret in passwords.hops.iter().flatten().chain(&passwords.proxy) {
        redactor.add(secret.expose());
    }
    let tunnel = match resolved {
        Ok(tunnel) => tunnel,
        Err(error) => {
            let run = Run::new(vec![StepKind::Resolve], redactor, observer);
            run.start(0);
            run.fail((FailureKind::Config, error.to_string()));
            return run.report(TestScope::Network, "", None, false);
        }
    };
    learn_tunnel_secrets(&redactor, &tunnel);
    let mut steps = vec![StepKind::Resolve];
    let layout = TunnelLayout::push(&mut steps, &tunnel);
    let run = Run::new(steps, redactor.clone(), observer);
    let slots = Slots::default();
    let interaction: Arc<dyn Interaction> = Arc::new(Recording {
        inner: interaction,
        redactor,
    });
    let spec = typed_tunnel_spec(&tunnel, passwords);
    let known = team_known_hosts(&tunnel, known_hosts);
    let work = async {
        run.start(0);
        run.ok_running();
        open_tunnel(&run, &slots, &layout, &spec, &known, interaction).await?;
        Ok(None)
    };
    let (_, stopped) = drive(&run, &slots, work, stop).await;
    run.report(
        TestScope::Network,
        &format!("network profile {}", tunnel.name),
        None,
        stopped,
    )
}

/// Runs the steps until they end or `stop` completes, then closes what they opened.
async fn drive(
    run: &Run<'_>,
    slots: &Slots,
    work: impl Future<Output = Result<Option<String>, Failure>>,
    stop: impl Future<Output = ()>,
) -> (Option<String>, bool) {
    let outcome = {
        // Dropped at the end of this block: a step stopped halfway lets go of what it held
        // (a half-open SSH chain closes its sessions, a hop command is killed).
        let work = std::pin::pin!(work);
        let stop = std::pin::pin!(stop);
        tokio::select! {
            result = work => Some(result),
            () = stop => None,
        }
    };
    let result = match outcome {
        Some(Ok(version)) => (version, false),
        Some(Err(failure)) => {
            run.fail(failure);
            (None, false)
        }
        None => {
            run.stop();
            (None, true)
        }
    };
    slots.close().await;
    result
}

/// What the steps have opened, kept outside them so it is closed even when they are stopped.
#[derive(Default)]
struct Slots {
    tunnel: Mutex<Option<Arc<Tunnel>>>,
    kube: Mutex<Option<Arc<KubeTunnel>>>,
}

impl Slots {
    async fn close(&self) {
        let tunnel = self.tunnel.lock().expect("lock poisoned").take();
        if let Some(tunnel) = tunnel {
            tunnel.close().await;
        }
        let kube = self.kube.lock().expect("lock poisoned").take();
        if let Some(kube) = kube {
            kube.close().await;
        }
    }
}

/// Where each step of a connection is in the list.
struct Plan {
    steps: Vec<StepKind>,
    tunnel: Option<TunnelLayout>,
    kubernetes: Option<usize>,
    target: Option<usize>,
    database: Option<usize>,
}

/// Where a tunnel's proxy and hops are in the list.
struct TunnelLayout {
    proxy: Option<usize>,
    ssm: Option<usize>,
    /// The first hop's step; the others follow it.
    first_hop: usize,
    hops: usize,
}

impl TunnelLayout {
    fn push(steps: &mut Vec<StepKind>, tunnel: &ResolvedTunnel) -> Self {
        let proxy = tunnel.proxy.as_ref().map(|proxy| {
            steps.push(StepKind::Proxy {
                proxy_type: match proxy.kind {
                    ProxyType::Socks5 => "socks5".into(),
                    ProxyType::Http => "http".into(),
                },
                host: proxy.host.clone(),
                port: proxy.port,
            });
            steps.len() - 1
        });
        let ssm = tunnel.ssm.as_ref().map(|ssm| {
            steps.push(StepKind::Ssm {
                target: ssm.target.clone(),
                region: ssm.region.clone(),
            });
            steps.len() - 1
        });
        let first_hop = steps.len();
        for (hop, spec) in tunnel.hops.iter().enumerate() {
            steps.push(StepKind::Hop {
                hop,
                user: spec.user.clone(),
                host: spec.host.clone(),
                port: spec.port,
                command: spec.command.is_some(),
            });
        }
        Self {
            proxy,
            ssm,
            first_hop,
            hops: tunnel.hops.len(),
        }
    }

    /// The first step the tunnel runs.
    fn first(&self) -> usize {
        self.proxy.or(self.ssm).unwrap_or(self.first_hop)
    }

    fn step_of(&self, stage: TunnelStage) -> usize {
        match stage {
            TunnelStage::Proxy => self.first(),
            TunnelStage::Ssm => self.ssm.unwrap_or(self.first()),
            TunnelStage::Hop(hop) => self.first_hop + hop.min(self.hops.saturating_sub(1)),
        }
    }
}

impl Plan {
    fn of_connection(resolved: &ResolvedConnection, scope: TestScope) -> Self {
        let mut steps = vec![StepKind::Resolve];
        let file = resolved.engine.is_file();
        let tunnel = match (&resolved.tunnel, &resolved.kubernetes) {
            (Some(tunnel), None) => Some(TunnelLayout::push(&mut steps, tunnel)),
            (Some(_) | None, Some(_)) | (None, None) => None,
        };
        let kubernetes = resolved.kubernetes.as_ref().map(|k| {
            steps.push(StepKind::Kubernetes {
                resource: k.resource.clone(),
                context: k.context.clone(),
                namespace: k.namespace.clone(),
            });
            steps.len() - 1
        });
        // A file has no address; a Kubernetes forward always accepts locally, so reaching it
        // says nothing more than its own step.
        let target = (!file && kubernetes.is_none()).then(|| {
            steps.push(StepKind::Target {
                host: resolved.params.host.clone(),
                port: resolved.params.port,
            });
            steps.len() - 1
        });
        let database = (scope == TestScope::Full).then(|| {
            steps.push(StepKind::Database {
                engine: resolved.engine.as_str().into(),
                database: resolved.params.database.clone(),
                user: resolved.params.user.clone(),
                file,
            });
            steps.len() - 1
        });
        Self {
            steps,
            tunnel,
            kubernetes,
            target,
            database,
        }
    }
}

async fn connection_steps(
    run: &Run<'_>,
    slots: &Slots,
    plan: &Plan,
    resolved: &ResolvedConnection,
    engines: &Engines,
    options: &TestOptions,
    interaction: Arc<dyn Interaction>,
) -> Result<Option<String>, Failure> {
    run.start(0);
    run.ok_running();
    let mut params = resolved.connect_params(options.password.clone());
    let timeout = params.connect_timeout();
    let mut tunnel = None;
    if let (Some(layout), Some(resolved_tunnel)) = (&plan.tunnel, &resolved.tunnel) {
        let spec = tunnel_spec(resolved_tunnel);
        let known = team_known_hosts(resolved_tunnel, &options.known_hosts);
        tunnel = Some(open_tunnel(run, slots, layout, &spec, &known, interaction).await?);
    }
    if let (Some(step), Some(kubernetes)) = (plan.kubernetes, &resolved.kubernetes) {
        run.start(step);
        let kube = Arc::new(KubeTunnel::new(kubernetes.clone()));
        *slots.kube.lock().expect("lock poisoned") = Some(kube.clone());
        params.connect_addr = Some(kube.forward(params.port).await.map_err(kube_failure)?);
        run.ok_running();
    }
    if let Some(step) = plan.target {
        run.start(step);
        match &tunnel {
            Some(tunnel) => tunnel
                .probe(&target(&params), timeout)
                .await
                .map_err(|e| ssh_failure(&e))?,
            None => probe_direct(&params.host, params.port, timeout).await?,
        }
        run.ok_running();
    }
    let Some(step) = plan.database else {
        return Ok(None);
    };
    run.start(step);
    if let Password::Prompt { save_to } = &resolved.password
        && options.password.is_none()
    {
        return Err(match save_to {
            // `keychain`, and the keychain has nothing for it.
            Some(_) => (
                FailureKind::KeychainEmpty,
                "the system keychain has no password stored for this connection, and none was typed"
                    .into(),
            ),
            // `prompt`, or `keychain` with no keychain on this system.
            None => (
                FailureKind::PasswordRequired,
                "this connection asks for its password, and none was typed".into(),
            ),
        });
    }
    if let Some(tunnel) = &tunnel {
        if resolved.engine == Engine::Sqlite {
            params.remote = Some(remote_sqlite(tunnel, resolved));
        } else {
            // A port of its own, whatever the connection fixes: the test must not take a
            // port an open connection already uses.
            params.connect_addr = Some(
                tunnel
                    .forward(target(&params))
                    .await
                    .map_err(|e| ssh_failure(&e))?,
            );
        }
    }
    let driver = engines.get(resolved.engine).ok_or_else(|| {
        (
            FailureKind::Other,
            format!("engine not available: {}", resolved.engine.as_str()),
        )
    })?;
    let connection = connect_driver(driver.as_ref(), resolved.engine, &params)
        .await
        .map_err(|e| open_failure(&e))?;
    let version = connection
        .server_version()
        .await
        .map_err(|e| driver_failure(&e))?;
    drop(connection);
    run.ok_running();
    Ok(Some(version))
}

/// Opens the tunnel step by step and keeps it in `slots`.
async fn open_tunnel(
    run: &Run<'_>,
    slots: &Slots,
    layout: &TunnelLayout,
    spec: &spelaion_ssh::TunnelSpec,
    known: &KnownHosts,
    interaction: Arc<dyn Interaction>,
) -> Result<Arc<Tunnel>, Failure> {
    // Errors before the first part starts (a hop command where it cannot be) belong to it.
    run.start(layout.first());
    let observe = |stage: TunnelStage| run.start(layout.step_of(stage));
    let tunnel = Tunnel::connect_observed(spec, known, interaction, &observe)
        .await
        .map_err(|e| ssh_failure(&e))?;
    let tunnel = Arc::new(tunnel);
    *slots.tunnel.lock().expect("lock poisoned") = Some(tunnel.clone());
    run.ok_running();
    Ok(tunnel)
}

/// A TCP connection to `host:port`, closed at once.
async fn probe_direct(host: &str, port: u16, timeout: Duration) -> Result<(), Failure> {
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port))).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err((
            FailureKind::HostUnreachable,
            format!("could not reach {host}:{port}: {e}"),
        )),
        Err(_) => Err((
            FailureKind::Timeout,
            format!("{host}:{port} did not answer in {} s", timeout.as_secs()),
        )),
    }
}

// ------------------------------------------------------------------------- classification

/// An SSH or proxy failure's kind. Changed host keys stay their own kind: their hint never
/// offers to accept the new key.
pub fn classify_ssh(error: &SshError) -> FailureKind {
    match error {
        SshError::Connect { .. } => FailureKind::HostUnreachable,
        SshError::Timeout { .. } => FailureKind::Timeout,
        SshError::HostKeyChanged { .. } => FailureKind::HostKeyChanged,
        SshError::HostKeyRejected { .. } => FailureKind::HostKeyRejected,
        SshError::Auth { .. } => FailureKind::SshAuth,
        SshError::Cancelled => FailureKind::Cancelled,
        SshError::Ssm { failure, .. } => match failure {
            SsmFailure::Timeout => FailureKind::Timeout,
            SsmFailure::MissingCli
            | SsmFailure::MissingPlugin
            | SsmFailure::Invalid
            | SsmFailure::Exited => FailureKind::Ssm,
        },
        SshError::Proxy { failure, .. } => match failure {
            ProxyFailure::Unreachable | ProxyFailure::Target => FailureKind::HostUnreachable,
            ProxyFailure::Rejected => FailureKind::ProxyAuth,
            ProxyFailure::Timeout => FailureKind::Timeout,
            ProxyFailure::Protocol => FailureKind::Other,
        },
        SshError::Target { timed_out, .. } => {
            if *timed_out {
                FailureKind::Timeout
            } else {
                FailureKind::HostUnreachable
            }
        }
        SshError::Hop { .. } | SshError::Other(_) => FailureKind::Other,
    }
}

/// A database failure's kind, from the engine's code where it has one.
pub fn classify_driver(error: &DriverError) -> FailureKind {
    match error {
        DriverError::Database {
            code,
            message,
            detail,
            ..
        } => {
            let code = code.as_deref().unwrap_or_default();
            let mysql = detail
                .as_deref()
                .and_then(|d| d.strip_prefix("MySQL error "))
                .unwrap_or_default();
            match (code, mysql) {
                // PostgreSQL and MySQL share SQLSTATE 28: invalid authorization.
                (code, _) if code.starts_with("28") => FailureKind::DbAuth,
                // PostgreSQL: invalid catalog name.
                ("3D000", _) => FailureKind::DbNotFound,
                // MySQL: unknown database; access denied to it.
                (_, "1049") => FailureKind::DbNotFound,
                (_, "1044" | "1045") => FailureKind::DbAuth,
                // SQL Server: login failed; cannot open the database the login asks for.
                ("18456" | "18452" | "18470", _) => FailureKind::DbAuth,
                ("4060", _) => FailureKind::DbNotFound,
                _ => classify_text(message),
            }
        }
        DriverError::Connection(message) | DriverError::Other(message) => classify_text(message),
        DriverError::Cancelled => FailureKind::Cancelled,
        DriverError::Decode { .. } => FailureKind::Other,
    }
}

/// Connection errors that only have a message (from the TLS, socket and driver libraries).
fn classify_text(message: &str) -> FailureKind {
    let text = message.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| text.contains(w));
    if any(&["tls", "ssl", "certificate", "handshake"]) {
        FailureKind::Tls
    } else if any(&["timed out", "timeout", "no response in", "did not answer"]) {
        FailureKind::Timeout
    } else if any(&[
        "password authentication failed",
        "access denied",
        "login failed",
    ]) {
        FailureKind::DbAuth
    } else if any(&[
        "does not exist",
        "unknown database",
        "no such file",
        "unable to open database file",
        "cannot open database",
    ]) {
        FailureKind::DbNotFound
    } else if any(&[
        "refused",
        "unreachable",
        "no such host",
        "failed to lookup",
        "name or service not known",
        "nodename nor servname",
        "no address",
        "not known",
    ]) {
        FailureKind::HostUnreachable
    } else {
        FailureKind::Other
    }
}

fn classify_kube(error: &KubeError) -> FailureKind {
    match error {
        KubeError::Spawn { .. } | KubeError::Kubectl(_) | KubeError::Down(_) => {
            FailureKind::Kubernetes
        }
        KubeError::Timeout => FailureKind::Timeout,
    }
}

fn ssh_failure(error: &SshError) -> Failure {
    (classify_ssh(error), error.to_string())
}

fn kube_failure(error: KubeError) -> Failure {
    (classify_kube(&error), error.to_string())
}

fn driver_failure(error: &DriverError) -> Failure {
    let message = match error {
        DriverError::Database {
            message, detail, ..
        } => match detail {
            Some(detail) => format!("{message} ({detail})"),
            None => message.clone(),
        },
        DriverError::Connection(_)
        | DriverError::Cancelled
        | DriverError::Decode { .. }
        | DriverError::Other(_) => error.to_string(),
    };
    (classify_driver(error), message)
}

fn open_failure(error: &OpenError) -> Failure {
    match error {
        OpenError::Driver(e) => driver_failure(e),
        OpenError::Timeout { .. } => (FailureKind::Timeout, error.to_string()),
        OpenError::Tunnel(e) => ssh_failure(e),
        OpenError::Kube(e) => (classify_kube(e), error.to_string()),
        OpenError::PasswordRequired { save_to: Some(_) } => {
            (FailureKind::KeychainEmpty, error.to_string())
        }
        OpenError::PasswordRequired { save_to: None } => {
            (FailureKind::PasswordRequired, error.to_string())
        }
        OpenError::UnknownEngine(_) => (FailureKind::Other, error.to_string()),
        // The test logs in without the session setup; only an opened session runs it.
        OpenError::Setup(_) => (FailureKind::Other, error.to_string()),
    }
}

// --------------------------------------------------------------------------------- the run

/// The steps' state, sending each change to the observer.
struct Run<'a> {
    steps: Mutex<Vec<TestStep>>,
    /// The running step and when it started.
    running: Mutex<Option<(usize, Instant)>>,
    redactor: Arc<Redactor>,
    observer: StepObserver<'a>,
}

impl<'a> Run<'a> {
    fn new(kinds: Vec<StepKind>, redactor: Arc<Redactor>, observer: StepObserver<'a>) -> Self {
        let run = Self {
            steps: Mutex::new(kinds.into_iter().map(TestStep::pending).collect()),
            running: Mutex::new(None),
            redactor,
            observer,
        };
        run.emit();
        run
    }

    /// Starts step `index`; the one running before it finished well. Starting the running
    /// step again changes nothing.
    fn start(&self, index: usize) {
        {
            let mut steps = self.steps.lock().expect("lock poisoned");
            let mut running = self.running.lock().expect("lock poisoned");
            if let Some((current, _)) = *running
                && current == index
            {
                return;
            }
            if let Some((current, since)) = running.take() {
                steps[current].status = StepStatus::Ok;
                steps[current].elapsed = Some(since.elapsed());
            }
            steps[index].status = StepStatus::Running;
            *running = Some((index, Instant::now()));
        }
        self.emit();
    }

    fn ok_running(&self) {
        self.end_running(StepStatus::Ok, None);
    }

    /// The running step failed; the rest are skipped.
    fn fail(&self, (kind, message): Failure) {
        self.end_running(StepStatus::Failed, Some(StepFailure { kind, message }));
    }

    fn stop(&self) {
        self.end_running(StepStatus::Stopped, None);
    }

    fn end_running(&self, status: StepStatus, failure: Option<StepFailure>) {
        {
            let mut steps = self.steps.lock().expect("lock poisoned");
            let mut running = self.running.lock().expect("lock poisoned");
            if let Some((current, since)) = running.take() {
                steps[current].status = status;
                steps[current].elapsed = Some(since.elapsed());
                steps[current].failure = failure;
            } else if let Some(failure) = failure {
                // Nothing was running: it belongs to the first step not done yet.
                if let Some(step) = steps.iter_mut().find(|s| s.status == StepStatus::Pending) {
                    step.status = StepStatus::Failed;
                    step.failure = Some(failure);
                }
            }
            if status != StepStatus::Ok {
                for step in steps.iter_mut() {
                    if step.status == StepStatus::Pending {
                        step.status = StepStatus::Skipped;
                    }
                }
            }
        }
        self.emit();
    }

    /// The steps as they may leave the core: redacted.
    fn snapshot(&self) -> Vec<TestStep> {
        let steps = self.steps.lock().expect("lock poisoned").clone();
        steps.iter().map(|s| self.redactor.step(s)).collect()
    }

    fn emit(&self) {
        (self.observer)(&self.snapshot());
    }

    fn report(
        &self,
        scope: TestScope,
        name: &str,
        server_version: Option<String>,
        stopped: bool,
    ) -> TestReport {
        let steps = self.snapshot();
        let ok = !stopped && steps.iter().all(|s| s.status == StepStatus::Ok);
        let server_version = server_version.map(|v| self.redactor.redact(&v));
        let diagnostics = self.redactor.redact(&diagnostics(
            scope,
            &self.redactor.redact(name),
            ok,
            stopped,
            server_version.as_deref(),
            &steps,
        ));
        TestReport {
            scope,
            ok,
            stopped,
            server_version,
            steps,
            diagnostics,
        }
    }
}

/// The plain text "Copy diagnostics" copies.
fn diagnostics(
    scope: TestScope,
    name: &str,
    ok: bool,
    stopped: bool,
    server_version: Option<&str>,
    steps: &[TestStep],
) -> String {
    let mut out = format!(
        "SpelaionDB connection test (core {}, {} {})\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    if !name.is_empty() {
        out.push_str(&format!("Connection: {name}\n"));
    }
    out.push_str(match scope {
        TestScope::Full => "Scope: full (network and login)\n",
        TestScope::Network => "Scope: network only (no login)\n",
    });
    let result = if stopped {
        "stopped".to_string()
    } else if ok {
        "OK".to_string()
    } else {
        match steps
            .iter()
            .enumerate()
            .find(|(_, s)| s.status == StepStatus::Failed)
        {
            Some((index, _)) => format!("failed at step {}", index + 1),
            None => "failed".to_string(),
        }
    };
    out.push_str(&format!("Result: {result}\n"));
    if let Some(version) = server_version {
        out.push_str(&format!("Server: {version}\n"));
    }
    out.push('\n');
    for (index, step) in steps.iter().enumerate() {
        let status = match step.status {
            StepStatus::Pending => "PENDING",
            StepStatus::Running => "RUNNING",
            StepStatus::Ok => "OK",
            StepStatus::Failed => "FAILED",
            StepStatus::Skipped => "SKIPPED",
            StepStatus::Stopped => "STOPPED",
        };
        out.push_str(&format!("{}. [{status}] {}", index + 1, step.title()));
        if let Some(elapsed) = step.elapsed {
            out.push_str(&format!(" ({})", format_elapsed(elapsed)));
        }
        out.push('\n');
        if let Some(failure) = &step.failure {
            out.push_str(&format!(
                "   Error ({}): {}\n   Hint: {}\n",
                failure.kind.label(),
                failure.message,
                failure.kind.hint()
            ));
        }
    }
    out
}

fn format_elapsed(elapsed: Duration) -> String {
    let ms = elapsed.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", elapsed.as_secs_f64())
    }
}

/// Learns every secret the configuration resolved for a connection.
pub(crate) fn learn_connection_secrets(redactor: &Redactor, resolved: &ResolvedConnection) {
    if let Password::Value(secret) = &resolved.password {
        redactor.add(secret.expose());
    }
    if let Some(password) = &resolved.params.password {
        redactor.add(password.expose());
    }
    if let Some(tunnel) = &resolved.tunnel {
        learn_tunnel_secrets(redactor, tunnel);
    }
}

fn learn_tunnel_secrets(redactor: &Redactor, tunnel: &ResolvedTunnel) {
    let proxy = tunnel.proxy.as_ref().map(|p| &p.password);
    for password in tunnel.hops.iter().map(|h| &h.password).chain(proxy) {
        if let Password::Value(secret) = password {
            redactor.add(secret.expose());
        }
    }
}

/// Passes dialogs through, remembering every answer as a secret to redact.
pub(crate) struct Recording {
    pub(crate) inner: Arc<dyn Interaction>,
    pub(crate) redactor: Arc<Redactor>,
}

#[async_trait]
impl Interaction for Recording {
    async fn confirm_unknown_host(&self, host: &HostInfo) -> bool {
        self.inner.confirm_unknown_host(host).await
    }

    async fn ask_secret(&self, request: &SecretRequest) -> Option<Secret> {
        let answer = self.inner.ask_secret(request).await;
        if let Some(secret) = &answer {
            self.redactor.add(secret.expose());
        }
        answer
    }
}

// ------------------------------------------------------------------------------ redaction

/// Replaces secrets in text: the values it was told about, and anything shaped like a
/// credential (`password=…`, a URL's `user:password@`, an `Authorization` header).
#[derive(Debug, Default)]
pub struct Redactor {
    secrets: Mutex<Vec<String>>,
}

/// What a secret becomes.
pub const REDACTED: &str = "***";

/// Keys whose value is a secret, matched at the end of a name (`PGPASSWORD`, `db_token`).
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "pwd",
    "pass",
    "secret",
    "token",
    "apikey",
    "api_key",
    "access_key",
    "private_key",
    "credentials",
];

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// A secret value to replace wherever it appears.
    pub fn add(&self, secret: &str) {
        if secret.is_empty() || secret == REDACTED {
            return;
        }
        let mut secrets = self.secrets.lock().expect("lock poisoned");
        if !secrets.iter().any(|s| s == secret) {
            secrets.push(secret.to_string());
            // Longest first, so a secret inside another is not replaced halfway.
            secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in self.secrets.lock().expect("lock poisoned").iter() {
            out = replace_secret(&out, secret);
        }
        let out = redact_url_credentials(&out);
        let out = redact_auth_headers(&out);
        redact_key_values(&out)
    }

    fn step(&self, step: &TestStep) -> TestStep {
        let text = |s: &String| self.redact(s);
        let kind = match &step.kind {
            StepKind::Resolve => StepKind::Resolve,
            StepKind::Proxy {
                proxy_type,
                host,
                port,
            } => StepKind::Proxy {
                proxy_type: proxy_type.clone(),
                host: text(host),
                port: *port,
            },
            StepKind::Hop {
                hop,
                user,
                host,
                port,
                command,
            } => StepKind::Hop {
                hop: *hop,
                user: text(user),
                host: text(host),
                port: *port,
                command: *command,
            },
            StepKind::Ssm { target, region } => StepKind::Ssm {
                target: text(target),
                region: region.as_ref().map(text),
            },
            StepKind::Kubernetes {
                resource,
                context,
                namespace,
            } => StepKind::Kubernetes {
                resource: text(resource),
                context: context.as_ref().map(text),
                namespace: namespace.as_ref().map(text),
            },
            StepKind::Target { host, port } => StepKind::Target {
                host: text(host),
                port: *port,
            },
            StepKind::Database {
                engine,
                database,
                user,
                file,
            } => StepKind::Database {
                engine: engine.clone(),
                database: text(database),
                user: text(user),
                file: *file,
            },
        };
        TestStep {
            kind,
            status: step.status,
            failure: step.failure.as_ref().map(|f| StepFailure {
                kind: f.kind,
                message: text(&f.message),
            }),
            elapsed: step.elapsed,
        }
    }
}

/// Every occurrence of `secret`. A very short one only as a whole word, so the rest of the
/// text stays readable.
fn replace_secret(text: &str, secret: &str) -> String {
    if secret.chars().count() >= 4 {
        return text.replace(secret, REDACTED);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(secret) {
        let before = rest[..at].chars().next_back();
        let after = rest[at + secret.len()..].chars().next();
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        out.push_str(&rest[..at]);
        if word(before) || word(after) {
            out.push_str(secret);
        } else {
            out.push_str(REDACTED);
        }
        rest = &rest[at + secret.len()..];
    }
    out.push_str(rest);
    out
}

/// `scheme://user:password@host` → `scheme://***@host`.
fn redact_url_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let start = at + 3;
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || "/?#\"'<>()[]{}".contains(c))
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(user_end) if user_end > 0 => {
                out.push_str(REDACTED);
                out.push_str(&tail[user_end..end]);
            }
            Some(_) | None => out.push_str(&tail[..end]),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `Basic …` and `Bearer …` credentials, as in an `Authorization` header.
fn redact_auth_headers(text: &str) -> String {
    let mut out = text.to_string();
    for scheme in ["basic ", "bearer "] {
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(at) = rest.to_ascii_lowercase().find(scheme) {
            let start = at + scheme.len();
            result.push_str(&rest[..start]);
            let tail = &rest[start..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || "+/=._~-".contains(c)))
                .unwrap_or(tail.len());
            // A long run of token characters; a plain word after "basic" stays.
            if end >= 8 {
                result.push_str(REDACTED);
            } else {
                result.push_str(&tail[..end]);
            }
            rest = &tail[end..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

/// `password=…`, `PGPASSWORD: …`, `"token": "…"` → the value becomes `***`.
fn redact_key_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    // (start, end) of each value to replace, in order.
    let mut values: Vec<(usize, usize)> = Vec::new();
    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(found) = lower[from..].find(key) {
            let key_end = from + found + key.len();
            from = key_end;
            // The key must end a name: `passwords` or `pass_through` do not count.
            if bytes
                .get(key_end)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
            {
                continue;
            }
            let mut i = key_end;
            if bytes.get(i).is_some_and(|b| *b == b'"' || *b == b'\'') {
                i += 1;
            }
            while bytes.get(i).is_some_and(|b| *b == b' ') {
                i += 1;
            }
            if !bytes.get(i).is_some_and(|b| *b == b'=' || *b == b':') {
                continue;
            }
            i += 1;
            while bytes.get(i).is_some_and(|b| *b == b' ') {
                i += 1;
            }
            let quote = bytes.get(i).copied().filter(|b| *b == b'"' || *b == b'\'');
            if quote.is_some() {
                i += 1;
            }
            let start = i;
            while let Some(b) = bytes.get(i) {
                let stop = match quote {
                    Some(q) => *b == q,
                    None => b.is_ascii_whitespace() || b";&,)\"'".contains(b),
                };
                if stop {
                    break;
                }
                i += 1;
            }
            if i > start && &text[start..i] != REDACTED {
                values.push((start, i));
            }
        }
    }
    values.sort_unstable();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, end) in values {
        if start < last {
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(REDACTED);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redact(secrets: &[&str], text: &str) -> String {
        let redactor = Redactor::new();
        for secret in secrets {
            redactor.add(secret);
        }
        redactor.redact(text)
    }

    #[test]
    fn known_secrets_are_replaced_everywhere() {
        assert_eq!(
            redact(&["hunter22"], "auth failed for hunter22 (hunter22)"),
            "auth failed for *** (***)"
        );
        // The longer one first: one inside another is not left half visible.
        assert_eq!(redact(&["abcd", "abcdefgh"], "x abcdefgh y"), "x *** y");
    }

    #[test]
    fn short_secrets_only_as_whole_words() {
        assert_eq!(redact(&["ab"], "ab table ab."), "*** table ***.");
        assert_eq!(redact(&["x"], "x"), "***");
    }

    #[test]
    fn url_credentials_are_removed() {
        assert_eq!(
            redact(&[], "proxy http://bob:s3cr3t@proxy.corp:3128/ said no"),
            "proxy http://***@proxy.corp:3128/ said no"
        );
        assert_eq!(
            redact(&[], "postgres://u:p@db/x and https://example.com/a@b"),
            "postgres://***@db/x and https://example.com/a@b"
        );
    }

    #[test]
    fn credential_shaped_values_are_removed() {
        assert_eq!(
            redact(&[], "PGPASSWORD=abc123 psql; token: zz9, api_key='k-1' ok"),
            "PGPASSWORD=*** psql; token: ***, api_key='***' ok"
        );
        assert_eq!(
            redact(&[], r#"{"password": "p w", "user": "me"}"#),
            r#"{"password": "***", "user": "me"}"#
        );
        assert_eq!(
            redact(
                &[],
                "Proxy-Authorization: Basic cHJveHl1c2VyOnByb3h5LXBhc3M="
            ),
            "Proxy-Authorization: Basic ***"
        );
        // Ordinary words are left alone.
        let plain = "password authentication failed for user \"x\" (tried: key, password)";
        assert_eq!(redact(&[], plain), plain);
    }

    #[test]
    fn hints_and_labels_pass_redaction_unchanged() {
        let kinds = [
            FailureKind::Config,
            FailureKind::PasswordRequired,
            FailureKind::KeychainEmpty,
            FailureKind::Cancelled,
            FailureKind::ProxyAuth,
            FailureKind::SshAuth,
            FailureKind::HostUnreachable,
            FailureKind::HostKeyChanged,
            FailureKind::HostKeyRejected,
            FailureKind::DbAuth,
            FailureKind::DbNotFound,
            FailureKind::Tls,
            FailureKind::Timeout,
            FailureKind::Kubernetes,
            FailureKind::Other,
        ];
        let redactor = Redactor::new();
        for kind in kinds {
            assert_eq!(redactor.redact(kind.hint()), kind.hint());
            assert_eq!(redactor.redact(kind.label()), kind.label());
        }
    }

    #[test]
    fn hints_never_offer_to_accept_a_changed_key() {
        let hint = FailureKind::HostKeyChanged.hint().to_lowercase();
        assert!(hint.contains("known_hosts"), "{hint}");
        for word in ["accept", "trust it", "continue anyway", "click"] {
            assert!(!hint.contains(word), "{word}: {hint}");
        }
    }

    #[test]
    fn database_errors_are_classified_by_code() {
        let db = |code: &str, detail: Option<&str>| DriverError::Database {
            code: Some(code.into()),
            message: "m".into(),
            detail: detail.map(str::to_string),
            hint: None,
            position: None,
        };
        assert_eq!(classify_driver(&db("28P01", None)), FailureKind::DbAuth);
        assert_eq!(classify_driver(&db("3D000", None)), FailureKind::DbNotFound);
        assert_eq!(
            classify_driver(&db("42000", Some("MySQL error 1049"))),
            FailureKind::DbNotFound
        );
        assert_eq!(
            classify_driver(&db("28000", Some("MySQL error 1045"))),
            FailureKind::DbAuth
        );
        assert_eq!(classify_driver(&db("18456", None)), FailureKind::DbAuth);
        assert_eq!(classify_driver(&db("4060", None)), FailureKind::DbNotFound);
        assert_eq!(
            classify_driver(&DriverError::Connection(
                "error performing TLS handshake: invalid peer certificate: UnknownIssuer".into()
            )),
            FailureKind::Tls
        );
        assert_eq!(
            classify_driver(&DriverError::Connection("no response in 15 s".into())),
            FailureKind::Timeout
        );
    }

    #[test]
    fn ssh_errors_are_classified() {
        let host = || "h".to_string();
        assert_eq!(
            classify_ssh(&SshError::Auth {
                hop: 1,
                host: host(),
                user: "u".into(),
                tried: vec![]
            }),
            FailureKind::SshAuth
        );
        assert_eq!(
            classify_ssh(&SshError::HostKeyChanged {
                hop: 0,
                host: host(),
                file: "k".into(),
                line: 1
            }),
            FailureKind::HostKeyChanged
        );
        assert_eq!(
            classify_ssh(&SshError::Proxy {
                proxy: host(),
                message: "m".into(),
                failure: ProxyFailure::Rejected
            }),
            FailureKind::ProxyAuth
        );
        assert_eq!(
            classify_ssh(&SshError::Timeout {
                hop: 0,
                host: host(),
                after: Duration::from_secs(1)
            }),
            FailureKind::Timeout
        );
    }
}
