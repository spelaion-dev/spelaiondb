//! Moving personal connections to another computer (ADR-088): the export gathers the chosen
//! personal connections, the personal network profiles they go through and the keychain
//! secrets their files name, and seals them with the user's password (`connections_file.rs`);
//! the import opens such a file, checks every item as a file on disk is checked, and writes
//! each one whole or not at all: its secrets to the keychain (through `secrets::store`), then
//! its file. Secrets never go to a file. Anything that runs a program is never imported, and a
//! repository's connections and profiles are never exported: they move with their repository
//! (ADR-022).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spelaion_driver_api::Secret;
use zeroize::Zeroizing;

use crate::accounts::{connection_account, proxy_account, tunnel_account};
use crate::atomic_file::{NewFileMode, replace};
use crate::connections_file::{self, ConnectionsFileError};
use crate::load::{
    Config, ConnectionEntry, ConnectionId, Source, TunnelEntry, parse_text_checked,
    validate_connection, validate_tunnel,
};
use crate::model::{ConnectionFile, SecretSpec, TunnelFile};
use crate::secrets::{SecretBackend, SecretRef, store};

/// The plaintext's version, inside the encrypted file.
const PLAIN_VERSION: u32 = 1;
/// Longest id or profile name, and most items, a file may carry.
const MAX_NAME: usize = 200;
const MAX_ITEMS: usize = 10_000;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plain {
    version: u32,
    app_version: String,
    exported_at: String,
    connections: Vec<Item>,
    network_profiles: Vec<Item>,
}

/// A connection (`id`: its path under `connections/`) or a network profile (`id`: its name), its
/// file's text, and the values of the keychain entries it names, by role: `password`,
/// `hop:<n>` (from 1) or `proxy`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    id: String,
    file: String,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("{0}")]
    File(#[from] ConnectionsFileError),
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("there is no personal connection {0}")]
    UnknownConnection(String),
    #[error("could not read the keychain: {0}")]
    Keychain(String),
}

/// What an export wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionsExported {
    pub connections: usize,
    pub network_profiles: usize,
    /// Keychain passwords carried, encrypted.
    pub secrets: usize,
    /// There is no system keychain here, so no password could be carried.
    pub without_keychain: bool,
    /// Network profiles the connections name that are not personal (a repository's, or none
    /// of that name): not in the file.
    pub profiles_not_carried: Vec<String>,
}

/// Whether an item is a connection or a network profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Connection,
    NetworkProfile,
}

/// What importing an item does, or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemState {
    /// No item of that id here: it is added.
    New,
    /// One of that id is here: skipped unless the user asks to replace it.
    Exists,
    /// It runs a program, reads a secret from somewhere else, names another connection's
    /// keychain entry or uses a variable of the environment (ADR-088, point 8): never
    /// imported, with why.
    Refused(String),
    /// It does not load as a file on disk would not, with why.
    Invalid(String),
}

/// One item of a connections file, as the preview shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportItem {
    pub kind: ItemKind,
    pub id: String,
    /// The connection's name, or the profile's name.
    pub name: String,
    pub state: ItemState,
    /// Keychain passwords it carries.
    pub secrets: usize,
}

/// What happened to an item on import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemOutcome {
    Added,
    Replaced,
    /// It exists here and was not to be replaced.
    Skipped,
    /// Not imported, with why (it runs a program, it does not load, its profile was not
    /// imported).
    NotImported(String),
    /// Writing it failed; its keychain entries are as they were.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResult {
    pub kind: ItemKind,
    pub id: String,
    pub outcome: ItemOutcome,
}

fn personal_connections(config: &Config) -> impl Iterator<Item = &ConnectionEntry> {
    config
        .connections
        .iter()
        .filter(|entry| entry.id.source == Source::User)
}

fn personal_profile<'a>(config: &'a Config, name: &str) -> Option<&'a TunnelEntry> {
    config
        .tunnels
        .iter()
        .find(|tunnel| tunnel.source == Source::User && tunnel.name == name)
}

