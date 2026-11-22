//! The user's changes to the app's keyboard shortcuts (SDB-447, ADR-064): a personal
//! preference kept with the other personal settings, one row per changed command, shared by
//! every workspace of this user directory. Only the differences from the app's defaults are
//! kept: a command's id with the keys the user gave it, or with none.
//!
//! The store keeps what it is given: which keys a command may take is decided by the core
//! (`spelaion-core`'s `shortcuts.rs`) before it saves and again when it reads. Only the
//! command's id is checked here, because it is part of the row's key.

use rusqlite::params;

use crate::{Store, StoreError};

/// The key of a changed command's row is this and the command's id.
const SHORTCUT_KEY: &str = "shortcut:";

/// Longest command id a row's key may carry.
pub const SHORTCUT_COMMAND_MAX: usize = 64;

/// One command whose keys the user changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutOverride {
    /// The command's id in the app's registry (`tab.new`, `editor.run`).
    pub command: String,
    /// The keys as the registry writes them (`Ctrl+Shift+Enter`), or `None` for no shortcut.
    pub keys: Option<String>,
}

/// Whether `command` can be a command's id: ASCII letters, digits and dots, starting with a
/// letter, of at most [`SHORTCUT_COMMAND_MAX`] characters.
pub fn is_command_id(command: &str) -> bool {
    let mut chars = command.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '.')
        && command.len() <= SHORTCUT_COMMAND_MAX
}

impl Store {
    /// Every stored change, by command id. A row whose key is not a command's id is left out.
    pub fn shortcut_overrides(&self) -> Result<Vec<ShortcutOverride>, StoreError> {
        let mut statement = self.conn.prepare(
            "select key, value from personal_settings
             where substr(key, 1, ?1) = ?2 order by key",
        )?;
        let rows = statement
            .query_map(params![SHORTCUT_KEY.len() as i64, SHORTCUT_KEY], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
        let mut overrides = Vec::new();
        for row in rows {
            let (key, value) = row?;
            let command = &key[SHORTCUT_KEY.len()..];
            if !is_command_id(command) {
                continue;
            }
            overrides.push(ShortcutOverride {
                command: command.to_string(),
                keys: (!value.is_empty()).then_some(value),
            });
        }
        Ok(overrides)
    }

    /// Replaces every stored change with `overrides`, in one transaction: a command left out
    /// goes back to its default.
    pub fn set_shortcut_overrides(&self, overrides: &[ShortcutOverride]) -> Result<(), StoreError> {
        if let Some(bad) = overrides.iter().find(|o| !is_command_id(&o.command)) {
            return Err(StoreError::new(format!(
                "invalid command id {:?}",
                bad.command
            )));
        }
        let transaction = self.conn.unchecked_transaction()?;
        transaction.execute(
            "delete from personal_settings where substr(key, 1, ?1) = ?2",
            params![SHORTCUT_KEY.len() as i64, SHORTCUT_KEY],
        )?;
        for change in overrides {
            transaction.execute(
                "insert into personal_settings (key, value) values (?1, ?2)
                 on conflict (key) do update set value = excluded.value",
                params![
                    format!("{SHORTCUT_KEY}{}", change.command),
                    change.keys.as_deref().unwrap_or("")
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-shortcuts-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("state.db")).unwrap(), dir)
    }

    fn change(command: &str, keys: Option<&str>) -> ShortcutOverride {
        ShortcutOverride {
            command: command.into(),
            keys: keys.map(str::to_string),
        }
    }

    #[test]
    fn changes_are_replaced_whole_and_none_reads_as_no_shortcut() {
        let (store, dir) = store();
        assert_eq!(store.shortcut_overrides().unwrap(), vec![]);
        store
            .set_shortcut_overrides(&[
                change("editor.runScript", Some("Ctrl+Alt+F9")),
                change("tab.new", None),
            ])
            .unwrap();
        assert_eq!(
            store.shortcut_overrides().unwrap(),
            vec![
                change("editor.runScript", Some("Ctrl+Alt+F9")),
                change("tab.new", None),
            ]
        );
        store
            .set_shortcut_overrides(&[change("tab.close", Some("Ctrl+F4"))])
            .unwrap();
        assert_eq!(
            store.shortcut_overrides().unwrap(),
            vec![change("tab.close", Some("Ctrl+F4"))],
            "a command left out goes back to its default"
        );
        store.set_shortcut_overrides(&[]).unwrap();
        assert_eq!(store.shortcut_overrides().unwrap(), vec![]);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_bad_command_id_writes_nothing() {
        let (store, dir) = store();
        store
            .set_shortcut_overrides(&[change("tab.new", Some("Ctrl+N"))])
            .unwrap();
        for bad in ["", "1tab", "tab new", "tab:new", &"a".repeat(65)] {
            assert!(
                store
                    .set_shortcut_overrides(&[change("tab.close", None), change(bad, None)])
                    .is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(
            store.shortcut_overrides().unwrap(),
            vec![change("tab.new", Some("Ctrl+N"))]
        );
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }
}
