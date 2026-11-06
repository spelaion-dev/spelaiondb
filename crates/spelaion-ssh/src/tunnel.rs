//! Opening the tunnel, local port forwarding and lifecycle.

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, DisconnectReason, Handle};
use russh::keys::PublicKeyOrCertificate;
use russh::{ChannelMsg, ChannelWriteHalf, Sig};
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::Child;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

use crate::auth::authenticate;
use crate::hostkeys::{self, KnownHosts};
use crate::{
    HopSpec, Interaction, LocalPort, ProxySpec, SecretRequest, SshError, Target, TunnelSpec,
};

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
        if spec.hops.is_empty() && spec.proxy.is_none() {
            return Err(SshError::Other("the tunnel has no hops".into()));
        }
        if spec.proxy.is_some() && spec.hops.first().is_some_and(|h| h.command.is_some()) {
            return Err(SshError::Other(
                "the first hop's command already is the way to it: it cannot go through a proxy too"
                    .into(),
            ));
        }
        if spec.hops.is_empty() {
            return Self::proxy_only(spec, &ui).await;
        }
        if spec.hops.iter().skip(1).any(|h| h.command.is_some()) {
            return Err(SshError::Other(
                "only the first hop can use an external command (command)".into(),
            ));
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
        let config = Arc::new(client::Config {
            keepalive_interval: spec.options.keepalive_interval,
            keepalive_max: usize::try_from(spec.options.keepalive_count).unwrap_or(usize::MAX),
            nodelay: true,
            ..Default::default()
        });
        let known = Arc::new(known_hosts.clone());
        let state = Arc::new(watch::Sender::new(TunnelState::Down {
            reason: "opening".into(),
        }));
        let mut sessions: Vec<Arc<Handle<HopHandler>>> = Vec::new();
        let mut child = None;

        for (hop, hop_spec) in spec.hops.iter().enumerate() {
            let outcome = Arc::new(Mutex::new(None));
            let handler = HopHandler {
                hop,
                host: hop_spec.host.clone(),
                port: hop_spec.port,
                known: known.clone(),
                ui: ui.clone(),
                outcome: outcome.clone(),
                state: state.clone(),
            };
            let timeout = spec.options.connect_timeout;
            let handshake = async {
                match (sessions.last(), &hop_spec.command) {
                    (None, Some(command)) => {
                        let (stream, process, stderr) = spawn_command(command, hop_spec, hop)?;
                        child = Some(process);
                        let result = client::connect_stream(config.clone(), stream, handler).await;
                        Ok(result.map_err(|e| {
                            let stderr = stderr.lock().expect("lock poisoned").trim().to_string();
                            if stderr.is_empty() {
                                e.to_string()
                            } else {
                                format!("{e} — the command said: {stderr}")
                            }
                        }))
                    }
                    (None, None) => match &proxy {
                        // The first hop through the proxy (ADR-027).
                        Some(proxy) => {
                            let stream = crate::proxy::connect(
                                proxy,
                                &hop_spec.host,
                                hop_spec.port,
                                timeout,
                                &ui,
                            )
                            .await?;
                            Ok(client::connect_stream(config.clone(), stream, handler)
                                .await
                                .map_err(|e| e.to_string()))
                        }
                        None => Ok(client::connect(
                            config.clone(),
                            (hop_spec.host.as_str(), hop_spec.port),
                            handler,
                        )
                        .await
                        .map_err(|e| e.to_string())),
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
            let connected = match tokio::time::timeout(timeout, handshake).await {
                Ok(result) => result?,
                Err(_) => Err(format!("no response in {}", seconds(timeout))),
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
            forwards: Mutex::new(Vec::new()),
            forward_gate: Semaphore::new(1),
            primary: std::sync::OnceLock::new(),
            child: Mutex::new(None),
        })
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
        let route = match (&self.proxy_only, self.sessions.last()) {
            (Some((proxy, timeout)), _) => Route::Proxy(proxy.clone(), *timeout),
            (None, Some(last)) => Route::Ssh(last.clone()),
            (None, None) => return Err(SshError::Other("the tunnel has no hops".into())),
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

/// `15 s`, or `1.5 s` for a fraction.
fn seconds(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{secs:.0} s")
    } else {
        format!("{secs} s")
    }
}

type CommandStream = tokio::io::Join<tokio::process::ChildStdout, tokio::process::ChildStdin>;

/// Launches a hop's command (`ProxyCommand`) and uses its stdin/stdout as the connection.
fn spawn_command(
    command: &str,
    spec: &HopSpec,
    hop: usize,
) -> Result<(CommandStream, Child, Arc<Mutex<String>>), SshError> {
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
    if let Some(mut stderr) = child.stderr.take() {
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
        });
    }
    Ok((tokio::io::join(stdout, stdin), child, stderr_text))
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
