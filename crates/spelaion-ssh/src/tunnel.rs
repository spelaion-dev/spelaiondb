//! Opening the tunnel, local port forwarding and lifecycle.

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::Preferred;
use russh::client::{self, DisconnectReason, Handle};
use russh::keys::{Algorithm, PublicKeyOrCertificate};
use russh::{ChannelMsg, ChannelWriteHalf, Sig};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::Child;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use crate::auth::authenticate;
use crate::hostkeys::{self, KnownHosts};
use crate::ssm::SsmSession;
use crate::{
    HopSpec, HostInfo, Interaction, LocalPort, ProxySpec, SecretRequest, SshError, SsmSpec, Target,
    TunnelSpec, TunnelStage,
};
use spelaion_driver_api::Secret;

/// How many questions a hop's handshake is waiting on a person to answer. The hop's connect
/// timeout only counts the time when there are none (ADR-007): comparing a fingerprint takes
/// as long as it takes, the network does not.
#[derive(Clone)]
struct Asking(Arc<watch::Sender<usize>>);

impl Asking {
    fn new() -> Self {
        Self(Arc::new(watch::Sender::new(0)))
    }

    /// Marks a question as open until the guard is dropped (answered, or the handshake given
    /// up).
    fn open(&self) -> AskingGuard {
        self.0.send_modify(|n| *n += 1);
        AskingGuard(self.0.clone())
    }
}

struct AskingGuard(Arc<watch::Sender<usize>>);

impl Drop for AskingGuard {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// The person, as a hop's handshake sees them: the same questions, with the time spent
/// answering kept out of the connect timeout.
struct Timed {
    inner: Arc<dyn Interaction>,
    asking: Asking,
}

#[async_trait::async_trait]
impl Interaction for Timed {
    async fn confirm_unknown_host(&self, host: &HostInfo) -> bool {
        let _open = self.asking.open();
        self.inner.confirm_unknown_host(host).await
    }

    async fn ask_secret(&self, request: &SecretRequest) -> Option<Secret> {
        let _open = self.asking.open();
        self.inner.ask_secret(request).await
    }
}

/// Runs `work` with at most `limit` of the time no person is being asked anything; `None` if
/// that time ran out first.
async fn within_network_time<T>(
    limit: Duration,
    asking: &Asking,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    let mut remaining = limit;
    let mut questions = asking.0.subscribe();
    tokio::pin!(work);
    loop {
        if *questions.borrow_and_update() > 0 {
            // The clock is stopped while the person answers. `asking` holds a sender, so
            // `changed` only ends when the count changes.
            tokio::select! {
                done = &mut work => return Some(done),
                _ = questions.changed() => continue,
            }
        }
        let started = tokio::time::Instant::now();
        tokio::select! {
            done = &mut work => return Some(done),
            () = tokio::time::sleep(remaining) => return None,
            _ = questions.changed() => {
                remaining = remaining.saturating_sub(started.elapsed());
            }
        }
    }
}

/// A command running on a tunnel's last hop ([`Tunnel::exec`]).
pub struct RemoteExec {
    pub stdin: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    /// Its output and its errors, merged.
    pub stdout: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    pub control: RemoteSignals,
}

/// Signals for a [`RemoteExec`].
#[derive(Clone)]
pub struct RemoteSignals(Arc<ChannelWriteHalf<client::Msg>>);

impl RemoteSignals {
    /// SIGINT, as Ctrl+C would send. Servers may ignore it (OpenSSH honours it).
    pub async fn interrupt(&self) -> Result<(), SshError> {
        self.0
            .signal(Sig::INT)
            .await
            .map_err(|e| SshError::Other(format!("could not interrupt the command: {e}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Up,
    /// Down or closed. It is never reopened on its own: the UI decides whether to reconnect.
    Down {
        reason: String,
    },
}

/// Session handler for a hop: verifies the server key and reports if it goes down.
pub(crate) struct HopHandler {
    hop: usize,
    host: String,
    port: u16,
    known: Arc<KnownHosts>,
    ui: Arc<dyn Interaction>,
    outcome: Arc<Mutex<Option<crate::SshError>>>,
    state: Arc<watch::Sender<TunnelState>>,
}

impl client::Handler for HopHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(hostkeys::verify(
            &self.known,
            &self.ui,
            self.hop,
            &self.host,
            self.port,
            key,
            &self.outcome,
        )
        .await)
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let detail = match &reason {
            DisconnectReason::ReceivedDisconnect(info) => {
                format!("the server closed the session ({info:?})")
            }
            DisconnectReason::Error(e) => e.to_string(),
        };
        self.state.send_if_modified(|state| match state {
            TunnelState::Up => {
                *state = TunnelState::Down {
                    reason: format!("hop {} ({}): {detail}", self.hop + 1, self.host),
                };
                true
            }
            TunnelState::Down { .. } => false,
        });
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(e) => Err(e),
        }
    }
}

/// An open chain of SSH sessions. Forwards to one or more targets are requested on it
/// ([`Tunnel::forward`]) and share the same sessions. Dropping it closes everything.
pub struct Tunnel {
    state: Arc<watch::Sender<TunnelState>>,
    sessions: Vec<Arc<Handle<HopHandler>>>,
    /// A tunnel without hops: the proxy reaches the targets (ADR-027), its password already
    /// asked if it needed one.
    proxy_only: Option<(Arc<ProxySpec>, Duration)>,
    /// A tunnel without hops: AWS SSM reaches the targets, one session per target (ADR-050).
    ssm_only: Option<(Arc<SsmSpec>, Duration)>,
    /// The AWS SSM sessions it started (to the first hop, or to each target), and where each
    /// leads. Stopped with the tunnel.
    ssm_sessions: Mutex<Vec<(Target, SsmSession)>>,
    /// Local ports and the target each leads to. Usually one per target; a connection that
    /// asks for a fixed port may add another.
    forwards: Mutex<Vec<(Target, SocketAddr, JoinHandle<()>)>>,
    forward_gate: Semaphore,
    /// Address of the first forward (the one from [`Tunnel::open`]).
    primary: std::sync::OnceLock<SocketAddr>,
    child: Mutex<Option<Child>>,
}

impl Tunnel {
    /// Shortcut: connects and forwards one target. [`Tunnel::local_addr`] is its local port.
    pub async fn open(
        spec: &TunnelSpec,
        target: Target,
        known_hosts: &KnownHosts,
        ui: Arc<dyn Interaction>,
    ) -> Result<Tunnel, SshError> {
        let tunnel = Self::connect(spec, known_hosts, ui).await?;
        tunnel.forward(target).await?;
        Ok(tunnel)
    }