/// Whether a named keychain entry (`keychain:<name>`) is one the app names after another
/// connection or profile (`repo:…`, `~/…`): an import never writes one nor a connection that
/// reads one, and an export never carries one, so a file cannot send another connection's
/// password to a server it chooses (ADR-009's names, ADR-088).
fn names_another_entry(name: &str, default_account: &str) -> bool {
    (name.starts_with("repo:") || name.starts_with("~/")) && name != default_account
}

/// The keychain entry a password spec reads, if it reads one of its own.
fn keychain_entry(spec: Option<&SecretSpec>, default_account: &str) -> Option<String> {
    match spec?.secret.parse::<SecretRef>().ok()? {
        SecretRef::Keychain(Some(name)) if names_another_entry(&name, default_account) => None,
        SecretRef::Keychain(name) => Some(name.unwrap_or_else(|| default_account.to_string())),
        SecretRef::Env(_)
        | SecretRef::OnePassword(_)
        | SecretRef::Command(_)
        | SecretRef::Prompt => None,
    }
}

/// Reads one keychain entry into `secrets` under `role`.
fn carry(
    backend: &dyn SecretBackend,
    account: Option<String>,
    role: String,
    secrets: &mut BTreeMap<String, String>,
) -> Result<(), TransferError> {
    let Some(account) = account else {
        return Ok(());
    };
    if let Some(value) = backend
        .keychain_get(&account)
        .map_err(TransferError::Keychain)?
    {
        secrets.insert(role, value);
    }
    Ok(())
}

/// The roles of a profile's keychain entries, with their accounts in `config`'s personal layer.
fn profile_accounts(
    config: &Config,
    name: &str,
    file: &TunnelFile,
) -> Vec<(String, Option<String>)> {
    let mut accounts = Vec::new();
    for (i, hop) in file.hops.iter().enumerate() {
        accounts.push((
            format!("hop:{}", i + 1),
            keychain_entry(
                hop.password.as_ref(),
                &tunnel_account(config, Source::User, name, i),
            ),
        ));
    }
    if let Some(proxy) = &file.proxy {
        accounts.push((
            "proxy".to_string(),
            keychain_entry(
                proxy.password.as_ref(),
                &proxy_account(config, Source::User, name),
            ),
        ));
    }
    accounts
}

fn read_text(path: &Path) -> Result<String, TransferError> {
    std::fs::read_to_string(path).map_err(|source| TransferError::Read {
        path: path.to_path_buf(),
        source,
    })
}

/// Seals the personal connections `ids` (their paths, `local` or `dev/orders`; empty: every
/// one) with the profiles they go through and their keychain passwords, and writes the file to
/// `path` (replacing it whole, or not at all).
pub fn export_connections(
    config: &Config,
    backend: &dyn SecretBackend,
    ids: &[String],
    password: &Secret,
    app_version: &str,
    exported_at: &str,
    path: &Path,
) -> Result<ConnectionsExported, TransferError> {
    let chosen: Vec<&ConnectionEntry> = if ids.is_empty() {
        personal_connections(config).collect()
    } else {
        ids.iter()
            .map(|id| {
                personal_connections(config)
                    .find(|entry| &entry.id.path == id)
                    .ok_or_else(|| TransferError::UnknownConnection(id.clone()))
            })
            .collect::<Result<_, _>>()?
    };
    let keychain = backend.keychain_available();
    let mut summary = ConnectionsExported {
        without_keychain: !keychain,
        ..Default::default()
    };
    let mut plain = Plain {
        version: PLAIN_VERSION,
        app_version: app_version.into(),
        exported_at: exported_at.into(),
        connections: Vec::new(),
        network_profiles: Vec::new(),
    };
    for entry in chosen {
        let mut secrets = BTreeMap::new();
        if keychain {
            carry(
                backend,
                keychain_entry(
                    entry.file.password.as_ref(),
                    &connection_account(config, &entry.id),
                ),
                "password".into(),
                &mut secrets,
            )?;
        }
        summary.secrets += secrets.len();
        plain.connections.push(Item {
            id: entry.id.path.clone(),
            file: read_text(&entry.file_path)?,
            secrets,
        });
        let Some(name) = entry.file.tunnel.as_deref() else {
            continue;
        };
        if plain.network_profiles.iter().any(|item| item.id == name)
            || summary.profiles_not_carried.iter().any(|n| n == name)
        {
            continue;
        }
        let Some(profile) = personal_profile(config, name) else {
            summary.profiles_not_carried.push(name.to_string());
            continue;
        };
        let mut secrets = BTreeMap::new();
        if keychain {
            for (role, account) in profile_accounts(config, name, &profile.file) {
                carry(backend, account, role, &mut secrets)?;
            }
        }
        summary.secrets += secrets.len();
        plain.network_profiles.push(Item {
            id: name.to_string(),
            file: read_text(&profile.file_path)?,
            secrets,
        });
    }
    summary.connections = plain.connections.len();
    summary.network_profiles = plain.network_profiles.len();
    let text = Zeroizing::new(
        serde_json::to_vec(&plain).map_err(|e| TransferError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::other(e),
        })?,
    );
    drop(plain);
    let sealed = connections_file::seal(&text, password)?;
    replace(path, &sealed, NewFileMode::OwnerOnly).map_err(|source| TransferError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(summary)
}

