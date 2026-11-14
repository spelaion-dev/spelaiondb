//! The pins of our engine plugins (ADR-042, ADR-055): which builds this SpelaionDB was made
//! and tested with, and the check that a file is one of them.
//!
//! `plugins.toml`, at the root of the repository, names for each plugin its repository, its
//! release, its protocol version and, for each platform, the SHA-256 of its archive and of the
//! program inside. It is compiled into the core: **a pin cannot be changed after the build**,
//! by any file, setting or variable.
//!
//! - **Only a pinned program is ever started.** A file next to the app that no pin names is
//!   not a plugin, whatever it is called.
//! - **A pinned program is started only if it is the pinned build**: its SHA-256 is checked
//!   every time its process is about to start ([`check_file`]). A file that is missing, cannot
//!   be read or has another hash is refused, with both hashes in the error; the engine does not
//!   fall back to anything else.
//! - **One check.** A plugin installed from the Plugins page is checked the same way, against
//!   the hash its signed catalogue entry gave (ADR-055); so is one a user added themselves,
//!   against the hash recorded when they added it.
//!
//! What the check is for: a wrong, damaged or out-of-date file, and a download that is not what
//! was pinned. It is not a defence against someone who can already write next to the app: they
//! can replace the app.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The platforms a plugin is built for (the release's targets).
pub const PLATFORMS: [&str; 4] = [
    "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
];

/// The platform this build runs on, as `plugins.toml` names it; `None` on one no plugin is
/// built for, where no plugin is ever started.
pub fn platform() -> Option<&'static str> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else {
        None
    }
}

/// The pins: every plugin of ours this build knows.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    schema_version: u32,
    #[serde(default, rename = "plugin")]
    plugins: Vec<PinnedPlugin>,
}

/// One plugin: a release of its repository.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedPlugin {
    /// The program's name, without `.exe`: `spelaiondb-plugin-duckdb`.
    pub name: String,
    /// `owner/name` on GitHub, under `spelaion-dev`.
    pub repository: String,
    /// The release pinned.
    pub version: String,
    /// The plugin protocol it speaks, `major.minor`.
    pub protocol: String,
    /// The engines it serves, by the name the configuration uses.
    pub engines: Vec<String>,
    /// The lowest SpelaionDB it works with (the catalogue of ADR-055 carries it).
    pub min_app: String,
    /// Its build for each platform, by target.
    #[serde(rename = "platform")]
    pub platforms: BTreeMap<String, PinnedBuild>,
}

/// One platform's build of a pinned plugin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinnedBuild {
    /// The archive's file name in the release.
    pub archive: String,
    /// The archive's SHA-256: what a download is checked against.
    pub archive_sha256: String,
    /// The archive's size in bytes.
    pub archive_size: u64,
    /// The SHA-256 of the program inside: what is checked before every start.
    pub program_sha256: String,
}

/// Why `plugins.toml` cannot be used.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PinError {
    #[error("plugins.toml cannot be read: {0}")]
    Syntax(String),
    #[error("plugins.toml has schema_version {0}; this SpelaionDB reads version 1")]
    Version(u32),
    #[error("plugins.toml, plugin {plugin:?}: {problem}")]
    Plugin { plugin: String, problem: String },
}

impl Pins {
    /// No pins: nothing is a plugin.
    pub fn none() -> Self {
        Self {
            schema_version: 1,
            plugins: Vec::new(),
        }
    }

    /// Reads and checks a `plugins.toml`. Anything it does not understand refuses the whole
    /// file: a pin half read is not a pin.
    pub fn parse(text: &str) -> Result<Self, PinError> {
        let pins: Self = toml::from_str(text).map_err(|e| PinError::Syntax(e.to_string()))?;
        if pins.schema_version != 1 {
            return Err(PinError::Version(pins.schema_version));
        }
        let mut names = HashSet::new();
        let mut engines = HashSet::new();
        for plugin in &pins.plugins {
            let wrong = |problem: String| PinError::Plugin {
                plugin: plugin.name.clone(),
                problem,
            };
            plugin.check().map_err(wrong)?;
            if !names.insert(plugin.name.as_str()) {
                return Err(wrong("it is pinned twice".into()));
            }
            for engine in &plugin.engines {
                if !engines.insert(engine.clone()) {
                    return Err(wrong(format!("the engine {engine:?} is pinned twice")));
                }
            }
        }
        Ok(pins)
    }

