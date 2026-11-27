//! What a connection's session is opened from, as the configuration has it now, without its
//! secrets (SDB-426): the connection's file, the network profile it names, `user.toml`, the
//! connection types, the repository and whether it is trusted. Everything the resolver reads
//! except the keychain, environment variables and commands, so it can be compared as often as
//! needed without asking a password manager or running a program.
//!
//! With several folders open (ADR-075), a personal connection is also opened from what it reads
//! of every open folder (`folders.rs`): its definition covers each folder's trust, connection
//! types and `known_hosts`, and the network profile it names wherever it comes from. A
//! folder's connection reads only its own folder, and its definition is the same whatever else
//! is open; with one folder open, every definition is what it was before several could be.
//!
//! A connection that goes through a network profile is also opened from the team's
//! `known_hosts` its tunnel is checked against (SDB-527): its own folder's
//! `.spelaion/known_hosts`, as the resolver gives it. A change to that file (a server's new
//! key) makes the next session open from the saved configuration, which goes through ADR-022's
//! review, instead of reusing a session opened before it. A connection without a profile, or
//! one through a Kubernetes profile (no SSH), reads no `known_hosts` and its definition does not
//! hold it.
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
    /// A personal connection with several folders open (ADR-075): what it reads of each, in
    /// the order they were opened. Empty for a folder's connection, and with one folder open.
    open_folders: Vec<FolderKey>,
    /// The team's `known_hosts` a tunnelled connection is checked against when it reads one
    /// folder (SDB-527): its bytes, or `None` when there is no tunnel, no folder or no file.
    /// With several folders, `open_folders` holds each one's.
    known_hosts: Option<Vec<u8>>,
}

/// What a personal connection reads of one open folder (ADR-075): whether it is trusted, its
/// connection types (which tighten the connection's type) and its `known_hosts` (which checks
/// the connection's tunnel), as they are now.
#[derive(Debug, Clone, PartialEq)]
struct FolderKey {
    root: Option<PathBuf>,
    trusted: bool,
    types: TypesKey,
    /// The file's bytes; `None` when there is none (or it cannot be read, which the resolver
    /// reports when it opens a session).
    known_hosts: Option<Vec<u8>>,
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
        self.connection_definition_open(id, trusted, &[])
    }

    /// The definition of connection `id` now with `open` folders open (ADR-075): each open
    /// folder's configuration (loaded alone with the personal layer, as the resolver is given
    /// them in `Resolver::with_open_folders`) and whether it is trusted, in the order they were
    /// opened. `trusted` is this configuration's own folder's. A personal connection's covers
    /// what it reads of every open folder; a folder's connection's, and any with one folder
    /// open, is [`Config::connection_definition`]'s.
    pub fn connection_definition_open(
        &self,
        id: &ConnectionId,
        trusted: bool,
        open: &[(&Config, bool)],
    ) -> Option<ConnectionDefinition> {
        let entry = self.connection(id)?;
        let configs: Vec<&Config> = open.iter().map(|(config, _)| *config).collect();
        let folders = crate::folders::folders_for(self, &configs, entry.id.source);
        let is_trusted = |folder: &Config| {
            if folder.workspace_root == self.workspace_root {
                return trusted;
            }
            open.iter()
                .find(|(config, _)| config.workspace_root == folder.workspace_root)
                .is_some_and(|(_, trusted)| *trusted)
        };
        let open_folders = if folders.len() > 1 {
            folders
                .iter()
                .map(|folder| FolderKey {
                    root: folder.workspace_root.clone(),
                    trusted: is_trusted(folder),
                    types: TypesKey::of(&folder.team_types),
                    known_hosts: folder
                        .workspace_root
                        .as_deref()
                        .and_then(|root| std::fs::read(crate::trust::team_known_hosts(root)).ok()),
                })
                .collect()
        } else {
            Vec::new()
        };
        // The profile the resolver would use, from whichever folder it comes.
        let tunnel = entry.file.tunnel.as_deref().and_then(|name| {
            match crate::folders::profile(self, &configs, entry.id.source, name) {
                crate::folders::Profile::Found(_, t) => Some((t.file_path.clone(), t.file.clone())),
                crate::folders::Profile::Missing | crate::folders::Profile::Several(_) => None,
            }
        });
        // A tunnel (not a Kubernetes profile, which opens no SSH) is checked against the
        // `known_hosts` of the folder it reads, as `Resolver::team_known_hosts` reads them.
        let ssh = tunnel
            .as_ref()
            .is_some_and(|(_, file)| file.kubernetes.is_none());
        let known_hosts = match (ssh, folders.as_slice()) {
            (true, [folder]) => folder
                .workspace_root
                .as_deref()
                .and_then(|root| std::fs::read(crate::trust::team_known_hosts(root)).ok()),
            _ => None,
        };
        Some(ConnectionDefinition {
            file_path: entry.file_path.clone(),
            file: entry.file.clone(),
            tunnel,
            user: self.user.clone(),
            user_unreadable: self.user_unreadable,
            types: [
                TypesKey::of(&self.team_types),
                TypesKey::of(&self.user_types),
            ],
            workspace_root: self.workspace_root.clone(),
            trusted,
            open_folders,
            known_hosts,
        })
    }
}
