//! Finding the vendors' client tools (ADR-049): in the folder the user chose, or on `PATH` and in
//! the places their installers use, and reading each one's version.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// A client tool a backup or a restore runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolName {
    PgDump,
    PgRestore,
    Psql,
    Mysqldump,
    Mysql,
}

impl ToolName {
    /// The name the user knows it by.
    pub fn label(self) -> &'static str {
        match self {
            ToolName::PgDump => "pg_dump",
            ToolName::PgRestore => "pg_restore",
            ToolName::Psql => "psql",
            ToolName::Mysqldump => "mysqldump",
            ToolName::Mysql => "mysql",
        }
    }

    /// The program names that provide it, without the system's executable suffix. MariaDB's
    /// clients also install as `mariadb-dump` and `mariadb` (newer ones only so).
    pub fn programs(self) -> &'static [&'static str] {
        match self {
            ToolName::PgDump => &["pg_dump"],
            ToolName::PgRestore => &["pg_restore"],
            ToolName::Psql => &["psql"],
            ToolName::Mysqldump => &["mysqldump", "mariadb-dump"],
            ToolName::Mysql => &["mysql", "mariadb"],
        }
    }
}

/// Whose client a tool is: their options differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFlavor {
    Postgres,
    Mysql,
    Mariadb,
}

/// A tool's version, read from its `--version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    /// The first line it printed.
    pub text: String,
    pub flavor: ToolFlavor,
    /// `(major, minor, patch)`; `(0, 0, 0)` when no number could be read.
    pub number: (u32, u32, u32),
}

impl ToolVersion {
    /// Reads `--version`'s first line, as each vendor writes it:
    /// `pg_dump (PostgreSQL) 17.11`, `mysqldump  Ver 8.4.13 for Linux on x86_64 (MySQL
    /// Community Server - GPL)`, `mariadb-dump from 11.4.13-MariaDB, client 10.19 for …` or
    /// `mysqldump  Ver 10.19 Distrib 10.11.6-MariaDB, for …`. A client that starts the line with
    /// the path it was started by (Windows' `mysql.exe`: `C:\Program Files\MySQL\MySQL Server
    /// 8.4\bin\mysql.exe  Ver 8.4.9 …`) is read by its name alone, so the folder's numbers are
    /// never taken for its version.
    pub fn parse(name: ToolName, output: &str) -> Self {
        let first = output
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let text = without_program_path(name, first);
        let numbers = version_tokens(&text);
        let (flavor, number) = match name {
            ToolName::PgDump | ToolName::PgRestore | ToolName::Psql => (
                ToolFlavor::Postgres,
                numbers.first().map(|(n, _)| *n).unwrap_or_default(),
            ),
            ToolName::Mysqldump | ToolName::Mysql => {
                if let Some((n, _)) = numbers.iter().find(|(_, mariadb)| *mariadb) {
                    (ToolFlavor::Mariadb, *n)
                } else if text.contains("MariaDB") {
                    (
                        ToolFlavor::Mariadb,
                        numbers.last().map(|(n, _)| *n).unwrap_or_default(),
                    )
                } else {
                    (
                        ToolFlavor::Mysql,
                        numbers.first().map(|(n, _)| *n).unwrap_or_default(),
                    )
                }
            }
        };
        Self {
            text,
            flavor,
            number,
        }
    }

    /// At least this version.
    pub fn at_least(&self, major: u32, minor: u32, patch: u32) -> bool {
        self.number >= (major, minor, patch)
    }
}

