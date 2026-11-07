//! New connections created from the UI: a draft (what the user typed) is validated, turned
//! into a [`ConnectionFile`] and written next to the others. Secrets never reach the file:
//! a password typed for `keychain` goes to the system keychain.

use std::path::{Path, PathBuf};

use spelaion_driver_api::Secret;

use crate::accounts::connection_account;
use crate::edit::create_connection;
use crate::edit::{read_doc, write_doc};
use crate::error::{ConfigError, ResolveError};
use crate::load::{
    Config, ConnectionEntry, ConnectionId, Source, validate_connection, workspace_dir,
};
use crate::model::{
    ConnectionFile, Engine, KubernetesConfig, McpConfig, SCHEMA_VERSION, SecretSpec, TlsConfig,
    TlsModeConfig, WritePolicy,
};
use crate::secrets::{SecretBackend, SecretRef};
use crate::vars::home_dir;

/// Where the password of a new connection comes from. It is never written to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordChoice {
    /// The engine does not need one (SQLite) or the server does not ask for it.
    None,
    /// Stored in the system keychain (see [`connection_account`]).
    Keychain,
    /// Asked every time the connection opens.
    Prompt,
    /// Read from an environment variable when connecting.
    Env(String),
    /// Any other reference already in the file (`keychain:<name>`, `op://…`, `cmd:…`): the
    /// form shows it and editing keeps it as is.
    Other(String),
    /// A short-lived token from a cloud provider's CLI, run on every connection (ADR-022).
    CloudToken(CloudToken),
}

/// Cloud providers whose databases take a short-lived token instead of a password (ADR-022).
/// The token comes from the provider's CLI, signed in on the user's computer, through a
/// `cmd:` reference the form writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudToken {
    /// `aws rds generate-db-auth-token` (IAM database authentication).
    AwsRds,
    /// `az account get-access-token` for Azure Database for PostgreSQL and MySQL.
    AzureAd,
    /// `gcloud sql generate-login-token` (Cloud SQL IAM database authentication).
    GoogleCloudSql,
}

impl CloudToken {
    pub const ALL: [CloudToken; 3] = [
        CloudToken::AwsRds,
        CloudToken::AzureAd,
        CloudToken::GoogleCloudSql,
    ];

    /// The `cmd:` reference that gets a token for `user` on `host:port`. Only letters,
    /// digits, `.`, `-`, `_` and `@` reach the command, so nothing can be injected into the
    /// shell that runs it.
    pub fn reference(self, host: &str, port: u16, user: &str) -> Result<String, String> {
        let safe = |what: &str, value: &str| {
            if !value.is_empty()
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@'))
            {
                Ok(())
            } else {
                Err(format!(
                    "the {what} of a connection with a cloud token can only contain letters, digits, '.', '-', '_' and '@'"
                ))
            }
        };
        match self {
            CloudToken::AwsRds => {
                safe("host", host)?;
                safe("user", user)?;
                // `<name>.<id>.<region>.rds.amazonaws.com`: the region the token is for.
                let labels: Vec<&str> = host.split('.').collect();
                let region = match labels.as_slice() {
                    [.., region, "rds", "amazonaws", "com"] => format!(" --region {region}"),
                    _ => String::new(),
                };
                Ok(format!(
                    "cmd:aws rds generate-db-auth-token --hostname {host} --port {port} --username {user}{region}"
                ))
            }
            CloudToken::AzureAd => Ok(
                "cmd:az account get-access-token --resource-type oss-rdbms --query accessToken --output tsv"
                    .into(),
            ),
            CloudToken::GoogleCloudSql => Ok("cmd:gcloud sql generate-login-token".into()),
        }
    }
}

