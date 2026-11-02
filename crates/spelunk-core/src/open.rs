//! Opening a connection from the configuration: tunnel (if any) + driver + session.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use spelunk_config::{Engine, Password, ResolvedConnection, ResolvedHop, SshAuth};
use spelunk_driver_api::{Driver, DriverError, Secret};
use spelunk_driver_postgres::PostgresDriver;
use spelunk_driver_sqlite::SqliteDriver;
use spelunk_ssh::{
    AuthMethod, HopSpec, Interaction, KnownHosts, SshError, Target, Tunnel, TunnelSpec, TunnelState,
};
use tokio::sync::watch;

use crate::{ResultStore, Session};

/// Available engines, by the name the configuration uses (`engine = "postgres"`).
pub struct Engines {
    drivers: HashMap<&'static str, Arc<dyn Driver>>,
}

impl Engines {
    /// The engines that ship with SpelunkDB.
    pub fn builtin() -> Self {
        let drivers: [Arc<dyn Driver>; 2] =
            [Arc::new(PostgresDriver), Arc::new(SqliteDriver::default())];
        Self {
            drivers: drivers.into_iter().map(|d| (d.engine(), d)).collect(),
        }
    }

    pub fn get(&self, engine: Engine) -> Option<Arc<dyn Driver>> {
        self.drivers.get(engine.as_str()).cloned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The database password must be asked for. If `save_to` has a value, it is the keychain
    /// entry to store it in if the user wants to remember it.
    #[error("the database password is required")]
    PasswordRequired { save_to: Option<String> },

    #[error("engine not available: {0}")]
    UnknownEngine(String),

    #[error("{0}")]
    Tunnel(#[from] SshError),

    #[error("{0}")]
    Driver(#[from] DriverError),
}

/// Open tunnels, shared between connections (ADR-004): two tabs against the same database,
/// or two databases behind the same bastion, use a single SSH chain. It only keeps weak
/// references: the tunnel closes when its last connection closes.
#[derive(Default)]
pub struct TunnelPool {
    tunnels: tokio::sync::Mutex<HashMap<String, Weak<Tunnel>>>,
}

impl TunnelPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Live, active tunnels.
    pub async fn active(&self) -> usize {
        let tunnels = self.tunnels.lock().await;
        tunnels
            .values()
            .filter_map(Weak::upgrade)
            .filter(|t| *t.state().borrow() == TunnelState::Up)
            .count()
    }

    async fn get_or_connect(
        &self,
        key: String,
        spec: &TunnelSpec,
        known_hosts: &KnownHosts,
        ui: Arc<dyn Interaction>,
    ) -> Result<Arc<Tunnel>, SshError> {
        let mut tunnels = self.tunnels.lock().await;
        tunnels.retain(|_, t| t.strong_count() > 0);
        if let Some(tunnel) = tunnels.get(&key).and_then(Weak::upgrade)
            && *tunnel.state().borrow() == TunnelState::Up
        {
            return Ok(tunnel);
        }
        // Down or missing: a new one (a downed tunnel is never reopened on its own).
        let tunnel = Arc::new(Tunnel::connect(spec, known_hosts, ui).await?);
        tunnels.insert(key, Arc::downgrade(&tunnel));
        Ok(tunnel)
    }
}

/// What is needed besides the resolved configuration.
pub struct OpenOptions {
    /// Shared tunnels (one per process: the app, the MCP…).
    pub tunnels: Arc<TunnelPool>,
    pub known_hosts: KnownHosts,
    /// Tunnel dialogs (unknown host, passphrase, 2FA).
    pub interaction: Arc<dyn Interaction>,
    /// Password typed by the user, if `ResolvedConnection::password` is `Prompt`.
    pub password: Option<Secret>,
}

/// An open connection. The tunnel lives as long as the session.
pub struct OpenConnection {
    // Field order = drop order: first the session, then the tunnel.
    pub session: Session,
    pub resolved: ResolvedConnection,
    tunnel: Option<Arc<Tunnel>>,
}

impl OpenConnection {
    /// Tunnel state, if the connection goes through one.
    pub fn tunnel_state(&self) -> Option<watch::Receiver<TunnelState>> {
        self.tunnel.as_ref().map(|t| t.state())
    }

    /// Closes the session and, if it was the last one using the tunnel, the tunnel too.
    pub async fn close(self) {
        let OpenConnection {
            session, tunnel, ..
        } = self;
        drop(session);
        if let Some(tunnel) = tunnel
            && Arc::strong_count(&tunnel) == 1
        {
            tunnel.close().await;
        }
    }
}

/// Opens the connection: if it has a tunnel, opens it first and connects the driver to its
/// local end, keeping the real host name to verify TLS.
pub async fn open_connection(
    resolved: ResolvedConnection,
    engines: &Engines,
    options: OpenOptions,
    results: Arc<ResultStore>,
) -> Result<OpenConnection, OpenError> {
    let driver = engines
        .get(resolved.engine)
        .ok_or_else(|| OpenError::UnknownEngine(resolved.engine.as_str().into()))?;
    if let Password::Prompt { save_to } = &resolved.password
        && options.password.is_none()
    {
        return Err(OpenError::PasswordRequired {
            save_to: save_to.clone(),
        });
    }
    let mut params = resolved.connect_params(options.password);

    let tunnel = match &resolved.tunnel {
        None => None,
        Some(resolved_tunnel) => {
            let spec = TunnelSpec {
                hops: resolved_tunnel.hops.iter().map(hop_spec).collect(),
            };
            // Same layer, name and hops = same tunnel. Secrets are not part of the key
            // (their Debug is `***`).
            let key = format!(
                "{:?}|{}|{:?}",
                resolved.id.source, resolved_tunnel.name, resolved_tunnel.hops
            );
            let tunnel = options
                .tunnels
                .get_or_connect(key, &spec, &options.known_hosts, options.interaction)
                .await?;
            let target = Target {
                host: params.host.clone(),
                port: params.port,
            };
            params.connect_addr = Some(tunnel.forward(target).await?);
            Some(tunnel)
        }
    };

    let connection = driver.connect(&params).await?;
    let session = Session::new(connection, driver.dialect(), results);
    Ok(OpenConnection {
        session,
        resolved,
        tunnel,
    })
}

fn hop_spec(hop: &ResolvedHop) -> HopSpec {
    HopSpec {
        host: hop.host.clone(),
        port: hop.port,
        user: hop.user.clone(),
        auth: hop
            .auth
            .iter()
            .map(|a| match a {
                SshAuth::Agent => AuthMethod::Agent,
                SshAuth::Key => AuthMethod::Key,
                SshAuth::Password => AuthMethod::Password,
                SshAuth::KeyboardInteractive => AuthMethod::KeyboardInteractive,
            })
            .collect(),
        identity_file: hop.identity_file.clone(),
        // `Prompt`: the tunnel will ask for the password through `Interaction` when needed.
        password: match &hop.password {
            Password::Value(secret) => Some(secret.clone()),
            Password::Prompt { .. } | Password::None => None,
        },
        command: hop.command.clone(),
    }
}