/// Whether `id` can name a file under `connections/` (segments of letters, digits, `-`, `_`,
/// `.` and spaces, none empty nor starting with a dot) or, with `nested` false, `tunnels/`.
fn valid_id(id: &str, nested: bool) -> bool {
    id.len() <= MAX_NAME
        && (nested || !id.contains('/'))
        && id.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment.trim() == segment
                && segment
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
        })
}

/// Why a password reference keeps an item out of an import: it runs a program, it reads a
/// secret from somewhere else (the environment, 1Password) that a server the file chooses
/// would then be sent, or it names another connection's keychain entry. `None` for the
/// keychain entry of its own, a name of the user's, `prompt`, or no password.
fn secret_refusal(spec: Option<&SecretSpec>, default_account: &str) -> Option<String> {
    let spec = spec?;
    match spec.secret.parse::<SecretRef>() {
        Err(_) => Some("its password reference cannot be read".into()),
        Ok(SecretRef::Command(_)) => Some("a password comes from a command (cmd:)".into()),
        Ok(SecretRef::Env(var)) => Some(format!(
            "a password is read from the environment (env:{var})"
        )),
        Ok(SecretRef::OnePassword(_)) => Some("a password is read from 1Password".into()),
        Ok(SecretRef::Keychain(Some(name))) if names_another_entry(&name, default_account) => {
            Some("a password names another connection's keychain entry".into())
        }
        Ok(SecretRef::Keychain(_) | SecretRef::Prompt) => None,
    }
}

/// Why a file's text keeps it out: a `${…}` variable other than the user's own (`${user.…}`),
/// which could put the environment into a host name that a lookup sends away, as a pasted
/// connection may not (ADR-030).
fn variables_refusal(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let end = after.find('}').unwrap_or(after.len());
        let name = &after[..end];
        if !name.starts_with("user.") {
            return Some(format!(
                "it uses ${{{name}}}: only ${{user.…}} variables are imported"
            ));
        }
        rest = &after[end..];
    }
    None
}

/// Why a connection is not imported, if it is not: it runs a program, reads a secret from
/// elsewhere, names another connection's keychain entry, or uses a variable of the environment.
fn connection_refusal(
    config: &Config,
    id: &str,
    file: &ConnectionFile,
    text: &str,
) -> Option<String> {
    let own = connection_account(
        config,
        &ConnectionId {
            source: Source::User,
            path: id.to_string(),
        },
    );
    if let Some(why) = secret_refusal(file.password.as_ref(), &own) {
        return Some(why);
    }
    if file.sqlite3.is_some() {
        return Some("it names a sqlite3 program".into());
    }
    if file
        .kubernetes
        .as_ref()
        .is_some_and(|k| k.kubectl.is_some())
    {
        return Some("it names a kubectl program".into());
    }
    variables_refusal(text)
}