    /// The pins compiled into this build: the repository's `plugins.toml` as it was when it
    /// was built. Nothing read at run time can change them.
    pub fn shipped() -> &'static Self {
        static SHIPPED: OnceLock<Pins> = OnceLock::new();
        SHIPPED.get_or_init(|| {
            // A file that does not parse pins nothing: no plugin is started. The tests of this
            // module fail on it, so it does not get built into a release.
            Self::parse(SHIPPED_TEXT).unwrap_or_else(|_| Self::none())
        })
    }

    pub fn plugins(&self) -> &[PinnedPlugin] {
        &self.plugins
    }

    /// The plugins that have a build for `platform`, each with that build.
    pub fn for_platform<'a>(
        &'a self,
        platform: &'a str,
    ) -> impl Iterator<Item = (&'a PinnedPlugin, &'a PinnedBuild)> {
        self.plugins
            .iter()
            .filter_map(move |plugin| Some((plugin, plugin.platforms.get(platform)?)))
    }
}

/// The repository's `plugins.toml`, as text.
const SHIPPED_TEXT: &str = include_str!("../../../../plugins.toml");

impl PinnedPlugin {
    /// The protocol version it speaks.
    pub fn protocol(&self) -> (u32, u32) {
        protocol_of(&self.protocol).unwrap_or((0, 0))
    }

    /// Its release, as numbers: what "newer" is decided with.
    pub fn version(&self) -> (u32, u32, u32) {
        version_of(&self.version).unwrap_or((0, 0, 0))
    }

    /// The lowest SpelaionDB it works with, as numbers.
    pub fn min_app(&self) -> (u32, u32, u32) {
        version_of(&self.min_app).unwrap_or((0, 0, 0))
    }

    /// Where its archive for a platform is: a release of its own repository under
    /// `spelaion-dev` on GitHub, over HTTPS. The address is written here from what was checked
    /// ([`PinnedPlugin::check`]); no pin and no catalogue carries one, so none can name
    /// another host or another repository.
    pub fn archive_address(&self, build: &PinnedBuild) -> String {
        format!(
            "https://github.com/{}/releases/download/v{}/{}",
            self.repository, self.version, build.archive
        )
    }

    /// Whether this is a pin as one must be written: what `plugins.toml` asks of each plugin,
    /// and the signed catalogue of each version it lists (ADR-055). The problem, if it is not.
    pub(crate) fn check(&self) -> Result<(), String> {
        if !is_program_name(&self.name) {
            return Err(
                "its name must be `spelaiondb-plugin-` and lowercase letters, digits or `-`".into(),
            );
        }
        if !is_our_repository(&self.repository, &self.name) {
            return Err(format!("its repository must be spelaion-dev/{}", self.name));
        }
        if !is_version(&self.version) || !is_version(&self.min_app) {
            return Err("`version` and `min_app` are three numbers, as in 0.1.0".into());
        }
        if protocol_of(&self.protocol).is_none() {
            return Err("`protocol` is two numbers, as in 1.0".into());
        }
        if self.engines.is_empty() {
            return Err("it serves no engine".into());
        }
        let mut engines = HashSet::new();
        for engine in &self.engines {
            if !is_engine_name(engine) {
                return Err(format!("{engine:?} is not an engine's name"));
            }
            if !engines.insert(engine.as_str()) {
                return Err(format!("the engine {engine:?} is pinned twice"));
            }
        }
        if self.platforms.is_empty() {
            return Err("it has no build".into());
        }
        for (platform, build) in &self.platforms {
            if !PLATFORMS.contains(&platform.as_str()) {
                return Err(format!("{platform:?} is not a platform we build for"));
            }
            if !is_sha256(&build.archive_sha256) || !is_sha256(&build.program_sha256) {
                return Err(format!(
                    "{platform}: a SHA-256 is 64 lowercase hexadecimal digits"
                ));
            }
            if build.archive_size == 0 || !is_file_name(&build.archive) {
                return Err(format!(
                    "{platform}: the archive needs a file name and a size"
                ));
            }
        }
        Ok(())
    }
}