/// What the user filled in to create a connection. Empty strings mean "not set".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionDraft {
    /// `Workspace` writes to the repository's `.spelaion/` (shared through git); `User`, to
    /// the personal directory.
    pub source: Source,
    /// Optional folders, such as `prod/eu`.
    pub folder: String,
    pub name: String,
    pub engine: Engine,
    pub env: String,
    pub host: String,
    pub port: Option<u16>,
    /// Database name; for SQLite and DuckDB, the file path.
    pub database: String,
    pub user: String,
    pub password: PasswordChoice,
    pub tls_mode: TlsModeConfig,
    pub tls_ca: String,
    /// Client certificate and its private key (ADR-022); empty for none.
    pub tls_cert: String,
    pub tls_key: String,
    pub tunnel: String,
    /// Fixed local port of the SSH tunnel; `None` for a free one.
    pub local_port: Option<u16>,
    /// Through `kubectl port-forward` instead of a tunnel (ADR-015).
    pub kubernetes: Option<KubernetesDraft>,
    /// SQLite on a server: the program that opens the file there; empty for `sqlite3`.
    pub sqlite3: String,
    pub read_only: bool,
    /// Seconds; `None` or 0 for off (ADR-028).
    pub keepalive: Option<u32>,
    pub mcp_exposed: bool,
    /// Agents may write through `execute_write` (ADR-019); otherwise they only read.
    pub mcp_writes: bool,
}

/// The Kubernetes tab of the connection form; empty fields mean the defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KubernetesDraft {
    pub context: String,
    pub namespace: String,
    pub resource: String,
    pub kubeconfig: String,
    pub kubectl: String,
}

/// Common Latin accents to their base letter, so `Análisis` becomes `analisis`.
fn fold_accent(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        other => other,
    }
}

