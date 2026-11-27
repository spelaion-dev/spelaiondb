//! `@include`'s path rules and limits (ADR-078, point 7): which `.sql` files a script the
//! editor runs may put in itself, and how much of them.
//!
//! `@include` is the one client command that reads a file the user did not pick, so it is
//! confined to the script's own folder tree: the open folder when the tab's file is inside it,
//! otherwise that file's own folder. A path is refused by how it is written before the file
//! system is asked anything (absolute, a drive letter, a `\`, a `~`, a `:`, a `${`, a device
//! name, a part ending in a dot or a space), and what is left is made canonical (symbolic
//! links, junctions and `..` resolved) and must still be a `.sql` file inside the canonical
//! tree. Files are read with [`read_sql_file`]'s rules; [`MAX_INCLUDE_DEPTH`],
//! [`MAX_INCLUDED_FILES`] and [`MAX_INCLUDED_BYTES`] bound one run.
//!
//! This file is listed in `.github/CODEOWNERS`: a change to what may be read waits for an
//! owner's approval.
//!
//! Every function here blocks on the file system: async callers run them on a blocking thread.

use std::fs;
use std::path::{Path, PathBuf};

use crate::sql_file::{MAX_FILE_BYTES, SqlFile, content_hash, read_sql_file};

/// How many `@include`s deep one run may go: the tab's script includes a file at level 1.
pub const MAX_INCLUDE_DEPTH: usize = 8;
/// How many files one run may include, counting each `@include` that is read.
pub const MAX_INCLUDED_FILES: usize = 100;
/// How much text the included files of one run may hold together, counting each `@include`.
pub const MAX_INCLUDED_BYTES: usize = 64 * 1024 * 1024;

/// Why an `@include` was refused. Nothing runs when one is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IncludeError {
    /// The run is not a file tab's.
    #[error(
        "@include runs only in a file tab: a console has no folder for its path to be relative to"
    )]
    Console,
    /// The tab's file has no folder that can be read.
    #[error("the folder of {0} cannot be read, so its @include lines cannot be either")]
    NoTree(String),
    /// The path, as written or once canonical.
    #[error("@include {path}: {reason}")]
    Path { path: String, reason: PathRefusal },
    /// A file that includes itself, at any depth.
    #[error("@include {0}: the file would include itself")]
    Cycle(String),
    #[error("@include {0}: more than {MAX_INCLUDE_DEPTH} levels of @include")]
    TooDeep(String),
    #[error("@include {0}: more than {MAX_INCLUDED_FILES} files would be included in one run")]
    TooMany(String),
    #[error("@include {0}: the included files would hold more than 64 MiB of text")]
    TooMuchText(String),
    /// [`read_sql_file`] refused it.
    #[error("{0} cannot be read: {1}")]
    Read(String, String),
    /// Its canonical name changed while it was read (a link replaced): not read.
    #[error("{0} was replaced while it was being read")]
    Replaced(String),
    /// Its bytes are no longer those the run was checked with.
    #[error("{0} changed after the run was checked: run it again")]
    Changed(String),
    /// One of its statements reads otherwise once written into the script.
    #[error(
        "{0}: a statement of this file cannot be written into the script where its @include line is (it leaves a string, a quoted name or a comment open, or needs a delimiter of its own)"
    )]
    Unjoinable(String),
}

/// What is wrong with an `@include` path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathRefusal {
    #[error("write the relative path of a .sql file after @include")]
    Empty,
    #[error("the path must be relative to the script's folder, written with /")]
    Absolute,
    /// A character or sequence a path may not hold, named for the message.
    #[error("a path cannot hold {0}")]
    Forbidden(&'static str),
    #[error("a part of the path is empty")]
    EmptyPart,
    #[error("a part of the path ends in a dot or a space")]
    TrailingDot,
    #[error("{0} is a device name, not a file")]
    DeviceName(String),
    #[error("it does not name a .sql file")]
    NotSql,
    #[error("it names a file outside the script's folder tree")]
    Outside,
    #[error("there is no such file in the script's folder tree")]
    NotFound,
    #[error("it is not a file")]
    NotAFile,
}

/// The folder tree a file tab's `@include` lines may read, canonical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptTree {
    root: PathBuf,
    file: PathBuf,
}

