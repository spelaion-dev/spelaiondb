//! Other people's plugins (ADR-041, point 5; ADR-055, point 5): an engine served by a program
//! the user added themselves, from a file or from an address they typed, which is not part of
//! SpelaionDB and which the project does not support.
//!
//! ```text
//! <personal folder>/plugins/theirs/
//!   <name>/spelaion-plugin.toml           its manifest, as it came in its archive
//!   <name>/pin.toml                       the SHA-256 the user trusted, and where it came from
//!   <name>/<program>                      the program
//!   .pending/<sha256>.tar.gz              an archive waiting for the user's answer
//! ```
//!
//! Every rule is here, never in the app:
//!
//! - **Only the user adds one, and only after the dialog.** [`TheirPlugins::inspect_file`] and
//!   [`TheirPlugins::inspect_address`] bring an archive to a place of its own and read it: its
//!   manifest is read in memory and every file in it is hashed, but **nothing in it is run or
//!   unpacked**. What they return ([`Pending`]) is what the dialog shows: what the plugin says
//!   it is and who it says it is from, the engine it adds, the SHA-256 of what arrived and,
//!   for an address, the whole address and its host. Only [`TheirPlugins::add`], given the
//!   hash the dialog showed, unpacks the program, and only if the archive still has that hash.
//! - **An address is HTTPS, read once, bounded.** It is fetched only by
//!   [`TheirPlugins::inspect_address`], which the Plugins page calls with what the user typed
//!   or pasted there: no setting, link, file association, command-line argument, repository
//!   file, connection file or agent reaches it, and nothing stores an address to read again
//!   (`pin.toml` keeps it to be shown, and nothing reads it back to fetch). It is the archive
//!   itself, or that plugin's own release manifest naming an archive for each platform (one
//!   step, no more). Both, and every redirect, are `https://`; the archive is bounded by
//!   [`MAX_ARCHIVE_BYTES`] (and by the size its release manifest gives), a manifest by
//!   [`MAX_RELEASE_BYTES`].
//! - **Pinned by hash.** The program's SHA-256 is recorded when the user adds it and checked
//!   before every start, as a pinned build of ours is (`pins::check_file`): a changed file is
//!   refused until the user trusts it again ([`TheirPlugins::trust_again`]).
//! - **Its manifest names one of the core's dialects, or none**, and an engine that is not one
//!   of ours (`spelaion_config::check_external_engine`). It is read with that dialect under the
//!   name `external`, which no barrier answers to: no read-only mode and nothing for agents.
//! - **Not ours, and said so**: every error it gives names it and says it is not part of
//!   SpelaionDB ([`ExternalPlugin::attribution`]). No updates are offered: a newer version is
//!   another file or address the user gives, with the dialog again.
//!
//! What this is not: a defence against someone who can already write in the personal folder,
//! who could replace a program and its pin together. It keeps a program the user trusted from
//! changing under them, and anything else from being started as one.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spelaion_config::atomic_file::{self, NewFileMode};
use spelaion_driver_api::{Dialect, Driver};

use super::fetch::{FetchError, Transport, download, is_https};
use super::installed::{
    AppBuild, MAX_MEMBERS, MAX_PROGRAM_BYTES, MAX_UNPACKED_BYTES, is_plain_name, unpack_program,
};
use super::pins::{is_sha256, protocol_of, sha256_of, version_of};

/// The manifest's file name, at the top of a plugin's archive.
pub const MANIFEST_FILE: &str = "spelaion-plugin.toml";

/// The largest archive that is read from a file or an address.
pub const MAX_ARCHIVE_BYTES: u64 = 256 << 20;

/// The largest manifest, in an archive or at an address.
pub const MAX_RELEASE_BYTES: u64 = 64 << 10;

/// The longest address accepted.
pub const MAX_ADDRESS: usize = 2048;

/// The pin's file name, next to the program.
const PIN_FILE: &str = "pin.toml";

/// What every manifest starts with.
const SCHEMA_VERSION: u32 = 1;

/// What a plugin says about itself, in the `spelaion-plugin.toml` at the top of its archive.
/// Nothing in it is checked by anyone but the core's rules below: the author is who it *says*
/// it is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Its name: lowercase letters, digits and `-`. Never one of ours (`spelaion…`).
    pub name: String,
    pub version: String,
    /// Who it says it is from.
    pub author: String,
    pub description: Option<String>,
    /// Where its author says to look for it: `https://`, shown as text and never opened.
    pub homepage: Option<String>,
    /// The plugin protocol version it speaks, `major.minor`.
    pub protocol: String,
    /// The platform it is built for, as `plugins.toml` names them.
    pub platform: String,
    /// Its program: a file at the top of the archive.
    pub program: String,
    /// The engine it adds, as connections name it in `external_engine`.
    pub engine: String,
    /// The engine's name as people write it.
    pub engine_label: String,
    /// The core's dialect its scripts are read with (an engine's name: `postgres`); standard
    /// SQL when there is none.
    pub dialect: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    schema_version: u32,
    name: String,
    version: String,
    author: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    homepage: Option<String>,
    protocol: String,
    platform: String,
    program: String,
    engine: String,
    engine_label: String,
    #[serde(default)]
    dialect: Option<String>,
}

/// A plugin's release manifest, at the address the user gave: an archive for each platform.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseFile {
    schema_version: u32,
    name: String,
    version: String,
    #[serde(default)]
    platform: BTreeMap<String, ReleaseBuild>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseBuild {
    archive: String,
    archive_sha256: String,
    archive_size: u64,
}

