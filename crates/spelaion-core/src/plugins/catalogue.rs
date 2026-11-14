//! The catalogue of our plugins (ADR-055): the one signed file that says which builds of our
//! engines exist besides the ones pinned into this build.
//!
//! It has the shape of `plugins.toml` (`[[plugin]]`, once for every version that was ever
//! pinned there by a merged pull request), with a serial number that only grows and the
//! builds that were withdrawn (`[[withdrawn]]`, each with its reason).
//!
//! - **Accepted only with our signature.** A minisign signature made by the catalogue's key,
//!   whose public half is compiled into this build ([`CatalogueKey::shipped`]). Nothing read
//!   at run time can name another key: no setting, variable or repository file is ever asked
//!   for one. **A build with no key accepts no catalogue.**
//! - **The signature is checked before a byte is read as a catalogue**, and the size before
//!   the signature ([`MAX_CATALOGUE_BYTES`]).
//! - **Read whole or not at all.** Each version must be a pin as `plugins.toml` asks
//!   ([`PinnedPlugin::check`]): a plugin's name, its repository under `spelaion-dev`, hashes.
//!   Anything else refuses the file.
//! - **It carries no address.** Where an archive is comes from what was checked
//!   ([`PinnedPlugin::archive_address`]): a release of that plugin's repository.
//!
//! The serial ("never one lower than the last accepted") is the store's to keep
//! ([`super::installed::PluginStore::accept_catalogue`]).

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use super::pins::{PinnedPlugin, is_program_name, version_of};

/// A catalogue larger than this is not read: ours lists a handful of plugins.
pub const MAX_CATALOGUE_BYTES: usize = 1 << 20;

/// The longest reason a withdrawn build may carry: it is shown to the user.
const MAX_REASON_CHARS: usize = 300;

/// The public half of the catalogue's key, as minisign prints it (the second line of its
/// `.pub` file). **Not the update key** (ADR-021): an app release can replace this key; this
/// key can never replace the app.
///
/// `None` until the key exists: until then no catalogue is accepted, and the only plugins are
/// the ones pinned into the build.
const SHIPPED_KEY: Option<&str> = None;

/// The key a catalogue must be signed with.
#[derive(Debug, Clone)]
pub struct CatalogueKey(minisign_verify::PublicKey);

impl CatalogueKey {
    /// The key compiled into this build: the only one the app ever uses. `None` when this
    /// build has none.
    pub fn shipped() -> Option<Self> {
        Self::from_base64(SHIPPED_KEY?).ok()
    }

    /// A key from its public half, as minisign prints it. For the tests, which sign with a key
    /// of their own; nothing in the app calls it with anything read at run time.
    pub fn from_base64(text: &str) -> Result<Self, CatalogueError> {
        minisign_verify::PublicKey::from_base64(text.trim())
            .map(Self)
            .map_err(|e| CatalogueError::Key(e.to_string()))
    }
}

/// Why a catalogue is not accepted.
#[derive(Debug, thiserror::Error)]
pub enum CatalogueError {
    #[error("this SpelaionDB has no key for the plugin catalogue, so it accepts none")]
    NoKey,
    #[error("that is not a catalogue key: {0}")]
    Key(String),
    #[error(
        "the plugin catalogue has {0} bytes; one larger than {MAX_CATALOGUE_BYTES} is not read"
    )]
    TooLarge(usize),
    #[error("the plugin catalogue is not signed by SpelaionDB: {0}")]
    Signature(String),
    #[error("the plugin catalogue cannot be read: {0}")]
    Syntax(String),
    #[error("the plugin catalogue has schema_version {0}; this SpelaionDB reads version 1")]
    Version(u32),
    #[error("the plugin catalogue, {plugin} {version}: {problem}")]
    Entry {
        plugin: String,
        version: String,
        problem: String,
    },
    #[error(
        "the plugin catalogue is older than the one already accepted (its serial is {offered}, \
         the accepted one is {accepted})"
    )]
    Older { offered: u64, accepted: u64 },
    #[error(
        "the plugin catalogue has the serial of the one already accepted ({0}) and is not the \
         same file"
    )]
    SameSerial(u64),
    #[error("the plugin catalogue could not be kept: {0}")]
    Io(String),
}