impl ScriptTree {
    /// The tree of the tab whose file is `file`: the innermost of `open_folders` that holds it,
    /// otherwise the file's own folder. Both are compared canonical.
    pub fn for_file(file: &Path, open_folders: &[PathBuf]) -> Result<Self, IncludeError> {
        let no_tree = || IncludeError::NoTree(file.display().to_string());
        let file = match fs::canonicalize(file) {
            Ok(file) => file,
            // A file deleted since the tab opened it: its folder is still the tree.
            Err(_) => {
                let parent = file.parent().ok_or_else(no_tree)?;
                let name = file.file_name().ok_or_else(no_tree)?;
                fs::canonicalize(parent).map_err(|_| no_tree())?.join(name)
            }
        };
        let own = file.parent().ok_or_else(no_tree)?.to_path_buf();
        let root = open_folders
            .iter()
            .filter_map(|folder| fs::canonicalize(folder).ok())
            .filter(|folder| file.starts_with(folder))
            .max_by_key(|folder| folder.components().count())
            .unwrap_or(own);
        Ok(Self { root, file })
    }

    /// The tree's canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The tab's file, canonical.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The folder the tab's own `@include` paths are relative to.
    pub(crate) fn dir(&self) -> &Path {
        self.file.parent().unwrap_or(&self.root)
    }

    /// How `path`, canonical and inside the tree, is named to the user: relative to the root,
    /// with `/`.
    pub fn name_of(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(relative) => relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            Err(_) => path.display().to_string(),
        }
    }

    /// The canonical file `written` names, from a file in `from_dir`: refused as written, before
    /// the file system is asked, and then once canonical. Nothing is read.
    pub(crate) fn locate(&self, from_dir: &Path, written: &str) -> Result<PathBuf, IncludeError> {
        let refuse = |reason| IncludeError::Path {
            path: written.to_string(),
            reason,
        };
        check_written(written).map_err(refuse)?;
        let canonical = match fs::canonicalize(from_dir.join(written)) {
            Ok(canonical) => canonical,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(refuse(PathRefusal::NotFound));
            }
            Err(e) => {
                return Err(IncludeError::Read(written.to_string(), e.to_string()));
            }
        };
        if !canonical.starts_with(&self.root) {
            return Err(refuse(PathRefusal::Outside));
        }
        if !is_sql_name(&canonical) {
            return Err(refuse(PathRefusal::NotSql));
        }
        match fs::metadata(&canonical) {
            Ok(metadata) if metadata.is_file() => Ok(canonical),
            Ok(_) => Err(refuse(PathRefusal::NotAFile)),
            Err(e) => Err(IncludeError::Read(written.to_string(), e.to_string())),
        }
    }

    /// Reads `path`, which [`ScriptTree::locate`] gave, with [`read_sql_file`]'s rules: refused
    /// when its canonical name is no longer `path` once read.
    pub(crate) fn read(&self, path: &Path) -> Result<SqlFile, IncludeError> {
        let name = self.name_of(path);
        let file =
            read_sql_file(path).map_err(|e| IncludeError::Read(name.clone(), e.to_string()))?;
        match fs::canonicalize(path) {
            Ok(again) if again == path => Ok(file),
            _ => Err(IncludeError::Replaced(name)),
        }
    }
}

/// A file a run included: what its guard's question and read-only check were given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludedFile {
    /// Canonical.
    pub path: PathBuf,
    /// As the user sees it: relative to the tree's root, with `/`.
    pub name: String,
    /// SHA-256 of the bytes that were read ([`content_hash`]).
    pub hash: String,
}

/// Refuses a run whose included files are no longer the bytes it was checked with (ADR-078,
/// point 4). Only hashes them: their text is never read again.
pub fn check_included_files(files: &[IncludedFile]) -> Result<(), IncludeError> {
    for file in files {
        let changed = || IncludeError::Changed(file.name.clone());
        let metadata = fs::metadata(&file.path).map_err(|_| changed())?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(changed());
        }
        let bytes = fs::read(&file.path).map_err(|_| changed())?;
        if content_hash(&bytes) != file.hash {
            return Err(changed());
        }
    }
    Ok(())
}

/// The canonical name ends in `.sql`, in any case.
fn is_sql_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(ends_in_sql)
}

