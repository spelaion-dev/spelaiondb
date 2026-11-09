//! From configuration to parameters ready to connect: variables substituted, paths
//! resolved and secrets read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use spelaion_driver_api::{ConnectParams, Secret, TlsMode};

use crate::accounts::{connection_account, proxy_account, tunnel_account};
use crate::draft::ConnectionDraft;
use crate::error::{ConfigError, ResolveError};
use crate::load::{Config, ConnectionEntry, ConnectionId, Source};
use crate::model::{
    Engine, IsolationLevel, McpConfig, ProxyType, SecretSpec, SshAuth, TlsModeConfig, TunnelFile,
};
use crate::secrets::{SecretBackend, SecretRef};
use crate::trust::TrustStore;
use crate::tunnel_draft::TunnelDraft;
use crate::types::{ConnectionType, ConnectionTypes, Safeguards};
use crate::vars::{expand, resolve_path};

/// State of a password after resolving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Password {
    None,
    Value(Secret),
    /// It must be asked for. If `save_to` has a value, it is the keychain entry to store it
    /// in if the user asks to remember it.
    Prompt {
        save_to: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub id: ConnectionId,
    pub name: String,
    pub engine: Engine,
    pub env: Option<String>,
    /// The connection type `env` names (ADR-029), with every layer applied; `None` for no
    /// label or a plain one.
    pub kind: Option<ConnectionType>,
    /// Driver parameters, without the password (it goes in `password`).
    pub params: ConnectParams,
    pub password: Password,
    pub tunnel: Option<ResolvedTunnel>,
    /// `kubectl port-forward` to this resource (ADR-015); `params.port` is its port.
    pub kubernetes: Option<ResolvedKubernetes>,
    /// A SQLite file on the tunnel's last hop (ADR-018): the program that opens it there.
    /// `params.database` is then its path on that machine.
    pub sqlite3: Option<String>,
    /// Where the SSH tunnel's local end listens for this connection.
    pub local_port: LocalPort,
    /// Keep the session alive after this long idle (ADR-028); `None`: off.
    pub keepalive: Option<Duration>,
    /// Statements run from the editor are cancelled after this long (ADR-035); `None`: never.
    pub statement_timeout: Option<Duration>,
    /// How every new session starts (ADR-037); the core applies it when a session opens.
    pub session: ResolvedSession,
    pub mcp: McpConfig,
}

/// A connection's `[session]` table, ready to apply (ADR-037).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSession {
    /// The setup script, as written (never expanded).
    pub setup: Option<String>,
    pub ignore_setup_errors: bool,
    /// The default schema, with `${…}` variables expanded.
    pub schema: Option<String>,
    pub isolation: Option<IsolationLevel>,
}

/// The local port of a tunnel's forward, on 127.0.0.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalPort {
    /// Any free port.
    Any,
    /// This one (`local_port`); it fails if it is taken.
    Fixed(u16),
    /// A free one in this range, both ends included (the personal `[tunnels]` setting).
    Range { min: u16, max: u16 },
}

impl ResolvedConnection {
    /// Final parameters. `prompted` is the password typed by the user, if one was asked for.
    pub fn connect_params(&self, prompted: Option<Secret>) -> ConnectParams {
        let mut params = self.params.clone();
        params.password = match &self.password {
            Password::Value(secret) => Some(secret.clone()),
            Password::Prompt { .. } => prompted,
            Password::None => None,
        };
        params
    }

    /// Its type is Production (`env = "prod"` or `"production"`, any case).
    pub fn is_production(&self) -> bool {
        self.kind
            .as_ref()
            .is_some_and(ConnectionType::is_production)
    }

