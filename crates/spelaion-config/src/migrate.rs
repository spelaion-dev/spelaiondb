//! Moving to the Spelaion names (ADR-023) without losing anything. The old personal folder
//! is copied (never moved) on the first start, keychain entries are copied the first time
//! they are read, and a repository's `.spelunk/` is still read during the 0.4 versions.

use std::path::{Path, PathBuf};

use crate::error::ConfigError;

/// The repository folder's name before ADR-023: still read in 0.4, with a notice.
pub const LEGACY_WORKSPACE_DIR: &str = ".spelunk";

/// The keychain service before ADR-023: entries are copied from it the first time they are
/// read.
pub(crate) const LEGACY_KEYCHAIN_SERVICE: &str = "SpelunkDB";

/// The personal folder's name before ADR-023, next to the new one.
const LEGACY_USER_DIR: &str = "spelunkdb";

/// The product's own local database (history, audit, settings) in the personal folder:
/// under `db/`, so the folder can be shared with the brand's other products (ADR-023).
pub fn state_db(user_dir: &Path) -> PathBuf {
    user_dir.join("db").join("state.db")
}

/// The personal folder before ADR-023, for the default one (`…/spelaion` → `…/spelunkdb`).
pub fn legacy_user_dir() -> Option<PathBuf> {
    crate::load::default_user_dir()
        .and_then(|dir| dir.parent().map(|parent| parent.join(LEGACY_USER_DIR)))
}

/// On the first start after the rename: copies the old personal folder to the new one, with
/// the local database under `db/`. The old folder stays as it was, as a backup. Nothing is
/// done if the new folder already exists or there is no old one. Returns whether it copied.
pub fn migrate_user_dir(old: &Path, new: &Path) -> Result<bool, ConfigError> {
    if new.exists() || !old.is_dir() {
        return Ok(false);
    }
    // Copied next to its final place and renamed at the end: an interruption never leaves
    // a half-copied folder that would stop the next start from copying again.
    let staging = new.with_extension("migrating");
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| ConfigError::new(&staging, e.to_string()))?;
    }
    copy_dir(old, &staging)?;
    let db = staging.join("db");
    for name in ["state.db", "state.db-wal", "state.db-shm"] {
        let from = staging.join(name);
        if from.is_file() {
            std::fs::create_dir_all(&db).map_err(|e| ConfigError::new(&db, e.to_string()))?;
            std::fs::rename(&from, db.join(name))
                .map_err(|e| ConfigError::new(&from, e.to_string()))?;
        }
    }
    std::fs::rename(&staging, new).map_err(|e| ConfigError::new(new, e.to_string()))?;
    Ok(true)
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), ConfigError> {
    std::fs::create_dir_all(to).map_err(|e| ConfigError::new(to, e.to_string()))?;
    let entries = std::fs::read_dir(from).map_err(|e| ConfigError::new(from, e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| ConfigError::new(from, e.to_string()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| ConfigError::new(&source, e.to_string()))?;
        if kind.is_dir() {
            copy_dir(&source, &target)?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target)
                .map_err(|e| ConfigError::new(&source, e.to_string()))?;
        }
    }
    Ok(())
}

/// Reads a keychain entry, falling back to the old service: an entry found there is copied
/// to the new one (and left where it was). `get` and `set` take the service and the account.
pub(crate) fn keychain_get_or_migrate(
    account: &str,
    service: &str,
    get: impl Fn(&str, &str) -> Result<Option<String>, String>,
    set: impl Fn(&str, &str, &str) -> Result<(), String>,
) -> Result<Option<String>, String> {
    if let Some(secret) = get(service, account)? {
        return Ok(Some(secret));
    }
    match get(LEGACY_KEYCHAIN_SERVICE, account)? {
        Some(secret) => {
            // Not copying is not a reason to fail: the old entry still works.
            let _ = set(service, account, &secret);
            Ok(Some(secret))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    #[test]
    fn old_keychain_entries_are_copied_on_first_read_and_kept() {
        let store = RefCell::new(BTreeMap::from([
            (
                ("SpelunkDB".to_string(), "~/orders/password".to_string()),
                "old-secret".to_string(),
            ),
            (
                ("Spelaion".to_string(), "~/new/password".to_string()),
                "new-secret".to_string(),
            ),
        ]));
        let get = |service: &str, account: &str| {
            Ok(store
                .borrow()
                .get(&(service.to_string(), account.to_string()))
                .cloned())
        };
        let set = |service: &str, account: &str, secret: &str| {
            store.borrow_mut().insert(
                (service.to_string(), account.to_string()),
                secret.to_string(),
            );
            Ok(())
        };
        let read = |account: &str| keychain_get_or_migrate(account, "Spelaion", get, set);
        assert_eq!(
            read("~/new/password").unwrap().as_deref(),
            Some("new-secret")
        );
        assert_eq!(
            read("~/orders/password").unwrap().as_deref(),
            Some("old-secret")
        );
        let store = store.borrow();
        assert_eq!(
            store
                .get(&("Spelaion".into(), "~/orders/password".into()))
                .map(String::as_str),
            Some("old-secret"),
            "copied to the new service"
        );
        assert!(
            store.contains_key(&("SpelunkDB".into(), "~/orders/password".into())),
            "the old entry stays"
        );
        drop(store);
        assert_eq!(read("~/missing/password").unwrap(), None);
    }
}