/// `name` is more than `.sql` and ends in it, in any case.
fn ends_in_sql(name: &str) -> bool {
    name.len() > 4
        && name.is_char_boundary(name.len() - 4)
        && name[name.len() - 4..].eq_ignore_ascii_case(".sql")
}

/// Names Windows opens as a device whatever folder or extension they are written with.
const DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$", "COM0", "COM1", "COM2", "COM3",
    "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "COM¹", "COM²", "COM³", "LPT0", "LPT1", "LPT2",
    "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Refuses a path by how it is written, on every system alike: relative, with `/`, no `\`,
/// `~`, `:` (drive letters, alternate data streams, devices), `${`, control character, empty
/// part, part ending in a dot or a space (Windows drops those), or device name, and ending in
/// `.sql`.
fn check_written(written: &str) -> Result<(), PathRefusal> {
    if written.is_empty() {
        return Err(PathRefusal::Empty);
    }
    for (needle, named) in [("\\", "a \\"), ("~", "a ~"), (":", "a :"), ("${", "${")] {
        if written.contains(needle) {
            return Err(PathRefusal::Forbidden(named));
        }
    }
    if written.chars().any(char::is_control) {
        return Err(PathRefusal::Forbidden("a control character"));
    }
    if written.starts_with('/') {
        return Err(PathRefusal::Absolute);
    }
    for part in written.split('/') {
        if part.is_empty() {
            return Err(PathRefusal::EmptyPart);
        }
        if part == "." || part == ".." {
            continue;
        }
        if part.ends_with('.') || part.ends_with(' ') {
            return Err(PathRefusal::TrailingDot);
        }
        let stem = part.split('.').next().unwrap_or(part).trim_end();
        if let Some(device) = DEVICE_NAMES
            .iter()
            .find(|device| stem.eq_ignore_ascii_case(device))
        {
            return Err(PathRefusal::DeviceName((*device).to_string()));
        }
    }
    let last = written.rsplit('/').next().unwrap_or(written);
    if !ends_in_sql(last) {
        return Err(PathRefusal::NotSql);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_refused_by_how_it_is_written() {
        for (written, reason) in [
            ("", PathRefusal::Empty),
            ("/etc/x.sql", PathRefusal::Absolute),
            ("//server/share/x.sql", PathRefusal::Absolute),
            ("\\\\server\\share\\x.sql", PathRefusal::Forbidden("a \\")),
            ("\\\\?\\C:\\x.sql", PathRefusal::Forbidden("a \\")),
            ("sub\\x.sql", PathRefusal::Forbidden("a \\")),
            ("C:/x.sql", PathRefusal::Forbidden("a :")),
            ("C:x.sql", PathRefusal::Forbidden("a :")),
            ("x.sql:stream", PathRefusal::Forbidden("a :")),
            ("x.sql::$DATA", PathRefusal::Forbidden("a :")),
            ("~/x.sql", PathRefusal::Forbidden("a ~")),
            ("${dir}/x.sql", PathRefusal::Forbidden("${")),
            ("a\u{0}.sql", PathRefusal::Forbidden("a control character")),
            ("a//x.sql", PathRefusal::EmptyPart),
            ("x.sql.", PathRefusal::TrailingDot),
            ("x.sql ", PathRefusal::TrailingDot),
            ("CON.sql", PathRefusal::DeviceName("CON".into())),
            ("sub/nul.sql", PathRefusal::DeviceName("NUL".into())),
            ("com1 .sql", PathRefusal::DeviceName("COM1".into())),
            ("conin$.sql", PathRefusal::DeviceName("CONIN$".into())),
            ("lpt¹.sql", PathRefusal::DeviceName("LPT¹".into())),
            ("x.txt", PathRefusal::NotSql),
            (".sql", PathRefusal::NotSql),
            ("x.sqlé", PathRefusal::NotSql),
        ] {
            assert_eq!(check_written(written), Err(reason), "{written:?}");
        }
        for written in [
            "x.sql",
            "sub/X.SQL",
            "../shared/x.sql",
            "./a b/console.sql",
            "a.b.sql",
        ] {
            assert_eq!(check_written(written), Ok(()), "{written:?}");
        }
    }
}
