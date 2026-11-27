//! The personal configuration's share of the settings file (SDB-31): `user.toml` (variables,
//! the tunnels' port range and the masked columns) and the personal `connection-types.toml`.
//! Personal connections and network profiles are not here: they hold secrets and move in the
//! encrypted connections file (ADR-088).
//!
//! The file carries each one's text as it is on disk. An import reads it with the same checks
//! as when it loads and only adds to what this computer has, so it never loosens a safeguard:
//! - the file's `${user.…}` variables are set (one with the same name takes the file's value),
//!   and the others here stay;
//! - the tunnels' port range is the file's when it has one;
//! - the file's masked columns are added to the ones here, and none is removed (ADR-051);
//! - a connection type the user has not defined here is added; one defined here stays as it
//!   is, whatever the file says, because replacing it could turn a safeguard off (ADR-029).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;

use crate::atomic_file::{NewFileMode, replace};
use crate::error::ConfigError;
use crate::load::{CONNECTION_TYPES_FILE, validate_types};
use crate::model::{ConnectionTypeConfig, ConnectionTypesFile, UserFile};

const USER_FILE: &str = "user.toml";

/// The longest text of either file the settings file may carry.
pub const PERSONAL_FILE_MAX: usize = 1024 * 1024;

/// The two personal files' texts, as they are on disk; `None` when there is no such file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonalFiles {
    pub user: Option<String>,
    pub connection_types: Option<String>,
}

/// What importing the personal files would change here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonalFilesPlan {
    /// Variables that are new or get another value, by name.
    pub vars: BTreeMap<String, String>,
    /// The file's port range, when it has one and it is not the one here.
    pub tunnel_ports: Option<(u16, u16)>,
    /// Masked columns the file has and this computer does not.
    pub masking_added: Vec<String>,
    /// Connection types the user has not defined here, by id.
    pub types_added: Vec<(String, ConnectionTypeConfig)>,
    /// Connection types of the file that stay as this computer defines them.
    pub types_kept: Vec<String>,
}

impl PersonalFilesPlan {
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
            && self.tunnel_ports.is_none()
            && self.masking_added.is_empty()
            && self.types_added.is_empty()
    }
}

/// The personal files as they are, to put in a settings file.
pub fn read_personal_files(user_dir: &Path) -> Result<PersonalFiles, ConfigError> {
    let read = |name: &str| -> Result<Option<String>, ConfigError> {
        let path = user_dir.join(name);
        if !path.is_file() {
            return Ok(None);
        }
        std::fs::read_to_string(&path)
            .map(Some)
            .map_err(|e| ConfigError::new(&path, format!("could not read: {e}")))
    };
    Ok(PersonalFiles {
        user: read(USER_FILE)?,
        connection_types: read(CONNECTION_TYPES_FILE)?,
    })
}

fn parse_text<T: DeserializeOwned>(path: &Path, text: &str) -> Result<T, ConfigError> {
    if text.len() > PERSONAL_FILE_MAX {
        return Err(ConfigError::new(path, "the file is too large"));
    }
    toml::from_str(text).map_err(|e| match e.span() {
        Some(span) => ConfigError::at(path, text, span.start, e.message()),
        None => ConfigError::new(path, e.message()),
    })
}

/// A settings file's `user.toml`, checked as when it loads (the masking patterns, the port
/// range and the variables' names and values).
fn check_user_text(path: &Path, text: &str) -> Result<UserFile, ConfigError> {
    let user: UserFile = parse_text(path, text)?;
    crate::masking::check_patterns("masking.always", &user.masking.always)
        .map_err(|m| ConfigError::new(path, m))?;
    user.tunnels
        .range()
        .map_err(|m| ConfigError::new(path, m))?;
    for (name, value) in &user.vars {
        if !crate::vars::valid_var_name(name) || value.trim().is_empty() {
            return Err(ConfigError::new(
                path,
                format!("[vars] {name:?} is not a variable this version can set"),
            ));
        }
    }
    Ok(user)
}

