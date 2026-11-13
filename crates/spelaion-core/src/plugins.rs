//! Engine plugins in the core (ADR-041, ADR-042): which program is started for an engine.
//!
//! `spelaion-plugin-host` starts the program it is given and talks to it. Deciding which
//! program that may be is the core's, and lives here:
//!
//! - **One process per plugin, per program.** Every `Engines` of this program shares the
//!   plugin of a given file ([`plugin_at`]): two tabs, or the app and one of its exports, never
//!   start two copies. DuckDB needs it: one open per file and process (ADR-016).
//! - **A development build can be pointed at a plugin built on this machine**
//!   (`SPELAION_DEV_PLUGIN_<ENGINE>`), with no pin. **A release build cannot**, by any setting,
//!   variable or repository file: [`dev_plugin`] answers nothing there.
//!
//! What a plugin writes to its standard error goes to this program's, a line at a time, with
//! the secrets it was given already taken out (`spelaiondb mcp` keeps standard output for its
//! own protocol).

pub mod pins;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

pub use spelaion_plugin_host::{Plugin, PluginDriver};

/// The plugins of this program, by the file they are and the hash that file must have (none
/// for a development build's own plugin).
type Registry = HashMap<(PathBuf, Option<String>), Plugin>;
static PLUGINS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// The same file under two spellings is still one plugin.
fn file_of(program: &Path) -> PathBuf {
    std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf())
}

fn registered(program: &Path, sha256: Option<&str>) -> Plugin {
    let key = (file_of(program), sha256.map(str::to_string));
    let mut plugins = PLUGINS
        .get_or_init(Default::default)
        .lock()
        .expect("lock poisoned");
    plugins
        .entry(key)
        .or_insert_with(|| {
            let name = program
                .file_stem()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "plugin".into());
            let plugin = Plugin::new(program)
                .with_log(Arc::new(move |line: &str| eprintln!("[{name}] {line}")));
            match sha256 {
                Some(sha256) => {
                    let sha256 = sha256.to_string();
                    plugin.with_check(Arc::new(move |file: &Path| {
                        pins::check_file(file, &sha256).map_err(|refused| refused.to_string())
                    }))
                }
                None => plugin,
            }
        })
        .clone()
}

/// The plugin whose program is `program`, shared by everything in this program that uses that
/// file. Nothing is started until one of its engines connects. **Unchecked:** only for the
/// plugin a development build was pointed at ([`dev_plugin`]).
pub fn plugin_at(program: &Path) -> Plugin {
    registered(program, None)
}

/// The plugin whose program is `program`, started only while that file's SHA-256 is `sha256`
/// (ADR-042): checked every time its process is about to start. The hash comes from the pins
/// compiled into this build, from a signed catalogue entry, or from what was recorded when the
/// user added the plugin (ADR-055); never from the file itself.
pub fn pinned_plugin(program: &Path, sha256: &str) -> Plugin {
    registered(program, Some(sha256))
}

/// The plugins of `program` this program knows, pinned or not.
fn plugins_of(program: &Path) -> Vec<Plugin> {
    let file = file_of(program);
    PLUGINS
        .get_or_init(Default::default)
        .lock()
        .expect("lock poisoned")
        .iter()
        .filter(|((path, _), _)| path == &file)
        .map(|(_, plugin)| plugin.clone())
        .collect()
}

/// Whether the plugin whose program is `program` is running now.
pub async fn is_running(program: &Path) -> bool {
    for plugin in plugins_of(program) {
        if plugin.is_running().await {
            return true;
        }
    }
    false
}

/// Stops the plugin whose program is `program`, if it runs: its sessions are lost (ADR-028)
/// and the next connection starts it again, checked again. What replacing a plugin's file needs
/// first (Windows keeps a running program's file locked).
pub async fn stop(program: &Path) {
    for plugin in plugins_of(program) {
        plugin.stop().await;
    }
}

/// How Windows starts a path in its verbatim form.
const VERBATIM: &str = r"\\?\";

/// The folder this program is in: where the plugins that ship with it are (ADR-042). The
/// program's real folder, when it was started through a link.
pub fn shipped_folder() -> Option<PathBuf> {
    let program = std::env::current_exe().ok()?;
    Some(plain(&file_of(&program)).parent()?.to_path_buf())
}

