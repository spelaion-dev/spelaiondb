//! Fetches the plugin builds `plugins.toml` pins for this platform and checks them (ADR-042):
//! what CI runs before the tests, what the release runs before it packs the installers, and
//! what a developer runs to have the engines that are no longer compiled in.
//!
//! This repository never compiles a plugin: it downloads the release each pin names from that
//! plugin's repository. Nothing fetched is trusted before it is checked:
//!
//! 1. the archive must have **the pinned size** (the download stops past it) and **the pinned
//!    SHA-256**, before anything is unpacked;
//! 2. it must hold the program at its top, by its own name;
//! 3. only that program is unpacked, and it must have **the pinned SHA-256 of the program**
//!    (the same check the app makes before it starts a plugin).
//!
//! A file that fails any of them is deleted, and the error names the release. No secret is
//! used: the plugins' repositories are public.
//!
//! The download is `curl` and the unpacking is `tar` (both are on every CI runner and on
//! Windows 10 and later, whose `tar` reads `.zip`): this tool adds no dependency to the
//! workspace.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use spelaion_core::plugins::pins::{
    PinnedBuild, PinnedPlugin, Pins, PluginRefused, check_file, program_file_name, sha256_of,
};

/// Where releases are, when nothing else is said.
pub const GITHUB: &str = "https://github.com";

/// A plugin that was fetched and is the pinned build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The plugin's name: `spelaiondb-plugin-duckdb`.
    pub plugin: String,
    pub version: String,
    /// The engines it serves.
    pub engines: Vec<String>,
    /// Its program, in the folder that was asked for.
    pub program: PathBuf,
}

