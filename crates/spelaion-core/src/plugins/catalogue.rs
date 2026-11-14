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

pub use super::pins::Withdrawn;
use super::pins::{PinnedPlugin, Pins, version_of};

/// A catalogue larger than this is not read: ours lists a handful of plugins.
pub const MAX_CATALOGUE_BYTES: usize = 1 << 20;

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

/// Why the catalogue that follows the published one cannot be written.
#[derive(Debug, thiserror::Error)]
pub enum NextError {
    #[error(
        "{plugin} {version} is already in the published catalogue with other contents: a \
         published version never changes, release another one"
    )]
    Changed { plugin: String, version: String },
    #[error(
        "{plugin} {version} is pinned and was withdrawn: a release must not carry a build we \
         withdrew"
    )]
    Withdrawn { plugin: String, version: String },
    #[error("the next catalogue would lose something the published one has")]
    Drops,
    #[error("the next catalogue would not be one: {0}")]
    Wrong(CatalogueError),
    #[error("the published catalogue has the highest serial there is")]
    Serial,
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
            let wrong = |problem: String| CatalogueError::Entry {
                plugin: entry.name.clone(),
                version: entry.version.clone(),
                problem: format!("withdrawn: {problem}"),
            };
            entry.check().map_err(wrong)?;
            if !withdrawn.insert((entry.name.as_str(), entry.version())) {
                return Err(wrong("it is withdrawn twice".into()));
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

    /// The catalogue `signed` is: one file, its signature's four lines and then its text
    /// ([`signed_catalogue`]), as it is published and kept.
    pub fn verify_signed(key: &CatalogueKey, signed: &[u8]) -> Result<Self, CatalogueError> {
        let (signature, text) = split_signed(signed)?;
        Self::verify(key, text, signature)
    }

    /// The text of the catalogue that follows `published` (none yet, the first time), given
    /// the pins as they are now: what the publishing workflow signs. `None` when it would say
    /// nothing new.
    ///
    /// **It only adds.** Every version and every withdrawn build of the published catalogue
    /// stays, as it was; each pinned version it does not list yet is added, and each build the
    /// pins withdraw. A pinned version that is already published must be the published one,
    /// byte for byte: a version's build never changes. The serial is the next one. (A
    /// withdrawn build's reason may be reworded: the pins' wording replaces the published
    /// one.)
    pub fn next(published: Option<&Catalogue>, pins: &Pins) -> Result<Option<String>, NextError> {
        let mut versions: Vec<&PinnedPlugin> = published
            .map(|published| published.plugins.iter().collect())
            .unwrap_or_default();
        let mut withdrawn: Vec<&Withdrawn> = published
            .map(|published| published.withdrawn.iter().collect())
            .unwrap_or_default();
        let mut news = false;
        for pin in pins.plugins() {
            match versions
                .iter()
                .find(|listed| listed.name == pin.name && listed.version() == pin.version())
            {
                Some(listed) if *listed == pin => {}
                Some(_) => {
                    return Err(NextError::Changed {
                        plugin: pin.name.clone(),
                        version: pin.version.clone(),
                    });
                }
                None => {
                    versions.push(pin);
                    news = true;
                }
            }
        }
        for entry in pins.withdrawn() {
            match withdrawn
                .iter_mut()
                .find(|listed| listed.name == entry.name && listed.version() == entry.version())
            {
                Some(listed) if listed.reason == entry.reason => {}
                Some(listed) => {
                    *listed = entry;
                    news = true;
                }
                None => {
                    withdrawn.push(entry);
                    news = true;
                }
            }
        }
        // The pins must not bring back a build that was withdrawn, here or in a catalogue
        // already published.
        if let Some(pin) = pins.plugins().iter().find(|pin| {
            withdrawn
                .iter()
                .any(|entry| entry.name == pin.name && entry.version() == pin.version())
        }) {
            return Err(NextError::Withdrawn {
                plugin: pin.name.clone(),
                version: pin.version.clone(),
            });
        }
        if published.is_some() && !news {
            return Ok(None);
        }
        let serial = match published {
            Some(published) => published.serial.checked_add(1).ok_or(NextError::Serial)?,
            None => 1,
        };
        let text = written(serial, &versions, &withdrawn);
        // What was written is read as the app will read it: an engine claimed by two plugins,
        // or anything else a catalogue may not hold, stops here and not in users' apps.
        let next = Self::read(text.as_bytes()).map_err(NextError::Wrong)?;
        // By construction; checked all the same, because a catalogue that loses a withdrawn
        // build lets it start again.
        if published.is_some_and(|published| !next.keeps(published)) {
            return Err(NextError::Drops);
        }
        Ok(Some(text))
    }

    /// Whether this catalogue has everything `older` had: each version as it was, and each
    /// withdrawn build.
    pub fn keeps(&self, older: &Catalogue) -> bool {
        older
            .plugins
            .iter()
            .all(|was| self.version(&was.name, was.version()) == Some(was))
            && older
                .withdrawn
                .iter()
                .all(|was| self.why_withdrawn(&was.name, was.version()).is_some())
    }
}