/// A build that was withdrawn: never started again once a catalogue that says so is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Withdrawn {
    /// The plugin's name: `spelaiondb-plugin-duckdb`.
    pub name: String,
    pub version: String,
    /// Why, as the user reads it.
    pub reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema_version: u32,
    serial: u64,
    #[serde(default, rename = "plugin")]
    plugins: Vec<PinnedPlugin>,
    #[serde(default)]
    withdrawn: Vec<Withdrawn>,
}

/// A catalogue that was signed by us and read whole.
#[derive(Debug, Clone)]
pub struct Catalogue {
    serial: u64,
    plugins: Vec<PinnedPlugin>,
    withdrawn: Vec<Withdrawn>,
}

impl Catalogue {
    /// The catalogue `text` is, if `signature` (a minisign signature file's text) is `key`'s
    /// signature of exactly those bytes.
    pub fn verify(
        key: &CatalogueKey,
        text: &[u8],
        signature: &str,
    ) -> Result<Self, CatalogueError> {
        if text.len() > MAX_CATALOGUE_BYTES {
            return Err(CatalogueError::TooLarge(text.len()));
        }
        // A signature file is four short lines.
        if signature.len() > 4096 {
            return Err(CatalogueError::Signature("it is not a signature".into()));
        }
        let signature = minisign_verify::Signature::decode(signature)
            .map_err(|e| CatalogueError::Signature(e.to_string()))?;
        // Never the legacy form (a signature of the bytes themselves): ours are made over the
        // file's hash, as minisign has done by default for years.
        key.0
            .verify(text, &signature, false)
            .map_err(|e| CatalogueError::Signature(e.to_string()))?;
        Self::read(text)
    }

    /// Reads and checks a catalogue's text. **Not a way in:** only [`Catalogue::verify`] calls
    /// it, after the signature (and this module's tests, for what a signed file may hold).
    fn read(text: &[u8]) -> Result<Self, CatalogueError> {
        let text = std::str::from_utf8(text)
            .map_err(|_| CatalogueError::Syntax("it is not UTF-8 text".into()))?;
        let file: File = toml::from_str(text).map_err(|e| CatalogueError::Syntax(e.to_string()))?;
        if file.schema_version != 1 {
            return Err(CatalogueError::Version(file.schema_version));
        }
        let mut versions = HashSet::new();
        // An engine belongs to one plugin, in every version of it.
        let mut engines: HashMap<&str, &str> = HashMap::new();
        for plugin in &file.plugins {
            let wrong = |problem: String| CatalogueError::Entry {
                plugin: plugin.name.clone(),
                version: plugin.version.clone(),
                problem,
            };
            plugin.check().map_err(wrong)?;
            if !versions.insert((plugin.name.as_str(), plugin.version())) {
                return Err(wrong("it is listed twice".into()));
            }
            for engine in &plugin.engines {
                let owner = engines.entry(engine.as_str()).or_insert(&plugin.name);
                if *owner != plugin.name {
                    return Err(wrong(format!("the engine {engine:?} is already {owner}'s")));
                }
            }
        }
        let mut withdrawn = HashSet::new();
        for entry in &file.withdrawn {
            let wrong = |problem: &str| CatalogueError::Entry {
                plugin: entry.name.clone(),
                version: entry.version.clone(),
                problem: format!("withdrawn: {problem}"),
            };
            if !is_program_name(&entry.name) {
                return Err(wrong("that is not a plugin's name"));
            }
            let Some(version) = version_of(&entry.version) else {
                return Err(wrong("a version is three numbers, as in 0.1.0"));
            };
            let reason = entry.reason.trim();
            if reason.is_empty()
                || entry.reason.chars().count() > MAX_REASON_CHARS
                || entry.reason.chars().any(char::is_control)
            {
                return Err(wrong(
                    "it needs a reason: one line of at most 300 characters",
                ));
            }
            if !withdrawn.insert((entry.name.as_str(), version)) {
                return Err(wrong("it is withdrawn twice"));
            }
        }
        Ok(Self {
            serial: file.serial,
            plugins: file.plugins,
            withdrawn: file.withdrawn,
        })
    }

