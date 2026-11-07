//! Layered configuration loading: repository (`.spelaion/`, shared in git) and user
//! (personal configuration directory, never in git).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::error::ConfigError;
use crate::model::{
    ConnectionFile, ConnectionTypeConfig, ConnectionTypesFile, Engine, SCHEMA_VERSION, TunnelFile,
    UserFile,
};

/// Configuration directory inside a repository.
pub const WORKSPACE_DIR: &str = ".spelaion";

/// Where a connection or tunnel comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    /// The repository's `.spelaion/`: shared with the team.
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

/// File name of the connection types (ADR-029), in `.spelaion/` and in the personal directory.
pub const CONNECTION_TYPES_FILE: &str = "connection-types.toml";

/// One layer's `connection-types.toml` (ADR-029).
#[derive(Debug, Clone, Default)]
pub enum TypesLayer {
    /// No file.
    #[default]
    Absent,
    Loaded {
        path: PathBuf,
        file: ConnectionTypesFile,
    },
    /// The file exists but does not load: connections with a type label are refused until it
    /// is fixed, because a missing type would loosen its safeguards.
    Broken(ConfigError),
}

impl TypesLayer {
    pub fn file(&self) -> Option<&ConnectionTypesFile> {
        match self {
            TypesLayer::Loaded { file, .. } => Some(file),
            TypesLayer::Absent | TypesLayer::Broken(_) => None,
        }
    }
}

/// All the configuration visible from a directory.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Repository root (the directory containing `.spelaion/`), if any.
    pub workspace_root: Option<PathBuf>,
    pub user_dir: PathBuf,
    pub user: UserFile,
    pub connections: Vec<ConnectionEntry>,
    pub tunnels: Vec<TunnelEntry>,
    /// The repository's connection types (ADR-029): used only once it is trusted.
    pub team_types: TypesLayer,
    /// The user's connection types.
    pub user_types: TypesLayer,
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

/// Default personal configuration directory (`%APPDATA%\spelaiondb` on Windows,
/// `~/Library/Application Support/spelaiondb` on macOS, `$XDG_CONFIG_HOME/spelaiondb` or
/// `~/.config/spelaiondb` on Linux).
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
    // The brand's folder: the products of the family share it (ADR-023).
    base.map(|d| d.join("spelaion"))
}

/// Creates the personal directory if needed and keeps it private: it holds the query history,
/// the audit and the personal variables. On Unix the default umask leaves a new folder
/// readable by every local user (0755), so a folder open to its group or to others is closed
/// to 0700; with the root private, nothing inside it is reachable, whatever each file's mode.
/// Windows' per-user profile folders are already private.
pub fn ensure_private_dir(dir: &Path) -> Result<(), ConfigError> {
    std::fs::create_dir_all(dir).map_err(|e| ConfigError::new(dir, e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)
            .map_err(|e| ConfigError::new(dir, e.to_string()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| ConfigError::new(dir, e.to_string()))?;
        }
    }
    Ok(())
}

/// The configuration folder of the repository at `root`: `.spelaion/`, or `.spelunk/` (its
/// name before ADR-023) when only that one exists.
pub fn workspace_dir(root: &Path) -> PathBuf {
    let current = root.join(WORKSPACE_DIR);
    let legacy = root.join(crate::migrate::LEGACY_WORKSPACE_DIR);
    if !current.is_dir() && legacy.is_dir() {
        legacy
    } else {
        current
    }
}

