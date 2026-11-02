//! Opening the tunnel, local port forwarding and lifecycle.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, DisconnectReason, Handle};
use russh::keys::PublicKeyOrCertificate;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::auth::authenticate;
use crate::hostkeys::{self, KnownHosts};
use crate::{HopSpec, Interaction, SshError, Target, TunnelSpec};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// One local port per target.
    forwards: Mutex<HashMap<Target, (SocketAddr, JoinHandle<()>)>>,
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
        if spec.hops.is_empty() {
            return Err(SshError::Other("the tunnel has no hops".into()));
        }
        if spec.hops.iter().skip(1).any(|h| h.command.is_some()) {
            return Err(SshError::Other(
                "only the first hop can use an external command (command)".into(),
            ));
        }
        let config = Arc::new(client::Config {
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 3,
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
            let connected = match (sessions.last(), &hop_spec.command) {
                (None, Some(command)) => {
                    let (stream, process, stderr) = spawn_command(command, hop_spec, hop)?;
                    child = Some(process);
                    let result = client::connect_stream(config.clone(), stream, handler).await;
                    result.map_err(|e| {
                        let stderr = stderr.lock().expect("lock poisoned").trim().to_string();
                        if stderr.is_empty() {
                            e.to_string()
                        } else {
                            format!("{e} — the command said: {stderr}")
                        }
                    })
                }
                (None, None) => tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    client::connect(
                        config.clone(),
                        (hop_spec.host.as_str(), hop_spec.port),
                        handler,
                    ),
                )
                .await
                .map_err(|_| format!("no response in {} s", CONNECT_TIMEOUT.as_secs()))
                .and_then(|r| r.map_err(|e| e.to_string())),
                (Some(previous), _) => match previous
                    .channel_open_direct_tcpip(
                        hop_spec.host.clone(),
                        hop_spec.port.into(),
                        "127.0.0.1",
                        0,
                    )
                    .await
                {
                    Ok(channel) => {
                        client::connect_stream(config.clone(), channel.into_stream(), handler)
                            .await
                            .map_err(|e| e.to_string())
                    }
                    Err(e) => Err(format!("the previous hop could not open the channel: {e}")),
                },
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
            forwards: Mutex::new(HashMap::new()),
            primary: std::sync::OnceLock::new(),
            child: Mutex::new(child),
        })
    }

    /// Local port that leads to `target` through the last session. Reuses the forward if it
    /// already exists: several connections to the same target share the port and sessions.
    pub async fn forward(&self, target: Target) -> Result<SocketAddr, SshError> {
        if let TunnelState::Down { reason } = &*self.state.borrow() {
            return Err(SshError::Other(format!("the tunnel is down: {reason}")));
        }
        if let Some((addr, _)) = self.forwards.lock().expect("lock poisoned").get(&target) {
            return Ok(*addr);
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| SshError::Other(e.to_string()))?;
        let addr = listener
            .local_addr()
            .map_err(|e| SshError::Other(e.to_string()))?;
        let last = self.sessions.last().expect("at least one hop").clone();
        let mut forwards = self.forwards.lock().expect("lock poisoned");
        // Another task may have created the same forward while the port was being opened.
        if let Some((existing, _)) = forwards.get(&target) {
            return Ok(*existing);
        }
        let task = tokio::spawn(forward(
            listener,
            last,
            target.clone(),
            self.state.subscribe(),
        ));
        forwards.insert(target, (addr, task));
        let _ = self.primary.set(addr);
        Ok(addr)
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
        for (_, (_, task)) in self.forwards.lock().expect("lock poisoned").drain() {
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

/// Accepts local connections and carries them to the target over a new channel of the last session.
async fn forward(
    listener: TcpListener,
    last: Arc<Handle<HopHandler>>,
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
        let last = last.clone();
        let target = target.clone();
        tokio::spawn(async move {
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
            // If the target refuses the connection, the local one is closed and the driver sees the error.
        });
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
