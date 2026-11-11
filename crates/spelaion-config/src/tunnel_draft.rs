//! SSH tunnels created and edited from the UI. Like connections: the form is validated
//! into a [`TunnelFile`], secrets go to the keychain (never to the file) and editing only
//! rewrites what changed.

use std::path::{Path, PathBuf};

use spelaion_driver_api::Secret;
use toml_edit::{Array, ArrayOfTables, InlineTable, Item, Table, Value};

use crate::accounts::{proxy_account, tunnel_account};
use crate::atomic_file;
use crate::draft::{PasswordChoice, portable_path, slug};
use crate::edit::{new_file_mode, read_doc, write_doc};
use crate::error::{ConfigError, ResolveError};
use crate::load::{Config, Source, TunnelEntry, validate_tunnel, workspace_dir};
use crate::model::{
    HopConfig, KubernetesProfileConfig, ProxyConfig, ProxyType, SCHEMA_VERSION, SecretSpec,
    SshAuth, TunnelFile,
};
use crate::secrets::{SecretBackend, SecretRef};

/// JSON Schema URL put in the header of new tunnel files.
pub const TUNNEL_SCHEMA_URL: &str = "https://spelaion.dev/schema/db/tunnel.schema.json";

const DEFAULT_SSH_PORT: u16 = 22;

/// One hop as typed in the form. Empty strings mean "not set".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HopDraft {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    /// Methods to try, in order.
    pub auth: Vec<SshAuth>,
    /// Private key; with `agent`, only the agent key matching it is tried.
    pub identity_file: String,
    /// Only used by `password` and `keyboard-interactive`.
    pub password: PasswordChoice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelDraft {
    pub source: Source,
    /// Also the file name (`.spelaion/tunnels/<slug>.toml`), which connections refer to.
    pub name: String,
    /// In order; the last one reaches the database.
    pub hops: Vec<HopDraft>,
    pub options: TunnelDraftOptions,
    /// A SOCKS5 or HTTP proxy in front of the first hop, or instead of hops (ADR-027).
    pub proxy: Option<ProxyDraft>,
    /// A Kubernetes profile (ADR-050): then there are no hops, proxy or SSH settings.
    pub kubernetes: Option<KubernetesProfileDraft>,
}

/// A Kubernetes network profile as typed in the form. Empty strings mean "not set": the
/// connection that uses the profile may set them, or the defaults apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KubernetesProfileDraft {
    pub context: String,
    pub namespace: String,
    pub resource: String,
    pub kubeconfig: String,
    pub kubectl: String,
}

/// The proxy as typed in the form. Empty strings mean "not set".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyDraft {
    pub kind: ProxyType,
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    /// Only used with a user.
    pub password: PasswordChoice,
}

/// Passwords typed in the tunnel form, never part of the draft: `hops[i]` for hop `i`, and
/// the proxy's. Only those whose reference is `keychain` are stored.
#[derive(Default)]
pub struct TunnelPasswords {
    pub hops: Vec<Option<Secret>>,
    pub proxy: Option<Secret>,
}

impl From<Vec<Option<Secret>>> for TunnelPasswords {
    fn from(hops: Vec<Option<Secret>>) -> Self {
        Self { hops, proxy: None }
    }
}

impl TunnelPasswords {
    fn hop(&self, i: usize) -> Option<&Secret> {
        self.hops
            .get(i)
            .and_then(Option::as_ref)
            .filter(|s| !s.expose().is_empty())
    }

    fn proxy(&self) -> Option<&Secret> {
        self.proxy.as_ref().filter(|s| !s.expose().is_empty())
    }
}

fn proxy_uses_keychain(file: &TunnelFile) -> bool {
    file.proxy
        .as_ref()
        .and_then(|p| p.password.as_ref())
        .is_some_and(|p| p.secret == "keychain")
}

/// The form's choice for a password, as the file's reference.
fn choice_of(spec: Option<&SecretSpec>) -> PasswordChoice {
    match spec.map(|p| p.secret.as_str()) {
        None => PasswordChoice::None,
        Some("keychain") => PasswordChoice::Keychain,
        Some("prompt") => PasswordChoice::Prompt,
        Some(other) => match other.strip_prefix("env:") {
            Some(var) => PasswordChoice::Env(var.to_string()),
            None => PasswordChoice::Other(other.to_string()),
        },
    }
}

