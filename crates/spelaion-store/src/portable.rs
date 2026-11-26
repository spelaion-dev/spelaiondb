//! The personal settings that move to another computer with the settings file (SDB-31): the
//! rows of `personal_settings` that describe how the user likes the app, never those that name
//! something of this computer (a folder, a program's path, the folders opened lately).
//!
//! The store only says which rows are portable, hands them out and replaces them whole, in one
//! transaction whose triggers still refuse a value they do not know. What a value means is
//! checked by the core before it gets here and again whenever it is read, as for a value saved
//! from Settings: an unusable value reads as its default.

use rusqlite::params;

use crate::{Store, StoreError};

/// Rows kept by their exact key that move to another computer.
const PORTABLE_KEYS: &[&str] = &[
    "language",
    "theme",
    "interface_size",
    "update_check",
    "connection_name_pattern",
    "results.kept_tabs",
    "backup.name_pattern",
];

/// Families of rows that move, by the start of their key. Every key written under one of these
/// is a preference of the app, not of this computer.
const PORTABLE_PREFIXES: &[&str] = &[
    "editor.",
    "grid.",
    "display.",
    "copy.",
    "export.",
    "script.",
    "map.",
    "side_panel.",
    "explorer.",
    "shortcut:",
    "mcp.",
];

/// Rows that stay on this computer: they name its folders and programs.
const THIS_COMPUTER_KEYS: &[&str] = &[
    "recent_folders",
    "backup.folder",
    "backup.postgres_tools",
    "backup.mysql_tools",
];

/// The MCP read-only mode (ADR-033): an import never turns it off.
const MCP_READ_ONLY: &str = "mcp.read_only";

/// Longest key and value a portable row may have. The longest values are editor snippets and
/// the new script template, which the core bounds well below this.
pub const PORTABLE_KEY_MAX: usize = 128;
pub const PORTABLE_VALUE_MAX: usize = 256 * 1024;

/// Whether a row moves with the settings file.
pub fn is_portable_setting(key: &str) -> bool {
    !THIS_COMPUTER_KEYS.contains(&key)
        && key.len() <= PORTABLE_KEY_MAX
        && (PORTABLE_KEYS.contains(&key)
            || PORTABLE_PREFIXES
                .iter()
                .any(|prefix| key.len() > prefix.len() && key.starts_with(prefix)))
}

/// Whether a row is one that stays on this computer by design (for the test that every key the
/// store writes is one or the other).
pub fn is_this_computer_setting(key: &str) -> bool {
    THIS_COMPUTER_KEYS.contains(&key)
}

/// What replacing the portable settings did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortableReplace {
    /// Rows written from the file.
    pub written: usize,
    /// The file had the MCP read-only mode off (or not set) while it was on here, and it stayed
    /// on: an import never lets agents write where they could not.
    pub kept_mcp_read_only: bool,
}

impl Store {
    /// Every portable row, by key.
    pub fn portable_settings(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut statement = self
            .conn
            .prepare("select key, value from personal_settings order by key")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut portable = Vec::new();
        for row in rows {
            let (key, value) = row?;
            if is_portable_setting(&key) {
                portable.push((key, value));
            }
        }
        Ok(portable)
    }