/// A catalogue and its signature as one file: the signature's four lines (a minisign
/// signature file's), then the catalogue's text byte for byte. How it is kept in the store, so
/// the two are replaced together, and how it is published, so the two are fetched together.
pub fn signed_catalogue(signature: &str, text: &[u8]) -> Vec<u8> {
    let mut signed = Vec::with_capacity(signature.len() + text.len() + 4);
    for line in signature.lines().take(4) {
        signed.extend_from_slice(line.trim_end().as_bytes());
        signed.push(b'\n');
    }
    signed.extend_from_slice(text);
    signed
}

/// The signature and the text of a signed catalogue file.
pub(crate) fn split_signed(signed: &[u8]) -> Result<(&str, &[u8]), CatalogueError> {
    // A catalogue and the four lines of its signature.
    if signed.len() > MAX_CATALOGUE_BYTES + 8192 {
        return Err(CatalogueError::TooLarge(signed.len()));
    }
    let no_signature = || CatalogueError::Signature("it has no signature".into());
    let end = signed
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(3)
        .map(|(at, _)| at + 1)
        .ok_or_else(no_signature)?;
    let signature = std::str::from_utf8(&signed[..end]).map_err(|_| no_signature())?;
    Ok((signature, &signed[end..]))
}

/// A catalogue's text, as the tool writes it: the published file is never edited by hand.
fn written(serial: u64, versions: &[&PinnedPlugin], withdrawn: &[&Withdrawn]) -> String {
    let mut text = String::from(
        "# The catalogue of SpelaionDB's plugins (ADR-055): every build that was pinned in\n\
         # plugins.toml, and the ones withdrawn. Written from plugins.toml and signed when the\n\
         # pins change; never edited by hand.\n\n",
    );
    text.push_str(&format!("schema_version = 1\nserial = {serial}\n"));
    for plugin in versions {
        let engines: Vec<String> = plugin.engines.iter().map(|engine| quoted(engine)).collect();
        text.push_str(&format!(
            "\n[[plugin]]\nname = {}\nrepository = {}\nversion = {}\nprotocol = {}\nengines = \
             [{}]\nmin_app = {}\n",
            quoted(&plugin.name),
            quoted(&plugin.repository),
            quoted(&plugin.version),
            quoted(&plugin.protocol),
            engines.join(", "),
            quoted(&plugin.min_app),
        ));
        for (platform, build) in &plugin.platforms {
            text.push_str(&format!(
                "\n[plugin.platform.{platform}]\narchive = {}\narchive_sha256 = \
                 {}\narchive_size = {}\nprogram_sha256 = {}\n",
                quoted(&build.archive),
                quoted(&build.archive_sha256),
                build.archive_size,
                quoted(&build.program_sha256),
            ));
        }
    }
    for entry in withdrawn {
        text.push_str(&format!(
            "\n[[withdrawn]]\nname = {}\nversion = {}\nreason = {}\n",
            quoted(&entry.name),
            quoted(&entry.version),
            quoted(&entry.reason),
        ));
    }
    text
}

