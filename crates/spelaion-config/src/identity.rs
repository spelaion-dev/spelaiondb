//! What identifies a connection (ADR-075, SDB-495): the folder it is defined in and its id.
//!
//! An id (`prod/orders`, or `~/local` for a personal connection) says which layer and which
//! file, not which repository: two repositories may each have a `prod/orders`. A
//! [`ConnectionKey`] adds the folder, the repository's canonical root, or none for a personal
//! connection, which is the same in every folder. It never goes inside the id's text, and no
//! folder is ever parsed out of an id. A PostgreSQL connection's database sits beside this
//! identity, never inside it (ADR-074).
//!
//! The local store's keys and the keychain's names come from here. They are the ones written
//! before this type existed, so nothing is migrated: the store's `workspace` column is the
//! folder's text, or empty for a personal connection, and a keychain entry is
//! `repo:<root>/<id>/password`, or `~/<id>/password`.

use std::path::{Path, PathBuf};

use crate::load::{Config, ConnectionId, Source};

/// A connection's identity: its folder and its id.
///
/// A personal connection has no folder, and a repository connection always has one: the two
/// cannot be mixed up (`new` refuses a mismatch).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    folder: Option<PathBuf>,
    id: ConnectionId,
}

impl ConnectionKey {
    /// The identity of `id` in the folder `folder` (already canonical). `None` when they do
    /// not match: a personal connection given a folder, or a repository one given none.
    pub fn new(folder: Option<PathBuf>, id: ConnectionId) -> Option<Self> {
        match (id.source, &folder) {
            (Source::User, None) | (Source::Workspace, Some(_)) => Some(Self { folder, id }),
            (Source::User, Some(_)) | (Source::Workspace, None) => None,
        }
    }

    /// The identity of `id` as `config` loads it: a repository connection's folder is the
    /// open repository's canonical root.
    pub fn of(config: &Config, id: &ConnectionId) -> Self {
        let folder = match id.source {
            Source::User => None,
            // A repository id without a repository is not something `load` makes; it keeps an
            // empty folder rather than passing for a personal connection.
            Source::Workspace => Some(
                config
                    .workspace_root
                    .as_deref()
                    .map_or_else(PathBuf::new, canonical_folder),
            ),
        };
        Self {
            folder,
            id: id.clone(),
        }
    }

    /// The repository's canonical root, or `None` for a personal connection.
    pub fn folder(&self) -> Option<&Path> {
        self.folder.as_deref()
    }

    pub fn id(&self) -> &ConnectionId {
        &self.id
    }

    /// The key the local store keeps this connection's things under (history, saved queries,
    /// explorer filters and bookmarks, variables, table filters, the agents' override, open
    /// tabs): the folder's text, or empty for a personal connection.
    pub fn store_scope(&self) -> String {
        folder_scope(self.folder.as_deref())
    }

    /// The keychain entry of its password: `~/<id>/password` or `repo:<root>/<id>/password`.
    pub fn keychain_account(&self) -> String {
        format!(
            "{}/{}/password",
            keychain_scope(self.folder.as_deref()),
            self.id.path
        )
    }
}

/// The store key of a folder's own things, or of the personal ones (`None`): what
/// [`ConnectionKey::store_scope`] answers for each of its connections.
pub fn folder_scope(folder: Option<&Path>) -> String {
    folder.map_or_else(String::new, |folder| folder.display().to_string())
}

/// The store key of the repository `config` has open, or empty when it has none.
pub fn repository_scope(config: &Config) -> String {
    config
        .workspace_root
        .as_deref()
        .map_or_else(String::new, |root| {
            folder_scope(Some(&canonical_folder(root)))
        })
}

/// A folder as identities name it: canonical, or as given when it cannot be resolved.
pub fn canonical_folder(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The first part of a keychain entry's name: `~`, or `repo:` and the folder.
pub(crate) fn keychain_scope(folder: Option<&Path>) -> String {
    match folder {
        None => "~".to_string(),
        Some(folder) => format!("repo:{}", folder.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repository(path: &str) -> ConnectionId {
        ConnectionId {
            source: Source::Workspace,
            path: path.into(),
        }
    }

    fn personal(path: &str) -> ConnectionId {
        ConnectionId {
            source: Source::User,
            path: path.into(),
        }
    }

    #[test]
    fn two_repositories_with_the_same_id_are_two_connections() {
        let a = ConnectionKey::new(
            Some(PathBuf::from("/work/service-a")),
            repository("prod/orders"),
        )
        .unwrap();
        let b = ConnectionKey::new(
            Some(PathBuf::from("/work/service-b")),
            repository("prod/orders"),
        )
        .unwrap();
        assert_ne!(a, b);
        assert_eq!(a.id(), b.id());
        assert_ne!(a.store_scope(), b.store_scope());
        assert_ne!(a.keychain_account(), b.keychain_account());
        assert_eq!(
            a.keychain_account(),
            format!(
                "repo:{}/prod/orders/password",
                Path::new("/work/service-a").display()
            )
        );
        // The id's text stays the id: no folder ever goes into it.
        assert_eq!(a.id().to_string(), "prod/orders");
    }

    #[test]
    fn a_personal_connection_has_no_folder_and_is_the_same_everywhere() {
        let key = ConnectionKey::new(None, personal("local")).unwrap();
        assert_eq!(key.folder(), None);
        assert_eq!(key.store_scope(), "");
        assert_eq!(key.keychain_account(), "~/local/password");
        assert_eq!(key.id().to_string(), "~/local");
    }

    #[test]
    fn a_folder_and_a_layer_that_disagree_are_refused() {
        assert!(ConnectionKey::new(Some(PathBuf::from("/work/a")), personal("local")).is_none());
        assert!(ConnectionKey::new(None, repository("prod/orders")).is_none());
    }

    #[test]
    fn a_loaded_connection_takes_the_open_repository_as_its_folder() {
        let dir = std::env::temp_dir().join(format!("spelaion-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            workspace_root: Some(dir.clone()),
            ..Config::default()
        };
        let key = ConnectionKey::of(&config, &repository("prod/orders"));
        let root = std::fs::canonicalize(&dir).unwrap();
        let scope = repository_scope(&config);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(key.folder(), Some(root.as_path()));
        assert_eq!(key.store_scope(), root.display().to_string());
        assert_eq!(scope, key.store_scope());
        assert_eq!(
            ConnectionKey::of(&config, &personal("local")).folder(),
            None
        );
        assert_eq!(repository_scope(&Config::default()), "");
    }
}