impl Fetched {
    /// The variables that point a development build at this program, one per engine
    /// (`SPELAION_DEV_PLUGIN_DUCKDB=<program>`): what the tests run through.
    pub fn variables(&self) -> Vec<(String, String)> {
        self.engines
            .iter()
            .map(|engine| {
                (
                    spelaion_core::plugins::dev_plugin_variable(engine),
                    self.program.display().to_string(),
                )
            })
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{release}: it could not be downloaded from {url}: {why}")]
    Download {
        release: String,
        url: String,
        why: String,
    },
    #[error("{release}: the archive has {found} bytes and the pin says {expected}")]
    Size {
        release: String,
        expected: u64,
        found: u64,
    },
    #[error("{release}: the archive's SHA-256 is {found} and the pin says {expected}")]
    Hash {
        release: String,
        expected: String,
        found: String,
    },
    #[error("{release}: the archive is not what a plugin's release holds: {why}")]
    Archive { release: String, why: String },
    #[error("{release}: {refused}")]
    Program {
        release: String,
        refused: PluginRefused,
    },
    #[error("{what}: {error}")]
    Io { what: String, error: std::io::Error },
}

/// Fetches every plugin `pins` has a build of for `platform` into `into`, from `base` (a
/// GitHub, or something laid out like one: `<base>/<repository>/releases/download/v<version>/
/// <archive>`). It stops at the first one that fails: a set of plugins that is not whole is
/// not what was pinned.
pub fn fetch(
    pins: &Pins,
    platform: &str,
    base: &str,
    into: &Path,
) -> Result<Vec<Fetched>, FetchError> {
    // Whole paths: what is printed is used from other folders (a test runs in its crate's).
    let into = &std::path::absolute(into).map_err(|error| FetchError::Io {
        what: format!("finding {}", into.display()),
        error,
    })?;
    std::fs::create_dir_all(into).map_err(|error| FetchError::Io {
        what: format!("creating {}", into.display()),
        error,
    })?;
    let mut fetched = Vec::new();
    for (plugin, build) in pins.for_platform(platform) {
        fetched.push(fetch_one(plugin, build, base, into)?);
    }
    Ok(fetched)
}

fn fetch_one(
    plugin: &PinnedPlugin,
    build: &PinnedBuild,
    base: &str,
    into: &Path,
) -> Result<Fetched, FetchError> {
    let release = format!("{} {}", plugin.name, plugin.version);
    let url = format!(
        "{}/{}/releases/download/v{}/{}",
        base.trim_end_matches('/'),
        plugin.repository,
        plugin.version,
        build.archive
    );
    let file = program_file_name(&plugin.name);
    // Everything happens in a folder of its own, removed whatever happens: nothing that was
    // not checked is ever left where the programs go.
    let work = Work::new(into, &plugin.name)?;
    let archive = work.path.join(&build.archive);

    download(&url, &archive, build.archive_size).map_err(|why| FetchError::Download {
        release: release.clone(),
        url: url.clone(),
        why,
    })?;
    let found = std::fs::metadata(&archive)
        .map_err(|error| FetchError::Io {
            what: format!("reading {}", archive.display()),
            error,
        })?
        .len();
    if found != build.archive_size {
        return Err(FetchError::Size {
            release,
            expected: build.archive_size,
            found,
        });
    }
    let hash = sha256_of(&archive).map_err(|error| FetchError::Io {
        what: format!("reading {}", archive.display()),
        error,
    })?;
    if hash != build.archive_sha256 {
        return Err(FetchError::Hash {
            release,
            expected: build.archive_sha256.clone(),
            found: hash,
        });
    }

    // It is the pinned archive. It must still be what a release holds: the program at its
    // top, by its own name, and nothing with a path.
    let members =
        run(Command::new(tar()).arg("-tf").arg(&archive)).map_err(|why| FetchError::Archive {
            release: release.clone(),
            why,
        })?;
    let members: Vec<&str> = members
        .lines()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .collect();
    if let Some(odd) = members.iter().find(|m| !is_plain_name(m)) {
        return Err(FetchError::Archive {
            release,
            why: format!("{odd:?} is not a file at its top"),
        });
    }
    if !members.contains(&file.as_str()) {
        return Err(FetchError::Archive {
            release,
            why: format!("it has no {file} (it holds {})", members.join(", ")),
        });
    }
    // Only the program is unpacked.
    run(Command::new(tar())
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(&work.path)
        .arg(&file))
    .map_err(|why| FetchError::Archive {
        release: release.clone(),
        why,
    })?;
    let unpacked = work.path.join(&file);
    check_file(&unpacked, &build.program_sha256).map_err(|refused| FetchError::Program {
        release: release.clone(),
        refused,
    })?;

    // Checked: now it goes where the programs are, in place of whatever was there.
    let program = into.join(&file);
    let _ = std::fs::remove_file(&program);
    std::fs::rename(&unpacked, &program).map_err(|error| FetchError::Io {
        what: format!("putting {} in place", program.display()),
        error,
    })?;
    Ok(Fetched {
        plugin: plugin.name.clone(),
        version: plugin.version.clone(),
        engines: plugin.engines.clone(),
        program,
    })
}

/// A folder for one plugin's download, removed when this value goes.
struct Work {
    path: PathBuf,
}

impl Work {
    fn new(into: &Path, plugin: &str) -> Result<Self, FetchError> {
        let path = into.join(format!(".fetching-{plugin}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).map_err(|error| FetchError::Io {
            what: format!("creating {}", path.display()),
            error,
        })?;
        Ok(Self { path })
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Downloads `url` to `to`, giving up past `most` bytes.
fn download(url: &str, to: &Path, most: u64) -> Result<(), String> {
    run(Command::new("curl")
        .arg("--fail")
        .arg("--location")
        .arg("--silent")
        .arg("--show-error")
        .arg("--proto")
        .arg("=https,file")
        .arg("--max-filesize")
        .arg(most.to_string())
        .arg("--output")
        .arg(to)
        .arg(url))
    .map(|_| ())
}

/// The system's `tar`. On Windows the one of the system folder, which reads `.zip`: the one a
/// shell may put first on the path (GNU tar) does not.
fn tar() -> PathBuf {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        let system = Path::new(&root).join("System32").join("tar.exe");
        if system.is_file() {
            return system;
        }
    }
    PathBuf::from("tar")
}

/// Runs a tool and returns what it printed, or why it failed.
fn run(command: &mut Command) -> Result<String, String> {
    let name = command.get_program().to_string_lossy().into_owned();
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{name} could not be run: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{name} failed: {}", said.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A file's own name: no folder, nothing that climbs out of one.
fn is_plain_name(member: &str) -> bool {
    !member.is_empty() && member != "." && member != ".." && !member.contains(['/', '\\', ':'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_files_own_name_is_a_member() {
        for good in [
            "spelaiondb-plugin-duckdb",
            "spelaiondb-plugin-duckdb.exe",
            "LICENSE",
        ] {
            assert!(is_plain_name(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../spelaiondb-plugin-duckdb",
            "bin/spelaiondb-plugin-duckdb",
            "/usr/bin/x",
            "C:\\Windows\\x.exe",
            "C:x.exe",
            "a\\b",
        ] {
            assert!(!is_plain_name(bad), "{bad}");
        }
    }
}