    /// Opens the chain of sessions hop by hop, without forwarding anything yet.
    pub async fn connect(
        spec: &TunnelSpec,
        known_hosts: &KnownHosts,
        ui: Arc<dyn Interaction>,
    ) -> Result<Tunnel, SshError> {
        Self::connect_observed(spec, known_hosts, ui, &|_| {}).await
    }

    /// Like [`Tunnel::connect`], telling `observe` each part as it starts, so the connection
    /// test can say which one failed (ADR-032). An error belongs to the last part started.
    pub async fn connect_observed(
        spec: &TunnelSpec,
        known_hosts: &KnownHosts,
        ui: Arc<dyn Interaction>,
        observe: &(dyn Fn(TunnelStage) + Send + Sync),
    ) -> Result<Tunnel, SshError> {
        if spec.hops.is_empty() && spec.proxy.is_none() && spec.ssm.is_none() {
            return Err(SshError::Other("the tunnel has no hops".into()));
        }
        if spec.ssm.is_some() && spec.proxy.is_some() {
            return Err(SshError::Other(
                "AWS SSM and a proxy cannot both lead to the first hop".into(),
            ));
        }
        if spec.ssm.is_some() && spec.hops.first().is_some_and(|h| h.command.is_some()) {
            return Err(SshError::Other(
                "the first hop's command already is the way to it: it cannot go through AWS SSM too"
                    .into(),
            ));
        }
        if spec.hops.is_empty() && spec.ssm.is_some() {
            observe(TunnelStage::Ssm);
            return Self::ssm_only(spec);
        }
        if spec.proxy.is_some() && spec.hops.first().is_some_and(|h| h.command.is_some()) {
            return Err(SshError::Other(
                "the first hop's command already is the way to it: it cannot go through a proxy too"
                    .into(),
            ));
        }
        if spec.hops.is_empty() {
            observe(TunnelStage::Proxy);
            return Self::proxy_only(spec, &ui).await;
        }
        if spec.hops.iter().skip(1).any(|h| h.command.is_some()) {
            return Err(SshError::Other(
                "only the first hop can use an external command (command)".into(),
            ));
        }
        if spec.proxy.is_some() {
            observe(TunnelStage::Proxy);
        }
        // Human interaction is not part of the network handshake timeout.
        let mut proxy = spec.proxy.clone();
        if let Some(proxy) = &mut proxy
            && let (Some(user), None) = (&proxy.user, &proxy.password)
        {
            proxy.password = Some(
                ui.ask_secret(&SecretRequest::ProxyPassword {
                    user: user.clone(),
                    host: proxy.host.clone(),
                })
                .await
                .ok_or(SshError::Cancelled)?,
            );
        }
        // Each hop asks its server for the key types `known_hosts` pins for it first.
        let hop_config = |hop_spec: &HopSpec| {
            let pinned = hostkeys::pinned_algorithms(known_hosts, &hop_spec.host, hop_spec.port);
            Arc::new(client::Config {
                keepalive_interval: spec.options.keepalive_interval,
                keepalive_max: usize::try_from(spec.options.keepalive_count).unwrap_or(usize::MAX),
                nodelay: true,
                preferred: Preferred {
                    key: preferred_key_types(&pinned, &Preferred::DEFAULT.key).into(),
                    ..Preferred::DEFAULT
                },
                ..Default::default()
            })
        };
        let known = Arc::new(known_hosts.clone());
        let state = Arc::new(watch::Sender::new(TunnelState::Down {
            reason: "opening".into(),
        }));
        let mut sessions: Vec<Arc<Handle<HopHandler>>> = Vec::new();
        let mut child = None;
        // AWS SSM to the first hop (ADR-050): its session leads there from a local port.
        let mut ssm_sessions = Vec::new();
        let mut ssm_first: Option<SocketAddr> = None;
        if let (Some(ssm), Some(first)) = (&spec.ssm, spec.hops.first()) {
            observe(TunnelStage::Ssm);
            let to = Target {
                host: first.host.clone(),
                port: first.port,
            };
            let down = state.clone();
            let session = SsmSession::start(
                ssm,
                &to.host,
                to.port,
                spec.options.connect_timeout,
                move |reason| ssm_ended(&down, reason),
            )
            .await?;
            ssm_first = Some(session.local);
            ssm_sessions.push((to, session));
        }

        for (hop, hop_spec) in spec.hops.iter().enumerate() {
            // The first hop behind a proxy starts once the proxy has let us through.
            if hop > 0 || proxy.is_none() || ssm_first.is_some() {
                observe(TunnelStage::Hop(hop));
            }
            let config = hop_config(hop_spec);
            let outcome = Arc::new(Mutex::new(None));
            // The questions of this hop's handshake (the unknown-host one) wait for the person:
            // their time is not counted against the connect timeout (ADR-007).
            let asking = Asking::new();
            let timed_ui: Arc<dyn Interaction> = Arc::new(Timed {
                inner: ui.clone(),
                asking: asking.clone(),
            });
            let handler = HopHandler {
                hop,
                host: hop_spec.host.clone(),
                port: hop_spec.port,
                known: known.clone(),
                ui: timed_ui.clone(),
                outcome: outcome.clone(),
                state: state.clone(),
            };
            let timeout = spec.options.connect_timeout;
            let handshake = async {
                match (sessions.last(), &hop_spec.command) {
                    (None, Some(command)) => {
                        let (stream, mut process, stderr) = spawn_command(command, hop_spec, hop)?;
                        match client::connect_stream(config.clone(), stream, handler).await {
                            Ok(session) => {
                                child = Some(process);
                                Ok(Ok(session))
                            }
                            Err(e) => {
                                // A command that failed may have just exited: its error output
                                // is still on its way. Stop it and wait (briefly) for the rest.
                                let _ = process.start_kill();
                                let said = stderr.collect(COMMAND_STDERR_WAIT).await;
                                Ok(Err(if said.is_empty() {
                                    e.to_string()
                                } else {
                                    format!("{e} — the command said: {said}")
                                }))
                            }
                        }
                    }
                    (None, None) => match &proxy {
                        // The first hop through the proxy (ADR-027).
                        Some(proxy) => {
                            let stream = crate::proxy::connect(
                                proxy,
                                &hop_spec.host,
                                hop_spec.port,
                                timeout,
                                &timed_ui,
                            )
                            .await?;
                            observe(TunnelStage::Hop(hop));
                            Ok(client::connect_stream(config.clone(), stream, handler)
                                .await
                                .map_err(|e| e.to_string()))
                        }
                        None => match ssm_first {
                            // Through the AWS SSM session's local end; the host key is still
                            // checked against the hop's own name (ADR-050).
                            Some(local) => Ok(client::connect(config.clone(), local, handler)
                                .await
                                .map_err(|e| e.to_string())),
                            None => Ok(client::connect(
                                config.clone(),
                                (hop_spec.host.as_str(), hop_spec.port),
                                handler,
                            )
                            .await
                            .map_err(|e| e.to_string())),
                        },
                    },
                    (Some(previous), _) => Ok(
                        match previous
                            .channel_open_direct_tcpip(
                                hop_spec.host.clone(),
                                hop_spec.port.into(),
                                "127.0.0.1",
                                0,
                            )
                            .await
                        {
                            Ok(channel) => client::connect_stream(
                                config.clone(),
                                channel.into_stream(),
                                handler,
                            )
                            .await
                            .map_err(|e| e.to_string()),
                            Err(e) => {
                                Err(format!("the previous hop could not open the channel: {e}"))
                            }
                        },
                    ),
                }
            };
            let connected = match within_network_time(timeout, &asking, handshake).await {
                Some(result) => result?,
                None => {
                    return Err(SshError::Timeout {
                        hop,
                        host: hop_spec.host.clone(),
                        after: timeout,
                    });
                }
            };
            let mut session = match connected {
                Ok(session) => session,
                Err(message) => {
                    // The detailed reason (changed key, rejected…) is left by the handler.
                    let detailed = outcome.lock().expect("lock poisoned").take();
                    return Err(detailed.unwrap_or(SshError::Connect {
                        hop,
                        host: hop_spec.host.clone(),
                        message,
                    }));
                }
            };
            authenticate(&mut session, hop, hop_spec, &ui).await?;
            sessions.push(Arc::new(session));
        }

        state.send_replace(TunnelState::Up);
        Ok(Tunnel {
            state,
            sessions,
            proxy_only: None,
            ssm_only: None,
            ssm_sessions: Mutex::new(ssm_sessions),
            forwards: Mutex::new(Vec::new()),
            forward_gate: Semaphore::new(1),
            primary: std::sync::OnceLock::new(),
            child: Mutex::new(child),
        })
    }