/// Network settings of the whole tunnel. `None` (or the default value) leaves them out of
/// the file, so the defaults apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TunnelDraftOptions {
    /// Seconds; 0 turns keep-alives off.
    pub keepalive_interval: Option<u32>,
    /// 0 never gives up.
    pub keepalive_count: Option<u32>,
    /// Seconds.
    pub connect_timeout: Option<u32>,
}

impl TunnelDraftOptions {
    fn of(file: &TunnelFile) -> Self {
        Self {
            keepalive_interval: file.keepalive_interval,
            keepalive_count: file.keepalive_count,
            connect_timeout: file.connect_timeout,
        }
    }
}

/// Directory holding the tunnels of a layer.
pub fn tunnels_dir(config: &Config, source: Source) -> Result<PathBuf, ConfigError> {
    match source {
        Source::User => Ok(config.user_dir.join("tunnels")),
        Source::Workspace => config
            .workspace_root
            .as_ref()
            .map(|root| workspace_dir(root).join("tunnels"))
            .ok_or_else(|| {
                ConfigError::new(&config.user_dir, "no repository is open to save it in")
            }),
    }
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn needs_password(auth: &[SshAuth]) -> bool {
    auth.iter()
        .any(|a| matches!(a, SshAuth::Password | SshAuth::KeyboardInteractive))
}

fn uses_keychain(hop: &HopConfig) -> bool {
    hop.password
        .as_ref()
        .is_some_and(|p| p.secret == "keychain")
}

/// Which of a tunnel's `keychain` passwords the keychain holds, never their values: the
/// editor offers to keep a saved password only when there is one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredTunnelPasswords {
    /// One per hop, in order.
    pub hops: Vec<bool>,
    pub proxy: bool,
}

pub fn tunnel_passwords_stored(
    config: &Config,
    entry: &TunnelEntry,
    backend: &dyn SecretBackend,
) -> StoredTunnelPasswords {
    let stored = |account: String| crate::secrets::is_stored(backend, &account);
    StoredTunnelPasswords {
        hops: (entry.file.hops.iter().enumerate())
            .map(|(i, hop)| {
                uses_keychain(hop) && stored(tunnel_account(config, entry.source, &entry.name, i))
            })
            .collect(),
        proxy: proxy_uses_keychain(&entry.file)
            && stored(proxy_account(config, entry.source, &entry.name)),
    }
}

impl TunnelDraft {
    /// Name used for the file and by connections: `slug(name)`.
    pub fn file_name(&self) -> Result<String, ConfigError> {
        let name = slug(&self.name);
        if name.is_empty() {
            return Err(ConfigError::new(
                Path::new(&self.name),
                "the tunnel name needs at least one letter or digit",
            ));
        }
        Ok(name)
    }

    pub(crate) fn path(&self, config: &Config) -> Result<PathBuf, ConfigError> {
        Ok(tunnels_dir(config, self.source)?.join(format!("{}.toml", self.file_name()?)))
    }

    /// The file this draft will be saved as, validated like the ones loaded from disk.
    pub fn to_file(&self, config: &Config) -> Result<TunnelFile, ConfigError> {
        let path = self.path(config)?;
        self.build_file(config, &path)
    }