/// The line with a leading path to one of the tool's programs (`…\bin\mysql.exe  Ver …`)
/// replaced by the program's name (`mysql  Ver …`); any other line as it is. The program is
/// the name that follows a separator and comes right before the vendors' `Ver` or `from`, so a
/// folder named like it (`/opt/mysql 9.1/…`) is never taken for it.
fn without_program_path(name: ToolName, line: &str) -> String {
    // ASCII lowercasing keeps every byte where it was, so offsets apply to `line` too.
    let lower = line.to_ascii_lowercase();
    for program in name.programs() {
        for file in [format!("{program}.exe"), (*program).to_string()] {
            let mut from = 0;
            while let Some(at) = lower[from..].find(&file) {
                let start = from + at;
                let end = start + file.len();
                let after_path = start > 0 && matches!(lower.as_bytes()[start - 1], b'\\' | b'/');
                let rest = &lower[end..];
                let then_version = rest.starts_with(char::is_whitespace) && {
                    let word = rest.trim_start();
                    word.starts_with("ver ") || word.starts_with("from ")
                };
                if after_path && then_version {
                    return format!("{program}{}", &line[end..]);
                }
                // The match starts with an ASCII letter, so the next byte is a char boundary.
                from = start + 1;
            }
        }
    }
    line.to_string()
}

/// Every `a.b[.c]` in the text, and whether `-MariaDB` follows it.
fn version_tokens(text: &str) -> Vec<((u32, u32, u32), bool)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let starts = bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric());
        if !starts {
            i += 1;
            continue;
        }
        let begin = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let token = text[begin..i].trim_end_matches('.');
        let parts: Vec<u32> = token.split('.').filter_map(|p| p.parse().ok()).collect();
        if parts.len() >= 2 {
            let number = (parts[0], parts[1], parts.get(2).copied().unwrap_or(0));
            found.push((number, text[i..].starts_with("-MariaDB")));
        }
    }
    found
}

/// How a tool is started. Normally the program that was found; tests start the tools of the
/// official images the test services already use, through `docker run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub name: ToolName,
    /// The program started.
    pub program: PathBuf,
    /// Arguments before the tool's own (a launcher's, with the tool's name last); empty for a
    /// program that was found.
    pub leading_args: Vec<String>,
    pub version: ToolVersion,
    /// The host the tool reaches the tunnel's local end at, when it is not this machine's
    /// `127.0.0.1` (a container's `host.docker.internal`).
    pub reach_host: Option<String>,
}