    /// A tunnel that is only a proxy: its password is asked now (once), and the proxy is
    /// checked, so a wrong address or password shows here rather than in the driver.
    async fn proxy_only(spec: &TunnelSpec, ui: &Arc<dyn Interaction>) -> Result<Tunnel, SshError> {
        let mut proxy = spec.proxy.clone().expect("a proxy-only tunnel has a proxy");
        let timeout = spec.options.connect_timeout;
        if let (Some(user), None) = (&proxy.user, &proxy.password) {
            proxy.password = Some(
                ui.ask_secret(&SecretRequest::ProxyPassword {
                    user: user.clone(),
                    host: proxy.host.clone(),
                })
                .await
                .ok_or(SshError::Cancelled)?,
            );
        }
        crate::proxy::check(&proxy, timeout).await?;
        Ok(Tunnel {
            state: Arc::new(watch::Sender::new(TunnelState::Up)),
            sessions: Vec::new(),
            proxy_only: Some((Arc::new(proxy), timeout)),
            ssm_only: None,
            ssm_sessions: Mutex::new(Vec::new()),
            forwards: Mutex::new(Vec::new()),
            forward_gate: Semaphore::new(1),
            primary: std::sync::OnceLock::new(),
            child: Mutex::new(None),
        })
    }

    /// A tunnel that is only AWS SSM (ADR-050): each target gets its own session when it is
    /// first forwarded or probed. The tools and the profile's values are checked now, so what
    /// is missing shows here rather than in the driver.
    fn ssm_only(spec: &TunnelSpec) -> Result<Tunnel, SshError> {
        let ssm = spec.ssm.clone().expect("an SSM-only tunnel has SSM");
        crate::ssm::find_tools(&ssm)?;
        crate::ssm::ssm_arguments(&ssm, "localhost", 1, 1)?;
        Ok(Tunnel {
            state: Arc::new(watch::Sender::new(TunnelState::Up)),
            sessions: Vec::new(),
            proxy_only: None,
            ssm_only: Some((Arc::new(ssm), spec.options.connect_timeout)),
            ssm_sessions: Mutex::new(Vec::new()),
            forwards: Mutex::new(Vec::new()),
            forward_gate: Semaphore::new(1),
            primary: std::sync::OnceLock::new(),
            child: Mutex::new(None),
        })
    }