/// Why a plugin was not added, trusted again or removed, as the user reads it.
#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0}")]
    Manifest(String),
    #[error("this is not a plugin's archive: {0}")]
    Archive(String),
    #[error("the archive is larger than the {most} bytes a plugin's may have")]
    TooLarge { most: u64 },
    #[error("{0}")]
    Address(String),
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("{0}")]
    NotUsable(String),
    #[error(
        "the engine {engine:?} is already served by the plugin {by}, which you added: remove it \
         first"
    )]
    EngineTaken { engine: String, by: String },
    #[error(
        "the file waiting to be added is no longer the one you were shown (its SHA-256 is \
         {found}, not {shown}): nothing was added. Add it again"
    )]
    Swapped { shown: String, found: String },
    #[error("no plugin is waiting to be added with the SHA-256 {0}: add it again")]
    NotPending(String),
    #[error("no plugin you added is called {0:?}")]
    Unknown(String),
    #[error(
        "the file of the plugin {name} is no longer the one you were shown (its SHA-256 is \
         {found}, not {shown}): nothing was trusted"
    )]
    ChangedAgain {
        name: String,
        shown: String,
        found: String,
    },
    #[error("{what}: {error}")]
    Io { what: String, error: String },
}

fn io(what: impl Into<String>) -> impl FnOnce(std::io::Error) -> AddError {
    let what = what.into();
    move |error| AddError::Io {
        what,
        error: error.to_string(),
    }
}

impl Manifest {
    /// Reads and checks a manifest. Anything it does not understand refuses it whole.
    pub fn parse(text: &str) -> Result<Self, AddError> {
        let wrong = |problem: String| AddError::Manifest(format!("its manifest {problem}"));
        let file: ManifestFile =
            toml::from_str(text).map_err(|e| wrong(format!("cannot be read: {}", e.message())))?;
        if file.schema_version != SCHEMA_VERSION {
            return Err(wrong(format!(
                "has schema_version {}, and this SpelaionDB reads {SCHEMA_VERSION}",
                file.schema_version
            )));
        }
        check_name(&file.name).map_err(wrong)?;
        if version_of(&file.version).is_none() {
            return Err(wrong(format!(
                "gives the version {:?}: three numbers, such as 1.0.0",
                file.version
            )));
        }
        check_text("author", &file.author, 100).map_err(wrong)?;
        if let Some(description) = &file.description {
            check_text("description", description, 500).map_err(wrong)?;
        }
        if let Some(homepage) = &file.homepage
            && (!is_https(homepage) || homepage.len() > MAX_ADDRESS)
        {
            return Err(wrong(format!(
                "gives the homepage {homepage:?}: an https:// address"
            )));
        }
        if protocol_of(&file.protocol).is_none() {
            return Err(wrong(format!(
                "gives the protocol {:?}: major.minor, such as 1.4",
                file.protocol
            )));
        }
        if !super::pins::PLATFORMS.contains(&file.platform.as_str()) {
            return Err(wrong(format!(
                "gives the platform {:?}, which is none of {}",
                file.platform,
                super::pins::PLATFORMS.join(", ")
            )));
        }
        if !is_plain_name(&file.program) || file.program == MANIFEST_FILE {
            return Err(wrong(format!(
                "names the program {:?}: a file at the top of the archive",
                file.program
            )));
        }
        if file.platform.contains("windows") && !file.program.ends_with(".exe") {
            return Err(wrong(format!(
                "names the program {:?}, and a Windows program ends in .exe",
                file.program
            )));
        }
        spelaion_config::check_external_engine(&file.engine).map_err(wrong)?;
        check_text("engine_label", &file.engine_label, 60).map_err(wrong)?;
        if let Some(dialect) = &file.dialect
            && !is_core_dialect(dialect)
        {
            return Err(wrong(format!(
                "names the dialect {dialect:?}, which is not one of SpelaionDB's: name one, such \
                 as postgres, or none for standard SQL"
            )));
        }
        Ok(Self {
            name: file.name,
            version: file.version,
            author: file.author,
            description: file.description,
            homepage: file.homepage,
            protocol: file.protocol,
            platform: file.platform,
            program: file.program,
            engine: file.engine,
            engine_label: file.engine_label,
            dialect: file.dialect,
        })
    }

    /// Whether this SpelaionDB can start it on this computer: its platform, and a protocol
    /// version of the major the core speaks, not newer than the core's.
    pub fn usable_on(&self, app: &AppBuild) -> Result<(), String> {
        if app.platform.as_deref() != Some(self.platform.as_str()) {
            return Err(format!(
                "the plugin {} {} is built for {}, not for this computer",
                self.name, self.version, self.platform
            ));
        }
        let (major, minor) = protocol_of(&self.protocol).unwrap_or((0, 0));
        if major != app.protocol.0 || minor > app.protocol.1 {
            return Err(format!(
                "the plugin {} {} speaks version {} of the plugin protocol, and this SpelaionDB \
                 speaks {}.{}",
                self.name, self.version, self.protocol, app.protocol.0, app.protocol.1
            ));
        }
        Ok(())
    }

    /// What goes before each of its errors: whose it is, and that it is not ours.
    pub fn attribution(&self) -> String {
        format!(
            "plugin {} {} (it says it is by {}; not part of SpelaionDB: report problems to its \
             author)",
            self.name, self.version, self.author
        )
    }

    /// How the core reads its engine's scripts: the dialect it names, under the name
    /// `external`.
    pub fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(
            spelaion_dialect::ExternalDialect::reading_as(self.dialect.as_deref())
                .expect("a manifest names only dialects the core has"),
        )
    }
}

/// A plugin's name: lowercase letters, digits and `-`, never one that passes for ours.
fn check_name(name: &str) -> Result<(), String> {
    let shaped = name.len() >= 2
        && name.len() <= 60
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !shaped {
        return Err(format!(
            "gives the name {name:?}: lowercase letters, digits and -, starting with a letter, up \
             to 60 characters"
        ));
    }
    if name.starts_with("spelaion") {
        return Err(format!(
            "gives the name {name:?}, and only SpelaionDB's own plugins start with \"spelaion\""
        ));
    }
    Ok(())
}

