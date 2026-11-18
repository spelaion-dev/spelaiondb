//! What the desktop app is asked to open from outside (SDB-397): its command line at start, or
//! the one a second launch hands to the running window ([`crate::instance`]). The adapter only
//! passes the words on; what they mean and what is accepted is decided here.
//!
//! ```text
//! spelaion-desktop [FOLDER] [FILE.sql …] [--connection ID] [--plan FILE]
//! ```
//!
//! A folder must exist; a file must be an existing `.sql` file no bigger than
//! [`MAX_FILE_BYTES`], as a file picked in the app is (it is then read like one, ADR-036); a
//! connection is only a name to select. Nothing here opens, connects or runs anything: the
//! app opens a folder through repository trust (ADR-022) and files as tabs, never run.

use std::path::{Path, PathBuf};

use crate::sql_file::MAX_FILE_BYTES;

/// The most `.sql` files one launch opens.
pub const MAX_LAUNCH_FILES: usize = 20;
/// The longest connection id accepted after `--connection`.
const MAX_CONNECTION_ID: usize = 256;

/// What a launch asks for, checked. Relative paths are resolved against the launch's folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchRequest {
    /// The folder to open.
    pub folder: Option<PathBuf>,
    /// The connection to select in the explorer (by id), never to connect.
    pub connection: Option<String>,
    /// `.sql` files to open in tabs, in order, without duplicates.
    pub files: Vec<PathBuf>,
    /// An execution plan file to open (`--plan`, or a plan file given alone first).
    pub plan: Option<PathBuf>,
    /// What was given and not accepted, with why.
    pub refused: Vec<Refused>,
}

impl LaunchRequest {
    /// Whether there is anything to do or say.
    pub fn is_empty(&self) -> bool {
        self.folder.is_none()
            && self.connection.is_none()
            && self.files.is_empty()
            && self.plan.is_none()
            && self.refused.is_empty()
    }
}

/// An argument that was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The argument as it was given.
    pub argument: String,
    pub reason: RefusedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefusedReason {
    #[error("it does not exist")]
    Missing,
    #[error("only .sql files are opened this way")]
    NotSql,
    #[error("it is bigger than the largest file the editor opens")]
    TooBig,
    #[error("only one folder can be opened")]
    SecondFolder,
    #[error("too many files at once")]
    TooManyFiles,
    #[error("it is not an option of SpelaionDB")]
    UnknownOption,
    #[error("the option needs a value")]
    MissingValue,
    #[error("it is not a connection id")]
    BadConnection,
}

/// Plan files given first and alone open the plan viewer, as before this parser existed.
const PLAN_EXTENSIONS: &[&str] = &["json", "plan", "sqlplan", "xml"];

/// Reads a launch's arguments (without the program's own path) given in folder `cwd`.
pub fn parse(args: &[String], cwd: &Path) -> LaunchRequest {
    let mut request = LaunchRequest::default();
    let refuse = |request: &mut LaunchRequest, argument: &str, reason| {
        request.refused.push(Refused {
            argument: argument.to_string(),
            reason,
        })
    };
    let mut i = 0;
    while i < args.len() {
        let argument = &args[i];
        i += 1;
        // `--name value` or `--name=value`.
        if let Some(option) = argument.strip_prefix("--") {
            let (name, inline) = match option.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (option, None),
            };
            if !matches!(name, "connection" | "plan") {
                refuse(&mut request, argument, RefusedReason::UnknownOption);
                continue;
            }
            let value = match inline {
                Some(value) => value,
                None if i < args.len() && !args[i].starts_with("--") => {
                    i += 1;
                    args[i - 1].clone()
                }
                None => {
                    refuse(&mut request, argument, RefusedReason::MissingValue);
                    continue;
                }
            };
            if name == "connection" {
                let id = value.trim();
                if id.is_empty()
                    || id.chars().count() > MAX_CONNECTION_ID
                    || id.chars().any(char::is_control)
                {
                    refuse(&mut request, &value, RefusedReason::BadConnection);
                } else {
                    request.connection = Some(id.to_string());
                }
            } else {
                let path = resolve(&value, cwd);
                if path.is_file() {
                    request.plan = Some(path);
                } else {
                    refuse(&mut request, &value, RefusedReason::Missing);
                }
            }
            continue;
        }
        if argument.starts_with('-') && argument.len() > 1 {
            refuse(&mut request, argument, RefusedReason::UnknownOption);
            continue;
        }
        let path = resolve(argument, cwd);
        if path.is_dir() {
            if request.folder.is_some() {
                refuse(&mut request, argument, RefusedReason::SecondFolder);
            } else {
                request.folder = Some(path);
            }
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            refuse(&mut request, argument, RefusedReason::Missing);
            continue;
        };
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        if extension.as_deref() != Some("sql") {
            let first_and_alone = i == 1 && request.plan.is_none();
            if first_and_alone
                && extension
                    .as_deref()
                    .is_some_and(|e| PLAN_EXTENSIONS.contains(&e))
            {
                request.plan = Some(path);
            } else {
                refuse(&mut request, argument, RefusedReason::NotSql);
            }
            continue;
        }
        if !metadata.is_file() {
            refuse(&mut request, argument, RefusedReason::Missing);
        } else if metadata.len() > MAX_FILE_BYTES {
            refuse(&mut request, argument, RefusedReason::TooBig);
        } else if request.files.len() >= MAX_LAUNCH_FILES {
            refuse(&mut request, argument, RefusedReason::TooManyFiles);
        } else if !request.files.contains(&path) {
            request.files.push(path);
        }
    }
    request
}