    /// The local end of this SSM-only tunnel's session to `target`, starting it the first time.
    async fn ssm_session(
        &self,
        ssm: &SsmSpec,
        timeout: Duration,
        target: &Target,
    ) -> Result<SocketAddr, SshError> {
        if let Some(local) = self
            .ssm_sessions
            .lock()
            .expect("lock poisoned")
            .iter()
            .find(|(t, _)| t == target)
            .map(|(_, s)| s.local)
        {
            return Ok(local);
        }
        let down = self.state.clone();
        let session = SsmSession::start(ssm, &target.host, target.port, timeout, move |reason| {
            ssm_ended(&down, reason)
        })
        .await?;
        let local = session.local;
        self.ssm_sessions
            .lock()
            .expect("lock poisoned")
            .push((target.clone(), session));
        Ok(local)
    }

    /// Runs `command` on the last hop, in an SSH session channel of its own, for programs
    /// the client talks to through their standard input and output (SQLite on a server,
    /// ADR-018). Its standard error is merged into the output. The bastions of a chain only
    /// forward; the last hop must allow commands, or the output says it refused.
    pub async fn exec(&self, command: &str) -> Result<RemoteExec, SshError> {
        if let TunnelState::Down { reason } = &*self.state.borrow() {
            return Err(SshError::Other(format!("the tunnel is down: {reason}")));
        }
        let Some(session) = self.sessions.last() else {
            return Err(SshError::Other(
                "a tunnel with only a proxy cannot run commands: add an SSH hop".into(),
            ));
        };
        let channel = session
            .channel_open_session()
            .await
            .map_err(|e| SshError::Other(format!("the last hop could not open a session: {e}")))?;
        channel
            .exec(true, command)
            .await
            .map_err(|e| SshError::Other(format!("the last hop did not run the command: {e}")))?;
        let (mut read, write) = channel.split();
        let (mut output, stdout) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(message) = read.wait().await {
                let written = match message {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                        output.write_all(&data).await
                    }
                    ChannelMsg::Failure => {
                        output
                            .write_all(
                                b"
the server refused to run a command (it may allow only port forwarding)
",
                            )
                            .await
                    }
                    ChannelMsg::Close => break,
                    _ => Ok(()),
                };
                if written.is_err() {
                    break;
                }
            }
            // The reader sees the end of the output.
            let _ = output.shutdown().await;
        });
        let write = Arc::new(write);
        Ok(RemoteExec {
            stdin: Box::pin(write.make_writer()),
            stdout: Box::pin(stdout),
            control: RemoteSignals(write),
        })
    }

    /// Checks that the tunnel reaches `target`: opens a connection to it from the last hop
    /// (or through the proxy alone) and closes it at once, within `timeout`. Nothing is sent
    /// to the target. The connection test's "reach the database" step (ADR-032).
    pub async fn probe(&self, target: &Target, timeout: Duration) -> Result<(), SshError> {
        if let TunnelState::Down { reason } = &*self.state.borrow() {
            return Err(SshError::Other(format!("the tunnel is down: {reason}")));
        }
        let failed = |message: String, timed_out: bool| SshError::Target {
            host: target.host.clone(),
            port: target.port,
            message,
            timed_out,
        };
        if let Some((ssm, start)) = &self.ssm_only {
            // The session itself is the check: AWS accepted it and the node forwards.
            let local = self.ssm_session(ssm, *start, target).await?;
            return match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(local)).await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(failed(e.to_string(), false)),
                Err(_) => Err(failed(format!("no answer in {}", seconds(timeout)), true)),
            };
        }
        match (&self.proxy_only, self.sessions.last()) {
            (Some((proxy, _)), _) => {
                // Its password was asked when the tunnel opened.
                let ui: Arc<dyn Interaction> = Arc::new(crate::NonInteractive);
                crate::proxy::connect(proxy, &target.host, target.port, timeout, &ui).await?;
                Ok(())
            }
            (None, Some(last)) => {
                let open = last.channel_open_direct_tcpip(
                    target.host.clone(),
                    target.port.into(),
                    "127.0.0.1",
                    0,
                );
                match tokio::time::timeout(timeout, open).await {
                    Ok(Ok(channel)) => {
                        let _ = channel.close().await;
                        Ok(())
                    }
                    Ok(Err(e)) => Err(failed(e.to_string(), false)),
                    Err(_) => Err(failed(format!("no answer in {}", seconds(timeout)), true)),
                }
            }
            (None, None) => Err(SshError::Other("the tunnel has no hops".into())),
        }
    }

    /// Local port that leads to `target` through the last session, on any free port.
    pub async fn forward(&self, target: Target) -> Result<SocketAddr, SshError> {
        self.forward_on(target, LocalPort::Any).await
    }

    /// Local port that leads to `target` through the last session, where `port` says. Reuses
    /// a forward to the same target on a port `port` accepts: several connections to the
    /// same target share the port and sessions.
    pub async fn forward_on(
        &self,
        target: Target,
        port: LocalPort,
    ) -> Result<SocketAddr, SshError> {
        // Serialize lookup and bind: two tabs opening the same fixed port must share it.
        let _permit = self
            .forward_gate
            .acquire()
            .await
            .expect("forward gate is never closed");
        if let TunnelState::Down { reason } = &*self.state.borrow() {
            return Err(SshError::Other(format!("the tunnel is down: {reason}")));
        }
        if let Some(addr) = self.existing(&target, port) {
            return Ok(addr);
        }
        let listener = listen(port).await?;
        let addr = listener
            .local_addr()
            .map_err(|e| SshError::Other(e.to_string()))?;
        let route = match (&self.ssm_only, &self.proxy_only, self.sessions.last()) {
            (Some((ssm, timeout)), _, _) => {
                Route::Local(self.ssm_session(ssm, *timeout, &target).await?)
            }
            (None, Some((proxy, timeout)), _) => Route::Proxy(proxy.clone(), *timeout),
            (None, None, Some(last)) => Route::Ssh(last.clone()),
            (None, None, None) => return Err(SshError::Other("the tunnel has no hops".into())),
        };
        // A proxy-only HTTP profile cannot authenticate until CONNECT has a target. Check it
        // here, where its error can reach the caller, rather than losing it in the relay task.
        if let Route::Proxy(proxy, timeout) = &route {
            let ui: Arc<dyn Interaction> = Arc::new(crate::NonInteractive);
            crate::proxy::connect(proxy, &target.host, target.port, *timeout, &ui).await?;
        }
        let task = tokio::spawn(forward(
            listener,
            route,
            target.clone(),
            self.state.subscribe(),
        ));
        self.forwards
            .lock()
            .expect("lock poisoned")
            .push((target, addr, task));
        let _ = self.primary.set(addr);
        Ok(addr)
    }

    fn existing(&self, target: &Target, port: LocalPort) -> Option<SocketAddr> {
        self.forwards
            .lock()
            .expect("lock poisoned")
            .iter()
            .find(|(t, addr, _)| t == target && port.accepts(addr.port()))
            .map(|(_, addr, _)| *addr)
    }

    /// Local address of the first forward (the one from [`Tunnel::open`]).
    ///
    /// # Panics
    /// If no forward has been requested yet.
    pub fn local_addr(&self) -> SocketAddr {
        *self
            .primary
            .get()
            .expect("the tunnel does not forward any target yet")
    }

    /// Current state and change notifications.
    pub fn state(&self) -> watch::Receiver<TunnelState> {
        self.state.subscribe()
    }

    /// Closes the tunnel in an orderly way (also for whoever shares it).
    pub async fn close(&self) {
        self.state.send_replace(TunnelState::Down {
            reason: "closed".into(),
        });
        self.abort_forwards();
        for session in self.sessions.iter().rev() {
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "", "es")
                .await;
        }
        let child = self.child.lock().expect("lock poisoned").take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
        let ssm: Vec<(Target, SsmSession)> = self
            .ssm_sessions
            .lock()
            .expect("lock poisoned")
            .drain(..)
            .collect();
        for (_, mut session) in ssm {
            session.close().await;
        }
    }

    fn abort_forwards(&self) {
        for (_, _, task) in self.forwards.lock().expect("lock poisoned").drain(..) {
            task.abort();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.abort_forwards();
        self.state.send_if_modified(|state| match state {
            TunnelState::Up => {
                *state = TunnelState::Down {
                    reason: "closed".into(),
                };
                true
            }
            TunnelState::Down { .. } => false,
        });
        // The external command has kill_on_drop; sessions close when their Handle is dropped.
    }
}

