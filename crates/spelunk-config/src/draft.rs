//! New connections created from the UI: a draft (what the user typed) is validated, turned
//! into a [`ConnectionFile`] and written next to the others. Secrets never reach the file:
//! a password typed for `keychain` goes to the system keychain.

use std::path::{Path, PathBuf};

use spelunk_driver_api::Secret;

use crate::edit::create_connection;
use crate::error::{ConfigError, ResolveError};
use crate::load::{
    Config, ConnectionEntry, ConnectionId, Source, WORKSPACE_DIR, validate_connection,
};
use crate::model::{
    ConnectionFile, Engine, McpConfig, SCHEMA_VERSION, SecretSpec, TlsConfig, TlsModeConfig,
};
use crate::secrets::SecretBackend;

/// Where the password of a new connection comes from. It is never written to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasswordChoice {
    /// The engine does not need one (SQLite) or the server does not ask for it.
    None,
    /// Stored in the system keychain under `<id>/password`.
    Keychain,
    /// Asked every time the connection opens.
    Prompt,
    /// Read from an environment variable when connecting.
    Env(String),
}

/// What the user filled in to create a connection. Empty strings mean "not set".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionDraft {
    /// `Workspace` writes to the repository's `.spelunk/` (shared through git); `User`, to
    /// the personal directory.
    pub source: Source,
    /// Optional folders, such as `prod/eu`.
    pub folder: String,
    pub name: String,
    pub engine: Engine,
    pub env: String,
    pub host: String,
    pub port: Option<u16>,
    /// Database name; for SQLite, the file path.
    pub database: String,
    pub user: String,
    pub password: PasswordChoice,
    pub tls_mode: TlsModeConfig,
    pub tls_ca: String,
    pub tunnel: String,
    pub read_only: bool,
    pub mcp_exposed: bool,
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
            .map(|root| root.join(WORKSPACE_DIR).join("connections"))
            .ok_or_else(|| {
                ConfigError::new(&config.user_dir, "no repository is open to save it in")
            }),
    }
}

/// A path inside the repository is stored relative to its root (with `/`), so the same file
/// works on every machine; anything else is kept as typed.
fn portable_path(path: &str, root: Option<&Path>) -> String {
    let Some(root) = root else {
        return path.to_string();
    };
    match Path::new(path).strip_prefix(root) {
        Ok(relative) if Path::new(path).is_absolute() => relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        _ => path.to_string(),
    }
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
        let network = self.engine != Engine::Sqlite;
        let password = match &self.password {
            PasswordChoice::None => None,
            PasswordChoice::Keychain => Some("keychain".to_string()),
            PasswordChoice::Prompt => Some("prompt".to_string()),
            PasswordChoice::Env(var) => {
                let var = var.trim();
                if !valid_env_var(var) {
                    return Err(ConfigError::new(
                        &path,
                        format!("invalid environment variable name: {var:?}"),
                    ));
                }
                Some(format!("env:{var}"))
            }
        };
        let root = match self.source {
            Source::Workspace => config.workspace_root.as_deref(),
            Source::User => None,
        };
        let tunnel = non_empty(&self.tunnel).filter(|_| network);
        if let Some(name) = &tunnel
            && config.tunnel_for(self.source, name).is_none()
        {
            return Err(ConfigError::new(
                &path,
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
                Engine::Sqlite => portable_path(&d, root),
                Engine::Postgres => d,
            }),
            user: non_empty(&self.user).filter(|_| network),
            password: password
                .filter(|_| network)
                .map(|secret| SecretSpec { secret }),
            tunnel,
            read_only: self.read_only,
            tls: if network {
                TlsConfig {
                    mode: self.tls_mode,
                    ca: non_empty(&self.tls_ca).map(|ca| portable_path(&ca, root)),
                }
            } else {
                TlsConfig::default()
            },
            mcp: McpConfig {
                exposed: self.mcp_exposed,
                ..McpConfig::default()
            },
        };
        validate_connection(&path, file)
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
        && draft.engine != Engine::Sqlite
        && let Some(secret) = password.filter(|s| !s.expose().is_empty())
    {
        let account = format!("{}/password", entry.id);
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