/// A pinned plugin's file name on this machine: `.exe` on Windows.
pub fn program_file_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Why a file is not started as a plugin.
#[derive(Debug, thiserror::Error)]
pub enum PluginRefused {
    #[error("{} is missing", .0.display())]
    Missing(PathBuf),
    #[error("{} cannot be read: {error}", .path.display())]
    Unreadable { path: PathBuf, error: String },
    #[error(
        "{} is not the build that was pinned (its SHA-256 is {found}, the pinned one is \
         {expected})",
        .path.display()
    )]
    Changed {
        path: PathBuf,
        expected: String,
        found: String,
    },
}

/// The SHA-256 of a file, in lowercase hexadecimal.
pub fn sha256_of(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The check: `path` is the file whose SHA-256 is `sha256`. Run before a plugin's process
/// starts, every time.
pub fn check_file(path: &Path, sha256: &str) -> Result<(), PluginRefused> {
    if !path.is_file() {
        return Err(PluginRefused::Missing(path.to_path_buf()));
    }
    let found = sha256_of(path).map_err(|e| PluginRefused::Unreadable {
        path: path.to_path_buf(),
        error: e.to_string(),
    })?;
    if found != sha256 {
        return Err(PluginRefused::Changed {
            path: path.to_path_buf(),
            expected: sha256.to_string(),
            found,
        });
    }
    Ok(())
}

pub(crate) fn is_program_name(name: &str) -> bool {
    name.strip_prefix("spelaiondb-plugin-").is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 40
            && !rest.starts_with('-')
            && !rest.ends_with('-')
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

fn is_our_repository(repository: &str, name: &str) -> bool {
    repository
        .strip_prefix("spelaion-dev/")
        .is_some_and(|rest| rest == name)
}

fn is_engine_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub(crate) fn is_version(text: &str) -> bool {
    version_of(text).is_some()
}

/// A version's three numbers: `0.10.2` is `(0, 10, 2)`.
pub(crate) fn version_of(text: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| is_number(p)) {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

fn protocol_of(text: &str) -> Option<(u32, u32)> {
    let (major, minor) = text.split_once('.')?;
    if !is_number(major) || !is_number(minor) {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.len() <= 5 && text.chars().all(|c| c.is_ascii_digit())
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// A plain file name: no folder, nothing that climbs out of one.
fn is_file_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 200
        && text != "."
        && text != ".."
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn one_plugin() -> String {
        format!(
            r#"schema_version = 1

[[plugin]]
name = "spelaiondb-plugin-duckdb"
repository = "spelaion-dev/spelaiondb-plugin-duckdb"
version = "0.1.0"
protocol = "1.0"
engines = ["duckdb"]
min_app = "0.10.0"

[plugin.platform.x86_64-pc-windows-msvc]
archive = "spelaiondb-plugin-duckdb-0.1.0-x86_64-pc-windows-msvc.zip"
archive_sha256 = "{HASH_A}"
archive_size = 1234
program_sha256 = "{HASH_B}"
"#
        )
    }

    /// The file in the repository is one this build can use: it parses, and every plugin in
    /// it speaks a protocol version this core speaks.
    #[test]
    fn the_shipped_pins_are_ones_this_core_can_use() {
        let pins = Pins::parse(SHIPPED_TEXT).expect("plugins.toml");
        assert_eq!(pins.plugins().len(), Pins::shipped().plugins().len());
        let ours = spelaion_plugin_host::VERSION;
        for plugin in pins.plugins() {
            let (major, minor) = plugin.protocol();
            assert!(
                major == ours.major && minor <= ours.minor,
                "{} speaks {}; this core speaks {ours}",
                plugin.name,
                plugin.protocol
            );
        }
    }

    #[test]
    fn a_pin_is_read_whole() {
        let pins = Pins::parse(&one_plugin()).unwrap();
        let plugin = &pins.plugins()[0];
        assert_eq!(plugin.name, "spelaiondb-plugin-duckdb");
        assert_eq!(plugin.engines, ["duckdb"]);
        assert_eq!(plugin.protocol(), (1, 0));
        let (_, build) = pins.for_platform("x86_64-pc-windows-msvc").next().unwrap();
        assert_eq!(build.program_sha256, HASH_B);
        assert_eq!(build.archive_sha256, HASH_A);
        // A platform it has no build for has no pin: nothing is started there.
        assert!(
            pins.for_platform("x86_64-unknown-linux-gnu")
                .next()
                .is_none()
        );
        assert!(
            Pins::parse("schema_version = 1\n")
                .unwrap()
                .plugins()
                .is_empty()
        );
    }

    /// Anything the reader does not understand refuses the file: a pin is never half read.
    #[test]
    fn a_file_that_is_not_understood_pins_nothing() {
        let good = one_plugin();
        let cases: Vec<(&str, String)> = vec![
            (
                "another schema",
                good.replace("schema_version = 1", "schema_version = 2"),
            ),
            (
                "an unknown key",
                good.replace("min_app", "url = \"http://x\"\nmin_app"),
            ),
            ("a hash that is not one", good.replace(HASH_B, "abc")),
            (
                "an uppercase hash",
                good.replace(HASH_B, &HASH_B.to_uppercase()),
            ),
            (
                "no program hash",
                good.replace(&format!("program_sha256 = \"{HASH_B}\"\n"), ""),
            ),
            (
                "a platform we do not build",
                good.replace("x86_64-pc-windows-msvc]", "riscv64-linux]"),
            ),
            (
                "someone else's repository",
                good.replace("spelaion-dev/", "evil/"),
            ),
            (
                "a repository that is not the plugin's",
                good.replace(
                    "repository = \"spelaion-dev/spelaiondb-plugin-duckdb\"",
                    "repository = \"spelaion-dev/spelaiondb\"",
                ),
            ),
            (
                "a name that is not a plugin's",
                good.replace(
                    "name = \"spelaiondb-plugin-duckdb\"",
                    "name = \"../spelaiondb\"",
                ),
            ),
            (
                "no engine",
                good.replace("engines = [\"duckdb\"]", "engines = []"),
            ),
            (
                "an engine that is not a name",
                good.replace("[\"duckdb\"]", "[\"duck db\"]"),
            ),
            (
                "a version that is not one",
                good.replace("version = \"0.1.0\"", "version = \"latest\""),
            ),
            (
                "a protocol that is not one",
                good.replace("protocol = \"1.0\"", "protocol = \"1\""),
            ),
            (
                "an archive with a path",
                good.replace("archive = \"spelaiondb", "archive = \"../spelaiondb"),
            ),
            (
                "an empty archive",
                good.replace("archive_size = 1234", "archive_size = 0"),
            ),
            (
                "no build",
                good.split("[plugin.platform").next().unwrap().to_string(),
            ),
            ("not TOML", "schema_version = ".to_string()),
        ];
        for (what, text) in cases {
            assert!(Pins::parse(&text).is_err(), "{what} was accepted");
        }
        // The same plugin, or the same engine, pinned twice.
        let twice = format!("{good}\n{}", good.replace("schema_version = 1\n", ""));
        assert!(
            Pins::parse(&twice).is_err(),
            "a plugin pinned twice was accepted"
        );
        let other = good
            .replace("schema_version = 1\n", "")
            .replace("spelaiondb-plugin-duckdb", "spelaiondb-plugin-quack");
        assert!(
            matches!(
                Pins::parse(&format!("{good}\n{other}")),
                Err(PinError::Plugin { problem, .. }) if problem.contains("pinned twice")
            ),
            "an engine pinned twice was accepted"
        );
    }

    #[test]
    fn a_file_is_accepted_only_with_its_hash() {
        let dir = std::env::temp_dir().join(format!("spelaion-pins-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("program");
        std::fs::write(&file, b"the pinned build").unwrap();
        let hash = sha256_of(&file).unwrap();
        check_file(&file, &hash).unwrap();

        // One byte more, and it is another file.
        std::fs::write(&file, b"the pinned build.").unwrap();
        let refused = check_file(&file, &hash).unwrap_err();
        assert!(
            matches!(&refused, PluginRefused::Changed { expected, found, .. }
                if expected == &hash && found != &hash),
            "{refused}"
        );
        assert!(refused.to_string().contains(&hash), "{refused}");

        // A missing file and a folder are not programs.
        assert!(matches!(
            check_file(&dir.join("absent"), &hash),
            Err(PluginRefused::Missing(_))
        ));
        assert!(matches!(
            check_file(&dir, &hash),
            Err(PluginRefused::Missing(_))
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_hash_is_sha_256() {
        let dir = std::env::temp_dir().join(format!("spelaion-pins-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc");
        std::fs::write(&file, b"abc").unwrap();
        // The test vector of FIPS 180-2.
        assert_eq!(
            sha256_of(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