/// Opens the local end of a forward on 127.0.0.1 (never on another address).
async fn listen(port: LocalPort) -> Result<TcpListener, SshError> {
    match port {
        LocalPort::Any => TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| SshError::Other(format!("could not open a local port: {e}"))),
        LocalPort::Fixed(port) => TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| SshError::Other(format!("could not use local port {port}: {e}"))),
        LocalPort::Range { min, max } => {
            for port in min..=max {
                if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
                    return Ok(listener);
                }
            }
            Err(SshError::Other(format!(
                "no free local port from {min} to {max} (the range for tunnels in your settings)"
            )))
        }
    }
}

/// Accepts local connections and carries them to the target over a new channel of the last session.
/// How a forward reaches its target.
#[derive(Clone)]
enum Route {
    /// A `direct-tcpip` channel of the last SSH session.
    Ssh(Arc<Handle<HopHandler>>),
    /// Through the proxy alone (ADR-027).
    Proxy(Arc<ProxySpec>, Duration),
    /// To a local port that leads there: an AWS SSM session's (ADR-050).
    Local(SocketAddr),
}

async fn forward(
    listener: TcpListener,
    route: Route,
    target: Target,
    mut state: watch::Receiver<TunnelState>,
) {
    let local_port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            // If the tunnel goes down, stop listening: the driver gets "connection refused"
            // immediately instead of waiting.
            _ = state.wait_for(|s| matches!(s, TunnelState::Down { .. })) => return,
        };
        let Ok((mut tcp, _)) = accepted else { return };
        let route = route.clone();
        let target = target.clone();
        tokio::spawn(async move {
            match route {
                Route::Ssh(last) => {
                    let channel = last
                        .channel_open_direct_tcpip(
                            target.host,
                            target.port.into(),
                            "127.0.0.1",
                            local_port.into(),
                        )
                        .await;
                    if let Ok(channel) = channel {
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                    }
                }
                Route::Local(local) => {
                    if let Ok(mut stream) = tokio::net::TcpStream::connect(local).await {
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                    }
                }
                Route::Proxy(proxy, timeout) => {
                    // The password was asked when the tunnel opened: nothing to ask here.
                    let ui: Arc<dyn Interaction> = Arc::new(crate::NonInteractive);
                    if let Ok(mut stream) =
                        crate::proxy::connect(&proxy, &target.host, target.port, timeout, &ui).await
                    {
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                    }
                }
            }
            // If the target refuses the connection, the local one is closed and the driver sees the error.
        });
    }
}