/// Why a network profile is not imported, if it is not: as a connection, and a hop that runs a
/// command or an `aws` program.
fn profile_refusal(config: &Config, name: &str, file: &TunnelFile, text: &str) -> Option<String> {
    if file.hops.iter().any(|hop| hop.command.is_some()) {
        return Some("a hop runs a command".into());
    }
    for (i, hop) in file.hops.iter().enumerate() {
        let own = tunnel_account(config, Source::User, name, i);
        if let Some(why) = secret_refusal(hop.password.as_ref(), &own) {
            return Some(why);
        }
    }
    if let Some(proxy) = &file.proxy {
        let own = proxy_account(config, Source::User, name);
        if let Some(why) = secret_refusal(proxy.password.as_ref(), &own) {
            return Some(why);
        }
    }
    if file.ssm.as_ref().is_some_and(|ssm| ssm.aws.is_some()) {
        return Some("it names an aws program".into());
    }
    if file
        .kubernetes
        .as_ref()
        .is_some_and(|k| k.kubectl.is_some())
    {
        return Some("it names a kubectl program".into());
    }
    variables_refusal(text)
}

/// Reads a connections file the user chose, refusing one larger than
/// [`connections_file::CONNECTIONS_FILE_MAX`] before reading it.
pub fn read_connections_file(path: &Path) -> Result<Vec<u8>, TransferError> {
    let read = |source| TransferError::Read {
        path: path.to_path_buf(),
        source,
    };
    let size = std::fs::metadata(path).map_err(read)?.len();
    if size > connections_file::CONNECTIONS_FILE_MAX as u64 {
        return Err(ConnectionsFileError::TooLarge.into());
    }
    std::fs::read(path).map_err(read)
}

/// A file opened and read, before anything is checked.
fn open_plain(bytes: &[u8], password: &Secret) -> Result<Plain, TransferError> {
    let text = connections_file::open(bytes, password)?;
    let plain: Plain =
        serde_json::from_slice(&text).map_err(|_| ConnectionsFileError::NotAConnectionsFile)?;
    if plain.version != PLAIN_VERSION
        || plain.connections.len() + plain.network_profiles.len() > MAX_ITEMS
    {
        return Err(ConnectionsFileError::NotAConnectionsFile.into());
    }
    Ok(plain)
}

/// An item checked for import: its parsed file, or why it is not imported.
struct Checked<T> {
    item: Item,
    name: String,
    file: Result<T, ItemState>,
}

fn check_connection(config: &Config, item: Item) -> Checked<ConnectionFile> {
    let path = config
        .user_dir
        .join("connections")
        .join(format!("{}.toml", item.id));
    let file = if valid_id(&item.id, true) {
        parse_text_checked::<ConnectionFile>(&path, &item.file)
            .and_then(|file| validate_connection(&path, file))
            .map_err(|e| ItemState::Invalid(e.to_string()))
    } else {
        Err(ItemState::Invalid(format!(
            "{:?} is not a connection id this version can write",
            item.id
        )))
    };
    let file =
        file.and_then(
            |file| match connection_refusal(config, &item.id, &file, &item.file) {
                Some(why) => Err(ItemState::Refused(why)),
                None => Ok(file),
            },
        );
    let name = file
        .as_ref()
        .ok()
        .and_then(|f| f.name.clone())
        .unwrap_or_else(|| item.id.clone());
    Checked { item, name, file }
}

fn check_profile(config: &Config, item: Item) -> Checked<TunnelFile> {
    let path = config
        .user_dir
        .join("tunnels")
        .join(format!("{}.toml", item.id));
    let file = if valid_id(&item.id, false) {
        parse_text_checked::<TunnelFile>(&path, &item.file)
            .and_then(|file| validate_tunnel(&path, file))
            .map_err(|e| ItemState::Invalid(e.to_string()))
    } else {
        Err(ItemState::Invalid(format!(
            "{:?} is not a network profile name this version can write",
            item.id
        )))
    };
    let file = file.and_then(
        |file| match profile_refusal(config, &item.id, &file, &item.file) {
            Some(why) => Err(ItemState::Refused(why)),
            None => Ok(file),
        },
    );
    let name = item.id.clone();
    Checked { item, name, file }
}

struct Opened {
    profiles: Vec<Checked<TunnelFile>>,
    connections: Vec<Checked<ConnectionFile>>,
}

