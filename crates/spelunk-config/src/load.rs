//! Layered configuration loading: repository (`.spelunk/`, shared in git) and user
//! (personal configuration directory, never in git).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::error::ConfigError;
use crate::model::{ConnectionFile, Engine, SCHEMA_VERSION, TunnelFile, UserFile};

/// Configuration directory inside a repository.
pub const WORKSPACE_DIR: &str = ".spelunk";

/// Where a connection or tunnel comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// The repository's `.spelunk/`: shared with the team.
    Workspace,
    /// The user's personal directory.
    User,
}

/// Stable identifier: the file path without extension (`prod/orders`).
/// Personal connections are shown with a leading `~/`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId {
    pub source: Source,
    pub path: String,
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            Source::Workspace => f.write_str(&self.path),
            Source::User => write!(f, "~/{}", self.path),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionEntry {
    pub id: ConnectionId,
    pub file_path: PathBuf,
    pub file: ConnectionFile,
}

impl ConnectionEntry {
    pub fn name(&self) -> String {
        self.file
            .name
            .clone()
            .unwrap_or_else(|| self.id.to_string())
    }

    /// Folders for the explorer tree (`prod/eu/orders` → `["prod", "eu"]`).
    pub fn folders(&self) -> Vec<&str> {
        let mut parts: Vec<&str> = self.id.path.split('/').collect();
        parts.pop();
        parts
    }
}

#[derive(Debug, Clone)]
pub struct TunnelEntry {
    pub source: Source,
    pub name: String,
    pub file_path: PathBuf,
    pub file: TunnelFile,
}

/// All the configuration visible from a directory.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Repository root (the directory containing `.spelunk/`), if any.
    pub workspace_root: Option<PathBuf>,
    pub user_dir: PathBuf,
    pub user: UserFile,
    pub connections: Vec<ConnectionEntry>,
    pub tunnels: Vec<TunnelEntry>,
    /// Files with problems. A broken file does not prevent loading the rest.
    pub problems: Vec<ConfigError>,
}

impl Config {
    pub fn connection(&self, id: &ConnectionId) -> Option<&ConnectionEntry> {
        self.connections.iter().find(|c| &c.id == id)
    }

    /// Tunnel by name: first in the same layer as the connection, then in the other one.
    pub fn tunnel_for(&self, source: Source, name: &str) -> Option<&TunnelEntry> {
        self.tunnels
            .iter()
            .find(|t| t.source == source && t.name == name)
            .or_else(|| self.tunnels.iter().find(|t| t.name == name))
    }
}

/// Default personal configuration directory (`%APPDATA%\spelunkdb` on Windows,
/// `~/Library/Application Support/spelunkdb` on macOS, `$XDG_CONFIG_HOME/spelunkdb` or
/// `~/.config/spelunkdb` on Linux).
pub fn default_user_dir() -> Option<PathBuf> {
    let absolute = |var: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    #[cfg(windows)]
    let base = absolute("APPDATA");
    #[cfg(target_os = "macos")]
    let base = crate::vars::home_dir().map(|h| h.join("Library/Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base =
        absolute("XDG_CONFIG_HOME").or_else(|| crate::vars::home_dir().map(|h| h.join(".config")));
    #[cfg(target_os = "macos")]
    let _ = absolute;
    base.map(|d| d.join("spelunkdb"))
}

/// Searches upwards for the directory containing `.spelunk/`, like git does with `.git/`.
pub fn find_workspace(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join(WORKSPACE_DIR).is_dir())
        .map(Path::to_path_buf)
}

/// Loads the configuration visible from `start` (if given) plus the user's.
pub fn load(start: Option<&Path>, user_dir: &Path) -> Config {
    let mut config = Config {
        user_dir: user_dir.to_path_buf(),
        ..Default::default()
    };

    let user_file = user_dir.join("user.toml");
    if user_file.is_file() {
        match parse_file::<UserFile>(&user_file) {
            Ok(user) => config.user = user,
            Err(e) => config.problems.push(e),
        }
    }
    config.workspace_root = start.and_then(find_workspace);

    let mut layers = vec![(Source::User, user_dir.to_path_buf())];
    if let Some(root) = &config.workspace_root {
        layers.insert(0, (Source::Workspace, root.join(WORKSPACE_DIR)));
    }
    for (source, dir) in layers {
        for path in toml_files(&dir.join("connections"), &mut config.problems) {
            let id = ConnectionId {
                source,
                path: id_from_path(&dir.join("connections"), &path),
            };
            match parse_file::<ConnectionFile>(&path)
                .and_then(|file| validate_connection(&path, file))
            {
                Ok(file) => config.connections.push(ConnectionEntry {
                    id,
                    file_path: path,
                    file,
                }),
                Err(e) => config.problems.push(e),
            }
        }
        for path in toml_files(&dir.join("tunnels"), &mut config.problems) {
            let name = id_from_path(&dir.join("tunnels"), &path);
            match parse_file::<TunnelFile>(&path).and_then(|file| validate_tunnel(&path, file)) {
                Ok(file) => config.tunnels.push(TunnelEntry {
                    source,
                    name,
                    file_path: path,
                    file,
                }),
                Err(e) => config.problems.push(e),
            }
        }
    }
    config.connections.sort_by(|a, b| a.id.cmp(&b.id));
    config
}

/// `.toml` files under `dir`, recursively (subdirectories are explorer folders).
fn toml_files(dir: &Path, problems: &mut Vec<ConfigError>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        let entries = match std::fs::read_dir(&current) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                problems.push(ConfigError::new(
                    &current,
                    format!("could not read the directory: {e}"),
                ));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "toml") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn id_from_path(base: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(base).unwrap_or(path).with_extension("");
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn parse_file<T: DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::new(path, format!("could not read: {e}")))?;
    toml::from_str(&content).map_err(|e| match e.span() {
        Some(span) => ConfigError::at(path, &content, span.start, e.message()),
        None => ConfigError::new(path, e.message()),
    })
}