/// File name for a connection name: lowercase ASCII letters, digits, `-` and `_`.
/// `"Orders (prod)"` → `"orders-prod"`, `"Análisis"` → `"analisis"`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        let c = fold_accent(c);
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn valid_env_var(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Directory holding the connections of a layer.
pub fn connections_dir(config: &Config, source: Source) -> Result<PathBuf, ConfigError> {
    match source {
        Source::User => Ok(config.user_dir.join("connections")),
        Source::Workspace => config
            .workspace_root
            .as_ref()
            .map(|root| workspace_dir(root).join("connections"))
            .ok_or_else(|| {
                ConfigError::new(&config.user_dir, "no repository is open to save it in")
            }),
    }
}

/// Paths shared through git must work on every machine: a path inside the repository is
/// stored relative to its root and one inside the home directory as `~/…` (both with `/`).
/// Anything else, and every personal path (`root` is `None`), is kept as typed.
pub(crate) fn portable_path(path: &str, root: Option<&Path>) -> String {
    let Some(root) = root else {
        return path.to_string();
    };
    let absolute = Path::new(path);
    if !absolute.is_absolute() {
        return path.to_string();
    }
    let joined = |relative: &Path| {
        relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    };
    if let Ok(relative) = absolute.strip_prefix(root) {
        return joined(relative);
    }
    if let Some(home) = home_dir()
        && let Ok(relative) = absolute.strip_prefix(&home)
    {
        return format!("~/{}", joined(relative));
    }
    path.to_string()
}

impl ConnectionDraft {
    /// Identifier (and file path without `.toml`): `folder/slug(name)`.
    pub fn id(&self) -> Result<ConnectionId, ConfigError> {
        let file = slug(&self.name);
        if file.is_empty() {
            return Err(ConfigError::new(
                Path::new(&self.name),
                "the name needs at least one letter or digit",
            ));
        }
        let folder = self.folder.trim().trim_matches('/').replace('\\', "/");
        let mut parts = Vec::new();
        for part in folder.split('/').filter(|p| !p.is_empty()) {
            let part = slug(part);
            if part.is_empty() {
                return Err(ConfigError::new(
                    Path::new(&self.folder),
                    format!("invalid folder: {:?}", self.folder),
                ));
            }
            parts.push(part);
        }
        parts.push(file);
        Ok(ConnectionId {
            source: self.source,
            path: parts.join("/"),
        })
    }

    /// The file this draft will be saved as, validated like the ones loaded from disk.
    pub fn to_file(&self, config: &Config) -> Result<ConnectionFile, ConfigError> {
        let id = self.id()?;
        let path = connections_dir(config, self.source)?.join(format!("{}.toml", id.path));
        self.build_file(config, &path)
    }

    /// Like [`Self::to_file`], for a file whose path is already decided (when editing).
    fn build_file(&self, config: &Config, path: &Path) -> Result<ConnectionFile, ConfigError> {
        if non_empty(&self.name).is_none() {
            return Err(ConfigError::new(path, "the name cannot be empty"));
        }
        let network = !self.engine.is_file();
        let password = match &self.password {
            PasswordChoice::None => None,
            PasswordChoice::Keychain => Some("keychain".to_string()),
            PasswordChoice::Prompt => Some("prompt".to_string()),
            PasswordChoice::Env(var) => {
                let var = var.trim();
                if !valid_env_var(var) {
                    return Err(ConfigError::new(
                        path,
                        format!("invalid environment variable name: {var:?}"),
                    ));
                }
                Some(format!("env:{var}"))
            }
            PasswordChoice::Other(reference) => {
                reference
                    .parse::<SecretRef>()
                    .map_err(|message| ConfigError::new(path, message))?;
                Some(reference.clone())
            }
            PasswordChoice::CloudToken(token) => {
                match self.engine {
                    Engine::Postgres | Engine::Mysql | Engine::Mariadb => {}
                    Engine::Sqlserver | Engine::Sqlite | Engine::Duckdb => {
                        return Err(ConfigError::new(
                            path,
                            format!(
                                "cloud tokens are for PostgreSQL, MySQL and MariaDB, not {}",
                                self.engine.as_str()
                            ),
                        ));
                    }
                }
                let port = self.port.or(self.engine.default_port()).unwrap_or_default();
                Some(
                    token
                        .reference(self.host.trim(), port, self.user.trim())
                        .map_err(|message| ConfigError::new(path, message))?,
                )
            }
        };
        let root = match self.source {
            Source::Workspace => config.workspace_root.as_deref(),
            Source::User => None,
        };
        let kubernetes = self
            .kubernetes
            .as_ref()
            .filter(|_| network)
            .map(|k| -> Result<KubernetesConfig, ConfigError> {
                Ok(KubernetesConfig {
                    context: non_empty(&k.context),
                    namespace: non_empty(&k.namespace),
                    resource: non_empty(&k.resource)
                        .ok_or_else(|| ConfigError::new(path, "choose the Kubernetes resource"))?,
                    kubeconfig: non_empty(&k.kubeconfig).map(|f| portable_path(&f, root)),
                    kubectl: non_empty(&k.kubectl),
                })
            })
            .transpose()?;
        // SQLite through a tunnel: the file is on the server (ADR-018).
        let remote_sqlite = self.engine == Engine::Sqlite && non_empty(&self.tunnel).is_some();
        let tunnel =
            non_empty(&self.tunnel).filter(|_| (network || remote_sqlite) && kubernetes.is_none());
        if let Some(name) = &tunnel
            && config.tunnel_for(self.source, name).is_none()
        {
            return Err(ConfigError::new(
                path,
                format!("tunnel {name:?} does not exist"),
            ));
        }
        let file = ConnectionFile {
            schema_version: SCHEMA_VERSION,
            name: non_empty(&self.name),
            engine: self.engine,
            env: non_empty(&self.env),
            host: non_empty(&self.host).filter(|_| network),
            // The engine's default port is implied: the file only says what differs.
            port: self
                .port
                .filter(|p| network && Some(*p) != self.engine.default_port()),
            database: non_empty(&self.database).map(|d| match self.engine {
                // A path on the server is the server's: as typed.
                Engine::Sqlite if remote_sqlite => d,
                Engine::Sqlite | Engine::Duckdb => portable_path(&d, root),
                Engine::Postgres | Engine::Mysql | Engine::Mariadb | Engine::Sqlserver => d,
            }),
            user: non_empty(&self.user).filter(|_| network),
            password: password
                .filter(|_| network)
                .map(|secret| SecretSpec { secret }),
            local_port: self
                .local_port
                .filter(|_| tunnel.is_some() && !remote_sqlite),
            tunnel,
            kubernetes,
            sqlite3: non_empty(&self.sqlite3).filter(|_| remote_sqlite),
            read_only: self.read_only,
            keepalive: self.keepalive.filter(|s| network && *s > 0),
            tls: if network {
                TlsConfig {
                    mode: self.tls_mode,
                    ca: non_empty(&self.tls_ca).map(|ca| portable_path(&ca, root)),
                    cert: non_empty(&self.tls_cert).map(|cert| portable_path(&cert, root)),
                    // `~/…` like other paths, but a key inside the repository stays absolute,
                    // so resolving refuses it instead of sharing it as a relative path.
                    key: non_empty(&self.tls_key).map(|key| {
                        let portable = portable_path(&key, root);
                        if Path::new(&portable).is_relative() && !portable.starts_with("~/") {
                            key
                        } else {
                            portable
                        }
                    }),
                }
            } else {
                TlsConfig::default()
            },
            mcp: McpConfig {
                exposed: self.mcp_exposed,
                writes: if self.mcp_writes {
                    WritePolicy::Allow
                } else {
                    WritePolicy::Deny
                },
                ..McpConfig::default()
            },
        };
        validate_connection(path, file)
    }

    /// An in-memory entry, as if the draft had been saved (to resolve and test it).
    pub(crate) fn entry(&self, config: &Config) -> Result<ConnectionEntry, ConfigError> {
        let id = self.id()?;
        let file_path = connections_dir(config, self.source)?.join(format!("{}.toml", id.path));
        Ok(ConnectionEntry {
            file: self.to_file(config)?,
            id,
            file_path,
        })
    }
}

/// Writes the draft as a new connection file and, for `keychain`, stores `password` in the
/// system keychain. Never overwrites an existing connection (nor its keychain entry).
pub fn add_connection(
    config: &Config,
    draft: &ConnectionDraft,
    password: Option<&Secret>,
    backend: &dyn SecretBackend,
) -> Result<ConnectionId, ResolveError> {
    let entry = draft.entry(config)?;
    let dir = connections_dir(config, draft.source)?;
    let path = create_connection(&dir, &entry.id.path, &entry.file)?;
    if draft.password == PasswordChoice::Keychain
        && !draft.engine.is_file()
        && let Some(secret) = password.filter(|s| !s.expose().is_empty())
    {
        let account = connection_account(config, &entry.id);
        if let Err(message) = backend.keychain_set(&account, secret.expose()) {
            // Without its password the new file would ask for it on first use, which is
            // confusing right after typing it: undo the whole operation instead.
            let _ = std::fs::remove_file(&path);
            return Err(ResolveError::Secret {
                reference: format!("keychain:{account}"),
                message,
            });
        }
    }
    Ok(entry.id)
}

impl ConnectionDraft {
    /// The form for an existing connection, to edit it.
    pub fn from_entry(entry: &ConnectionEntry) -> Self {
        let file = &entry.file;
        let password = match file.password.as_ref().map(|p| p.secret.as_str()) {
            None => PasswordChoice::None,
            Some("keychain") => PasswordChoice::Keychain,
            Some("prompt") => PasswordChoice::Prompt,
            Some(other) => match other.strip_prefix("env:") {
                Some(var) => PasswordChoice::Env(var.to_string()),
                // A command the form wrote for a cloud token is shown as that token again.
                None => {
                    let port = file.port.or(file.engine.default_port()).unwrap_or_default();
                    let host = file.host.clone().unwrap_or_default();
                    let user = file.user.clone().unwrap_or_default();
                    CloudToken::ALL
                        .into_iter()
                        .find(|token| token.reference(&host, port, &user).as_deref() == Ok(other))
                        .map(PasswordChoice::CloudToken)
                        .unwrap_or_else(|| PasswordChoice::Other(other.to_string()))
                }
            },
        };
        let file_name = entry.id.path.rsplit('/').next().unwrap_or_default();
        Self {
            source: entry.id.source,
            folder: entry.folders().join("/"),
            name: file.name.clone().unwrap_or_else(|| file_name.to_string()),
            engine: file.engine,
            env: file.env.clone().unwrap_or_default(),
            host: file.host.clone().unwrap_or_default(),
            port: file.port,
            database: file.database.clone().unwrap_or_default(),
            user: file.user.clone().unwrap_or_default(),
            password,
            tls_mode: file.tls.mode,
            tls_ca: file.tls.ca.clone().unwrap_or_default(),
            tls_cert: file.tls.cert.clone().unwrap_or_default(),
            tls_key: file.tls.key.clone().unwrap_or_default(),
            tunnel: file.tunnel.clone().unwrap_or_default(),
            local_port: file.local_port,
            keepalive: file.keepalive,
            sqlite3: file.sqlite3.clone().unwrap_or_default(),
            kubernetes: file.kubernetes.as_ref().map(|k| KubernetesDraft {
                context: k.context.clone().unwrap_or_default(),
                namespace: k.namespace.clone().unwrap_or_default(),
                resource: k.resource.clone(),
                kubeconfig: k.kubeconfig.clone().unwrap_or_default(),
                kubectl: k.kubectl.clone().unwrap_or_default(),
            }),
            read_only: file.read_only,
            mcp_exposed: file.mcp.exposed,
            mcp_writes: file.mcp.writes == WritePolicy::Allow,
        }
    }
}

impl ConnectionDraft {
    /// The entry an edit would produce: same identifier, file and layer as `entry`.
    pub(crate) fn edited_entry(
        &self,
        config: &Config,
        entry: &ConnectionEntry,
    ) -> Result<ConnectionEntry, ConfigError> {
        let draft = ConnectionDraft {
            source: entry.id.source,
            engine: entry.file.engine,
            ..self.clone()
        };
        Ok(ConnectionEntry {
            id: entry.id.clone(),
            file_path: entry.file_path.clone(),
            file: draft.build_file(config, &entry.file_path)?,
        })
    }
}

fn uses_default_keychain(file: &ConnectionFile) -> bool {
    file.password
        .as_ref()
        .is_some_and(|p| p.secret == "keychain")
}

/// Sets (or removes, with `None`) one key of the document, creating parent tables as needed
/// and dropping tables left empty. The comment on an existing value is kept.
fn put(doc: &mut toml_edit::DocumentMut, key: &[&str], value: Option<toml_edit::Value>) {
    use toml_edit::{Item, Table};
    let Some((last, parents)) = key.split_last() else {
        return;
    };
    let mut table: &mut Table = doc.as_table_mut();
    for part in parents {
        if value.is_none() && table.get(part).and_then(Item::as_table).is_none() {
            return;
        }
        table = table
            .entry(part)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .expect("checked or just created as a table");
    }
    match value {
        Some(value) => match table.get_mut(last).and_then(Item::as_value_mut) {
            Some(existing) => {
                let decor = existing.decor().clone();
                *existing = value;
                *existing.decor_mut() = decor;
            }
            None => {
                table.insert(last, Item::Value(value));
            }
        },
        None => {
            table.remove(last);
        }
    }
    if let Some(parent) = parents.first()
        && doc
            .get(parent)
            .and_then(Item::as_table)
            .is_some_and(Table::is_empty)
    {
        doc.remove(parent);
    }
}

/// A revision identifies the complete connection and the file it was loaded from.
pub fn connection_revision(entry: &ConnectionEntry) -> Result<String, ConfigError> {
    crate::edit::revision(&entry.file_path, &entry.file)
}

/// Saves only if the item still matches the revision used to open the form.
pub fn update_connection_at_revision(
    config: &Config,
    id: &ConnectionId,
    draft: &ConnectionDraft,
    password: Option<&Secret>,
    backend: &dyn SecretBackend,
    revision: &str,
) -> Result<ConnectionFile, ResolveError> {
    let entry = config.connection(id).ok_or_else(|| {
        ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
    })?;
    if connection_revision(entry)? != revision {
        return Err(ConfigError::new(
            &entry.file_path,
            "the connection changed; reopen the editor",
        )
        .into());
    }
    update_connection(config, id, draft, password, backend)
}

/// Saves the edited form over an existing connection, preserving unmanaged keys and comments.
/// A typed keychain password replaces the stored one; a failure restores the file.
pub fn update_connection(
    config: &Config,
    id: &ConnectionId,
    draft: &ConnectionDraft,
    password: Option<&Secret>,
    backend: &dyn SecretBackend,
) -> Result<ConnectionFile, ResolveError> {
    use toml_edit::{InlineTable, Value};
    let entry = config.connection(id).ok_or_else(|| {
        ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
    })?;
    let path = &entry.file_path;
    let new = draft.edited_entry(config, entry)?.file;
    let old = &entry.file;

    let (original, mut doc) = read_doc(path)?;
    let on_disk: ConnectionFile = toml::from_str(&original)
        .map_err(|e| ConfigError::new(path, format!("could not read connection: {e}")))?;
    if &validate_connection(path, on_disk)? != old {
        return Err(ConfigError::new(path, "the connection changed; reopen the editor").into());
    }
    let text = |v: &Option<String>| v.clone().map(Value::from);
    if old.name != new.name {
        put(&mut doc, &["name"], text(&new.name));
    }
    if old.env != new.env {
        put(&mut doc, &["env"], text(&new.env));
    }
    if old.host != new.host {
        put(&mut doc, &["host"], text(&new.host));
    }
    // `None` and the engine's default port mean the same: only a real change is written.
    let port = |p: Option<u16>| p.filter(|p| Some(*p) != new.engine.default_port());
    if port(old.port) != port(new.port) {
        put(
            &mut doc,
            &["port"],
            new.port.map(|p| Value::from(i64::from(p))),
        );
    }
    if old.database != new.database {
        put(&mut doc, &["database"], text(&new.database));
    }
    if old.user != new.user {
        put(&mut doc, &["user"], text(&new.user));
    }
    if old.password != new.password {
        let value = new.password.as_ref().map(|p| {
            let mut table = InlineTable::new();
            table.insert("secret", Value::from(p.secret.as_str()));
            Value::InlineTable(table)
        });
        put(&mut doc, &["password"], value);
    }
    if old.tunnel != new.tunnel {
        put(&mut doc, &["tunnel"], text(&new.tunnel));
    }
    if old.keepalive != new.keepalive {
        put(
            &mut doc,
            &["keepalive"],
            new.keepalive.map(|s| toml_edit::Value::from(i64::from(s))),
        );
    }
    if old.local_port != new.local_port {
        put(
            &mut doc,
            &["local_port"],
            new.local_port.map(|p| toml_edit::Value::from(i64::from(p))),
        );
    }
    if old.sqlite3 != new.sqlite3 {
        put(&mut doc, &["sqlite3"], text(&new.sqlite3));
    }
    if old.kubernetes != new.kubernetes {
        match &new.kubernetes {
            None => {
                doc.as_table_mut().remove("kubernetes");
            }
            Some(k) => {
                put(&mut doc, &["kubernetes", "context"], text(&k.context));
                put(&mut doc, &["kubernetes", "namespace"], text(&k.namespace));
                put(
                    &mut doc,
                    &["kubernetes", "resource"],
                    Some(Value::from(k.resource.as_str())),
                );
                put(&mut doc, &["kubernetes", "kubeconfig"], text(&k.kubeconfig));
                put(&mut doc, &["kubernetes", "kubectl"], text(&k.kubectl));
            }
        }
    }
    if old.read_only != new.read_only {
        put(
            &mut doc,
            &["read_only"],
            new.read_only.then(|| Value::from(true)),
        );
    }
    if old.tls.mode != new.tls.mode {
        let mode = (new.tls.mode != TlsModeConfig::default()).then(|| {
            Value::from(match new.tls.mode {
                TlsModeConfig::Disable => "disable",
                TlsModeConfig::Prefer => "prefer",
                TlsModeConfig::Require => "require",
                TlsModeConfig::VerifyCa => "verify-ca",
                TlsModeConfig::VerifyFull => "verify-full",
            })
        });
        put(&mut doc, &["tls", "mode"], mode);
    }
    if old.tls.ca != new.tls.ca {
        put(&mut doc, &["tls", "ca"], text(&new.tls.ca));
    }
    if old.tls.cert != new.tls.cert {
        put(&mut doc, &["tls", "cert"], text(&new.tls.cert));
    }
    if old.tls.key != new.tls.key {
        put(&mut doc, &["tls", "key"], text(&new.tls.key));
    }
    if old.mcp.exposed != new.mcp.exposed {
        put(
            &mut doc,
            &["mcp", "exposed"],
            new.mcp.exposed.then(|| Value::from(true)),
        );
    }
    // The form only knows allow and deny: a hand-written `confirm` stays until it is changed.
    let allows = |file: &ConnectionFile| file.mcp.writes == WritePolicy::Allow;
    if allows(old) != allows(&new) {
        put(
            &mut doc,
            &["mcp", "writes"],
            allows(&new).then(|| Value::from("allow")),
        );
    }

    // The result must still be a valid connection before it replaces the file.
    let edited = doc.to_string();
    let parsed: ConnectionFile = toml::from_str(&edited)
        .map_err(|e| ConfigError::new(path, format!("the edited file is not valid: {e}")))?;
    let parsed = validate_connection(path, parsed)?;
    write_doc(path, &doc)?;

    let account = connection_account(config, &entry.id);
    let keychain = if uses_default_keychain(&new) {
        match password.filter(|s| !s.expose().is_empty()) {
            Some(secret) => backend.keychain_set(&account, secret.expose()),
            None => Ok(()),
        }
    } else if uses_default_keychain(old) {
        crate::secrets::forget(backend, &account)
    } else {
        Ok(())
    };
    if let Err(message) = keychain {
        let _ = std::fs::write(path, original);
        return Err(ResolveError::Secret {
            reference: format!("keychain:{account}"),
            message,
        });
    }
    Ok(parsed)
}

/// Deletes a connection file, its default keychain entry and the folders it leaves empty.
pub fn delete_connection(
    config: &Config,
    id: &ConnectionId,
    backend: &dyn SecretBackend,
) -> Result<(), ResolveError> {
    let entry = config.connection(id).ok_or_else(|| {
        ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
    })?;
    std::fs::remove_file(&entry.file_path)
        .map_err(|e| ConfigError::new(&entry.file_path, format!("could not delete: {e}")))?;
    let root = connections_dir(config, id.source)?;
    let mut dir = entry.file_path.parent();
    while let Some(current) = dir {
        // `remove_dir` only removes empty directories: it stops at the first one in use.
        if current == root || !current.starts_with(&root) || std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
    if uses_default_keychain(&entry.file) {
        let account = connection_account(config, &entry.id);
        crate::secrets::forget(backend, &account).map_err(|message| ResolveError::Secret {
            reference: format!("keychain:{account}"),
            message,
        })?;
    }
    Ok(())
}