    fn build_file(&self, config: &Config, path: &Path) -> Result<TunnelFile, ConfigError> {
        // Repository tunnels are shared: key paths must work on every machine.
        let root = match self.source {
            Source::Workspace => config.workspace_root.as_deref(),
            Source::User => None,
        };
        let mut hops = Vec::with_capacity(self.hops.len());
        for (i, hop) in self.hops.iter().enumerate() {
            let at = |field: &str| {
                ConfigError::new(path, format!("hop {}: {field} cannot be empty", i + 1))
            };
            let mut auth = Vec::new();
            for method in &hop.auth {
                if !auth.contains(method) {
                    auth.push(*method);
                }
            }
            if auth.is_empty() {
                return Err(at("the authentication method"));
            }
            let password = if needs_password(&auth) {
                match &hop.password {
                    PasswordChoice::None => None,
                    PasswordChoice::Keychain => Some("keychain".to_string()),
                    PasswordChoice::Prompt => Some("prompt".to_string()),
                    PasswordChoice::Env(var) => Some(format!("env:{}", var.trim())),
                    PasswordChoice::Other(reference) => Some(reference.clone()),
                    PasswordChoice::CloudToken(_) => {
                        return Err(ConfigError::new(
                            path,
                            format!(
                                "hop {}: cloud tokens are for databases, not SSH hops",
                                i + 1
                            ),
                        ));
                    }
                }
            } else {
                None
            };
            if let Some(secret) = &password {
                secret.parse::<SecretRef>().map_err(|message| {
                    ConfigError::new(path, format!("hop {}: {message}", i + 1))
                })?;
            }
            hops.push(HopConfig {
                host: non_empty(&hop.host).ok_or_else(|| at("the host"))?,
                port: hop.port.filter(|p| *p != DEFAULT_SSH_PORT),
                user: non_empty(&hop.user).ok_or_else(|| at("the user"))?,
                auth,
                identity_file: non_empty(&hop.identity_file).map(|f| portable_path(&f, root)),
                password: password.map(|secret| SecretSpec { secret }),
                command: None,
            });
        }
        let proxy = match &self.proxy {
            None => None,
            Some(proxy) => {
                let host = non_empty(&proxy.host)
                    .ok_or_else(|| ConfigError::new(path, "the proxy's host cannot be empty"))?;
                let port = proxy
                    .port
                    .filter(|p| *p > 0)
                    .ok_or_else(|| ConfigError::new(path, "the proxy needs a port"))?;
                let user = non_empty(&proxy.user);
                let password = match (&user, &proxy.password) {
                    (None, _) | (Some(_), PasswordChoice::None) => None,
                    (Some(_), PasswordChoice::Keychain) => Some("keychain".to_string()),
                    (Some(_), PasswordChoice::Prompt) => Some("prompt".to_string()),
                    (Some(_), PasswordChoice::Env(var)) => Some(format!("env:{}", var.trim())),
                    (Some(_), PasswordChoice::Other(reference)) => Some(reference.clone()),
                    (Some(_), PasswordChoice::CloudToken(_)) => {
                        return Err(ConfigError::new(
                            path,
                            "cloud tokens are for databases, not proxies",
                        ));
                    }
                };
                if let Some(secret) = &password {
                    secret
                        .parse::<SecretRef>()
                        .map_err(|message| ConfigError::new(path, format!("proxy: {message}")))?;
                }
                Some(ProxyConfig {
                    kind: proxy.kind,
                    host,
                    port,
                    user,
                    password: password.map(|secret| SecretSpec { secret }),
                })
            }
        };
        if let Some(kubernetes) = &self.kubernetes {
            if !self.hops.is_empty() || self.proxy.is_some() {
                return Err(ConfigError::new(
                    path,
                    "a Kubernetes profile reaches the database through kubectl: it has no hops or proxy",
                ));
            }
            // The form's SSH settings do not apply: they are left out.
            return validate_tunnel(
                path,
                TunnelFile {
                    schema_version: SCHEMA_VERSION,
                    keepalive_interval: None,
                    keepalive_count: None,
                    connect_timeout: None,
                    proxy: None,
                    kubernetes: Some(KubernetesProfileConfig {
                        context: non_empty(&kubernetes.context),
                        namespace: non_empty(&kubernetes.namespace),
                        resource: non_empty(&kubernetes.resource),
                        kubeconfig: non_empty(&kubernetes.kubeconfig)
                            .map(|f| portable_path(&f, root)),
                        kubectl: non_empty(&kubernetes.kubectl),
                    }),
                    hops: Vec::new(),
                },
            );
        }
        let options = &self.options;
        validate_tunnel(
            path,
            TunnelFile {
                schema_version: SCHEMA_VERSION,
                keepalive_interval: options
                    .keepalive_interval
                    .filter(|s| *s != TunnelFile::DEFAULT_KEEPALIVE_INTERVAL),
                keepalive_count: options
                    .keepalive_count
                    .filter(|n| *n != TunnelFile::DEFAULT_KEEPALIVE_COUNT),
                connect_timeout: options
                    .connect_timeout
                    .filter(|s| *s != TunnelFile::DEFAULT_CONNECT_TIMEOUT),
                proxy,
                kubernetes: None,
                hops,
            },
        )
    }