/// An SSM session that ended on its own takes the tunnel down with the AWS CLI's message; it
/// is never restarted (ADR-007).
fn ssm_ended(state: &watch::Sender<TunnelState>, reason: String) {
    state.send_if_modified(|current| match current {
        TunnelState::Up => {
            *current = TunnelState::Down {
                reason: format!("the AWS SSM session ended: {reason}"),
            };
            true
        }
        TunnelState::Down { .. } => false,
    });
}

/// `15 s`, or `1.5 s` for a fraction.
pub(crate) fn seconds(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{secs:.0} s")
    } else {
        format!("{secs} s")
    }
}

/// The host key types a hop asks its server for: russh's defaults, with those `known_hosts`
/// pins for the host first. A server usually has several keys and a file often pins one
/// (the one `ssh` first negotiated): asked in the default order, the server would present
/// another, which is a blocking "host key changed" (as OpenSSH does, the pinned types go
/// first). The pinned types keep the defaults' order among themselves, so the server presents
/// the same key as with the defaults whenever that type is pinned. A pinned `ssh-rsa` key is
/// asked with its SHA-2 signatures first. The check itself does not change.
fn preferred_key_types(pinned: &[Algorithm], defaults: &[Algorithm]) -> Vec<Algorithm> {
    let is_pinned = |candidate: &&Algorithm| {
        pinned.iter().any(|p| {
            p == *candidate
                || matches!(
                    (p, candidate),
                    (Algorithm::Rsa { .. }, Algorithm::Rsa { .. })
                )
        })
    };
    let (first, rest): (Vec<&Algorithm>, Vec<&Algorithm>) = defaults.iter().partition(is_pinned);
    first.into_iter().chain(rest).cloned().collect()
}

