//! Moving a connection to another folder: drag and drop in the explorer, and "Move to
//! folder…" in its menu.
//!
//! A connection's folders are the directories of its file, so moving it moves the file (under
//! `.spelaion/connections/` for a repository connection, which the team then commits). The
//! file keeps its name, its layer and every byte; only its identifier changes. What hangs on
//! the identifier follows it here, in the core:
//!
//! - **The keychain password** stored under the connection's default entry goes to the new
//!   entry and leaves the old one, so a later connection of the old name never inherits it. An
//!   entry left under the new name by an older connection is never sent to this one's server.
//! - **Repository trust (ADR-022):** the pin moves with the file, as it was
//!   ([`TrustStore::move_connection`]). A move never adds trust: a connection as the user
//!   trusted it stays usable, one changed since is refused until reviewed, and a new one stays
//!   new. A move is never a review.
//!
//! A file changed on disk since it was loaded is not moved, and a move never replaces a file:
//! anything that fails puts back what was done.

use std::path::Path;

use crate::accounts::connection_account;
use crate::draft::{connections_dir, folder_parts, remove_empty_folders, uses_default_keychain};
use crate::error::{ConfigError, ResolveError};
use crate::load::{Config, ConnectionId, Source, validate_connection};
use crate::model::ConnectionFile;
use crate::secrets::{SecretBackend, forget};
use crate::trust::TrustStore;

/// A connection moved by [`move_connection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// Its identifier now (the same as before when it already was in that folder).
    pub id: ConnectionId,
    /// Its keychain password went to the new identifier's entry.
    pub password_moved: bool,
}

/// Moves a connection to `folder` (`""` for no folder, `prod/eu` for nested folders; each part
/// is made a slug, as in the connection form), in the same layer. See the module
/// documentation for its password and trust.
pub fn move_connection(
    config: &Config,
    trust: &mut TrustStore,
    id: &ConnectionId,
    folder: &str,
    backend: &dyn SecretBackend,
) -> Result<Moved, ResolveError> {
    let entry = config.connection(id).ok_or_else(|| {
        ConfigError::new(Path::new(&id.to_string()), "the connection does not exist")
    })?;
    let stem = id.path.rsplit('/').next().unwrap_or(&id.path).to_string();
    let mut parts = folder_parts(folder)?;
    parts.push(stem.clone());
    let to = ConnectionId {
        source: id.source,
        path: parts.join("/"),
    };
    if &to == id {
        return Ok(Moved {
            id: to,
            password_moved: false,
        });
    }
    let dir = connections_dir(config, id.source)?;
    let target = dir.join(format!("{}.toml", to.path));
    if config.connection(&to).is_some() || target.exists() {
        return Err(ConfigError::new(
            &target,
            format!("that folder already has a connection file named {stem}.toml"),
        )
        .into());
    }

    // The file as it was loaded, byte for byte: one changed on disk since is not moved.
    let source = &entry.file_path;
    let bytes = std::fs::read(source)
        .map_err(|e| ConfigError::new(source, format!("could not read connection: {e}")))?;
    let text = String::from_utf8(bytes.clone())
        .map_err(|_| ConfigError::new(source, "the connection file is not UTF-8"))?;
    let on_disk: ConnectionFile = toml::from_str(&text)
        .map_err(|e| ConfigError::new(source, format!("could not read connection: {e}")))?;
    if validate_connection(source, on_disk)? != entry.file {
        return Err(
            ConfigError::new(source, "the connection changed; refresh the explorer").into(),
        );
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
    }
    // A new file, never one over another (a move never replaces a connection), with the
    // moved file's mode.
    if let Err(e) = crate::atomic_file::create_like(&target, &bytes, source) {
        remove_empty_folders(&dir, &target);
        let message = match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                format!("that folder already has a connection file named {stem}.toml")
            }
            _ => format!("could not move the connection: {e}"),
        };
        return Err(ConfigError::new(&target, message).into());
    }
    let undo_file = || {
        let _ = std::fs::remove_file(&target);
        remove_empty_folders(&dir, &target);
    };

    let old_account = connection_account(config, id);
    let new_account = connection_account(config, &to);
    let mut password_moved = false;
    // Without a system keychain there is nothing to move: it asks, as it did.
    if uses_default_keychain(&entry.file) && backend.keychain_available() {
        let stored = match backend.keychain_get(&old_account) {
            Ok(Some(secret)) => {
                password_moved = true;
                crate::secrets::store(backend, &new_account, &secret)
            }
            // Nothing stored: an entry left under the new name must not reach this server.
            Ok(None) => forget(backend, &new_account),
            // A keychain that exists but fails (locked, refused) is an error.
            Err(message) => Err(message),
        };
        if let Err(message) = stored {
            undo_file();
            return Err(ResolveError::Secret {
                reference: format!("keychain:{new_account}"),
                message,
            });
        }
    }
    let undo_password = || {
        if password_moved {
            let _ = forget(backend, &new_account);
        }
    };

    if id.source == Source::Workspace
        && let Err(e) = trust.move_connection(config, id, &to)
    {
        undo_password();
        undo_file();
        return Err(e.into());
    }

    if let Err(e) = std::fs::remove_file(source) {
        if id.source == Source::Workspace {
            let _ = trust.move_connection(config, &to, id);
        }
        undo_password();
        undo_file();
        return Err(ConfigError::new(source, format!("could not move the connection: {e}")).into());
    }
    remove_empty_folders(&dir, source);

    if password_moved {
        // The move is done: a failure here only leaves the old entry behind, said plainly.
        forget(backend, &old_account).map_err(|message| ResolveError::Secret {
            reference: format!("keychain:{old_account}"),
            message: format!("the connection moved, but its old password entry stayed: {message}"),
        })?;
    }
    Ok(Moved {
        id: to,
        password_moved,
    })
}