/// Where the tools come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSource {
    /// `PATH` and the places installers use.
    Detect,
    /// Only this folder (the user chose it).
    Folder(PathBuf),
    /// A launcher that runs the tool by name: `program leading_args… <tool> <args>`, reaching
    /// the local end at `reach_host` if given. Used by the tests (`docker run … image`).
    Launcher {
        program: PathBuf,
        leading_args: Vec<String>,
        reach_host: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ToolError {
    #[error("{tool} was not found (looked in {})", searched.join(", "))]
    NotFound {
        tool: &'static str,
        searched: Vec<String>,
    },
    #[error("{tool} could not be started: {message}")]
    Broken { tool: String, message: String },
}

/// How long `--version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Finds `name`. From a folder, the first of its program names there. Otherwise every copy on
/// `PATH` and in the installers' places, the one of `prefer`'s flavor (MySQL's or MariaDB's
/// client for that server) and then the newest version first, `PATH` order breaking ties.
pub async fn locate(
    name: ToolName,
    source: &ToolSource,
    prefer: Option<ToolFlavor>,
) -> Result<Tool, ToolError> {
    match source {
        ToolSource::Launcher {
            program,
            leading_args,
            reach_host,
        } => {
            let mut leading = leading_args.clone();
            // MariaDB's own names where its client is wanted (newer images have only those).
            let programs = name.programs();
            let program_name = if prefer == Some(ToolFlavor::Mariadb) {
                programs[programs.len() - 1]
            } else {
                programs[0]
            };
            leading.push(program_name.to_string());
            let version = read_version(name, program, &leading).await?;
            Ok(Tool {
                name,
                program: program.clone(),
                leading_args: leading,
                version,
                reach_host: reach_host.clone(),
            })
        }
        ToolSource::Folder(folder) => {
            for program in name.programs() {
                let path = folder.join(executable(program));
                if path.is_file() {
                    let version = read_version(name, &path, &[]).await?;
                    return Ok(found(name, path, version));
                }
            }
            Err(ToolError::NotFound {
                tool: name.label(),
                searched: vec![folder.display().to_string()],
            })
        }
        ToolSource::Detect => {
            let folders = search_folders(name);
            let mut best: Option<Tool> = None;
            let mut seen = HashSet::new();
            for folder in &folders {
                for program in name.programs() {
                    let path = folder.join(executable(program));
                    if !path.is_file() || !seen.insert(canonical(&path)) {
                        continue;
                    }
                    let Ok(version) = read_version(name, &path, &[]).await else {
                        continue;
                    };
                    let candidate = found(name, path, version);
                    let better = match &best {
                        None => true,
                        Some(current) => rank(&candidate, prefer) > rank(current, prefer),
                    };
                    if better {
                        best = Some(candidate);
                    }
                }
            }
            best.ok_or_else(|| ToolError::NotFound {
                tool: name.label(),
                searched: summarize(&folders),
            })
        }
    }
}

fn found(name: ToolName, program: PathBuf, version: ToolVersion) -> Tool {
    Tool {
        name,
        program,
        leading_args: Vec::new(),
        version,
        reach_host: None,
    }
}

fn rank(tool: &Tool, prefer: Option<ToolFlavor>) -> (bool, (u32, u32, u32)) {
    (Some(tool.version.flavor) == prefer, tool.version.number)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn executable(program: &str) -> String {
    format!("{program}{}", std::env::consts::EXE_SUFFIX)
}

/// The folders searched, for the "not found" message: `PATH` as one entry, then the others.
fn summarize(folders: &[PathBuf]) -> Vec<String> {
    let path: HashSet<PathBuf> = path_folders().into_iter().collect();
    let mut out = vec!["PATH".to_string()];
    out.extend(
        folders
            .iter()
            .filter(|f| !path.contains(*f))
            .map(|f| f.display().to_string()),
    );
    out
}

/// Runs `program leading… --version` (no shell) and reads its answer.
async fn read_version(
    name: ToolName,
    program: &Path,
    leading: &[String],
) -> Result<ToolVersion, ToolError> {
    let mut command = Command::new(program);
    command
        .args(leading)
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    super::process::no_window(&mut command);
    let broken = |message: String| ToolError::Broken {
        tool: program.display().to_string(),
        message,
    };
    let output = tokio::time::timeout(VERSION_TIMEOUT, command.output())
        .await
        .map_err(|_| broken("it did not answer --version".into()))?
        .map_err(|e| broken(e.to_string()))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(broken(err.lines().next().unwrap_or("").trim().to_string()));
    }
    Ok(ToolVersion::parse(
        name,
        &String::from_utf8_lossy(&output.stdout),
    ))
}

fn path_folders() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// `PATH`, then where each vendor's installers put their clients on this system.
fn search_folders(name: ToolName) -> Vec<PathBuf> {
    let mut folders = path_folders();
    let postgres = matches!(
        name,
        ToolName::PgDump | ToolName::PgRestore | ToolName::Psql
    );
    for pattern in install_patterns(postgres) {
        folders.extend(expand(&pattern));
    }
    let mut seen = HashSet::new();
    folders.retain(|f| seen.insert(f.clone()));
    folders
}

/// Install folders, with at most one `*` path segment.
fn install_patterns(postgres: bool) -> Vec<String> {
    let mut out = Vec::new();
    if cfg!(windows) {
        let mut roots: Vec<String> = ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .collect();
        roots.dedup();
        let local = std::env::var("LOCALAPPDATA").ok();
        for root in &roots {
            if postgres {
                out.push(format!(r"{root}\PostgreSQL\*\bin"));
                out.push(format!(r"{root}\pgAdmin 4\runtime"));
            } else {
                out.push(format!(r"{root}\MySQL\MySQL Server *\bin"));
                out.push(format!(r"{root}\MySQL\MySQL Workbench *"));
                out.push(format!(r"{root}\MariaDB *\bin"));
            }
        }
        if postgres && let Some(local) = local {
            out.push(format!(r"{local}\Programs\pgAdmin 4\runtime"));
        }
    } else if cfg!(target_os = "macos") {
        if postgres {
            out.extend(
                [
                    "/Applications/Postgres.app/Contents/Versions/*/bin",
                    "/opt/homebrew/opt/libpq/bin",
                    "/usr/local/opt/libpq/bin",
                    "/opt/homebrew/opt/postgresql@*/bin",
                    "/usr/local/opt/postgresql@*/bin",
                    "/Applications/pgAdmin 4.app/Contents/SharedSupport",
                ]
                .map(String::from),
            );
        } else {
            out.extend(
                [
                    "/opt/homebrew/opt/mysql-client/bin",
                    "/usr/local/opt/mysql-client/bin",
                    "/opt/homebrew/opt/mysql-client@*/bin",
                    "/usr/local/opt/mysql-client@*/bin",
                    "/opt/homebrew/opt/mariadb/bin",
                    "/usr/local/mysql/bin",
                ]
                .map(String::from),
            );
        }
        // A GUI app on macOS does not get the shell's PATH.
        out.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(String::from));
    } else {
        if postgres {
            out.extend(
                [
                    "/usr/lib/postgresql/*/bin",
                    "/usr/pgsql-*/bin",
                    "/usr/local/pgsql/bin",
                ]
                .map(String::from),
            );
        }
        out.extend(["/usr/bin", "/usr/local/bin"].map(String::from));
    }
    out
}