    /// Replaces every portable row with `rows`, in one transaction: a portable setting the rows
    /// leave out goes back to its default, and the rows of this computer stay. A row that is not
    /// portable, too long, or that a trigger refuses fails the whole replace and nothing changes.
    /// The MCP read-only mode stays on when it is on here.
    pub fn replace_portable_settings(
        &self,
        rows: &[(String, String)],
    ) -> Result<PortableReplace, StoreError> {
        if let Some((key, _)) = rows.iter().find(|(key, _)| !is_portable_setting(key)) {
            return Err(StoreError::new(format!(
                "{key:?} is not a setting that moves to another computer"
            )));
        }
        if let Some((key, _)) = rows
            .iter()
            .find(|(_, value)| value.len() > PORTABLE_VALUE_MAX)
        {
            return Err(StoreError::new(format!("the value of {key:?} is too long")));
        }
        let read_only_here = self.mcp_read_only()?;
        let transaction = self.conn.unchecked_transaction()?;
        let existing: Vec<String> = {
            let mut statement = transaction.prepare("select key from personal_settings")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<_, _>>()?
        };
        for key in existing.iter().filter(|key| is_portable_setting(key)) {
            transaction.execute("delete from personal_settings where key = ?1", [key])?;
        }
        for (key, value) in rows {
            transaction.execute(
                "insert into personal_settings (key, value) values (?1, ?2)
                 on conflict (key) do update set value = excluded.value",
                params![key, value],
            )?;
        }
        let read_only_in_file = rows
            .iter()
            .find(|(key, _)| key == MCP_READ_ONLY)
            .is_some_and(|(_, value)| value != "off");
        let kept_mcp_read_only = read_only_here && !read_only_in_file;
        if kept_mcp_read_only {
            transaction.execute(
                "insert into personal_settings (key, value) values (?1, 'on')
                 on conflict (key) do update set value = excluded.value",
                [MCP_READ_ONLY],
            )?;
        }
        transaction.commit()?;
        Ok(PortableReplace {
            written: rows.len(),
            kept_mcp_read_only,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-portable-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (Store::open(&dir.join("state.db")).unwrap(), dir)
    }

    /// A folder path that is absolute on every system, as a recent folder must be.
    fn work() -> String {
        std::env::temp_dir().join("work").display().to_string()
    }

    fn rows(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn this_computer_keys_never_move_and_portable_ones_do() {
        for key in THIS_COMPUTER_KEYS {
            assert!(!is_portable_setting(key), "{key}");
        }
        for key in [
            "theme",
            "editor.font",
            "editor.snippet:sel",
            "shortcut:editor.run",
            "mcp.read_only",
        ] {
            assert!(is_portable_setting(key), "{key}");
        }
        for key in ["editor.", "unknown", "backup.folder", "shortcut:"] {
            assert!(!is_portable_setting(key), "{key}");
        }
    }

    #[test]
    fn every_key_the_store_writes_is_portable_or_of_this_computer() {
        // Every setter of a personal setting, with values that write every key it has. A new
        // setting fails here until it is put on one side or the other.
        let (store, dir) = store("every-key");
        store.set_language("es").unwrap();
        store.set_theme("dark").unwrap();
        store.set_interface_size(110).unwrap();
        store.set_update_check(false).unwrap();
        store.add_recent_folder(&work()).unwrap();
        store.set_connection_name_pattern(Some("{engine}")).unwrap();
        store
            .set_explorer_preferences(&crate::ExplorerPreferences::default())
            .unwrap();
        store
            .set_editor_preferences(&crate::EditorPreferences::default())
            .unwrap();
        store.set_reconnect_on_start(true).unwrap();
        store
            .set_grid_preferences(&crate::GridPreferences::default())
            .unwrap();
        store
            .set_display_preferences(&crate::DisplayPreferences::default())
            .unwrap();
        store
            .set_copy_preferences(&crate::CopyPreferences::default())
            .unwrap();
        store
            .set_export_preferences(&crate::ExportPreferences::default())
            .unwrap();
        store
            .set_script_preferences(&crate::ScriptPreferences::default())
            .unwrap();
        store
            .set_map_preferences(&crate::MapPreferences::default())
            .unwrap();
        store
            .set_side_panel_preferences(&crate::SidePanelPreferences::default())
            .unwrap();
        store
            .set_backup_preferences(&crate::BackupPreferences {
                folder: "C:/backups".into(),
                postgres_tools: "C:/pg/bin".into(),
                mysql_tools: "C:/mysql/bin".into(),
                name_pattern: "{database}".into(),
            })
            .unwrap();
        store.set_mcp_read_only(true).unwrap();
        store
            .set_mcp_output_format(crate::ToolOutput::Toon)
            .unwrap();
        store
            .set_shortcut_overrides(&[crate::ShortcutOverride {
                command: "editor.run".into(),
                keys: Some("Ctrl+R".into()),
            }])
            .unwrap();
        store
            .put_editor_snippet(&crate::EditorSnippet {
                name: "sel".into(),
                description: String::new(),
                body: "select 1".into(),
            })
            .unwrap();
        store.set_new_script_template("-- new").unwrap();

        let mut statement = store
            .conn
            .prepare("select key from personal_settings")
            .unwrap();
        let keys: Vec<String> = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(keys.len() > 40, "{keys:?}");
        for key in &keys {
            assert!(
                is_portable_setting(key) || is_this_computer_setting(key),
                "{key} is neither portable nor of this computer"
            );
        }
        drop(statement);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_replace_is_whole_keeps_this_computer_and_leaves_out_means_default() {
        let (store, dir) = store("replace");
        store.set_theme("dark").unwrap();
        store.add_recent_folder(&work()).unwrap();
        store
            .set_shortcut_overrides(&[crate::ShortcutOverride {
                command: "tab.new".into(),
                keys: Some("Ctrl+N".into()),
            }])
            .unwrap();
        let done = store
            .replace_portable_settings(&rows(&[("language", "es"), ("editor.font", "Fira")]))
            .unwrap();
        assert_eq!(done.written, 2);
        assert_eq!(store.get_language().unwrap(), "es");
        assert_eq!(
            store.get_theme().unwrap(),
            "system",
            "left out: the default"
        );
        assert_eq!(store.shortcut_overrides().unwrap(), vec![]);
        assert_eq!(store.recent_folders().unwrap(), vec![work()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_row_changes_nothing() {
        let (store, dir) = store("refused");
        store.set_theme("dark").unwrap();
        for bad in [
            rows(&[("language", "es"), ("theme", "blue")]),
            rows(&[("language", "es"), ("recent_folders", "C:/x")]),
            rows(&[("language", "es"), ("not.a.setting", "x")]),
            rows(&[("language", "es"), ("mcp.read_only", "maybe")]),
            vec![
                ("language".into(), "es".into()),
                ("editor.font".into(), "x".repeat(PORTABLE_VALUE_MAX + 1)),
            ],
        ] {
            assert!(store.replace_portable_settings(&bad).is_err(), "{bad:?}");
            assert_eq!(store.get_theme().unwrap(), "dark");
            assert_eq!(store.get_language().unwrap(), "system");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_import_never_turns_the_mcp_read_only_mode_off() {
        let (store, dir) = store("mcp");
        store.set_mcp_read_only(true).unwrap();
        for file in [rows(&[("mcp.read_only", "off")]), rows(&[])] {
            let done = store.replace_portable_settings(&file).unwrap();
            assert!(done.kept_mcp_read_only);
            assert!(store.mcp_read_only().unwrap());
        }
        let done = store
            .replace_portable_settings(&rows(&[("mcp.read_only", "on")]))
            .unwrap();
        assert!(!done.kept_mcp_read_only);
        assert!(store.mcp_read_only().unwrap());
        // Off here: the file may turn it on.
        store.set_mcp_read_only(false).unwrap();
        store
            .replace_portable_settings(&rows(&[("mcp.read_only", "on")]))
            .unwrap();
        assert!(store.mcp_read_only().unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