/// Searches upwards for the directory containing `.spelaion/` (or the old `.spelunk/`), like
/// git does with `.git/`.
pub fn find_workspace(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| {
            dir.join(WORKSPACE_DIR).is_dir()
                || dir.join(crate::migrate::LEGACY_WORKSPACE_DIR).is_dir()
        })
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
            Ok(mut user) => {
                // A wrong range is reported and ignored: tunnels then take any free port.
                if let Err(message) = user.tunnels.range() {
                    config.problems.push(ConfigError::new(&user_file, message));
                    user.tunnels = Default::default();
                }
                config.user = user;
            }
            Err(e) => config.problems.push(e),
        }
    }
    config.workspace_root = start.and_then(find_workspace);
    config.user_types = load_types(&user_dir.join(CONNECTION_TYPES_FILE), &mut config.problems);
    if let Some(root) = &config.workspace_root {
        config.team_types = load_types(
            &workspace_dir(root).join(CONNECTION_TYPES_FILE),
            &mut config.problems,
        );
    }

    let mut layers = vec![(Source::User, user_dir.to_path_buf())];
    if let Some(root) = &config.workspace_root {
        let legacy = root.join(crate::migrate::LEGACY_WORKSPACE_DIR);
        if legacy.is_dir() {
            config.problems.push(ConfigError::new(
                &legacy,
                if root.join(WORKSPACE_DIR).is_dir() {
                    "both .spelaion/ and .spelunk/ exist: .spelaion/ is used; move what is left in .spelunk/ into it and delete it"
                } else {
                    "the folder is now called .spelaion/: rename it with `git mv .spelunk .spelaion` (.spelunk/ is read until 0.5)"
                },
            ));
        }
        layers.insert(0, (Source::Workspace, workspace_dir(root)));
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

fn load_types(path: &Path, problems: &mut Vec<ConfigError>) -> TypesLayer {
    if !path.exists() {
        return TypesLayer::Absent;
    }
    match parse_file::<ConnectionTypesFile>(path).and_then(|file| validate_types(path, file)) {
        Ok(file) => TypesLayer::Loaded {
            path: path.to_path_buf(),
            file,
        },
        Err(e) => {
            problems.push(e.clone());
            TypesLayer::Broken(e)
        }
    }
}

/// A connection type's id: lowercase ASCII letters, digits, `-` and `_`, starting with a
/// letter or digit, at most 40 characters.
pub fn valid_type_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && id
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

pub(crate) fn validate_types(
    path: &Path,
    file: ConnectionTypesFile,
) -> Result<ConnectionTypesFile, ConfigError> {
    check_version(path, file.schema_version)?;
    for (id, kind) in &file.types {
        validate_type(id, kind).map_err(|m| ConfigError::new(path, m))?;
    }
    Ok(file)
}

/// One type of a `connection-types.toml`; the message names it.
pub(crate) fn validate_type(id: &str, kind: &ConnectionTypeConfig) -> Result<(), String> {
    if !valid_type_id(id) {
        return Err(format!(
            "connection type {id:?}: an id is lowercase letters, digits, - and _ (at most 40)"
        ));
    }
    if id == crate::types::PRODUCTION_ALIAS {
        return Err(format!(
            "connection type {id:?} is another name of Production: define [types.{}] instead",
            crate::types::PRODUCTION
        ));
    }
    if let Some(name) = &kind.name {
        let length = name.trim().chars().count();
        if length == 0 || length > ConnectionTypeConfig::MAX_NAME {
            return Err(format!(
                "connection type {id:?}: the name needs 1 to {} characters",
                ConnectionTypeConfig::MAX_NAME
            ));
        }
    }
    if kind
        .description
        .as_ref()
        .is_some_and(|d| d.chars().count() > ConnectionTypeConfig::MAX_DESCRIPTION)
    {
        return Err(format!(
            "connection type {id:?}: the description is at most {} characters",
            ConnectionTypeConfig::MAX_DESCRIPTION
        ));
    }
    if kind
        .close_idle_after
        .is_some_and(|s| s > ConnectionTypeConfig::MAX_CLOSE_IDLE_AFTER)
    {
        return Err(format!(
            "connection type {id:?}: close_idle_after is in seconds, at most {} (0 turns it off)",
            ConnectionTypeConfig::MAX_CLOSE_IDLE_AFTER
        ));
    }
    Ok(())
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
                "schema_version {version} is from a newer version of SpelaionDB (this one understands up to {SCHEMA_VERSION})"
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
        // Through Kubernetes the host is only a name for TLS: it may be left out.
        Engine::Postgres => [
            ("host", file.host.is_none() && file.kubernetes.is_none()),
            ("database", file.database.is_none()),
            ("user", file.user.is_none()),
        ]
        .into_iter()
        .filter_map(|(k, missing)| missing.then_some(k))
        .collect(),
        // A MySQL connection may have no default database: `USE` or qualified names work.
        Engine::Mysql | Engine::Mariadb | Engine::Sqlserver => [
            ("host", file.host.is_none() && file.kubernetes.is_none()),
            ("user", file.user.is_none()),
        ]
        .into_iter()
        .filter_map(|(k, m)| m.then_some(k))
        .collect(),
        Engine::Sqlite | Engine::Duckdb => [("database", file.database.is_none())]
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
    // A SQLite file may be on a server, reached through a tunnel (ADR-018); DuckDB may not.
    let remote_sqlite = file.engine == Engine::Sqlite && file.tunnel.is_some();
    if file.engine.is_file()
        && (file.kubernetes.is_some() || file.tunnel.is_some() && !remote_sqlite)
    {
        return Err(ConfigError::new(
            path,
            format!(
                "{} is a local file: it does not support {}",
                if file.engine == Engine::Sqlite {
                    "SQLite"
                } else {
                    "DuckDB"
                },
                if file.kubernetes.is_some() {
                    "Kubernetes"
                } else {
                    "a tunnel"
                }
            ),
        ));
    }
    if file.tls.cert.is_some() != file.tls.key.is_some() {
        return Err(ConfigError::new(
            path,
            "tls.cert and tls.key go together: the client certificate and its private key",
        ));
    }
    if file.tls.cert.is_some() {
        match file.engine {
            Engine::Postgres | Engine::Mysql | Engine::Mariadb => {}
            Engine::Sqlserver => {
                return Err(ConfigError::new(
                    path,
                    "SQL Server does not log SQL users in with a client certificate: use a password or a token",
                ));
            }
            Engine::Sqlite | Engine::Duckdb => {
                return Err(ConfigError::new(
                    path,
                    "a local file has no client certificate",
                ));
            }
        }
    }
    if let Some(seconds) = file.keepalive {
        if file.engine.is_file() && seconds > 0 {
            return Err(ConfigError::new(
                path,
                "keepalive is for database servers: a local file has no session to keep alive",
            ));
        }
        if seconds > ConnectionFile::MAX_KEEPALIVE {
            return Err(ConfigError::new(
                path,
                format!(
                    "keepalive is in seconds, at most {} (0 turns it off)",
                    ConnectionFile::MAX_KEEPALIVE
                ),
            ));
        }
    }
    if let Some(port) = file.local_port {
        if file.tunnel.is_none() || remote_sqlite {
            return Err(ConfigError::new(
                path,
                "local_port is the local end of an SSH tunnel: it needs a tunnel to a database server",
            ));
        }
        if port == 0 {
            return Err(ConfigError::new(
                path,
                "local_port must be from 1 to 65535; leave it out for a free one",
            ));
        }
    }
    if file.sqlite3.is_some() && !remote_sqlite {
        return Err(ConfigError::new(
            path,
            "sqlite3 only applies to a SQLite file on a server (a connection with a tunnel)",
        ));
    }
    if let Some(kubernetes) = &file.kubernetes {
        if file.tunnel.is_some() {
            return Err(ConfigError::new(
                path,
                "a connection goes through an SSH tunnel or Kubernetes, not both",
            ));
        }
        check_kubernetes_resource(&kubernetes.resource).map_err(|m| ConfigError::new(path, m))?;
    }
    Ok(file)
}

pub(crate) fn validate_tunnel(path: &Path, file: TunnelFile) -> Result<TunnelFile, ConfigError> {
    check_version(path, file.schema_version)?;
    if file.hops.is_empty() && file.proxy.is_none() {
        return Err(ConfigError::new(
            path,
            "a tunnel needs at least one hop in [[hops]], or a [proxy]",
        ));
    }
    if let Some(proxy) = &file.proxy {
        if proxy.host.trim().is_empty() {
            return Err(ConfigError::new(path, "the proxy needs a host"));
        }
        if proxy.port == 0 {
            return Err(ConfigError::new(
                path,
                "the proxy's port must be from 1 to 65535",
            ));
        }
        if proxy.password.is_some() && proxy.user.is_none() {
            return Err(ConfigError::new(
                path,
                "the proxy's password goes with a user: add `user` or remove `password`",
            ));
        }
        if file.hops.first().is_some_and(|hop| hop.command.is_some()) {
            return Err(ConfigError::new(
                path,
                "the first hop's command already is the way to it: it cannot go through the proxy too",
            ));
        }
    }
    if file
        .keepalive_interval
        .is_some_and(|s| s > TunnelFile::MAX_KEEPALIVE_INTERVAL)
    {
        return Err(ConfigError::new(
            path,
            format!(
                "keepalive_interval is in seconds, at most {} (0 turns keep-alives off)",
                TunnelFile::MAX_KEEPALIVE_INTERVAL
            ),
        ));
    }
    if file
        .keepalive_count
        .is_some_and(|n| n > TunnelFile::MAX_KEEPALIVE_COUNT)
    {
        return Err(ConfigError::new(
            path,
            format!(
                "keepalive_count is at most {} (0 never gives up)",
                TunnelFile::MAX_KEEPALIVE_COUNT
            ),
        ));
    }
    if file
        .connect_timeout
        .is_some_and(|s| s == 0 || s > TunnelFile::MAX_CONNECT_TIMEOUT)
    {
        return Err(ConfigError::new(
            path,
            format!(
                "connect_timeout is in seconds, from 1 to {}",
                TunnelFile::MAX_CONNECT_TIMEOUT
            ),
        ));
    }
    Ok(file)
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