    /// The form for an existing tunnel, to edit it.
    pub fn from_entry(entry: &TunnelEntry) -> Self {
        let hops = entry
            .file
            .hops
            .iter()
            .map(|hop| HopDraft {
                host: hop.host.clone(),
                port: hop.port,
                user: hop.user.clone(),
                auth: hop.auth.clone(),
                identity_file: hop.identity_file.clone().unwrap_or_default(),
                password: match hop.password.as_ref().map(|p| p.secret.as_str()) {
                    None => PasswordChoice::None,
                    Some("keychain") => PasswordChoice::Keychain,
                    Some("prompt") => PasswordChoice::Prompt,
                    Some(other) => match other.strip_prefix("env:") {
                        Some(var) => PasswordChoice::Env(var.to_string()),
                        None => PasswordChoice::Other(other.to_string()),
                    },
                },
            })
            .collect();
        Self {
            source: entry.source,
            name: entry.name.clone(),
            hops,
            options: TunnelDraftOptions::of(&entry.file),
            proxy: entry.file.proxy.as_ref().map(|proxy| ProxyDraft {
                kind: proxy.kind,
                host: proxy.host.clone(),
                port: Some(proxy.port),
                user: proxy.user.clone().unwrap_or_default(),
                password: choice_of(proxy.password.as_ref()),
            }),
            kubernetes: entry
                .file
                .kubernetes
                .as_ref()
                .map(|k| KubernetesProfileDraft {
                    context: k.context.clone().unwrap_or_default(),
                    namespace: k.namespace.clone().unwrap_or_default(),
                    resource: k.resource.clone().unwrap_or_default(),
                    kubeconfig: k.kubeconfig.clone().unwrap_or_default(),
                    kubectl: k.kubectl.clone().unwrap_or_default(),
                }),
        }
    }
}

fn find<'a>(
    config: &'a Config,
    source: Source,
    name: &str,
) -> Result<&'a TunnelEntry, ConfigError> {
    config
        .tunnels
        .iter()
        .find(|t| t.source == source && t.name == name)
        .ok_or_else(|| ConfigError::new(Path::new(name), format!("tunnel {name:?} does not exist")))
}

/// Typed passwords to store: those whose reference is `keychain` and that got a non-empty one.
fn store_passwords(
    config: &Config,
    source: Source,
    name: &str,
    file: &TunnelFile,
    passwords: &TunnelPasswords,
    backend: &dyn SecretBackend,
) -> Result<Vec<String>, ResolveError> {
    let set = |account: String, secret: &Secret| {
        crate::secrets::store(backend, &account, secret.expose())
            .map_err(|message| ResolveError::Secret {
                reference: format!("keychain:{account}"),
                message,
            })
            .map(|()| account)
    };
    let mut stored = Vec::new();
    for (i, hop) in file.hops.iter().enumerate() {
        if let Some(secret) = passwords.hop(i)
            && uses_keychain(hop)
        {
            stored.push(set(tunnel_account(config, source, name, i), secret)?);
        }
    }
    if let Some(secret) = passwords.proxy()
        && proxy_uses_keychain(file)
    {
        stored.push(set(proxy_account(config, source, name), secret)?);
    }
    Ok(stored)
}

