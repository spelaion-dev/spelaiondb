use std::fmt;
use std::path::{Path, PathBuf};

/// A problem in a configuration file, with its position so the editor can show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: PathBuf,
    /// Line and column (1-based), if known.
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub message: String,
}

impl ConfigError {
    pub fn new(file: &Path, message: impl Into<String>) -> Self {
        Self {
            file: file.to_path_buf(),
            line: None,
            column: None,
            message: message.into(),
        }
    }

    /// Error with the position of a byte range within the file contents.
    pub fn at(file: &Path, content: &str, offset: usize, message: impl Into<String>) -> Self {
        let before = &content[..offset.min(content.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        Self {
            file: file.to_path_buf(),
            line: Some(line),
            column: Some(column),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
            if let Some(column) = self.column {
                write!(f, ":{column}")?;
            }
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for ConfigError {}

/// Why a connection could not be resolved for connecting.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The connection comes from a repository the user has not trusted yet.
    #[error("you must trust the repository {} before its secrets or commands can be used", root.display())]
    Untrusted { root: PathBuf },

    /// A repository connection or tunnel is new or changed since the user trusted the
    /// repository (ADR-022): nothing was resolved.
    #[error("{change}. Review the change in SpelunkDB or with `spelunk trust` before using it")]
    Changed {
        root: PathBuf,
        change: crate::trust::ItemChange,
    },

    /// The connection (or its tunnel) uses `${user.…}` variables the user has not defined:
    /// all of them, so they can be asked for at once (ADR-022). Nothing was resolved.
    #[error(
        "this connection needs {}: add {} to the [vars] section of your user.toml, or open SpelunkDB to fill {} in",
        names.iter().map(|n| format!("${{user.{n}}}")).collect::<Vec<_>>().join(", "),
        if names.len() == 1 { "it" } else { "them" },
        if names.len() == 1 { "it" } else { "them" }
    )]
    MissingVariables { names: Vec<String> },

    #[error("{0}")]
    Config(#[from] ConfigError),

    #[error("secret {reference}: {message}")]
    Secret { reference: String, message: String },
}