fn open_and_check(
    config: &Config,
    bytes: &[u8],
    password: &Secret,
) -> Result<Opened, TransferError> {
    let plain = open_plain(bytes, password)?;
    let profiles: Vec<Checked<TunnelFile>> = plain
        .network_profiles
        .into_iter()
        .map(|item| check_profile(config, item))
        .collect();
    let connections = plain
        .connections
        .into_iter()
        .map(|item| {
            let mut checked = check_connection(config, item);
            // A connection whose profile in the file will not be imported is not either.
            if let Ok(file) = &checked.file
                && let Some(name) = file.tunnel.as_deref()
                && profiles
                    .iter()
                    .any(|p| p.item.id == name && p.file.is_err())
            {
                checked.file = Err(ItemState::Refused(format!(
                    "its network profile {name} is not imported"
                )));
            }
            checked
        })
        .collect();
    Ok(Opened {
        profiles,
        connections,
    })
}

fn state_of<T>(checked: &Checked<T>, exists: bool) -> ItemState {
    match &checked.file {
        Err(state) => state.clone(),
        Ok(_) if exists => ItemState::Exists,
        Ok(_) => ItemState::New,
    }
}

fn connection_exists(config: &Config, id: &str) -> bool {
    personal_connections(config).any(|entry| entry.id.path == id)
        || config
            .user_dir
            .join("connections")
            .join(format!("{id}.toml"))
            .exists()
}

fn profile_exists(config: &Config, name: &str) -> bool {
    personal_profile(config, name).is_some()
        || config
            .user_dir
            .join("tunnels")
            .join(format!("{name}.toml"))
            .exists()
}

/// Opens a connections file and says what importing each item would do. Nothing is written.
pub fn preview_connections_import(
    config: &Config,
    bytes: &[u8],
    password: &Secret,
) -> Result<Vec<ImportItem>, TransferError> {
    let opened = open_and_check(config, bytes, password)?;
    let mut items = Vec::new();
    for checked in &opened.profiles {
        items.push(ImportItem {
            kind: ItemKind::NetworkProfile,
            id: checked.item.id.clone(),
            name: checked.name.clone(),
            state: state_of(checked, profile_exists(config, &checked.item.id)),
            secrets: checked.item.secrets.len(),
        });
    }
    for checked in &opened.connections {
        items.push(ImportItem {
            kind: ItemKind::Connection,
            id: checked.item.id.clone(),
            name: checked.name.clone(),
            state: state_of(checked, connection_exists(config, &checked.item.id)),
            secrets: checked.item.secrets.len(),
        });
    }
    Ok(items)
}

/// One keychain entry as it was before an item wrote it.
struct Before {
    account: String,
    value: Option<Secret>,
}