type CommandStream = tokio::io::Join<tokio::process::ChildStdout, tokio::process::ChildStdin>;

/// How long a failed hop command's error output may take to arrive after it is stopped.
const COMMAND_STDERR_WAIT: Duration = Duration::from_secs(2);

/// A hop command's error output, read as it comes so the command never blocks on it.
struct CommandStderr {
    text: Arc<Mutex<String>>,
    reader: Option<JoinHandle<()>>,
}

impl CommandStderr {
    /// Everything it said, once its output ends or `wait` passes. Read as soon as the
    /// handshake fails, a command that exits at once (`sh` does, faster than `cmd`) would
    /// otherwise have said nothing yet.
    async fn collect(self, wait: Duration) -> String {
        if let Some(reader) = self.reader {
            let _ = tokio::time::timeout(wait, reader).await;
        }
        self.text.lock().expect("lock poisoned").trim().to_string()
    }
}

/// Launches a hop's command (`ProxyCommand`) and uses its stdin/stdout as the connection.
fn spawn_command(
    command: &str,
    spec: &HopSpec,
    hop: usize,
) -> Result<(CommandStream, Child, CommandStderr), SshError> {
    let command = command
        .replace("%h", &spec.host)
        .replace("%p", &spec.port.to_string());
    let mut process = shell_command(&command);
    let mut child = process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| SshError::Connect {
            hop,
            host: spec.host.clone(),
            message: format!("could not launch the command: {e}"),
        })?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    // stderr is kept to explain failures (and must be read so the command does not block).
    let stderr_text = Arc::new(Mutex::new(String::new()));
    let reader = child.stderr.take().map(|mut stderr| {
        let sink = stderr_text.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut text = sink.lock().expect("lock poisoned");
                if text.len() < 16 * 1024 {
                    text.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        })
    });
    Ok((
        tokio::io::join(stdout, stdin),
        child,
        CommandStderr {
            text: stderr_text,
            reader,
        },
    ))
}

