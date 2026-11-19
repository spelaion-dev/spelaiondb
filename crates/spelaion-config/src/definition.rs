//! What a connection's session is opened from, as the configuration has it now, without its
//! secrets (SDB-426): the connection's file, the network profile it names, `user.toml`, the
//! connection types, the repository and whether it is trusted. Everything the resolver reads
//! except the keychain, environment variables and commands, so it can be compared as often as
//! needed without asking a password manager or running a program.
//!
//! A session opened earlier is still what the configuration says while this stays the same.
//! The app keeps the definition each session was opened from, and the core compares it with
//! the current one before another session is made from it (`spelaion-core`'s
//! `session_source.rs`).

use std::path::PathBuf;

use crate::load::{Config, TypesLayer};
use crate::model::{ConnectionFile, ConnectionTypesFile, TunnelFile, UserFile};
use crate::{ConfigError, ConnectionId};

/// Everything a connection's session is opened from, except its secrets.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionDefinition {
    file_path: PathBuf,
    file: ConnectionFile,
    /// The network profile it names, as found (SSH, proxy, SSM or Kubernetes).
    tunnel: Option<(PathBuf, TunnelFile)>,
    /// `user.toml`: variables, masking, tunnel ports.
    user: UserFile,
    user_unreadable: bool,
    /// The repository's and the user's connection types (ADR-029).
    types: [TypesKey; 2],
    workspace_root: Option<PathBuf>,
    /// Whether the repository is trusted (ADR-022).
    trusted: bool,
}

/// A `TypesLayer` that can be compared.
#[derive(Debug, Clone, PartialEq)]
enum TypesKey {
    Absent,
    Loaded(PathBuf, ConnectionTypesFile),
    Broken(ConfigError),
}

impl TypesKey {
    fn of(layer: &TypesLayer) -> Self {
        match layer {
            TypesLayer::Absent => TypesKey::Absent,
            TypesLayer::Loaded { path, file } => TypesKey::Loaded(path.clone(), file.clone()),
            TypesLayer::Broken(e) => TypesKey::Broken(e.clone()),
        }
    }
}

impl Config {
    /// The definition of connection `id` now; `None` if it no longer exists. `trusted` is
    /// whether the repository is trusted (`TrustStore::is_trusted`), which changes what a
    /// repository connection resolves to.
    pub fn connection_definition(
        &self,
        id: &ConnectionId,
        trusted: bool,
    ) -> Option<ConnectionDefinition> {
        let entry = self.connection(id)?;
        Some(ConnectionDefinition {
            file_path: entry.file_path.clone(),
            file: entry.file.clone(),
            tunnel: entry
                .file
                .tunnel
                .as_deref()
                .and_then(|name| self.tunnel_for(entry.id.source, name))
                .map(|t| (t.file_path.clone(), t.file.clone())),
            user: self.user.clone(),
            user_unreadable: self.user_unreadable,
            types: [
                TypesKey::of(&self.team_types),
                TypesKey::of(&self.user_types),
            ],
            workspace_root: self.workspace_root.clone(),
            trusted,
        })
    }
}