/// Writes an item's secrets, then its file; when a step fails, the keychain entries it
/// changed are put back and the file is not written.
fn write_item(
    backend: &dyn SecretBackend,
    secrets: &[(String, Secret)],
    path: &Path,
    text: &str,
) -> Result<(), String> {
    let mut before: Vec<Before> = Vec::new();
    let result = (|| {
        for (account, secret) in secrets {
            let value = backend.keychain_get(account)?.map(Secret::new);
            before.push(Before {
                account: account.clone(),
                value,
            });
            store(backend, account, secret.expose())?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        replace(path, text.as_bytes(), NewFileMode::OwnerOnly).map_err(|e| e.to_string())
    })();
    if result.is_err() {
        for entry in before.into_iter().rev() {
            let _ = match entry.value {
                Some(value) => store(backend, &entry.account, value.expose()),
                None => backend.keychain_delete(&entry.account),
            };
        }
    }
    result
}

/// The keychain accounts of a connection's secrets on this computer.
fn connection_secrets(config: &Config, checked: &Checked<ConnectionFile>) -> Vec<(String, Secret)> {
    let Ok(file) = &checked.file else {
        return Vec::new();
    };
    let id = ConnectionId {
        source: Source::User,
        path: checked.item.id.clone(),
    };
    let account = keychain_entry(file.password.as_ref(), &connection_account(config, &id));
    match (account, checked.item.secrets.get("password")) {
        (Some(account), Some(value)) => vec![(account, Secret::new(value.clone()))],
        _ => Vec::new(),
    }
}

fn profile_secrets(config: &Config, checked: &Checked<TunnelFile>) -> Vec<(String, Secret)> {
    let Ok(file) = &checked.file else {
        return Vec::new();
    };
    profile_accounts(config, &checked.item.id, file)
        .into_iter()
        .filter_map(|(role, account)| {
            Some((
                account?,
                Secret::new(checked.item.secrets.get(&role)?.clone()),
            ))
        })
        .collect()
}

/// Imports a connections file: every item checked first, then the profiles and the
/// connections, each one whole or not at all. An item that exists here is replaced only when
/// its id is in `replace` (`profiles` for network profiles); the rest are skipped. The caller
/// reads the configuration again afterwards.
pub fn import_connections(
    config: &Config,
    backend: &dyn SecretBackend,
    bytes: &[u8],
    password: &Secret,
    replace_connections: &[String],
    replace_profiles: &[String],
) -> Result<Vec<ImportResult>, TransferError> {
    let opened = open_and_check(config, bytes, password)?;
    let keychain = backend.keychain_available();
    let mut results = Vec::new();
    let mut profiles_written: Vec<String> = Vec::new();
    for checked in &opened.profiles {
        let id = checked.item.id.clone();
        let exists = profile_exists(config, &id);
        let outcome = match &checked.file {
            Err(ItemState::Refused(why) | ItemState::Invalid(why)) => {
                ItemOutcome::NotImported(why.clone())
            }
            Err(ItemState::New | ItemState::Exists) => ItemOutcome::NotImported(String::new()),
            Ok(_) if exists && !replace_profiles.contains(&id) => ItemOutcome::Skipped,
            Ok(_) => {
                let secrets = if keychain {
                    profile_secrets(config, checked)
                } else {
                    Vec::new()
                };
                let path = config.user_dir.join("tunnels").join(format!("{id}.toml"));
                match write_item(backend, &secrets, &path, &checked.item.file) {
                    Ok(()) => {
                        profiles_written.push(id.clone());
                        if exists {
                            ItemOutcome::Replaced
                        } else {
                            ItemOutcome::Added
                        }
                    }
                    Err(why) => ItemOutcome::Failed(why),
                }
            }
        };
        results.push(ImportResult {
            kind: ItemKind::NetworkProfile,
            id,
            outcome,
        });
    }
    for checked in &opened.connections {
        let id = checked.item.id.clone();
        let exists = connection_exists(config, &id);
        // A profile of the file that failed to write leaves its connections out too.
        let profile_failed = checked
            .file
            .as_ref()
            .ok()
            .and_then(|f| f.tunnel.as_deref())
            .is_some_and(|name| {
                opened.profiles.iter().any(|p| p.item.id == name)
                    && !profiles_written.iter().any(|w| w == name)
                    && !profile_exists(config, name)
            });
        let outcome = match &checked.file {
            Err(ItemState::Refused(why) | ItemState::Invalid(why)) => {
                ItemOutcome::NotImported(why.clone())
            }
            Err(ItemState::New | ItemState::Exists) => ItemOutcome::NotImported(String::new()),
            Ok(_) if exists && !replace_connections.contains(&id) => ItemOutcome::Skipped,
            Ok(_) if profile_failed => {
                ItemOutcome::NotImported("its network profile was not imported".into())
            }
            Ok(_) => {
                let secrets = if keychain {
                    connection_secrets(config, checked)
                } else {
                    Vec::new()
                };
                let path = config
                    .user_dir
                    .join("connections")
                    .join(format!("{id}.toml"));
                match write_item(backend, &secrets, &path, &checked.item.file) {
                    Ok(()) if exists => ItemOutcome::Replaced,
                    Ok(()) => ItemOutcome::Added,
                    Err(why) => ItemOutcome::Failed(why),
                }
            }
        };
        results.push(ImportResult {
            kind: ItemKind::Connection,
            id,
            outcome,
        });
    }
    Ok(results)
}