/// What importing `files` would change in the personal folder `user_dir`. Every text is checked
/// as when it loads, and an error names the file, so nothing is written from a file that does
/// not load.
pub fn plan_personal_files(
    user_dir: &Path,
    files: &PersonalFiles,
) -> Result<PersonalFilesPlan, ConfigError> {
    let mut plan = PersonalFilesPlan::default();
    let user_path = user_dir.join(USER_FILE);
    let types_path = user_dir.join(CONNECTION_TYPES_FILE);
    let here = crate::load::load(None, user_dir);
    if let Some(text) = &files.user {
        let user = check_user_text(&user_path, text)?;
        for (name, value) in user.vars {
            if here.user.vars.get(&name) != Some(&value) {
                plan.vars.insert(name, value);
            }
        }
        let range = user.tunnels.range().ok().flatten();
        if range.is_some() && range != here.user.tunnels.range().ok().flatten() {
            plan.tunnel_ports = range;
        }
        for pattern in user.masking.always {
            let pattern = pattern.trim().to_string();
            if !pattern.is_empty()
                && !here.user.masking.always.iter().any(|p| p.trim() == pattern)
                && !plan.masking_added.contains(&pattern)
            {
                plan.masking_added.push(pattern);
            }
        }
    }
    if let Some(text) = &files.connection_types {
        let file: ConnectionTypesFile = parse_text(&types_path, text)?;
        let file = validate_types(&types_path, file)?;
        let mine = here.user_types.file().map(|f| &f.types);
        for (id, kind) in file.types {
            if mine.is_some_and(|types| types.contains_key(&id)) {
                plan.types_kept.push(id);
            } else {
                plan.types_added.push((id, kind));
            }
        }
    }
    Ok(plan)
}

/// The personal files as they were before an import wrote them, to put them back when a later
/// step of the import fails.
#[derive(Debug)]
pub struct PersonalFilesBefore {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl PersonalFilesBefore {
    fn take(user_dir: &Path) -> Result<Self, ConfigError> {
        let mut files = Vec::new();
        for name in [USER_FILE, CONNECTION_TYPES_FILE] {
            let path = user_dir.join(name);
            let bytes = if path.is_file() {
                Some(
                    std::fs::read(&path)
                        .map_err(|e| ConfigError::new(&path, format!("could not read: {e}")))?,
                )
            } else {
                None
            };
            files.push((path, bytes));
        }
        Ok(Self { files })
    }