/// A TOML string. What goes in was checked to hold no control character; a quote or a
/// backslash in a reason is escaped.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
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

    #[test]
    fn a_signed_catalogue_is_its_signature_then_its_text() {
        let signature = "untrusted comment: a\r\nAAAA\r\ntrusted comment: b\r\nBBBB\r\n";
        let text = b"schema_version = 1\nserial = 3\n\n# a comment\n";
        let signed = signed_catalogue(signature, text);
        let (again, same) = split_signed(&signed).expect("four lines and a text");
        assert_eq!(
            again,
            "untrusted comment: a\nAAAA\ntrusted comment: b\nBBBB\n"
        );
        assert_eq!(same, text);
        // Fewer than four lines is not a signed catalogue, and neither is something huge.
        assert!(split_signed(b"one\ntwo\nthree").is_err());
        assert!(split_signed(b"").is_err());
        assert!(matches!(
            split_signed(&vec![b'\n'; MAX_CATALOGUE_BYTES + 8193]),
            Err(CatalogueError::TooLarge(_))
        ));
    }

    /// The pins of a `plugins.toml` that holds `text`.
    fn pins(text: &str) -> Pins {
        Pins::parse(&format!("schema_version = 1\n{text}")).expect("the test's pins")
    }

    fn withdrawn(number: &str, reason: &str) -> String {
        format!(
            "\n[[withdrawn]]\nname = \"spelaiondb-plugin-duckdb\"\nversion = \
             \"{number}\"\nreason = \"{reason}\"\n"
        )
    }

    fn next(published: Option<&Catalogue>, pins: &Pins) -> Catalogue {
        let text = Catalogue::next(published, pins)
            .expect("a next catalogue")
            .expect("something new");
        Catalogue::read(text.as_bytes()).expect("what was written is a catalogue")
    }

    #[test]
    fn the_first_catalogue_lists_the_pins() {
        let now = pins(&format!(
            "{}{}",
            version("0.1.0"),
            withdrawn("0.0.9", "It lost rows.")
        ));
        let first = next(None, &now);
        assert_eq!(first.serial(), 1);
        assert_eq!(first.versions(), now.plugins());
        assert_eq!(first.withdrawn(), now.withdrawn());
        // With nothing pinned yet there is still a first catalogue: it offers nothing.
        let empty = next(None, &Pins::none());
        assert_eq!(empty.serial(), 1);
        assert!(empty.versions().is_empty());
    }

    /// What was published stays published: a version, as it was; a withdrawn build, for good.
    #[test]
    fn the_next_catalogue_only_adds() {
        // Serial 7: 0.1.0 and 0.1.1 listed, 0.1.0 withdrawn.
        let published = Catalogue::read(catalogue().as_bytes()).unwrap();
        // The pin moved to 0.2.0, and 0.1.1 is withdrawn too.
        let now = pins(&format!(
            "{}{}",
            version("0.2.0"),
            withdrawn("0.1.1", "It could not cancel.")
        ));
        let eight = next(Some(&published), &now);
        assert_eq!(eight.serial(), 8);
        assert!(eight.keeps(&published));
        let listed: Vec<&str> = eight
            .versions()
            .iter()
            .map(|v| v.version.as_str())
            .collect();
        assert_eq!(listed, ["0.1.0", "0.1.1", "0.2.0"]);
        assert_eq!(
            eight.why_withdrawn("spelaiondb-plugin-duckdb", (0, 1, 0)),
            Some("It lost rows on import.")
        );
        assert_eq!(
            eight.why_withdrawn("spelaiondb-plugin-duckdb", (0, 1, 1)),
            Some("It could not cancel.")
        );

        // The same pins again say nothing new: no catalogue, no serial spent.
        assert!(Catalogue::next(Some(&eight), &now).unwrap().is_none());
        // Pins that no longer mention the old versions or the withdrawn builds (the file was
        // tidied, or someone tried): nothing is dropped, and nothing is new.
        let tidied = pins(&version("0.2.0"));
        assert!(Catalogue::next(Some(&eight), &tidied).unwrap().is_none());
        // Nothing pinned at all: the engine left the installer, and its builds stay offered.
        assert!(
            Catalogue::next(Some(&eight), &Pins::none())
                .unwrap()
                .is_none()
        );

        // A reason reworded is the one thing that changes.
        let reworded = pins(&format!(
            "{}{}",
            version("0.2.0"),
            withdrawn("0.1.1", "It could not cancel a running statement.")
        ));
        let nine = next(Some(&eight), &reworded);
        assert_eq!(nine.serial(), 9);
        assert!(nine.keeps(&eight));
        assert_eq!(
            nine.why_withdrawn("spelaiondb-plugin-duckdb", (0, 1, 1)),
            Some("It could not cancel a running statement.")
        );
        assert_eq!(nine.versions(), eight.versions());
    }

    #[test]
    fn a_published_version_never_changes_and_a_withdrawn_build_is_never_pinned() {
        let published = Catalogue::read(catalogue().as_bytes()).unwrap();
        // 0.1.1 again, with another program in it.
        let changed = pins(&version("0.1.1").replace(HASH_B, &"c".repeat(64)));
        assert!(matches!(
            Catalogue::next(Some(&published), &changed),
            Err(NextError::Changed { version, .. }) if version == "0.1.1"
        ));
        // The same version under another spelling is still that version.
        let respelled =
            pins(&version("0.1.1").replace("version = \"0.1.1\"", "version = \"0.01.1\""));
        assert!(matches!(
            Catalogue::next(Some(&published), &respelled),
            Err(NextError::Changed { .. })
        ));
        // 0.1.0 was withdrawn by a catalogue already published: it cannot be pinned again,
        // even though the pins no longer say it is withdrawn.
        let back = pins(&version("0.1.0"));
        assert!(matches!(
            Catalogue::next(Some(&published), &back),
            Err(NextError::Withdrawn { version, .. }) if version == "0.1.0"
        ));
        // An engine that is already another plugin's.
        let taken =
            pins(&version("0.3.0").replace("spelaiondb-plugin-duckdb", "spelaiondb-plugin-quack"));
        assert!(matches!(
            Catalogue::next(Some(&published), &taken),
            Err(NextError::Wrong(CatalogueError::Entry { .. }))
        ));
        // `keeps` is what "only adds" means: a catalogue without one of them does not keep it.
        let without_withdrawn = Catalogue::read(
            catalogue()
                .split("\n[[withdrawn]]")
                .next()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert!(!without_withdrawn.keeps(&published));
        assert!(published.keeps(&without_withdrawn));
        let one_version = Catalogue::read(
            format!("schema_version = 1\nserial = 9\n{}", version("0.1.1")).as_bytes(),
        )
        .unwrap();
        assert!(!one_version.keeps(&without_withdrawn));
    }

    /// A reason is the one free text in the file: whatever it holds, it is read back as it
    /// was given, and cannot add a line to the catalogue.
    #[test]
    fn a_reason_is_written_as_it_was_given() {
        // As plugins.toml would hold it: a path with backslashes and a quoted word.
        let now = pins(&withdrawn(
            "0.1.0",
            r#"It read C:\\data as \"data\" = [[plugin]]."#,
        ));
        assert_eq!(
            now.withdrawn()[0].reason,
            r#"It read C:\data as "data" = [[plugin]]."#
        );
        let first = next(None, &now);
        assert_eq!(first.withdrawn(), now.withdrawn());
        assert!(first.versions().is_empty());
        assert_eq!(quoted(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