/// Writes a new tunnel file and stores the typed `keychain` passwords. Never overwrites an
/// existing tunnel; if the keychain fails, nothing is left.
pub fn add_tunnel(
    config: &Config,
    draft: &TunnelDraft,
    passwords: &TunnelPasswords,
    backend: &dyn SecretBackend,
) -> Result<String, ResolveError> {
    let name = draft.file_name()?;
    let path = draft.path(config)?;
    let file = draft.build_file(config, &path)?;
    if path.exists() {
        return Err(ConfigError::new(&path, "a tunnel with that name already exists").into());
    }
    let body = toml::to_string_pretty(&file).map_err(|e| ConfigError::new(&path, e.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
    }
    let content = format!("#:schema {TUNNEL_SCHEMA_URL}\n\n{body}");
    atomic_file::create_new(&path, content.as_bytes(), new_file_mode(draft.source)).map_err(
        |e| {
            let message = match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    "a tunnel with that name already exists".to_string()
                }
                _ => format!("could not write: {e}"),
            };
            ConfigError::new(&path, message)
        },
    )?;
    if let Err(e) = store_passwords(config, draft.source, &name, &file, passwords, backend) {
        let _ = std::fs::remove_file(&path);
        for i in 0..file.hops.len() {
            let _ = backend.keychain_delete(&tunnel_account(config, draft.source, &name, i));
        }
        let _ = backend.keychain_delete(&proxy_account(config, draft.source, &name));
        return Err(e);
    }
    Ok(name)
}

/// Sets (or removes) a key of a table, keeping the comment of an existing value.
fn put(table: &mut Table, key: &str, value: Option<Value>) {
    match value {
        Some(value) => match table.get_mut(key).and_then(Item::as_value_mut) {
            Some(existing) => {
                let decor = existing.decor().clone();
                *existing = value;
                *existing.decor_mut() = decor;
            }
            None => {
                table.insert(key, Item::Value(value));
            }
        },
        None => {
            table.remove(key);
        }
    }
}

fn auth_value(auth: &[SshAuth]) -> Value {
    let mut array = Array::new();
    for method in auth {
        array.push(match method {
            SshAuth::Agent => "agent",
            SshAuth::Key => "key",
            SshAuth::Password => "password",
            SshAuth::KeyboardInteractive => "keyboard-interactive",
        });
    }
    Value::Array(array)
}

fn secret_value(spec: &Option<SecretSpec>) -> Option<Value> {
    spec.as_ref().map(|p| {
        let mut table = InlineTable::new();
        table.insert("secret", Value::from(p.secret.as_str()));
        Value::InlineTable(table)
    })
}

/// Writes the changed keys of hop `new` into `table`, comparing with `old` (`None` for a hop
/// that did not exist). `command` is never touched: the form does not manage it.
fn write_hop(table: &mut Table, old: Option<&HopConfig>, new: &HopConfig) {
    let text = |v: &Option<String>| v.clone().map(Value::from);
    if old.map(|o| &o.host) != Some(&new.host) {
        put(table, "host", Some(Value::from(new.host.as_str())));
    }
    let port = |p: Option<u16>| p.filter(|p| *p != DEFAULT_SSH_PORT);
    if old.map(|o| port(o.port)) != Some(port(new.port)) {
        put(
            table,
            "port",
            port(new.port).map(|p| Value::from(i64::from(p))),
        );
    }
    if old.map(|o| &o.user) != Some(&new.user) {
        put(table, "user", Some(Value::from(new.user.as_str())));
    }
    if old.map(|o| &o.auth) != Some(&new.auth) {
        put(table, "auth", Some(auth_value(&new.auth)));
    }
    if old.map(|o| &o.identity_file) != Some(&new.identity_file) {
        put(table, "identity_file", text(&new.identity_file));
    }
    if old.map(|o| &o.password) != Some(&new.password) {
        put(table, "password", secret_value(&new.password));
    }
}