/// `sh -c <command>` or, on Windows, `cmd /C <command>`. On Windows the command is passed
/// without re-escaping: Rust escapes quotes as `\"`, which `cmd.exe` does not understand, and
/// a quoted path would end up broken.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C").raw_arg(command);
        process
    }
    #[cfg(not(windows))]
    {
        let mut process = tokio::process::Command::new("sh");
        process.args(["-c", command]);
        process
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{EcdsaCurve, HashAlg};

    const P256: Algorithm = Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP256,
    };

    fn names(types: &[Algorithm]) -> Vec<String> {
        types.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn pinned_key_types_come_first_in_the_default_order() {
        let defaults = &Preferred::DEFAULT.key;
        // Nothing pinned (an unknown host): the defaults, as they are.
        assert_eq!(preferred_key_types(&[], defaults), defaults.to_vec());
        // Only ECDSA pinned: asked first; nothing is left out.
        let ecdsa = preferred_key_types(&[P256], defaults);
        assert_eq!(ecdsa[0], P256);
        assert_eq!(ecdsa.len(), defaults.len());
        // `ssh-rsa` in known_hosts: the same key with SHA-2 signatures first.
        let rsa = preferred_key_types(&[Algorithm::Rsa { hash: None }], defaults);
        assert_eq!(
            names(&rsa[..3]),
            ["rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"]
        );
        assert_eq!(rsa[3], Algorithm::Ed25519);
        // Several pinned: the defaults' order among them, whatever the file's order.
        let both = preferred_key_types(&[Algorithm::Rsa { hash: None }, P256], defaults);
        assert_eq!(both[0], P256);
        assert_eq!(
            both[1],
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha512)
            }
        );
        let with_ed = preferred_key_types(&[P256, Algorithm::Ed25519], defaults);
        assert_eq!(&with_ed[..2], &[Algorithm::Ed25519, P256]);
        // A pinned type russh does not offer is not added.
        let dsa = preferred_key_types(&[Algorithm::Dsa], defaults);
        assert_eq!(dsa, defaults.to_vec());
    }

    /// Work that takes `before` of network time, then asks the person for `answer`, then
    /// takes `after` more.
    async fn work(asking: &Asking, before: Duration, answer: Duration, after: Duration) {
        tokio::time::sleep(before).await;
        {
            let _open = asking.open();
            tokio::time::sleep(answer).await;
        }
        tokio::time::sleep(after).await;
    }

    #[tokio::test]
    async fn the_time_a_person_takes_to_answer_is_not_counted() {
        let asking = Asking::new();
        let limit = Duration::from_millis(300);
        let ms = Duration::from_millis;
        // 150 ms of network time in all, around an answer that takes much longer than the limit.
        let done =
            within_network_time(limit, &asking, work(&asking, ms(100), ms(800), ms(50))).await;
        assert!(done.is_some());
        assert_eq!(*asking.0.borrow(), 0);
    }

    #[tokio::test]
    async fn network_time_still_runs_out_around_an_answer() {
        let asking = Asking::new();
        let limit = Duration::from_millis(300);
        let ms = Duration::from_millis;
        // 200 ms before the question and 200 ms after it: more than the limit in all.
        let started = tokio::time::Instant::now();
        let done =
            within_network_time(limit, &asking, work(&asking, ms(200), ms(500), ms(200))).await;
        assert!(done.is_none());
        // It ran out after the answer: the question's time was not counted, the rest was.
        let elapsed = started.elapsed();
        assert!(elapsed >= ms(800), "{elapsed:?}");
        assert!(elapsed < ms(1500), "{elapsed:?}");
    }

    #[tokio::test]
    async fn work_that_never_ends_times_out_when_nobody_is_asked() {
        let asking = Asking::new();
        let started = tokio::time::Instant::now();
        let done = within_network_time(
            Duration::from_millis(200),
            &asking,
            std::future::pending::<()>(),
        )
        .await;
        assert!(done.is_none());
        assert!(started.elapsed() < Duration::from_millis(1000));
    }

    #[tokio::test]
    async fn an_abandoned_question_does_not_stop_the_clock_for_good() {
        let asking = Asking::new();
        // A handshake given up while asking drops its question with it.
        let open = asking.open();
        drop(open);
        assert_eq!(*asking.0.borrow(), 0);
        let done = within_network_time(
            Duration::from_millis(200),
            &asking,
            std::future::pending::<()>(),
        )
        .await;
        assert!(done.is_none());
    }
}