/// One line of text shown to the user: not empty, not too long, nothing that is not text.
fn check_text(field: &str, text: &str, most: usize) -> Result<(), String> {
    if text.trim().is_empty() || text.chars().count() > most || text.chars().any(char::is_control) {
        return Err(format!(
            "gives {field} as {text:?}: one line of at most {most} characters"
        ));
    }
    Ok(())
}

/// Whether `name` is one of the core's dialects, by the engine name it is known by.
fn is_core_dialect(name: &str) -> bool {
    name != spelaion_dialect::EXTERNAL
        && spelaion_config::Engine::parse(name).is_some()
        && spelaion_dialect::for_engine(name).is_some()
}

/// Where the archive waiting for the user's answer came from: what the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Origin {
    /// A file the user picked.
    File { path: String },
    /// An address the user typed or pasted. `archive` is the archive's own address when
    /// `address` was its release manifest; `arrived_from` is where the bytes of the archive
    /// finally came from, after any redirect, when that is not where they were asked for.
    Address {
        address: String,
        host: String,
        archive: Option<String>,
        archive_host: Option<String>,
        arrived_from: Option<String>,
    },
}

/// A plugin waiting for the user's answer: everything the dialog shows. Nothing in it was run
/// or unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub manifest: Manifest,
    /// The SHA-256 of the archive that arrived: what the user is asked to trust, and what
    /// [`TheirPlugins::add`] is given back.
    pub archive_sha256: String,
    pub archive_size: u64,
    /// The SHA-256 of the program inside it, which is checked before every start.
    pub program_sha256: String,
    pub origin: Origin,
    /// The version of the same plugin it would replace, if one was added before.
    pub replaces: Option<String>,
}

/// What a plugin the user added is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalState {
    /// Its program is the file the user trusted: its engine uses it.
    Trusted,
    /// Its program is not the file the user trusted (`found` is its SHA-256 now): refused
    /// until the user trusts it again or removes it.
    Changed { found: String },
    /// Its program is gone.
    Missing,
    /// Its folder cannot be read as a plugin, or this SpelaionDB cannot start it: why.
    Unusable(String),
}

/// A plugin the user added, as its folder holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPlugin {
    /// Its folder's name, which is its manifest's name.
    pub name: String,
    pub manifest: Option<Manifest>,
    /// Its program, where it is or would be.
    pub program: PathBuf,
    /// The SHA-256 the user trusted.
    pub sha256: Option<String>,
    /// What the user gave it from: a file's path or an address, shown and never read again.
    pub source: Option<String>,
    pub state: ExternalState,
}

impl ExternalPlugin {
    /// What goes before each of its errors.
    pub fn attribution(&self) -> String {
        match &self.manifest {
            Some(manifest) => manifest.attribution(),
            None => format!(
                "plugin {} (not part of SpelaionDB: report problems to its author)",
                self.name
            ),
        }
    }

    /// Why its engine does not open, when it does not.
    pub fn refusal(&self) -> Option<String> {
        let why = match &self.state {
            ExternalState::Trusted => return None,
            ExternalState::Changed { found } => format!(
                "its program changed since you trusted it (its SHA-256 is {found}, and you \
                 trusted {}): it is not started until you trust it again on the Plugins page, \
                 or remove it",
                self.sha256.as_deref().unwrap_or("?")
            ),
            ExternalState::Missing => format!(
                "its program {} is missing: add the plugin again, or remove it",
                self.program.display()
            ),
            ExternalState::Unusable(why) => why.clone(),
        };
        Some(format!("{}: {why}", self.attribution()))
    }
}

/// The pin, as `pin.toml` keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinFile {
    program_sha256: String,
    archive_sha256: String,
    /// The file or address it came from: shown, never read again.
    source: String,
}

/// What the dialog's archive waits with: where it came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingOrigin {
    origin: Origin,
}

/// The plugins the user added, in their personal folder's `plugins/theirs`.
#[derive(Debug, Clone)]
pub struct TheirPlugins {
    folder: PathBuf,
}

/// An engine of a plugin the user added, as the core opens connections with it.
pub struct ExternalEngine {
    pub plugin: ExternalPlugin,
    /// Its driver: the plugin, started only while its program has the trusted hash, or one that
    /// says why it is not started.
    pub driver: Arc<dyn Driver>,
    /// How its scripts are read.
    pub dialect: Arc<dyn Dialect>,
}

