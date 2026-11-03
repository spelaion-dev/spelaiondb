//! SpelunkDB SSH tunnels (ADR-007).
//!
//! A [`Tunnel`] chains SSH sessions (each hop runs over a `direct-tcpip` channel of the
//! previous one) and forwards an ephemeral local port on `127.0.0.1` to the target. Drivers
//! connect to that port, so it works with any engine and with PostgreSQL cancellation,
//! which opens a new connection.
//!
//! Rules this crate guarantees:
//! - A shell is never requested from the server: only `direct-tcpip` channels.
//! - A changed host key is a blocking error ([`SshError::HostKeyChanged`]).
//! - Once down, a tunnel stays in [`TunnelState::Down`]; nothing is ever reopened or re-run
//!   on its own.

mod auth;
mod hostkeys;
mod tunnel;

use std::path::PathBuf;

use async_trait::async_trait;
use spelunk_driver_api::Secret;

pub use hostkeys::KnownHosts;
pub use tunnel::{RemoteExec, RemoteSignals, Tunnel, TunnelState};

/// A tunnel: hops in order. The last one is the one that reaches the target.
#[derive(Debug, Clone)]
pub struct TunnelSpec {
    pub hops: Vec<HopSpec>,
}

#[derive(Debug, Clone)]
pub struct HopSpec {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Methods to try, in order.
    pub auth: Vec<AuthMethod>,
    /// A specific private key. With an agent, only the agent key matching it is tried:
    /// trying them all hits `MaxAuthTries` on strict servers.
    pub identity_file: Option<PathBuf>,
    pub password: Option<Secret>,
    /// External command whose stdin/stdout is the connection to this hop (like `ProxyCommand`;
    /// `%h` and `%p` are substituted). First hop only.
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Agent,
    Key,
    Password,
    KeyboardInteractive,
}

/// Final target of the tunnel, as seen from the last hop.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// Data of a server whose key is not in any `known_hosts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    /// Position of the hop (0-based).
    pub hop: usize,
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    /// `SHA256:…` fingerprint, the same one OpenSSH shows.
    pub fingerprint: String,
}

/// Something to ask the person while the tunnel opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRequest {
    KeyPassphrase {
        hop: usize,
        path: PathBuf,
        retry: bool,
    },
    Password {
        hop: usize,
        user: String,
        host: String,
    },
    /// `keyboard-interactive` question (typically a 2FA code).
    Challenge {
        hop: usize,
        host: String,
        instructions: String,
        prompt: String,
        echo: bool,
    },
}

/// Dialogs a tunnel may need to open. The app shows them; the headless MCP answers no to
/// everything (so an unknown host is rejected).
#[async_trait]
pub trait Interaction: Send + Sync {
    /// Unknown host: `true` to trust it and store its fingerprint.
    async fn confirm_unknown_host(&self, host: &HostInfo) -> bool;
    /// `None` cancels opening the tunnel.
    async fn ask_secret(&self, request: &SecretRequest) -> Option<Secret>;
}

/// Headless: rejects unknown hosts and answers no question.
pub struct NonInteractive;

#[async_trait]
impl Interaction for NonInteractive {
    async fn confirm_unknown_host(&self, _host: &HostInfo) -> bool {
        false
    }

    async fn ask_secret(&self, _request: &SecretRequest) -> Option<Secret> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("hop {} ({host}): could not connect: {message}", hop + 1)]
    Connect {
        hop: usize,
        host: String,
        message: String,
    },

    #[error(
        "hop {} ({host}): THE SERVER KEY HAS CHANGED! This could be an attack. The stored fingerprint is in {}:{line}",
        hop + 1,
        file.display()
    )]
    HostKeyChanged {
        hop: usize,
        host: String,
        file: PathBuf,
        line: usize,
    },

    #[error("hop {} ({host}): unknown server and its fingerprint was not confirmed", hop + 1)]
    HostKeyRejected { hop: usize, host: String },

    #[error("hop {} ({user}@{host}): authentication rejected (tried: {})", hop + 1, tried.join(", "))]
    Auth {
        hop: usize,
        host: String,
        user: String,
        tried: Vec<String>,
    },

    #[error("cancelled")]
    Cancelled,

    #[error("hop {} ({host}): {message}", hop + 1)]
    Hop {
        hop: usize,
        host: String,
        message: String,
    },

    #[error("tunnel: {0}")]
    Other(String),
}