    /// What its type asks for (ADR-029): `guard.rs` and the edit policy apply it.
    pub fn safeguards(&self) -> Safeguards {
        self.kind
            .as_ref()
            .map(|kind| kind.safeguards)
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTunnel {
    pub name: String,
    pub hops: Vec<ResolvedHop>,
    /// The team's `.spelaion/known_hosts` (ADR-022), checked before the user's own files:
    /// only from a trusted repository and only as the user trusted it.
    pub known_hosts: Option<PathBuf>,
    pub options: TunnelSettings,
    /// The first hop is reached through it; with no hops, it reaches the database (ADR-027).
    pub proxy: Option<ResolvedProxy>,
}

#[derive(Debug, Clone)]
pub struct ResolvedProxy {
    pub kind: ProxyType,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    /// `Prompt`: asked when the tunnel opens, if the proxy needs it.
    pub password: Password,
}

/// How a tunnel's sessions behave on the network, with the defaults applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TunnelSettings {
    /// `None`: no keep-alives.
    pub keepalive_interval: Option<Duration>,
    /// 0 never gives up.
    pub keepalive_count: u32,
    pub connect_timeout: Duration,
}

impl TunnelSettings {
    pub fn of(file: &TunnelFile) -> Self {
        let interval = file
            .keepalive_interval
            .unwrap_or(TunnelFile::DEFAULT_KEEPALIVE_INTERVAL);
        Self {
            keepalive_interval: (interval > 0).then(|| Duration::from_secs(interval.into())),
            keepalive_count: file
                .keepalive_count
                .unwrap_or(TunnelFile::DEFAULT_KEEPALIVE_COUNT),
            connect_timeout: Duration::from_secs(
                file.connect_timeout
                    .unwrap_or(TunnelFile::DEFAULT_CONNECT_TIMEOUT)
                    .into(),
            ),
        }
    }
}

/// A connection's Kubernetes resource, ready for `kubectl port-forward` (ADR-015).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKubernetes {
    pub context: Option<String>,
    pub namespace: Option<String>,
    pub resource: String,
    pub kubeconfig: Option<PathBuf>,
    /// `kubectl` unless the tunnel names another executable.
    pub kubectl: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedHop {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: Vec<SshAuth>,
    pub identity_file: Option<PathBuf>,
    pub password: Password,
    pub command: Option<String>,
}

pub struct Resolver<'a> {
    config: &'a Config,
    trust: &'a TrustStore,
    backend: &'a dyn SecretBackend,
}

impl<'a> Resolver<'a> {
    pub fn new(config: &'a Config, trust: &'a TrustStore, backend: &'a dyn SecretBackend) -> Self {
        Self {
            config,
            trust,
            backend,
        }
    }

    /// Resolves a connection. Repository connections require trusting the repository: even
    /// `${env:…}` in the host would be enough to leak a secret through DNS.
    pub fn resolve(&self, id: &ConnectionId) -> Result<ResolvedConnection, ResolveError> {
        let entry = self.config.connection(id).ok_or_else(|| {
            ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
        })?;
        // A saved repository connection must be as the user trusted it (ADR-022). Drafts are
        // what the user is typing: theirs.
        if entry.id.source == Source::Workspace {
            let root = self.base_dir(Source::Workspace)?;
            self.trust
                .check_connection(&root, entry)
                .map_err(|change| ResolveError::Changed { root, change })?;
        }
        self.resolve_entry(entry)
    }

    /// Checks the provenance of a connection editor before using its command fields.
    /// Existing entries must still match the snapshot; a new draft is the user's input.
    pub fn check_editor_access(
        &self,
        source: Source,
        editing: Option<&ConnectionId>,
    ) -> Result<(), ResolveError> {
        let source = editing.map_or(source, |id| id.source);
        if source == Source::Workspace && self.config.workspace_root.is_none() {
            return Err(ConfigError::new(&self.config.user_dir, "no repository is open").into());
        }
        let base = self.base_dir(source)?;
        if let Some(id) = editing {
            let entry = self.config.connection(id).ok_or_else(|| {
                ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
            })?;
            if source == Source::Workspace {
                self.trust
                    .check_connection(&base, entry)
                    .map_err(|change| ResolveError::Changed { root: base, change })?;
            }
        }
        Ok(())
    }

    /// Resolves a connection that has not been saved yet (to test it from the UI). The same
    /// trust rules apply: a repository draft can use the repository's tunnels.
    pub fn resolve_draft(
        &self,
        draft: &ConnectionDraft,
    ) -> Result<ResolvedConnection, ResolveError> {
        self.resolve_entry(&draft.entry(self.config)?)
    }

    /// Resolves the edited form of an existing connection (to test it before saving): same
    /// identifier, so a password stored in the keychain is found even if the name changed.
    pub fn resolve_edit(
        &self,
        id: &ConnectionId,
        draft: &ConnectionDraft,
    ) -> Result<ResolvedConnection, ResolveError> {
        self.check_editor_access(id.source, Some(id))?;
        let entry = self.config.connection(id).ok_or_else(|| {
            ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
        })?;
        self.resolve_entry(&draft.edited_entry(self.config, entry)?)
    }