fn check_version(path: &Path, version: u32) -> Result<(), ConfigError> {
    if version > SCHEMA_VERSION {
        return Err(ConfigError::new(
            path,
            format!(
                "schema_version {version} is from a newer version of SpelunkDB (this one understands up to {SCHEMA_VERSION})"
            ),
        ));
    }
    if version == 0 {
        return Err(ConfigError::new(
            path,
            "schema_version must be 1 or greater",
        ));
    }
    Ok(())
}

pub(crate) fn validate_connection(
    path: &Path,
    file: ConnectionFile,
) -> Result<ConnectionFile, ConfigError> {
    check_version(path, file.schema_version)?;
    let missing: Vec<&str> = match file.engine {
        Engine::Postgres => [
            ("host", file.host.is_none()),
            ("database", file.database.is_none()),
            ("user", file.user.is_none()),
        ]
        .into_iter()
        .filter_map(|(k, missing)| missing.then_some(k))
        .collect(),
        // A MySQL connection may have no default database: `USE` or qualified names work.
        Engine::Mysql | Engine::Mariadb => {
            [("host", file.host.is_none()), ("user", file.user.is_none())]
                .into_iter()
                .filter_map(|(k, m)| m.then_some(k))
                .collect()
        }
        Engine::Sqlite => [("database", file.database.is_none())]
            .into_iter()
            .filter_map(|(k, m)| m.then_some(k))
            .collect(),
    };
    if !missing.is_empty() {
        return Err(ConfigError::new(
            path,
            format!(
                "a {} connection is missing: {}",
                file.engine.as_str(),
                missing.join(", ")
            ),
        ));
    }
    if file.engine == Engine::Sqlite && file.tunnel.is_some() {
        return Err(ConfigError::new(
            path,
            "SQLite is a local file: it does not support a tunnel",
        ));
    }
    Ok(file)
}

pub(crate) fn validate_tunnel(path: &Path, file: TunnelFile) -> Result<TunnelFile, ConfigError> {
    check_version(path, file.schema_version)?;
    match (&file.kubernetes, file.hops.is_empty()) {
        (Some(_), false) => Err(ConfigError::new(
            path,
            "a tunnel is SSH ([[hops]]) or Kubernetes ([kubernetes]), not both",
        )),
        (None, true) => Err(ConfigError::new(
            path,
            "a tunnel needs at least one hop in [[hops]] or a [kubernetes] section",
        )),
        (Some(kubernetes), true) => {
            check_kubernetes_resource(&kubernetes.resource)
                .map_err(|m| ConfigError::new(path, m))?;
            Ok(file)
        }
        (None, false) => Ok(file),
    }
}

/// `svc/<name>`, `pod/<name>` or `deployment/<name>` (and kubectl's other spellings).
pub(crate) fn check_kubernetes_resource(resource: &str) -> Result<(), String> {
    let (kind, name) = resource.trim().split_once('/').ok_or_else(|| {
        format!("the resource {resource:?} must be svc/<name>, pod/<name> or deployment/<name>")
    })?;
    let known = matches!(
        kind.to_ascii_lowercase().as_str(),
        "svc"
            | "service"
            | "services"
            | "pod"
            | "pods"
            | "po"
            | "deployment"
            | "deployments"
            | "deploy"
    );
    let valid_name = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    if known && valid_name {
        Ok(())
    } else {
        Err(format!(
            "the resource {resource:?} must be svc/<name>, pod/<name> or deployment/<name>"
        ))
    }
}