/// `argument` as an absolute path: relative ones from `cwd`, `.` and `..` taken out.
fn resolve(argument: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(argument);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    // The real path when it exists (links followed, as a picked file's), else as written.
    std::fs::canonicalize(&joined)
        .map(strip_verbatim)
        .unwrap_or(joined)
}

/// Windows' `canonicalize` gives `\\?\C:\…`: the path as people write it.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        Some(_) | None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("spelaion-launch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("a.sql"), "select 1;").unwrap();
        std::fs::write(dir.join("B.SQL"), "select 2;").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::write(dir.join("plan.json"), "{}").unwrap();
        let dir = strip_verbatim(std::fs::canonicalize(&dir).unwrap());
        Scratch(dir)
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_folder_files_and_a_connection_are_read_relative_to_the_launch_folder() {
        let dir = scratch("basic");
        let request = parse(
            &args(&["repo", "a.sql", "B.SQL", "--connection", "shop"]),
            &dir.0,
        );
        assert_eq!(request.folder, Some(dir.0.join("repo")));
        assert_eq!(
            request.files,
            vec![dir.0.join("a.sql"), dir.0.join("B.SQL")]
        );
        assert_eq!(request.connection.as_deref(), Some("shop"));
        assert!(request.refused.is_empty(), "{:?}", request.refused);
        // `--connection=id`, absolute paths, and a file named twice is opened once.
        let a = dir.0.join("a.sql").display().to_string();
        let request = parse(
            &args(&["--connection=docker/pg", &a, "a.sql", "./a.sql"]),
            &dir.0,
        );
        assert_eq!(request.connection.as_deref(), Some("docker/pg"));
        assert_eq!(request.files, vec![dir.0.join("a.sql")]);
        assert_eq!(request.folder, None);
    }

    #[test]
    fn what_is_not_a_folder_or_an_sql_file_is_refused_and_said() {
        let dir = scratch("refused");
        let request = parse(
            &args(&[
                "a.sql",
                "notes.txt",
                "missing.sql",
                "repo",
                ".",
                "--run",
                "-x",
                "--connection",
            ]),
            &dir.0,
        );
        assert_eq!(request.files, vec![dir.0.join("a.sql")]);
        assert_eq!(request.folder, Some(dir.0.join("repo")));
        let reasons: Vec<(&str, RefusedReason)> = request
            .refused
            .iter()
            .map(|r| (r.argument.as_str(), r.reason))
            .collect();
        assert_eq!(
            reasons,
            vec![
                ("notes.txt", RefusedReason::NotSql),
                ("missing.sql", RefusedReason::Missing),
                (".", RefusedReason::SecondFolder),
                ("--run", RefusedReason::UnknownOption),
                ("-x", RefusedReason::UnknownOption),
                ("--connection", RefusedReason::MissingValue),
            ]
        );
        for bad in ["--connection=", "--connection=a\nb"] {
            let request = parse(&args(&[bad]), &dir.0);
            assert_eq!(request.connection, None);
            assert_eq!(
                request.refused[0].reason,
                RefusedReason::BadConnection,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_folder_named_like_an_sql_file_is_a_folder_and_a_big_file_is_refused() {
        let dir = scratch("odd");
        std::fs::create_dir_all(dir.0.join("odd.sql")).unwrap();
        let request = parse(&args(&["odd.sql"]), &dir.0);
        assert_eq!(request.folder, Some(dir.0.join("odd.sql")));
        let big = std::fs::File::create(dir.0.join("big.sql")).unwrap();
        big.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(big);
        assert_eq!(
            parse(&args(&["big.sql"]), &dir.0).refused[0].reason,
            RefusedReason::TooBig
        );
        let many: Vec<String> = (0..MAX_LAUNCH_FILES + 2)
            .map(|n| {
                let name = format!("f{n}.sql");
                std::fs::write(dir.0.join(&name), "").unwrap();
                name
            })
            .collect();
        let request = parse(&many, &dir.0);
        assert_eq!(request.files.len(), MAX_LAUNCH_FILES);
        assert_eq!(request.refused.len(), 2);
        assert!(
            request
                .refused
                .iter()
                .all(|r| r.reason == RefusedReason::TooManyFiles)
        );
    }

    #[test]
    fn a_plan_file_opens_given_first_or_with_plan_as_before() {
        let dir = scratch("plan");
        assert_eq!(
            parse(&args(&["plan.json"]), &dir.0).plan,
            Some(dir.0.join("plan.json"))
        );
        assert_eq!(
            parse(&args(&["--plan", "plan.json"]), &dir.0).plan,
            Some(dir.0.join("plan.json"))
        );
        // Not first: a JSON file is not an SQL file.
        let request = parse(&args(&["a.sql", "plan.json"]), &dir.0);
        assert_eq!(request.plan, None);
        assert_eq!(request.refused[0].reason, RefusedReason::NotSql);
        assert!(parse(&[], &dir.0).is_empty());
    }
}