/// Writes the changed tunnel-wide settings (they come before the `[[hops]]` tables).
fn write_options(table: &mut Table, old: &TunnelFile, new: &TunnelFile) {
    let number = |v: Option<u32>| v.map(|v| Value::from(i64::from(v)));
    if old.keepalive_interval != new.keepalive_interval {
        put(table, "keepalive_interval", number(new.keepalive_interval));
    }
    if old.keepalive_count != new.keepalive_count {
        put(table, "keepalive_count", number(new.keepalive_count));
    }
    if old.connect_timeout != new.connect_timeout {
        put(table, "connect_timeout", number(new.connect_timeout));
    }
    if old.proxy != new.proxy {
        match &new.proxy {
            None => {
                table.remove("proxy");
            }
            Some(proxy) => {
                let item = table
                    .entry("proxy")
                    .or_insert_with(|| Item::Table(Table::new()));
                if let Some(section) = item.as_table_mut() {
                    let text = |v: &Option<String>| v.clone().map(Value::from);
                    let kind = match proxy.kind {
                        ProxyType::Socks5 => "socks5",
                        ProxyType::Http => "http",
                    };
                    put(section, "type", Some(Value::from(kind)));
                    put(section, "host", Some(Value::from(proxy.host.as_str())));
                    put(section, "port", Some(Value::from(i64::from(proxy.port))));
                    put(section, "user", text(&proxy.user));
                    put(section, "password", secret_value(&proxy.password));
                }
            }
        }
    }
    if old.kubernetes != new.kubernetes {
        match &new.kubernetes {
            None => {
                table.remove("kubernetes");
            }
            Some(kubernetes) => {
                let item = table
                    .entry("kubernetes")
                    .or_insert_with(|| Item::Table(Table::new()));
                if let Some(section) = item.as_table_mut() {
                    let text = |v: &Option<String>| v.clone().map(Value::from);
                    put(section, "context", text(&kubernetes.context));
                    put(section, "namespace", text(&kubernetes.namespace));
                    put(section, "resource", text(&kubernetes.resource));
                    put(section, "kubeconfig", text(&kubernetes.kubeconfig));
                    put(section, "kubectl", text(&kubernetes.kubectl));
                }
            }
        }
    }
}

/// A revision identifies the complete tunnel and the file it was loaded from.
pub fn tunnel_revision(entry: &TunnelEntry) -> Result<String, ConfigError> {
    crate::edit::revision(&entry.file_path, &entry.file)
}

/// Saves only if the item still matches the revision used to open the form.
pub fn update_tunnel_at_revision(
    config: &Config,
    source: Source,
    name: &str,
    draft: &TunnelDraft,
    passwords: &TunnelPasswords,
    backend: &dyn SecretBackend,
    revision: &str,
) -> Result<TunnelFile, ResolveError> {
    let entry = find(config, source, name)?;
    if tunnel_revision(entry)? != revision {
        return Err(
            ConfigError::new(&entry.file_path, "the tunnel changed; reopen the editor").into(),
        );
    }
    update_tunnel(config, source, name, draft, passwords, backend)
}