impl TheirPlugins {
    /// The plugins in `folder`. Nothing is created until something is kept there.
    pub fn at(folder: impl Into<PathBuf>) -> Self {
        Self {
            folder: folder.into(),
        }
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    fn pending_folder(&self) -> PathBuf {
        self.folder.join(".pending")
    }

    fn pending_archive(&self, sha256: &str) -> PathBuf {
        self.pending_folder().join(format!("{sha256}.tar.gz"))
    }

    fn pending_origin(&self, sha256: &str) -> PathBuf {
        self.pending_folder().join(format!("{sha256}.origin.toml"))
    }

    /// Reads the archive the user picked, for the dialog: copied to a place of its own
    /// (bounded), its manifest read and its files hashed. Nothing is run or unpacked.
    pub fn inspect_file(&self, file: &Path, app: &AppBuild) -> Result<Pending, AddError> {
        let incoming = Incoming::new(&self.pending_folder())?;
        let mut source =
            std::fs::File::open(file).map_err(io(format!("reading {}", file.display())))?;
        let mut sink = std::fs::File::create(&incoming.path).map_err(io("keeping the archive"))?;
        let mut buffer = vec![0u8; 1 << 16];
        let mut copied = 0u64;
        loop {
            let read = source
                .read(&mut buffer)
                .map_err(io(format!("reading {}", file.display())))?;
            if read == 0 {
                break;
            }
            copied += read as u64;
            if copied > MAX_ARCHIVE_BYTES {
                return Err(AddError::TooLarge {
                    most: MAX_ARCHIVE_BYTES,
                });
            }
            sink.write_all(&buffer[..read])
                .map_err(io("keeping the archive"))?;
        }
        sink.sync_all().map_err(io("keeping the archive"))?;
        drop(sink);
        let origin = Origin::File {
            path: file.display().to_string(),
        };
        self.keep_pending(incoming, origin, app, None)
    }

    /// Reads, once, the address the user typed or pasted into the Plugins page, for the
    /// dialog: an `https://` address of a plugin's archive, or of its release manifest naming
    /// an archive for each platform. Every redirect stays on HTTPS, and what arrives is
    /// bounded. Nothing that arrives is run or unpacked. **Only the Plugins page calls this,
    /// with what the user typed**: nothing else in SpelaionDB has an address to give it.
    pub async fn inspect_address(
        &self,
        transport: &dyn Transport,
        address: &str,
        app: &AppBuild,
    ) -> Result<Pending, AddError> {
        let address = address.trim();
        let host = check_address(address)?;
        let incoming = Incoming::new(&self.pending_folder())?;
        let what = "the plugin";
        let mut file = std::fs::File::create(&incoming.path).map_err(io("keeping the archive"))?;
        let (fetched, came_from) =
            download(transport, what, address, MAX_ARCHIVE_BYTES, &mut file).await?;
        drop(file);
        if is_gzip(&incoming.path)? {
            let origin = Origin::Address {
                address: address.to_string(),
                host,
                archive: None,
                archive_host: None,
                arrived_from: (came_from != address).then(|| came_from.clone()),
            };
            return self.keep_pending(incoming, origin, app, None);
        }
        // Not an archive: then it is the plugin's release manifest, which is small.
        if fetched > MAX_RELEASE_BYTES {
            return Err(AddError::Address(format!(
                "{address} is neither a plugin's archive (.tar.gz) nor its release manifest"
            )));
        }
        let text = std::fs::read_to_string(&incoming.path).map_err(|_| {
            AddError::Address(format!(
                "{address} is neither a plugin's archive (.tar.gz) nor its release manifest"
            ))
        })?;
        drop(incoming);
        let release = read_release(&text)?;
        let platform = app
            .platform
            .as_deref()
            .ok_or_else(|| AddError::NotUsable("no plugin is built for this computer".into()))?;
        let build = release.platform.get(platform).ok_or_else(|| {
            AddError::NotUsable(format!(
                "the plugin {} {} has no build for this computer ({platform})",
                release.name, release.version
            ))
        })?;
        let archive_host = check_address(&build.archive)?;
        let incoming = Incoming::new(&self.pending_folder())?;
        let mut file = std::fs::File::create(&incoming.path).map_err(io("keeping the archive"))?;
        let (fetched, came_from) = download(
            transport,
            "the plugin's archive",
            &build.archive,
            build.archive_size,
            &mut file,
        )
        .await?;
        drop(file);
        if fetched != build.archive_size {
            return Err(FetchError::Short {
                what: "the plugin's archive",
                expected: build.archive_size,
                found: fetched,
            }
            .into());
        }
        let found = sha256_of(&incoming.path).map_err(io("reading the archive"))?;
        if found != build.archive_sha256 {
            return Err(AddError::Archive(format!(
                "its SHA-256 is {found}, and its release manifest says {}",
                build.archive_sha256
            )));
        }
        let origin = Origin::Address {
            address: address.to_string(),
            host,
            archive: Some(build.archive.clone()),
            archive_host: Some(archive_host),
            arrived_from: (came_from != build.archive).then(|| came_from.clone()),
        };
        self.keep_pending(
            incoming,
            origin,
            app,
            Some((release.name.as_str(), release.version.as_str())),
        )
    }

    /// Reads the archive in `incoming` and keeps it as waiting for the user's answer, named by
    /// its SHA-256. `release`: the name and version its release manifest gave, which its own
    /// manifest must give too.
    fn keep_pending(
        &self,
        incoming: Incoming,
        origin: Origin,
        app: &AppBuild,
        release: Option<(&str, &str)>,
    ) -> Result<Pending, AddError> {
        let read = read_archive(&incoming.path)?;
        let manifest = read.manifest;
        if let Some((name, version)) = release
            && (name != manifest.name || version_of(version) != version_of(&manifest.version))
        {
            return Err(AddError::Manifest(format!(
                "the release manifest is for {name} {version}, and the archive holds {} {}",
                manifest.name, manifest.version
            )));
        }
        manifest.usable_on(app).map_err(AddError::NotUsable)?;
        // One plugin per engine: another one's engine is not taken over by a file.
        let mut replaces = None;
        for plugin in self.plugins(app) {
            if plugin.name == manifest.name {
                replaces = plugin.manifest.as_ref().map(|m| m.version.clone());
            } else if plugin
                .manifest
                .as_ref()
                .is_some_and(|other| other.engine == manifest.engine)
            {
                return Err(AddError::EngineTaken {
                    engine: manifest.engine.clone(),
                    by: plugin.name,
                });
            }
        }
        let archive = self.pending_archive(&read.archive_sha256);
        let pending = Pending {
            manifest,
            archive_sha256: read.archive_sha256.clone(),
            archive_size: read.archive_size,
            program_sha256: read.program_sha256,
            origin: origin.clone(),
            replaces,
        };
        let origin_text = toml::to_string(&PendingOrigin { origin }).map_err(|e| AddError::Io {
            what: "keeping where the plugin came from".into(),
            error: e.to_string(),
        })?;
        std::fs::write(self.pending_origin(&read.archive_sha256), origin_text)
            .map_err(io("keeping where the plugin came from"))?;
        incoming.keep_as(&archive)?;
        Ok(pending)
    }

    /// Drops an archive that waited for the user's answer, which was no.
    pub fn discard(&self, archive_sha256: &str) {
        if is_sha256(archive_sha256) {
            let _ = std::fs::remove_file(self.pending_archive(archive_sha256));
            let _ = std::fs::remove_file(self.pending_origin(archive_sha256));
        }
    }

    /// Adds the plugin whose archive waits with `archive_sha256`, the hash the dialog showed:
    /// **the user's yes**. The archive must still have that hash; only its program is unpacked,
    /// and it is pinned with the program's hash. A plugin of the same name is replaced (its
    /// process stopped first: its sessions are lost, ADR-028).
    pub async fn add(
        &self,
        archive_sha256: &str,
        app: &AppBuild,
    ) -> Result<ExternalPlugin, AddError> {
        if !is_sha256(archive_sha256) {
            return Err(AddError::NotPending(archive_sha256.to_string()));
        }
        let archive = self.pending_archive(archive_sha256);
        if !archive.is_file() {
            return Err(AddError::NotPending(archive_sha256.to_string()));
        }
        let origin_text = std::fs::read_to_string(self.pending_origin(archive_sha256))
            .map_err(|_| AddError::NotPending(archive_sha256.to_string()))?;
        let origin: PendingOrigin = toml::from_str(&origin_text)
            .map_err(|_| AddError::NotPending(archive_sha256.to_string()))?;
        // Read again: what is unpacked is what the user was shown, or nothing.
        let read = read_archive(&archive)?;
        if read.archive_sha256 != archive_sha256 {
            self.discard(archive_sha256);
            return Err(AddError::Swapped {
                shown: archive_sha256.to_string(),
                found: read.archive_sha256,
            });
        }
        let manifest = read.manifest;
        manifest.usable_on(app).map_err(AddError::NotUsable)?;
        for plugin in self.plugins(app) {
            if plugin.name != manifest.name
                && plugin
                    .manifest
                    .as_ref()
                    .is_some_and(|other| other.engine == manifest.engine)
            {
                return Err(AddError::EngineTaken {
                    engine: manifest.engine.clone(),
                    by: plugin.name,
                });
            }
        }

        let work = Incoming::new(&self.pending_folder())?;
        let unpacked = work.path.with_extension("program");
        let _remove = Removed(unpacked.clone());
        let found = unpack_program(&archive, "plugin.tar.gz", &manifest.program, &unpacked)
            .map_err(AddError::Archive)?;
        if found != read.program_sha256 {
            self.discard(archive_sha256);
            return Err(AddError::Swapped {
                shown: archive_sha256.to_string(),
                found: format!("a program whose SHA-256 is {found}"),
            });
        }

        let folder = self.folder.join(&manifest.name);
        let program = folder.join(&manifest.program);
        // A plugin of that name may be running: it is stopped before its files change.
        for old in self.plugins(app) {
            if old.name == manifest.name {
                super::stop(&old.program).await;
            }
        }
        replace_folder(&folder).await?;
        std::fs::create_dir_all(&folder).map_err(io(format!("making {}", folder.display())))?;
        std::fs::rename(&unpacked, &program)
            .map_err(io(format!("putting {} in place", program.display())))?;
        atomic_file::replace(
            &folder.join(MANIFEST_FILE),
            read.manifest_text.as_bytes(),
            NewFileMode::Default,
        )
        .map_err(io("keeping the plugin's manifest"))?;
        let source = match &origin.origin {
            Origin::File { path } => path.clone(),
            Origin::Address { address, .. } => address.clone(),
        };
        let pin = PinFile {
            program_sha256: found.clone(),
            archive_sha256: archive_sha256.to_string(),
            source: source.clone(),
        };
        write_pin(&folder, &pin)?;
        self.discard(archive_sha256);
        Ok(ExternalPlugin {
            name: manifest.name.clone(),
            manifest: Some(manifest),
            program,
            sha256: Some(found),
            source: Some(source),
            state: ExternalState::Trusted,
        })
    }

    /// Trusts again the program of the plugin `name` whose file changed, as it is now: only if
    /// its SHA-256 is still `program_sha256`, the hash the dialog showed. **The user's yes.**
    pub fn trust_again(
        &self,
        name: &str,
        program_sha256: &str,
        app: &AppBuild,
    ) -> Result<ExternalPlugin, AddError> {
        let plugin = self
            .plugins(app)
            .into_iter()
            .find(|plugin| plugin.name == name)
            .ok_or_else(|| AddError::Unknown(name.to_string()))?;
        let manifest = match (&plugin.manifest, &plugin.state) {
            (Some(manifest), ExternalState::Changed { .. } | ExternalState::Trusted) => {
                manifest.clone()
            }
            (_, _) => {
                return Err(AddError::NotUsable(plugin.refusal().unwrap_or_else(|| {
                    format!("the plugin {name} cannot be trusted again")
                })));
            }
        };
        let found = sha256_of(&plugin.program).map_err(io("reading the program"))?;
        if found != program_sha256 {
            return Err(AddError::ChangedAgain {
                name: name.to_string(),
                shown: program_sha256.to_string(),
                found,
            });
        }
        let folder = self.folder.join(name);
        let old = read_pin(&folder);
        let pin = PinFile {
            program_sha256: found.clone(),
            archive_sha256: old
                .as_ref()
                .map(|pin| pin.archive_sha256.clone())
                .unwrap_or_default(),
            source: old.map(|pin| pin.source).unwrap_or_default(),
        };
        write_pin(&folder, &pin)?;
        Ok(ExternalPlugin {
            sha256: Some(found),
            state: ExternalState::Trusted,
            manifest: Some(manifest),
            ..plugin
        })
    }

    /// Removes the plugin `name` and its pin, stopping it first if it runs: its sessions are
    /// lost (ADR-028). One that is not there is already removed.
    pub async fn remove(&self, name: &str) -> Result<(), AddError> {
        if check_name(name).is_err() {
            return Err(AddError::Unknown(name.to_string()));
        }
        let folder = self.folder.join(name);
        if let Some(manifest) = read_manifest(&folder) {
            super::stop(&folder.join(&manifest.program)).await;
        }
        replace_folder(&folder).await
    }

    /// Every plugin the user added, with what each is now. Each program is hashed.
    pub fn plugins(&self, app: &AppBuild) -> Vec<ExternalPlugin> {
        let Ok(entries) = std::fs::read_dir(&self.folder) else {
            return Vec::new();
        };
        let mut plugins: Vec<ExternalPlugin> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| check_name(name).is_ok())
            .map(|name| self.plugin(&name, app))
            .collect();
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        plugins
    }

