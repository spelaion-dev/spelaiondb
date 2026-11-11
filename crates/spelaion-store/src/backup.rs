//! The backup dialog's remembered choices (ADR-049): the folder of each family's client tools
//! (empty: look for them), the last output folder and the file name pattern.

use crate::{Store, StoreError};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackupPreferences {
    /// The folder with `pg_dump`, `pg_restore` and `psql`; empty: found automatically.
    pub postgres_tools: String,
    /// The folder with `mysqldump` and `mysql`; empty: found automatically.
    pub mysql_tools: String,
    /// Where the last backup was written.
    pub folder: String,
    /// The file name pattern; empty: the core's default.
    pub name_pattern: String,
}

/// The longest path or pattern kept.
pub const BACKUP_TEXT_MAX: usize = 1000;

impl BackupPreferences {
    pub fn validate(&self) -> Result<(), StoreError> {
        for (what, value) in [
            ("tools folder", &self.postgres_tools),
            ("tools folder", &self.mysql_tools),
            ("output folder", &self.folder),
            ("file name pattern", &self.name_pattern),
        ] {
            if value.len() > BACKUP_TEXT_MAX || value.chars().any(char::is_control) {
                return Err(StoreError(format!(
                    "the {what} is at most {BACKUP_TEXT_MAX} characters, on one line"
                )));
            }
        }
        Ok(())
    }
}

impl Store {
    /// The remembered choices; one that no longer passes the checks reads as empty.
    pub fn get_backup_preferences(&self) -> Result<BackupPreferences, StoreError> {
        let read = |key: &str| -> Result<String, StoreError> {
            Ok(self.personal_setting(key)?.unwrap_or_default())
        };
        let preferences = BackupPreferences {
            postgres_tools: read("backup.postgres_tools")?,
            mysql_tools: read("backup.mysql_tools")?,
            folder: read("backup.folder")?,
            name_pattern: read("backup.name_pattern")?,
        };
        Ok(if preferences.validate().is_ok() {
            preferences
        } else {
            BackupPreferences::default()
        })
    }

    pub fn set_backup_preferences(
        &self,
        preferences: &BackupPreferences,
    ) -> Result<(), StoreError> {
        preferences.validate()?;
        let transaction = self.conn.unchecked_transaction()?;
        self.set_personal_setting("backup.postgres_tools", preferences.postgres_tools.trim())?;
        self.set_personal_setting("backup.mysql_tools", preferences.mysql_tools.trim())?;
        self.set_personal_setting("backup.folder", preferences.folder.trim())?;
        self.set_personal_setting("backup.name_pattern", preferences.name_pattern.trim())?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_choices_are_remembered_and_checked() {
        let path =
            std::env::temp_dir().join(format!("spelaion-backup-prefs-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_backup_preferences().unwrap(),
            BackupPreferences::default()
        );
        let chosen = BackupPreferences {
            postgres_tools: r"C:\Program Files\PostgreSQL\17\bin".into(),
            mysql_tools: String::new(),
            folder: "/home/me/backups".into(),
            name_pattern: "{database}_{date}".into(),
        };
        store.set_backup_preferences(&chosen).unwrap();
        assert_eq!(store.get_backup_preferences().unwrap(), chosen);
        let bad = BackupPreferences {
            folder: "a\nb".into(),
            ..chosen.clone()
        };
        assert!(store.set_backup_preferences(&bad).is_err());
        assert_eq!(store.get_backup_preferences().unwrap(), chosen);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