/// `path` as a person writes it. Windows gives a resolved path in its verbatim form
/// (`\\?\C:\…`), which is what a user would then read in "this plugin is missing".
fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(VERBATIM) {
        // Only a drive's path: a verbatim network path has no plain form this short.
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// The variable that points a development build at the plugin of `engine` (the name the
/// configuration uses): `SPELAION_DEV_PLUGIN_DUCKDB` for `duckdb`.
pub fn dev_plugin_variable(engine: &str) -> String {
    format!("SPELAION_DEV_PLUGIN_{}", engine.to_ascii_uppercase())
}

/// The plugin a build was pointed at through [`dev_plugin_variable`], whose value is `value`.
/// `release`: this is a release build, which is never pointed anywhere (ADR-042): what it
/// starts is decided by its pins, not by its environment.
pub fn dev_plugin(release: bool, value: Option<OsString>) -> Option<PathBuf> {
    if release {
        return None;
    }
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Whether this is a release build.
pub(crate) const RELEASE_BUILD: bool = !cfg!(debug_assertions);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_development_build_follows_the_variable() {
        let value = Some(OsString::from("target/debug/spelaion-plugin-duckdb"));
        assert_eq!(
            dev_plugin(false, value),
            Some(PathBuf::from("target/debug/spelaion-plugin-duckdb"))
        );
        assert_eq!(dev_plugin(false, None), None);
        assert_eq!(dev_plugin(false, Some(OsString::new())), None);
    }

    /// ADR-042: a release build cannot be pointed at a plugin, whatever the variable says.
    #[test]
    fn a_release_build_is_never_pointed_at_a_plugin() {
        for value in [
            None,
            Some(OsString::new()),
            Some(OsString::from("plugin")),
            Some(OsString::from("C:\\plugins\\spelaion-plugin-duckdb.exe")),
            Some(OsString::from("/usr/lib/spelaiondb/spelaion-plugin-duckdb")),
            Some(OsString::from("../../evil")),
        ] {
            assert_eq!(dev_plugin(true, value.clone()), None, "{value:?}");
        }
    }

    #[test]
    fn a_resolved_path_is_shown_as_a_person_writes_it() {
        assert_eq!(
            plain(Path::new(r"\\?\C:\Program Files\SpelaionDB")),
            PathBuf::from(r"C:\Program Files\SpelaionDB")
        );
        // Anything else stays as it is.
        for same in [
            r"C:\Program Files\SpelaionDB",
            "/usr/lib/spelaiondb",
            r"\\?\UNC\server\share\app",
            r"\\server\share\app",
        ] {
            assert_eq!(plain(Path::new(same)), PathBuf::from(same), "{same}");
        }
        // Where this program is has no verbatim prefix either.
        let folder = shipped_folder().expect("this test is a program somewhere");
        assert!(
            !folder.to_string_lossy().starts_with(VERBATIM),
            "{folder:?}"
        );
        assert!(folder.is_absolute());
    }

    #[test]
    fn the_variable_is_named_after_the_engine() {
        assert_eq!(dev_plugin_variable("duckdb"), "SPELAION_DEV_PLUGIN_DUCKDB");
        assert_eq!(
            dev_plugin_variable("sqlserver"),
            "SPELAION_DEV_PLUGIN_SQLSERVER"
        );
    }

    #[test]
    fn one_file_is_one_plugin_however_it_is_spelled() {
        let directory = std::env::temp_dir();
        let file = directory.join(format!("spelaion-plugins-test-{}", std::process::id()));
        std::fs::write(&file, b"").unwrap();
        let spelled = directory.join(".").join(file.file_name().unwrap());
        let a = plugin_at(&file);
        let b = plugin_at(&spelled);
        // The same plugin: the driver of one is the process of the other.
        assert_eq!(
            std::fs::canonicalize(a.program()).unwrap(),
            std::fs::canonicalize(b.program()).unwrap()
        );
        assert_eq!(a.program(), b.program());
        let _ = std::fs::remove_file(file);
    }
}