    /// Writes each file back as it was (and removes one that did not exist).
    pub fn restore(self) -> Result<(), ConfigError> {
        let mut first_error = None;
        for (path, bytes) in self.files {
            let result = match bytes {
                Some(bytes) => replace(&path, &bytes, NewFileMode::OwnerOnly),
                None if path.exists() => std::fs::remove_file(&path),
                None => Ok(()),
            };
            if let Err(e) = result {
                first_error.get_or_insert(ConfigError::new(&path, e.to_string()));
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Writes a plan from [`plan_personal_files`], through the same writers as Settings (each keeps
/// the rest of its file, comments included). When one fails, the files are put back as they
/// were and its error is returned. On success, what they were before, for the caller to put
/// back if a later step of its import fails.
pub fn apply_personal_files(
    user_dir: &Path,
    plan: &PersonalFilesPlan,
) -> Result<PersonalFilesBefore, ConfigError> {
    let before = PersonalFilesBefore::take(user_dir)?;
    let result = (|| {
        if !plan.vars.is_empty() {
            crate::edit::set_user_vars(user_dir, &plan.vars)?;
        }
        if let Some(range) = plan.tunnel_ports {
            crate::edit::set_tunnel_ports(user_dir, Some(range))?;
        }
        if !plan.masking_added.is_empty() {
            let mut patterns = crate::load::load(None, user_dir).user.masking.always;
            patterns.extend(plan.masking_added.iter().cloned());
            crate::edit::set_user_masking(user_dir, &patterns)?;
        }
        for (id, kind) in &plan.types_added {
            crate::types_edit::set_personal_type(user_dir, id, kind)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(before),
        Err(e) => {
            // The error that stopped the import is the one to tell; a failure to put a file
            // back is rarer and leaves what the writers wrote, which loads.
            let _ = before.restore();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-settings-files-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const OLD_USER: &str = "schema_version = 1\n\n[vars]\ndb_user = \"pablo\"\nregion = \"eu\"\n\n[tunnels]\nport_min = 40000\nport_max = 40100\n\n[masking]\nalways = [\"*.email\", \"*.ssn\"]\n";
    const OLD_TYPES: &str = "schema_version = 1\n\n[types.staging]\nname = \"Staging\"\ntone = \"amber\"\nconfirm_data_changes = true\n\n[types.lab]\nname = \"Lab\"\ntone = \"green\"\n";

    #[test]
    fn a_round_trip_moves_variables_ports_masking_and_types() {
        let old = scratch("old");
        std::fs::write(old.join(USER_FILE), OLD_USER).unwrap();
        std::fs::write(old.join(CONNECTION_TYPES_FILE), OLD_TYPES).unwrap();
        let files = read_personal_files(&old).unwrap();

        let new = scratch("new");
        let plan = plan_personal_files(&new, &files).unwrap();
        assert_eq!(plan.vars.len(), 2);
        assert_eq!(plan.tunnel_ports, Some((40000, 40100)));
        assert_eq!(plan.masking_added, vec!["*.email", "*.ssn"]);
        assert_eq!(plan.types_added.len(), 2);
        apply_personal_files(&new, &plan).unwrap();

        let loaded = crate::load::load(None, &new);
        assert!(loaded.problems.is_empty(), "{:?}", loaded.problems);
        assert_eq!(loaded.user.vars.get("db_user").unwrap(), "pablo");
        assert_eq!(loaded.user.tunnels.range().unwrap(), Some((40000, 40100)));
        assert_eq!(loaded.user.masking.always, vec!["*.email", "*.ssn"]);
        let types = &loaded.user_types.file().unwrap().types;
        assert!(types["staging"].confirm_data_changes);
        assert!(types.contains_key("lab"));
        // Importing the same file again changes nothing.
        assert!(plan_personal_files(&new, &files).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(old);
        let _ = std::fs::remove_dir_all(new);
    }

    #[test]
    fn an_import_never_removes_a_masked_column_nor_replaces_a_type_here() {
        let new = scratch("loosen");
        std::fs::write(
            new.join(USER_FILE),
            "schema_version = 1\n\n[masking]\nalways = [\"*.card\"]\n",
        )
        .unwrap();
        std::fs::write(
            new.join(CONNECTION_TYPES_FILE),
            "schema_version = 1\n\n[types.staging]\nname = \"Staging\"\nconfirm_data_changes = true\n",
        )
        .unwrap();
        let files = PersonalFiles {
            user: Some("schema_version = 1\n\n[masking]\nalways = []\n".into()),
            connection_types: Some(
                "schema_version = 1\n\n[types.staging]\nname = \"Staging\"\nconfirm_data_changes = false\n"
                    .into(),
            ),
        };
        let plan = plan_personal_files(&new, &files).unwrap();
        assert_eq!(plan.types_kept, vec!["staging"]);
        assert!(plan.is_empty(), "{plan:?}");
        apply_personal_files(&new, &plan).unwrap();
        let loaded = crate::load::load(None, &new);
        assert_eq!(loaded.user.masking.always, vec!["*.card"]);
        assert!(loaded.user_types.file().unwrap().types["staging"].confirm_data_changes);
        let _ = std::fs::remove_dir_all(new);
    }

    #[test]
    fn a_file_that_does_not_load_is_refused_before_anything_is_written() {
        let new = scratch("refused");
        std::fs::write(new.join(USER_FILE), OLD_USER).unwrap();
        for files in [
            PersonalFiles {
                user: Some("schema_version = 1\nunknown_key = 1\n".into()),
                connection_types: None,
            },
            PersonalFiles {
                user: Some("schema_version = 1\n[tunnels]\nport_min = 5\n".into()),
                connection_types: None,
            },
            PersonalFiles {
                user: Some("schema_version = 1\n[vars]\n\"bad name\" = \"x\"\n".into()),
                connection_types: None,
            },
            PersonalFiles {
                user: None,
                connection_types: Some("schema_version = 1\n[types.BAD]\n".into()),
            },
            PersonalFiles {
                user: Some("x".repeat(PERSONAL_FILE_MAX + 1)),
                connection_types: None,
            },
        ] {
            assert!(plan_personal_files(&new, &files).is_err(), "{files:?}");
        }
        assert_eq!(
            std::fs::read_to_string(new.join(USER_FILE)).unwrap(),
            OLD_USER
        );
        let _ = std::fs::remove_dir_all(new);
    }

    #[test]
    fn restore_puts_the_files_back_as_they_were() {
        let new = scratch("restore");
        std::fs::write(new.join(USER_FILE), OLD_USER).unwrap();
        let files = PersonalFiles {
            user: None,
            connection_types: Some(OLD_TYPES.into()),
        };
        let plan = plan_personal_files(&new, &files).unwrap();
        let before = apply_personal_files(&new, &plan).unwrap();
        assert!(new.join(CONNECTION_TYPES_FILE).is_file());
        before.restore().unwrap();
        assert!(!new.join(CONNECTION_TYPES_FILE).exists());
        assert_eq!(
            std::fs::read_to_string(new.join(USER_FILE)).unwrap(),
            OLD_USER
        );
        let _ = std::fs::remove_dir_all(new);
    }
}