    fn plugin(&self, name: &str, app: &AppBuild) -> ExternalPlugin {
        let folder = self.folder.join(name);
        let pin = read_pin(&folder);
        let unusable = |why: String, manifest: Option<Manifest>, program: PathBuf| ExternalPlugin {
            name: name.to_string(),
            manifest,
            program,
            sha256: pin.as_ref().map(|pin| pin.program_sha256.clone()),
            source: pin.as_ref().map(|pin| pin.source.clone()),
            state: ExternalState::Unusable(why),
        };
        let text = match std::fs::read_to_string(folder.join(MANIFEST_FILE)) {
            Ok(text) => text,
            Err(error) => {
                return unusable(
                    format!("its manifest cannot be read ({error}): add it again, or remove it"),
                    None,
                    folder.clone(),
                );
            }
        };
        let manifest = match Manifest::parse(&text) {
            Ok(manifest) if manifest.name == name => manifest,
            Ok(other) => {
                return unusable(
                    format!("its folder holds the plugin {}: remove it", other.name),
                    None,
                    folder.clone(),
                );
            }
            Err(error) => return unusable(error.to_string(), None, folder.clone()),
        };
        let program = folder.join(&manifest.program);
        if let Err(why) = manifest.usable_on(app) {
            return unusable(why, Some(manifest), program);
        }
        let Some(pin) = pin.as_ref().filter(|pin| is_sha256(&pin.program_sha256)) else {
            return unusable(
                "it has no pin: add it again, or remove it".into(),
                Some(manifest),
                program,
            );
        };
        let state = if !program.is_file() {
            ExternalState::Missing
        } else {
            match sha256_of(&program) {
                Ok(found) if found == pin.program_sha256 => ExternalState::Trusted,
                Ok(found) => ExternalState::Changed { found },
                Err(error) => {
                    ExternalState::Unusable(format!("its program cannot be read: {error}"))
                }
            }
        };
        ExternalPlugin {
            name: name.to_string(),
            manifest: Some(manifest),
            program,
            sha256: Some(pin.program_sha256.clone()),
            source: Some(pin.source.clone()),
            state,
        }
    }

