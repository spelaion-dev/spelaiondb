//! From configuration to parameters ready to connect: variables substituted, paths
//! resolved and secrets read.

use std::path::{Path, PathBuf};

use spelunk_driver_api::{ConnectParams, Secret, TlsMode};

use crate::accounts::{connection_account, tunnel_account};
use crate::draft::ConnectionDraft;
use crate::error::{ConfigError, ResolveError};
use crate::load::{Config, ConnectionEntry, ConnectionId, Source};
use crate::model::{Engine, McpConfig, SecretSpec, SshAuth, TlsModeConfig, TunnelFile};
use crate::secrets::{SecretBackend, SecretRef};
use crate::trust::TrustStore;
use crate::tunnel_draft::TunnelDraft;
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
    /// Driver parameters, without the password (it goes in `password`).
    pub params: ConnectParams,
    pub password: Password,
    pub tunnel: Option<ResolvedTunnel>,
    /// `kubectl port-forward` to this resource (ADR-015); `params.port` is its port.
    pub kubernetes: Option<ResolvedKubernetes>,
    /// A SQLite file on the tunnel's last hop (ADR-018): the program that opens it there.
    /// `params.database` is then its path on that machine.
    pub sqlite3: Option<String>,
    pub mcp: McpConfig,
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

    /// `env = "prod"`: the editor asks before writes here (`guard.rs`).
    pub fn is_production(&self) -> bool {
        self.env
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case("prod") || e.eq_ignore_ascii_case("production"))
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedTunnel {
    pub name: String,
    pub hops: Vec<ResolvedHop>,
    /// The team's `.spelunk/known_hosts` (ADR-022), checked before the user's own files:
    /// only from a trusted repository and only as the user trusted it.
    pub known_hosts: Option<PathBuf>,
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
        let entry = self.config.connection(id).ok_or_else(|| {
            ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
        })?;
        self.resolve_entry(&draft.edited_entry(self.config, entry)?)
    }

    fn resolve_entry(&self, entry: &ConnectionEntry) -> Result<ResolvedConnection, ResolveError> {
        let base = self.base_dir(entry.id.source)?;
        let file = &entry.file;
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
            application_name: Some("SpelunkDB".into()),
            tls: match file.tls.mode {
                TlsModeConfig::Disable => TlsMode::Disable,
                TlsModeConfig::Prefer => TlsMode::Prefer,
                TlsModeConfig::Require => TlsMode::Require,
                TlsModeConfig::VerifyCa => TlsMode::VerifyCa,
                TlsModeConfig::VerifyFull => TlsMode::VerifyFull,
            },
            ca_certificate: text(&file.tls.ca)?.map(|ca| resolve_path(&ca, &base)),
            read_only: file.read_only,
            connect_addr: None,
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
                            format!("tunnel {name:?} does not exist in .spelunk/tunnels/"),
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
            params,
            password,
            tunnel,
            kubernetes,
            sqlite3: match &file.sqlite3 {
                Some(program) => Some(self.expand(&entry.file_path, program)?),
                None => None,
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

    /// The team's `.spelunk/known_hosts`, if the open repository has one, is trusted and the
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
        Ok(ResolvedTunnel {
            name: name.to_string(),
            hops,
            known_hosts: None,
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
                match self.backend.keychain_get(&account).map_err(fail)? {
                    Some(secret) => secret,
                    // First time: ask and offer to store it.
                    None => {
                        return Ok(Password::Prompt {
                            save_to: Some(account),
                        });
                    }
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