/// Saves edited hops by position, preserving comments and `command`. A stored password is
/// forgotten when its hop stops using the keychain or points to another server.
pub fn update_tunnel(
    config: &Config,
    source: Source,
    name: &str,
    draft: &TunnelDraft,
    passwords: &TunnelPasswords,
    backend: &dyn SecretBackend,
) -> Result<TunnelFile, ResolveError> {
    let entry = find(config, source, name)?;
    let path = &entry.file_path;
    let draft = TunnelDraft {
        source,
        ..draft.clone()
    };
    let new = draft.build_file(config, path)?;
    let old = &entry.file;

    let (original, mut doc) = read_doc(path)?;
    write_options(doc.as_table_mut(), old, &new);
    let on_disk: TunnelFile = toml::from_str(&original)
        .map_err(|e| ConfigError::new(path, format!("could not read tunnel: {e}")))?;
    if &validate_tunnel(path, on_disk)? != old {
        return Err(ConfigError::new(path, "the tunnel changed; reopen the editor").into());
    }
    {
        let hops = doc
            .entry("hops")
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .ok_or_else(|| ConfigError::new(path, "hops must be a list of [[hops]] tables"))?;
        for (i, hop) in new.hops.iter().enumerate() {
            match hops.get_mut(i) {
                Some(table) => write_hop(table, old.hops.get(i), hop),
                None => {
                    let mut table = Table::new();
                    write_hop(&mut table, None, hop);
                    hops.push(table);
                }
            }
        }
        while hops.len() > new.hops.len() {
            hops.remove(hops.len() - 1);
        }
    }
    let edited = doc.to_string();
    let parsed: TunnelFile = toml::from_str(&edited)
        .map_err(|e| ConfigError::new(path, format!("the edited file is not valid: {e}")))?;
    let parsed = validate_tunnel(path, parsed)?;
    write_doc(path, &doc, new_file_mode(source))?;

    let mut result = store_passwords(config, source, name, &new, passwords, backend).map(|_| ());
    if result.is_ok() {
        for (i, old_hop) in old.hops.iter().enumerate() {
            if !uses_keychain(old_hop) {
                continue;
            }
            let typed = passwords.hop(i).is_some();
            let keep = new.hops.get(i).is_some_and(|hop| {
                uses_keychain(hop)
                    && (typed
                        || (hop.host == old_hop.host
                            && hop.user == old_hop.user
                            && hop.port.unwrap_or(DEFAULT_SSH_PORT)
                                == old_hop.port.unwrap_or(DEFAULT_SSH_PORT)))
            });
            if !keep {
                let account = tunnel_account(config, source, name, i);
                if let Err(message) = crate::secrets::forget(backend, &account) {
                    result = Err(ResolveError::Secret {
                        reference: format!("keychain:{account}"),
                        message,
                    });
                    break;
                }
            }
        }
    }
    // The proxy's stored password is forgotten when it stops using the keychain or the proxy
    // points elsewhere, as a hop's is.
    if result.is_ok() && proxy_uses_keychain(old) {
        let same = |a: &ProxyConfig, b: &ProxyConfig| {
            a.kind == b.kind && a.host == b.host && a.port == b.port && a.user == b.user
        };
        let keep = proxy_uses_keychain(&new)
            && (passwords.proxy().is_some()
                || matches!((&old.proxy, &new.proxy), (Some(a), Some(b)) if same(a, b)));
        if !keep {
            let account = proxy_account(config, source, name);
            if let Err(message) = crate::secrets::forget(backend, &account) {
                result = Err(ResolveError::Secret {
                    reference: format!("keychain:{account}"),
                    message,
                });
            }
        }
    }
    if result.is_err() {
        // Put the file back as it was, the same way it was written.
        let _ = atomic_file::replace(path, original.as_bytes(), new_file_mode(source));
    }
    result.map(|()| parsed)
}

/// The connections that go through a tunnel (a network profile, ADR-027), by display name:
/// those naming it whose layer finds this one (a repository connection may use a personal
/// tunnel of the same name only if the repository has none).
pub fn tunnel_users(config: &Config, source: Source, name: &str) -> Vec<String> {
    config
        .connections
        .iter()
        .filter(|c| {
            c.file.tunnel.as_deref() == Some(name)
                && config
                    .tunnel_for(c.id.source, name)
                    .is_some_and(|t| t.source == source)
        })
        .map(|c| c.name())
        .collect()
}

/// Deletes a tunnel file and its stored passwords. Refused while a connection uses it.
pub fn delete_tunnel(
    config: &Config,
    source: Source,
    name: &str,
    backend: &dyn SecretBackend,
) -> Result<(), ResolveError> {
    let entry = find(config, source, name)?;
    let users = tunnel_users(config, source, name);
    if !users.is_empty() {
        return Err(ConfigError::new(
            &entry.file_path,
            format!("the tunnel is used by: {}", users.join(", ")),
        )
        .into());
    }
    std::fs::remove_file(&entry.file_path)
        .map_err(|e| ConfigError::new(&entry.file_path, format!("could not delete: {e}")))?;
    let mut accounts: Vec<String> = entry
        .file
        .hops
        .iter()
        .enumerate()
        .filter(|(_, hop)| uses_keychain(hop))
        .map(|(i, _)| tunnel_account(config, source, name, i))
        .collect();
    if proxy_uses_keychain(&entry.file) {
        accounts.push(proxy_account(config, source, name));
    }
    for account in accounts {
        crate::secrets::forget(backend, &account).map_err(|message| ResolveError::Secret {
            reference: format!("keychain:{account}"),
            message,
        })?;
    }
    Ok(())
}