    /// The engines of the plugins the user added: each started only while its program has the
    /// SHA-256 they trusted (checked before every start), or saying why it is not started.
    pub fn engines(&self, app: &AppBuild) -> Vec<ExternalEngine> {
        self.plugins(app)
            .into_iter()
            .filter_map(|plugin| {
                let manifest = plugin.manifest.clone()?;
                let driver: Arc<dyn Driver> = match (&plugin.state, &plugin.sha256) {
                    (ExternalState::Trusted, Some(sha256)) => Arc::new(
                        super::their_plugin(&plugin.program, sha256, &plugin.attribution())
                            .driver(&manifest.engine),
                    ),
                    _ => Arc::new(super::choice::RefusedPlugin {
                        engine: manifest.engine.clone(),
                        why: plugin.refusal().unwrap_or_default(),
                    }),
                };
                Some(ExternalEngine {
                    dialect: manifest.dialect(),
                    driver,
                    plugin,
                })
            })
            .collect()
    }
}

/// What was read from an archive.
struct ReadArchive {
    manifest: Manifest,
    manifest_text: String,
    archive_sha256: String,
    archive_size: u64,
    program_sha256: String,
}

/// Reads a plugin's archive without unpacking anything: a `.tar.gz` of a few ordinary files at
/// its top, by their own names, among them its manifest and the program it names, each once.
/// The manifest is read in memory; every file is hashed as it goes by, within the bounds.
fn read_archive(path: &Path) -> Result<ReadArchive, AddError> {
    let archive_size = std::fs::metadata(path)
        .map_err(io("reading the archive"))?
        .len();
    if archive_size > MAX_ARCHIVE_BYTES {
        return Err(AddError::TooLarge {
            most: MAX_ARCHIVE_BYTES,
        });
    }
    let archive_sha256 = sha256_of(path).map_err(io("reading the archive"))?;
    if !is_gzip(path)? {
        return Err(AddError::Archive(
            "it is not a .tar.gz, the only archive SpelaionDB reads".into(),
        ));
    }
    let bad = |why: String| AddError::Archive(why);
    let read = |e: std::io::Error| AddError::Archive(format!("it cannot be read: {e}"));
    let file = std::fs::File::open(path).map_err(io("reading the archive"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut members = 0usize;
    let mut total = 0u64;
    let mut manifest_text = None;
    let mut hashes: BTreeMap<String, (String, u64)> = BTreeMap::new();
    for entry in tar.entries().map_err(read)? {
        let mut entry = entry.map_err(read)?;
        members += 1;
        if members > MAX_MEMBERS {
            return Err(bad(format!("it has more than {MAX_MEMBERS} files")));
        }
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if !is_plain_name(&name) {
            return Err(bad(format!("{name:?} is not a file at its top")));
        }
        if !entry.header().entry_type().is_file() {
            return Err(bad(format!("{name} is not an ordinary file")));
        }
        if hashes.contains_key(&name) {
            return Err(bad(format!("it holds {name} twice")));
        }
        let size = entry.size();
        total = total.saturating_add(size);
        if total > MAX_UNPACKED_BYTES {
            return Err(bad(format!(
                "it unpacks to more than {MAX_UNPACKED_BYTES} bytes"
            )));
        }
        let keep = name == MANIFEST_FILE;
        if keep && size > MAX_RELEASE_BYTES {
            return Err(bad(format!(
                "its {MANIFEST_FILE} has more than {MAX_RELEASE_BYTES} bytes"
            )));
        }
        let mut hasher = Sha256::new();
        let mut kept = Vec::new();
        let mut buffer = vec![0u8; 1 << 16];
        let mut seen = 0u64;
        loop {
            let n = entry.read(&mut buffer).map_err(read)?;
            if n == 0 {
                break;
            }
            seen += n as u64;
            if seen > size {
                return Err(bad(format!("{name} is larger than the archive says")));
            }
            hasher.update(&buffer[..n]);
            if keep {
                kept.extend_from_slice(&buffer[..n]);
            }
        }
        if keep {
            manifest_text = Some(
                String::from_utf8(kept)
                    .map_err(|_| bad(format!("its {MANIFEST_FILE} is not text")))?,
            );
        }
        hashes.insert(name, (hex(&hasher.finalize()), size));
    }
    let manifest_text =
        manifest_text.ok_or_else(|| bad(format!("it has no {MANIFEST_FILE} at its top")))?;
    let manifest = Manifest::parse(&manifest_text)?;
    let (program_sha256, program_size) =
        hashes.get(&manifest.program).cloned().ok_or_else(|| {
            bad(format!(
                "it has no {}, the program its manifest names",
                manifest.program
            ))
        })?;
    if program_size > MAX_PROGRAM_BYTES {
        return Err(bad(format!(
            "{} has {program_size} bytes; a program larger than {MAX_PROGRAM_BYTES} is not added",
            manifest.program
        )));
    }
    Ok(ReadArchive {
        manifest,
        manifest_text,
        archive_sha256,
        archive_size,
        program_sha256,
    })
}

/// Reads and checks a release manifest.
fn read_release(text: &str) -> Result<ReleaseFile, AddError> {
    let wrong = |problem: String| AddError::Manifest(format!("its release manifest {problem}"));
    let release: ReleaseFile =
        toml::from_str(text).map_err(|e| wrong(format!("cannot be read: {}", e.message())))?;
    if release.schema_version != SCHEMA_VERSION {
        return Err(wrong(format!(
            "has schema_version {}, and this SpelaionDB reads {SCHEMA_VERSION}",
            release.schema_version
        )));
    }
    check_name(&release.name).map_err(wrong)?;
    if version_of(&release.version).is_none() {
        return Err(wrong(format!("gives the version {:?}", release.version)));
    }
    for (platform, build) in &release.platform {
        if !is_sha256(&build.archive_sha256) {
            return Err(wrong(format!(
                "gives {platform} an archive_sha256 that is not a SHA-256"
            )));
        }
        if build.archive_size == 0 || build.archive_size > MAX_ARCHIVE_BYTES {
            return Err(wrong(format!(
                "gives {platform} an archive of {} bytes: more than 0 and at most \
                 {MAX_ARCHIVE_BYTES}",
                build.archive_size
            )));
        }
    }
    Ok(release)
}

/// Checks an address the user gave, and returns its host as the dialog shows it: `https://`,
/// a host written in ASCII (an internationalised name in its `xn--` form, so two hosts that
/// look alike read apart), no user name or password, nothing that is not plain text.
pub fn check_address(address: &str) -> Result<String, AddError> {
    let refused = |why: &str| AddError::Address(format!("{address:?} was not read: {why}"));
    if address.len() > MAX_ADDRESS {
        return Err(refused("it is longer than an address may be"));
    }
    if !is_https(address) {
        return Err(refused("only an https:// address is read"));
    }
    let rest = &address["https://".len()..];
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(refused(
            "an address with a user name or a password is not read",
        ));
    }
    if authority.is_empty() || !authority.is_ascii() {
        return Err(refused(
            "write its host in ASCII (an internationalised name in its xn-- form)",
        ));
    }
    if !authority
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    {
        return Err(refused("its host is not a host's name"));
    }
    Ok(authority.to_ascii_lowercase())
}

/// Whether a file starts as a gzip stream does.
fn is_gzip(path: &Path) -> Result<bool, AddError> {
    let mut start = [0u8; 2];
    let mut file = std::fs::File::open(path).map_err(io("reading the archive"))?;
    match file.read_exact(&mut start) {
        Ok(()) => Ok(start == [0x1f, 0x8b]),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(io("reading the archive")(error)),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_manifest(folder: &Path) -> Option<Manifest> {
    let text = std::fs::read_to_string(folder.join(MANIFEST_FILE)).ok()?;
    Manifest::parse(&text).ok()
}

fn read_pin(folder: &Path) -> Option<PinFile> {
    let text = std::fs::read_to_string(folder.join(PIN_FILE)).ok()?;
    toml::from_str(&text).ok()
}

fn write_pin(folder: &Path, pin: &PinFile) -> Result<(), AddError> {
    let text = toml::to_string(pin).map_err(|e| AddError::Io {
        what: "keeping the plugin's pin".into(),
        error: e.to_string(),
    })?;
    atomic_file::replace(
        &folder.join(PIN_FILE),
        text.as_bytes(),
        NewFileMode::Default,
    )
    .map_err(io("keeping the plugin's pin"))
}

/// Removes a plugin's folder. Windows keeps a program's file locked until its process is gone,
/// a moment after it was stopped.
async fn replace_folder(folder: &Path) -> Result<(), AddError> {
    let mut tries = 0;
    loop {
        match std::fs::remove_dir_all(folder) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) if tries == 100 => {
                return Err(io(format!("removing {}", folder.display()))(error));
            }
            Err(_) => {
                tries += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

/// A file being brought in, removed when this value goes unless it was kept.
struct Incoming {
    path: PathBuf,
}

impl Incoming {
    fn new(folder: &Path) -> Result<Self, AddError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(folder).map_err(io("making room for the plugin"))?;
        Ok(Self {
            path: folder.join(format!(
                "incoming-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
        })
    }

    /// Keeps the file at `to`, replacing what was there.
    fn keep_as(self, to: &Path) -> Result<(), AddError> {
        let _ = std::fs::remove_file(to);
        std::fs::rename(&self.path, to).map_err(io("keeping the archive"))
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A file removed when this value goes.
struct Removed(PathBuf);

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_https_address_with_a_plain_host_is_read() {
        assert_eq!(
            check_address("https://example.com/foodb.tar.gz").unwrap(),
            "example.com"
        );
        assert_eq!(
            check_address("https://Example.COM:8443/a?b=c#d").unwrap(),
            "example.com:8443"
        );
        for refused in [
            "http://example.com/foodb.tar.gz",
            "file:///C:/foodb.tar.gz",
            "ftp://example.com/foodb.tar.gz",
            "HTTPS://example.com/foodb.tar.gz",
            "https://user:password@example.com/foodb.tar.gz",
            "https://user@example.com/foodb.tar.gz",
            "https://exаmple.com/foodb.tar.gz",
            "https:///foodb.tar.gz",
            "https://example.com/foo db.tar.gz",
            "https://example.com/foodb.tar.gz\n",
            "https://exa_mple.com/x",
            "spelaiondb://plugins/add?address=https://example.com/x",
            "",
        ] {
            assert!(check_address(refused).is_err(), "{refused:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_ADDRESS));
        assert!(check_address(&long).is_err());
    }

    fn manifest(changes: &[(&str, &str)]) -> String {
        let mut fields = vec![
            ("schema_version", "1".to_string()),
            ("name", "\"foodb\"".to_string()),
            ("version", "\"1.2.0\"".to_string()),
            ("author", "\"Acme Ltd.\"".to_string()),
            ("protocol", "\"1.0\"".to_string()),
            ("platform", "\"x86_64-unknown-linux-gnu\"".to_string()),
            ("program", "\"foodb-plugin\"".to_string()),
            ("engine", "\"foodb\"".to_string()),
            ("engine_label", "\"FooDB\"".to_string()),
        ];
        for (key, value) in changes {
            match fields.iter_mut().find(|(k, _)| k == key) {
                Some(field) => field.1 = value.to_string(),
                None => fields.push((key, value.to_string())),
            }
        }
        fields
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(key, value)| format!("{key} = {value}\n"))
            .collect()
    }

    #[test]
    fn a_manifest_is_read_whole_or_not_at_all() {
        let read = Manifest::parse(&manifest(&[("dialect", "\"postgres\"")])).unwrap();
        assert_eq!(read.engine, "foodb");
        assert_eq!(read.dialect().name(), "external");
        // No dialect: standard SQL, under the same name.
        assert_eq!(
            Manifest::parse(&manifest(&[])).unwrap().dialect().name(),
            "external"
        );
        for (change, why) in [
            (("name", "\"spelaiondb-plugin-postgres\""), "spelaion"),
            (("name", "\"spelaion-foodb\""), "spelaion"),
            (("name", "\"Foo DB\""), "lowercase"),
            (("engine", "\"postgres\""), "SpelaionDB's own"),
            (("engine", "\"cockroachdb\""), "SpelaionDB's own"),
            (("engine", "\"external\""), "SpelaionDB's own"),
            (("engine", "\"spelaionfoo\""), "spelaion"),
            (("dialect", "\"external\""), "not one of SpelaionDB's"),
            (("dialect", "\"foodb\""), "not one of SpelaionDB's"),
            (("program", "\"../foodb-plugin\""), "top of the archive"),
            (("program", "\"bin/foodb-plugin\""), "top of the archive"),
            (
                ("program", "\"spelaion-plugin.toml\""),
                "top of the archive",
            ),
            (("platform", "\"x86_64-pc-windows-msvc\""), ".exe"),
            (("platform", "\"riscv64\""), "none of"),
            (("homepage", "\"http://acme.example\""), "https://"),
            (("version", "\"1.2\""), "three numbers"),
            (("protocol", "\"one\""), "major.minor"),
            (("author", "\"\""), "author"),
            (("schema_version", "2"), "schema_version"),
            (("mcp", "true"), "cannot be read"),
            (("read_barrier", "\"engine\""), "cannot be read"),
        ] {
            let error = Manifest::parse(&manifest(&[change]))
                .unwrap_err()
                .to_string();
            assert!(error.contains(why), "{change:?}: {error}");
        }
    }

    #[test]
    fn a_plugin_is_started_only_where_it_was_built_for_and_on_our_protocol() {
        let read = Manifest::parse(&manifest(&[])).unwrap();
        let app = |platform: &str, protocol: (u32, u32)| AppBuild {
            platform: Some(platform.into()),
            version: (0, 19, 0),
            protocol,
        };
        assert!(
            read.usable_on(&app("x86_64-unknown-linux-gnu", (1, 4)))
                .is_ok()
        );
        assert!(
            read.usable_on(&app("x86_64-pc-windows-msvc", (1, 4)))
                .is_err()
        );
        assert!(
            read.usable_on(&app("x86_64-unknown-linux-gnu", (2, 0)))
                .is_err()
        );
        let newer = Manifest::parse(&manifest(&[("protocol", "\"1.9\"")])).unwrap();
        assert!(
            newer
                .usable_on(&app("x86_64-unknown-linux-gnu", (1, 4)))
                .is_err()
        );
    }

    #[test]
    fn its_errors_say_whose_it_is() {
        let read = Manifest::parse(&manifest(&[])).unwrap();
        let attribution = read.attribution();
        assert!(attribution.contains("foodb 1.2.0"), "{attribution}");
        assert!(attribution.contains("Acme Ltd."), "{attribution}");
        assert!(
            attribution.contains("not part of SpelaionDB"),
            "{attribution}"
        );
    }
}
