//! SSH tunnels created and edited from the UI. Like connections: the form is validated
//! into a [`TunnelFile`], secrets go to the keychain (never to the file) and editing only
//! rewrites what changed.

use std::path::{Path, PathBuf};

use spelunk_driver_api::Secret;
use toml_edit::{Array, ArrayOfTables, InlineTable, Item, Table, Value};

use crate::accounts::tunnel_account;
use crate::draft::{PasswordChoice, portable_path, slug};
use crate::edit::{read_doc, write_doc};
use crate::error::{ConfigError, ResolveError};
use crate::load::{Config, Source, TunnelEntry, WORKSPACE_DIR, validate_tunnel};
use crate::model::{HopConfig, SCHEMA_VERSION, SecretSpec, SshAuth, TunnelFile};
use crate::secrets::{SecretBackend, SecretRef};

/// JSON Schema URL put in the header of new tunnel files.
pub const TUNNEL_SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/spelunkdb/spelunkdb/main/schema/tunnel.schema.json";

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
    /// Also the file name (`.spelunk/tunnels/<slug>.toml`), which connections refer to.
    pub name: String,
    /// In order; the last one reaches the database.
    pub hops: Vec<HopDraft>,
}

/// Directory holding the tunnels of a layer.
pub fn tunnels_dir(config: &Config, source: Source) -> Result<PathBuf, ConfigError> {
    match source {
        Source::User => Ok(config.user_dir.join("tunnels")),
        Source::Workspace => config
            .workspace_root
            .as_ref()
            .map(|root| root.join(WORKSPACE_DIR).join("tunnels"))
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
        validate_tunnel(
            path,
            TunnelFile {
                schema_version: SCHEMA_VERSION,
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

/// Typed passwords to store: hops whose password is `keychain` and that got a non-empty one.
fn store_passwords(
    config: &Config,
    source: Source,
    name: &str,
    file: &TunnelFile,
    passwords: &[Option<Secret>],
    backend: &dyn SecretBackend,
) -> Result<Vec<String>, ResolveError> {
    let mut stored = Vec::new();
    for (i, hop) in file.hops.iter().enumerate() {
        let Some(secret) = passwords.get(i).and_then(Option::as_ref) else {
            continue;
        };
        if uses_keychain(hop) && !secret.expose().is_empty() {
            let account = tunnel_account(config, source, name, i);
            backend
                .keychain_set(&account, secret.expose())
                .map_err(|message| ResolveError::Secret {
                    reference: format!("keychain:{account}"),
                    message,
                })?;
            stored.push(account);
        }
    }
    Ok(stored)
}

/// Writes a new tunnel file and stores the typed `keychain` passwords (`passwords[i]` for
/// hop `i`). Never overwrites an existing tunnel; if the keychain fails, nothing is left.
pub fn add_tunnel(
    config: &Config,
    draft: &TunnelDraft,
    passwords: &[Option<Secret>],
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
    std::fs::write(&path, format!("#:schema {TUNNEL_SCHEMA_URL}\n\n{body}"))
        .map_err(|e| ConfigError::new(&path, e.to_string()))?;
    if let Err(e) = store_passwords(config, draft.source, &name, &file, passwords, backend) {
        let _ = std::fs::remove_file(&path);
        for i in 0..file.hops.len() {
            let _ = backend.keychain_delete(&tunnel_account(config, draft.source, &name, i));
        }
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

/// Saves the edited form over an existing tunnel (same file and name). Hops are matched by
/// position: changed keys are rewritten, extra hops appended and missing ones removed;
/// comments and `command` stay. A stored password is forgotten when its hop stops using
/// the keychain or now points to another server, so it is never sent to the wrong host.
pub fn update_tunnel(
    config: &Config,
    source: Source,
    name: &str,
    draft: &TunnelDraft,
    passwords: &[Option<Secret>],
    backend: &dyn SecretBackend,
) -> Result<(), ResolveError> {
    let entry = find(config, source, name)?;
    let path = &entry.file_path;
    let draft = TunnelDraft {
        source,
        ..draft.clone()
    };
    let new = draft.build_file(config, path)?;
    let old = &entry.file;

    let (original, mut doc) = read_doc(path)?;
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
    validate_tunnel(path, parsed)?;
    write_doc(path, &doc)?;

    let mut result = store_passwords(config, source, name, &new, passwords, backend).map(|_| ());
    if result.is_ok() {
        for (i, old_hop) in old.hops.iter().enumerate() {
            if !uses_keychain(old_hop) {
                continue;
            }
            let typed = passwords
                .get(i)
                .and_then(Option::as_ref)
                .is_some_and(|s| !s.expose().is_empty());
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
                if let Err(message) = backend.keychain_delete(&account) {
                    result = Err(ResolveError::Secret {
                        reference: format!("keychain:{account}"),
                        message,
                    });
                    break;
                }
            }
        }
    }
    if result.is_err() {
        let _ = std::fs::write(path, original);
    }
    result
}

/// Deletes a tunnel file and its stored passwords. Refused while a connection uses it.
pub fn delete_tunnel(
    config: &Config,
    source: Source,
    name: &str,
    backend: &dyn SecretBackend,
) -> Result<(), ResolveError> {
    let entry = find(config, source, name)?;
    let users: Vec<String> = config
        .connections
        .iter()
        .filter(|c| {
            c.file.tunnel.as_deref() == Some(name)
                && config
                    .tunnel_for(c.id.source, name)
                    .is_some_and(|t| t.source == source)
        })
        .map(|c| c.name())
        .collect();
    if !users.is_empty() {
        return Err(ConfigError::new(
            &entry.file_path,
            format!("the tunnel is used by: {}", users.join(", ")),
        )
        .into());
    }
    std::fs::remove_file(&entry.file_path)
        .map_err(|e| ConfigError::new(&entry.file_path, format!("could not delete: {e}")))?;
    for (i, hop) in entry.file.hops.iter().enumerate() {
        if uses_keychain(hop) {
            let account = tunnel_account(config, source, name, i);
            backend
                .keychain_delete(&account)
                .map_err(|message| ResolveError::Secret {
                    reference: format!("keychain:{account}"),
                    message,
                })?;
        }
    }
    Ok(())
}