    /// Its serial number: a catalogue with a lower one than the last accepted is refused.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// Every version it lists, withdrawn or not.
    pub fn versions(&self) -> &[PinnedPlugin] {
        &self.plugins
    }

    /// The builds it withdraws.
    pub fn withdrawn(&self) -> &[Withdrawn] {
        &self.withdrawn
    }

    /// The version `version` of the plugin `name`, if it lists it.
    pub fn version(&self, name: &str, version: (u32, u32, u32)) -> Option<&PinnedPlugin> {
        self.plugins
            .iter()
            .find(|plugin| plugin.name == name && plugin.version() == version)
    }

    /// Why that build was withdrawn, if it was.
    pub fn why_withdrawn(&self, name: &str, version: (u32, u32, u32)) -> Option<&str> {
        self.withdrawn
            .iter()
            .find(|entry| entry.name == name && version_of(&entry.version) == Some(version))
            .map(|entry| entry.reason.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn version(number: &str) -> String {
        format!(
            r#"
[[plugin]]
name = "spelaiondb-plugin-duckdb"
repository = "spelaion-dev/spelaiondb-plugin-duckdb"
version = "{number}"
protocol = "1.0"
engines = ["duckdb"]
min_app = "0.10.0"

[plugin.platform.x86_64-pc-windows-msvc]
archive = "spelaiondb-plugin-duckdb-{number}-x86_64-pc-windows-msvc.tar.gz"
archive_sha256 = "{HASH_A}"
archive_size = 1234
program_sha256 = "{HASH_B}"
"#
        )
    }

    fn catalogue() -> String {
        format!(
            "schema_version = 1\nserial = 7\n{}{}\n[[withdrawn]]\nname = \
             \"spelaiondb-plugin-duckdb\"\nversion = \"0.1.0\"\nreason = \"It lost rows on \
             import.\"\n",
            version("0.1.0"),
            version("0.1.1")
        )
    }

    #[test]
    fn a_catalogue_is_read_whole() {
        let read = Catalogue::read(catalogue().as_bytes()).expect("the catalogue");
        assert_eq!(read.serial(), 7);
        assert_eq!(read.versions().len(), 2);
        let newer = read
            .version("spelaiondb-plugin-duckdb", (0, 1, 1))
            .expect("0.1.1");
        assert_eq!(newer.engines, ["duckdb"]);
        assert!(
            read.version("spelaiondb-plugin-duckdb", (0, 2, 0))
                .is_none()
        );
        assert_eq!(
            read.why_withdrawn("spelaiondb-plugin-duckdb", (0, 1, 0)),
            Some("It lost rows on import.")
        );
        assert_eq!(
            read.why_withdrawn("spelaiondb-plugin-duckdb", (0, 1, 1)),
            None
        );
        // One with nothing in it is a catalogue too: nothing is offered.
        let empty = Catalogue::read(b"schema_version = 1\nserial = 1\n").unwrap();
        assert!(empty.versions().is_empty() && empty.withdrawn().is_empty());
    }

    /// Anything the reader does not understand refuses the file, as with the pins.
    #[test]
    fn a_catalogue_that_is_not_understood_offers_nothing() {
        let good = catalogue();
        let cases: Vec<(&str, String)> = vec![
            (
                "another schema",
                good.replace("schema_version = 1", "schema_version = 2"),
            ),
            ("no serial", good.replace("serial = 7\n", "")),
            (
                "a negative serial",
                good.replace("serial = 7", "serial = -1"),
            ),
            (
                "an address",
                good.replace(
                    "min_app = \"0.10.0\"",
                    "min_app = \"0.10.0\"\nurl = \"https://evil.example/x\"",
                ),
            ),
            (
                "a key",
                good.replace("serial = 7", "serial = 7\nkey = \"RWQ\""),
            ),
            (
                "someone else's repository",
                good.replace("spelaion-dev/", "evil/"),
            ),
            (
                "an archive with a path",
                good.replace("archive = \"spelaiondb", "archive = \"../spelaiondb"),
            ),
            ("a hash that is not one", good.replace(HASH_B, "abc")),
            (
                "a version listed twice",
                format!("{good}{}", version("0.1.1")),
            ),
            (
                "a version listed twice under another spelling",
                format!("{good}{}", version("0.01.1")),
            ),
            (
                "an engine that is another plugin's",
                format!(
                    "{good}{}",
                    version("0.3.0").replace("spelaiondb-plugin-duckdb", "spelaiondb-plugin-quack")
                ),
            ),
            (
                "a withdrawn build with no reason",
                good.replace("It lost rows on import.", " "),
            ),
            (
                "a reason with a line break",
                good.replace("It lost rows on import.", "It lost\\nrows."),
            ),
            (
                "a reason that is a page",
                good.replace("It lost rows on import.", &"x".repeat(301)),
            ),
            (
                "a withdrawn build that is not a plugin's",
                good.replace(
                    "name = \"spelaiondb-plugin-duckdb\"\nversion = \"0.1.0\"\nreason",
                    "name = \"../x\"\nversion = \"0.1.0\"\nreason",
                ),
            ),
            (
                "a withdrawn version that is not one",
                good.replace("version = \"0.1.0\"\nreason", "version = \"all\"\nreason"),
            ),
            (
                "a build withdrawn twice",
                format!(
                    "{good}\n[[withdrawn]]\nname = \"spelaiondb-plugin-duckdb\"\nversion = \
                     \"0.1.0\"\nreason = \"Again.\"\n"
                ),
            ),
            ("not TOML", "schema_version = ".to_string()),
        ];
        for (what, text) in cases {
            assert!(
                Catalogue::read(text.as_bytes()).is_err(),
                "{what} was accepted"
            );
        }
        assert!(
            Catalogue::read(&[0xff, 0xfe, 0x00]).is_err(),
            "bytes that are not text were accepted"
        );
    }

    /// An archive's address is written by the core, from a name and a repository it checked:
    /// always a release of that plugin's repository under `spelaion-dev`, over HTTPS.
    #[test]
    fn an_archive_is_only_ever_in_its_plugins_repository() {
        let read = Catalogue::read(catalogue().as_bytes()).unwrap();
        for plugin in read.versions() {
            for build in plugin.platforms.values() {
                let address = plugin.archive_address(build);
                assert!(
                    address.starts_with(&format!(
                        "https://github.com/spelaion-dev/{}/releases/download/v{}/",
                        plugin.name, plugin.version
                    )),
                    "{address}"
                );
                assert!(address.ends_with(&build.archive), "{address}");
                assert!(!address.contains(".."), "{address}");
            }
        }
    }

    /// Until the key exists, this build accepts no catalogue; and when it has one, it is a key.
    #[test]
    fn the_shipped_key_is_a_key_or_there_is_none() {
        match SHIPPED_KEY {
            None => assert!(CatalogueKey::shipped().is_none()),
            Some(text) => {
                CatalogueKey::from_base64(text).expect("the catalogue key compiled in");
            }
        }
        assert!(CatalogueKey::from_base64("not a key").is_err());
        assert!(CatalogueKey::from_base64("").is_err());
    }
}