    fn resolve_entry(&self, entry: &ConnectionEntry) -> Result<ResolvedConnection, ResolveError> {
        let base = self.base_dir(entry.id.source)?;
        self.check_variables(entry)?;
        let file = &entry.file;
        // Before any secret is read: a type the team changed waits for the user's review, and
        // a file that does not load stops every typed connection (ADR-029).
        let kind = ConnectionTypes::load(self.config, self.trust).resolve(file.env.as_deref())?;
        let text = |value: &Option<String>| -> Result<Option<String>, ResolveError> {
            value
                .as_deref()
                .map(|v| self.expand(&entry.file_path, v))
                .transpose()
        };

        let database = text(&file.database)?.unwrap_or_default();
        let kubernetes = file
            .kubernetes
            .as_ref()
            .map(|k| -> Result<ResolvedKubernetes, ResolveError> {
                let field =
                    |v: &Option<String>| text(v).map(|v| v.filter(|v| !v.trim().is_empty()));
                Ok(ResolvedKubernetes {
                    context: field(&k.context)?,
                    namespace: field(&k.namespace)?,
                    resource: self.expand(&entry.file_path, &k.resource)?,
                    kubeconfig: field(&k.kubeconfig)?.map(|f| resolve_path(&f, &base)),
                    kubectl: field(&k.kubectl)?.unwrap_or_else(|| "kubectl".into()),
                })
            })
            .transpose()?;
        let host = match text(&file.host)? {
            Some(host) => host,
            // Through Kubernetes the host only names the server for TLS.
            None if kubernetes.is_some() => "localhost".into(),
            None => String::new(),
        };
        let params = ConnectParams {
            // Set by the core from the tunnel (ADR-018).
            remote: None,
            host,
            port: file.port.or(file.engine.default_port()).unwrap_or(0),
            user: text(&file.user)?.unwrap_or_default(),
            password: None,
            database: match file.engine {
                // On a server (with a tunnel), the path is the server's: kept as written.
                engine if engine.is_file() && database != ":memory:" && file.tunnel.is_none() => {
                    resolve_path(&database, &base).display().to_string()
                }
                _ => database,
            },
            application_name: Some("SpelaionDB".into()),
            tls: match file.tls.mode {
                TlsModeConfig::Disable => TlsMode::Disable,
                TlsModeConfig::Prefer => TlsMode::Prefer,
                TlsModeConfig::Require => TlsMode::Require,
                TlsModeConfig::VerifyCa => TlsMode::VerifyCa,
                TlsModeConfig::VerifyFull => TlsMode::VerifyFull,
            },
            ca_certificate: text(&file.tls.ca)?.map(|ca| resolve_path(&ca, &base)),
            client_certificate: text(&file.tls.cert)?.map(|cert| resolve_path(&cert, &base)),
            client_key: match text(&file.tls.key)? {
                None => None,
                Some(key) => Some(self.private_key(entry, &key, &base)?),
            },
            read_only: file.read_only,
            connect_addr: None,
            connect_timeout: file
                .connect_timeout
                .filter(|s| *s > 0)
                .map(|s| Duration::from_secs(s.into())),
        };
        let password = self.password(
            &entry.file_path,
            file.password.as_ref(),
            &connection_account(self.config, &entry.id),
        )?;

        let tunnel = match &file.tunnel {
            None => None,
            Some(name) => {
                let tunnel = self
                    .config
                    .tunnel_for(entry.id.source, name)
                    .ok_or_else(|| {
                        ConfigError::new(
                            &entry.file_path,
                            format!("tunnel {name:?} does not exist in .spelaion/tunnels/"),
                        )
                    })?;
                // A repository tunnel, even under a draft, must be as the user trusted it.
                if tunnel.source == Source::Workspace {
                    let root = self.base_dir(Source::Workspace)?;
                    self.trust
                        .check_tunnel(&root, tunnel)
                        .map_err(|change| ResolveError::Changed { root, change })?;
                }
                let mut resolved = self.resolve_tunnel(
                    name,
                    tunnel.source,
                    &tunnel.file,
                    &base,
                    &entry.file_path,
                )?;
                // A changed shared file stops the connection until it is reviewed.
                resolved.known_hosts = self.team_known_hosts(true)?;
                Some(resolved)
            }
        };

        Ok(ResolvedConnection {
            id: entry.id.clone(),
            name: entry.name(),
            engine: file.engine,
            env: file.env.clone(),
            kind,
            params,
            password,
            tunnel,
            kubernetes,
            sqlite3: match &file.sqlite3 {
                Some(program) => Some(self.expand(&entry.file_path, program)?),
                None => None,
            },
            local_port: match (file.local_port, self.config.user.tunnels.range()) {
                (Some(port), _) => LocalPort::Fixed(port),
                (None, Ok(Some((min, max)))) => LocalPort::Range { min, max },
                // An invalid range was already reported when loading.
                (None, Ok(None) | Err(_)) => LocalPort::Any,
            },
            keepalive: file
                .keepalive
                .filter(|s| *s > 0)
                .map(|s| Duration::from_secs(s.into())),
            statement_timeout: file
                .statement_timeout
                .filter(|s| *s > 0)
                .map(|s| Duration::from_secs(s.into())),
            session: ResolvedSession {
                setup: file.session.setup.clone(),
                ignore_setup_errors: file.session.ignore_setup_errors,
                schema: text(&file.session.schema)?,
                isolation: file.session.isolation,
            },
            mcp: file.mcp.clone(),
        })
    }