/// The folders a pattern names: itself, or with its one `*` segment matched against what exists.
pub(crate) fn expand(pattern: &str) -> Vec<PathBuf> {
    let path = Path::new(pattern);
    let components: Vec<_> = path.components().collect();
    let Some(star) = components
        .iter()
        .position(|c| c.as_os_str().to_string_lossy().contains('*'))
    else {
        return if path.is_dir() {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    };
    let parent: PathBuf = components[..star].iter().collect();
    let segment = components[star].as_os_str().to_string_lossy().to_string();
    let (prefix, suffix) = segment.split_once('*').unwrap_or((&segment, ""));
    let tail: PathBuf = components[star + 1..].iter().collect();
    let Ok(entries) = std::fs::read_dir(&parent) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.len() >= prefix.len() + suffix.len()
                && name.starts_with(prefix)
                && name.ends_with(suffix)
        })
        .map(|e| e.path().join(&tail))
        .filter(|p| p.is_dir())
        .collect();
    // Newest first when the segment is a version (`17` before `9.6`).
    out.sort_by_key(|p| std::cmp::Reverse(version_tokens_of(p)));
    out
}

fn version_tokens_of(path: &Path) -> Vec<u32> {
    path.to_string_lossy()
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|n| n.parse().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_as_each_vendor_writes_them() {
        let pg = ToolVersion::parse(ToolName::PgDump, "pg_dump (PostgreSQL) 17.11\n");
        assert_eq!(pg.flavor, ToolFlavor::Postgres);
        assert_eq!(pg.number, (17, 11, 0));
        let pg = ToolVersion::parse(ToolName::Psql, "psql (PostgreSQL) 9.6.24");
        assert_eq!(pg.number, (9, 6, 24));
        let my = ToolVersion::parse(
            ToolName::Mysqldump,
            "mysqldump  Ver 8.4.13 for Linux on x86_64 (MySQL Community Server - GPL)",
        );
        assert_eq!(my.flavor, ToolFlavor::Mysql);
        assert_eq!(my.number, (8, 4, 13));
        assert!(my.at_least(8, 4, 3) && !my.at_least(9, 1, 0));
        let maria = ToolVersion::parse(
            ToolName::Mysqldump,
            "mariadb-dump from 11.4.13-MariaDB, client 10.19 for debian-linux-gnu (x86_64)",
        );
        assert_eq!(maria.flavor, ToolFlavor::Mariadb);
        assert_eq!(maria.number, (11, 4, 13));
        let old = ToolVersion::parse(
            ToolName::Mysql,
            "mysql  Ver 15.1 Distrib 10.11.6-MariaDB, for debian-linux-gnu (x86_64) using  EditLine wrapper",
        );
        assert_eq!(old.flavor, ToolFlavor::Mariadb);
        assert_eq!(old.number, (10, 11, 6));
        let unknown = ToolVersion::parse(ToolName::Mysql, "");
        assert_eq!(unknown.number, (0, 0, 0));
    }

    #[test]
    fn a_client_that_prints_its_own_path_is_read_by_its_name() {
        // What MySQL 8.4.9's `mysql.exe --version` prints when started from its install folder
        // (its `mysqldump.exe` prints only its name).
        let windows = ToolVersion::parse(
            ToolName::Mysql,
            r"C:\Program Files\MySQL\MySQL Server 8.4\bin\mysql.exe  Ver 8.4.9 for Win64 on x86_64 (MySQL Community Server - GPL)",
        );
        assert_eq!(windows.flavor, ToolFlavor::Mysql);
        assert_eq!(windows.number, (8, 4, 9));
        assert_eq!(
            windows.text,
            "mysql  Ver 8.4.9 for Win64 on x86_64 (MySQL Community Server - GPL)"
        );
        // Any case, either separator, MariaDB's names, a folder with the program's name in it.
        let maria = ToolVersion::parse(
            ToolName::Mysqldump,
            r"c:\program files\MariaDB 11.4\bin\MARIADB-DUMP.EXE from 11.4.5-MariaDB, client 10.19 for Win64 (AMD64)",
        );
        assert_eq!(maria.flavor, ToolFlavor::Mariadb);
        assert_eq!(maria.number, (11, 4, 5));
        assert!(maria.text.starts_with("mariadb-dump from 11.4.5-MariaDB"));
        let unix = ToolVersion::parse(
            ToolName::Mysql,
            "/opt/mysql 9.1/mysql/bin/mysql  Ver 9.4.0 for Linux on x86_64",
        );
        assert_eq!(unix.number, (9, 4, 0));
        assert_eq!(unix.text, "mysql  Ver 9.4.0 for Linux on x86_64");
        // Nothing to strip: a name without a path, or a path that is not the program's.
        let plain = ToolVersion::parse(ToolName::Mysql, "mysql  Ver 8.4.13 for Linux");
        assert_eq!(plain.text, "mysql  Ver 8.4.13 for Linux");
        let other = ToolVersion::parse(ToolName::Psql, "psql (PostgreSQL) 17.11");
        assert_eq!(other.text, "psql (PostgreSQL) 17.11");
    }

    #[test]
    fn installer_folders_with_spaces_expand() {
        // `MySQL\MySQL Server *\bin`: the `*` follows a fixed prefix with a space in it.
        let dir = std::env::temp_dir().join(format!("spelaion-installs-{}", std::process::id()));
        for v in [
            "MySQL Server 8.0",
            "MySQL Server 8.4",
            "MySQL Workbench 8.0",
        ] {
            std::fs::create_dir_all(dir.join("MySQL").join(v).join("bin")).unwrap();
        }
        let pattern = format!("{}/MySQL/MySQL Server */bin", dir.display());
        let found: Vec<PathBuf> = expand(&pattern);
        assert_eq!(
            found,
            [
                dir.join("MySQL").join("MySQL Server 8.4").join("bin"),
                dir.join("MySQL").join("MySQL Server 8.0").join("bin"),
            ]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_searches_the_installers_default_folders() {
        let root = std::env::var("ProgramFiles").unwrap();
        assert!(install_patterns(true).contains(&format!(r"{root}\PostgreSQL\*\bin")));
        assert!(install_patterns(false).contains(&format!(r"{root}\MySQL\MySQL Server *\bin")));
    }

    #[test]
    fn patterns_expand_one_segment_newest_first() {
        let dir = std::env::temp_dir().join(format!("spelaion-expand-{}", std::process::id()));
        for v in ["9.6", "17", "16", "notes"] {
            let bin = dir.join(v).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
        }
        std::fs::remove_dir_all(dir.join("notes").join("bin")).unwrap();
        let pattern = format!("{}/*/bin", dir.display());
        let found: Vec<String> = expand(&pattern)
            .iter()
            .map(|p| {
                p.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(found, ["17", "16", "9.6"]);
        assert!(expand(&format!("{}/missing/*/bin", dir.display())).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