    /// Stores a password typed by the user in the keychain (`Password::Prompt::save_to`).
    pub fn remember_password(&self, account: &str, secret: &Secret) -> Result<(), ResolveError> {
        self.backend
            .keychain_set(account, secret.expose())
            .map_err(|message| ResolveError::Secret {
                reference: format!("keychain:{account}"),
                message,
            })
    }

    /// Every `${user.…}` variable the connection and its tunnel use must be defined; the
    /// missing ones are reported together, for the app to ask for them at once (ADR-022).
    fn check_variables(&self, entry: &ConnectionEntry) -> Result<(), ResolveError> {
        let vars = &self.config.user.vars;
        // The setup is SQL sent as written (ADR-037): a `${…}` in it is not a variable.
        let mut file = entry.file.clone();
        file.session.setup = None;
        let mut texts = crate::vars::text_values(&file);
        if let Some(name) = &entry.file.tunnel
            && let Some(tunnel) = self.config.tunnel_for(entry.id.source, name)
        {
            texts.extend(crate::vars::text_values(&tunnel.file));
        }
        let mut names: Vec<String> = Vec::new();
        for text in &texts {
            for name in crate::vars::missing_user_vars(text, vars) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        if names.is_empty() {
            Ok(())
        } else {
            Err(ResolveError::MissingVariables { names })
        }
    }

    /// A client certificate's private key: a repository connection may not keep it inside the
    /// repository, where it would be committed and shared (ADR-022).
    fn private_key(
        &self,
        entry: &ConnectionEntry,
        key: &str,
        base: &Path,
    ) -> Result<PathBuf, ResolveError> {
        let path = resolve_path(key, base);
        if entry.id.source == Source::Workspace {
            // As written, and canonical when the file exists (another spelling of the same
            // place). A key that does not exist yet is still refused by the first check.
            let inside = |p: &Path| {
                let canonical = |p: &Path| std::fs::canonicalize(p).ok();
                p.starts_with(base)
                    || matches!(
                        (canonical(p), canonical(base)),
                        (Some(p), Some(root)) if p.starts_with(&root)
                    )
            };
            if inside(&path) {
                return Err(ConfigError::new(
                    &entry.file_path,
                    "tls.key is inside the repository, where it would be shared: keep private keys in your personal folder (~/…) or name them with a ${user.…} variable",
                )
                .into());
            }
        }
        Ok(path)
    }

    /// Directory relative paths are resolved against; checks trust.
    fn base_dir(&self, source: Source) -> Result<PathBuf, ResolveError> {
        match source {
            Source::User => Ok(self.config.user_dir.clone()),
            Source::Workspace => {
                let root = self
                    .config
                    .workspace_root
                    .clone()
                    .expect("repository connection without a root");
                if !self.trust.is_trusted(&root) {
                    return Err(ResolveError::Untrusted { root });
                }
                Ok(root)
            }
        }
    }

    /// Resolves a tunnel that has not been saved yet (to test it from the UI). Repository
    /// tunnels require trusting the repository, like their connections.
    pub fn resolve_tunnel_draft(
        &self,
        draft: &TunnelDraft,
    ) -> Result<ResolvedTunnel, ResolveError> {
        let base = self.base_dir(draft.source)?;
        let path = draft.path(self.config)?;
        let file = draft.to_file(self.config)?;
        let mut resolved =
            self.resolve_tunnel(&draft.file_name()?, draft.source, &file, &base, &path)?;
        // Testing a tunnel from its editor: an unreviewed shared file is left out (the hosts
        // are then asked about, as without one), not an error.
        resolved.known_hosts = self.team_known_hosts(false)?;
        Ok(resolved)
    }

    /// The team's `.spelaion/known_hosts`, if the open repository has one, is trusted and the
    /// file is as the user trusted it. `strict`: a changed file is an error to review;
    /// otherwise it is just not used.
    fn team_known_hosts(&self, strict: bool) -> Result<Option<PathBuf>, ResolveError> {
        let Some(root) = self.config.workspace_root.as_deref() else {
            return Ok(None);
        };
        let path = crate::trust::team_known_hosts(root);
        if !path.is_file() || !self.trust.is_trusted(root) {
            return Ok(None);
        }
        match self.trust.check_known_hosts(root) {
            Ok(()) => Ok(Some(path)),
            Err(change) if strict => Err(ResolveError::Changed {
                root: root.to_path_buf(),
                change,
            }),
            Err(_) => Ok(None),
        }
    }

    /// Hops ready to open: variables substituted, key paths resolved against `base` and
    /// passwords read (keychain entries named by [`tunnel_account`]).
    fn resolve_tunnel(
        &self,
        name: &str,
        source: Source,
        file: &TunnelFile,
        base: &Path,
        errors_at: &Path,
    ) -> Result<ResolvedTunnel, ResolveError> {
        let mut hops = Vec::with_capacity(file.hops.len());
        for (i, hop) in file.hops.iter().enumerate() {
            hops.push(ResolvedHop {
                host: self.expand(errors_at, &hop.host)?,
                port: hop.port.unwrap_or(22),
                user: self.expand(errors_at, &hop.user)?,
                auth: hop.auth.clone(),
                identity_file: hop
                    .identity_file
                    .as_deref()
                    .map(|f| self.expand(errors_at, f).map(|f| resolve_path(&f, base)))
                    .transpose()?,
                password: self.password(
                    errors_at,
                    hop.password.as_ref(),
                    &tunnel_account(self.config, source, name, i),
                )?,
                command: hop.command.clone(),
            });
        }
        let proxy = match &file.proxy {
            None => None,
            Some(proxy) => Some(ResolvedProxy {
                kind: proxy.kind,
                host: self.expand(errors_at, &proxy.host)?,
                port: proxy.port,
                user: proxy
                    .user
                    .as_deref()
                    .map(|user| self.expand(errors_at, user))
                    .transpose()?,
                password: self.password(
                    errors_at,
                    proxy.password.as_ref(),
                    &proxy_account(self.config, source, name),
                )?,
            }),
        };
        Ok(ResolvedTunnel {
            name: name.to_string(),
            hops,
            known_hosts: None,
            options: TunnelSettings::of(file),
            proxy,
        })
    }

    /// Substitutes variables; errors point at `file`.
    fn expand(&self, file: &Path, value: &str) -> Result<String, ResolveError> {
        expand(value, &self.config.user.vars, &|name| {
            self.backend.env(name)
        })
        .map_err(|message| ConfigError::new(file, message).into())
    }

    fn password(
        &self,
        file: &Path,
        spec: Option<&SecretSpec>,
        default_account: &str,
    ) -> Result<Password, ResolveError> {
        let Some(spec) = spec else {
            return Ok(Password::None);
        };
        let reference: SecretRef = spec
            .secret
            .parse()
            .map_err(|message: String| ConfigError::new(file, message))?;
        let fail = |message: String| ResolveError::Secret {
            reference: reference.to_string(),
            message,
        };
        let value = match &reference {
            SecretRef::Prompt => return Ok(Password::Prompt { save_to: None }),
            SecretRef::Keychain(name) => {
                let account = name.clone().unwrap_or_else(|| default_account.to_string());
                match self.backend.keychain_get(&account) {
                    Ok(Some(secret)) => secret,
                    // First time: ask and offer to store it.
                    Ok(None) => {
                        return Ok(Password::Prompt {
                            save_to: Some(account),
                        });
                    }
                    // No keychain at all (Linux without a Secret Service): asked like
                    // "prompt", with nowhere to remember it. A keychain that is there but
                    // fails (locked, refused) stays an error.
                    Err(_) if !self.backend.keychain_available() => {
                        return Ok(Password::Prompt { save_to: None });
                    }
                    Err(message) => return Err(fail(message)),
                }
            }
            SecretRef::Env(var) => self
                .backend
                .env(var)
                .ok_or_else(|| fail(format!("environment variable {var} is not set")))?,
            SecretRef::OnePassword(uri) => self.backend.run("op", &["read", uri]).map_err(fail)?,
            SecretRef::Command(command) => self.backend.run_shell(command).map_err(fail)?,
        };
        Ok(Password::Value(Secret::new(value)))
    }
}
