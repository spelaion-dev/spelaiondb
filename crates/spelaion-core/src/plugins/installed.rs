//! The plugins the user installed from the Plugins page (ADR-055), in their personal folder:
//! the last catalogue this SpelaionDB accepted, and the builds installed from it.
//!
//! ```text
//! <personal folder>/plugins/
//!   catalogue.signed                      the last catalogue accepted, with its signature
//!   ours/<plugin>/<version>/<program>     a build of ours, installed from an archive
//! ```
//!
//! **The folder is not trusted.** Nothing in it says what may be started:
//!
//! - The catalogue is kept with its signature and **checked again every time it is read**: a
//!   file edited by hand is no catalogue. One with a lower serial than the one kept is never
//!   accepted, nor another file with the same serial.
//! - **A file dropped in `ours/` is not a plugin.** A build there is started only if the pins
//!   compiled into this build or the accepted catalogue name that plugin and version for this
//!   platform ([`super::choice::states`]), and only while its file has the SHA-256 they give
//!   (checked before every start, [`super::pins::check_file`]).
//! - **An archive is installed only as a catalogue entry**: it must have that entry's size and
//!   SHA-256 before anything in it is read, hold only plain files at its top, and the program
//!   inside must have the entry's program SHA-256. Only the program is unpacked, to a place of
//!   its own, and it reaches `ours/` only after the last check.
//!
//! Nothing here downloads: whoever fetches an archive (from
//! [`super::pins::PinnedPlugin::archive_address`]) or a catalogue hands the file over, and the
//! same checks apply to one the user picked from their disk.
//!
//! What this is not: a defence against someone who can already write in the user's personal
//! folder and choose what the app downloads. They could put back an old catalogue we signed
//! (bringing back a build we withdrew); they still cannot make the app start a program we did
//! not build.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use spelaion_config::atomic_file::{self, NewFileMode};

use super::catalogue::{Catalogue, CatalogueError, CatalogueKey, MAX_CATALOGUE_BYTES};
use super::pins::{
    self, PinnedBuild, PinnedPlugin, is_program_name, program_file_name, sha256_of, version_of,
};

/// The largest program an archive may hold (DuckDB's, the largest of ours, is about 50 MiB).
pub const MAX_PROGRAM_BYTES: u64 = 512 << 20;

/// The most an archive may unpack to, all its members together.
const MAX_UNPACKED_BYTES: u64 = 1 << 30;

/// The most members an archive may have: ours hold the program and its licence.
const MAX_MEMBERS: usize = 16;

/// What this build of SpelaionDB is, for deciding which builds of a plugin it can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppBuild {
    /// The platform, as `plugins.toml` names it; `None` on one no plugin is built for.
    pub platform: Option<String>,
    /// This SpelaionDB's version.
    pub version: (u32, u32, u32),
    /// The plugin protocol version the core speaks, `(major, minor)`.
    pub protocol: (u32, u32),
}

impl AppBuild {
    /// This build. A version that is not three numbers counts as the lowest there is: no
    /// catalogue entry is then for this app.
    pub fn this() -> Self {
        let protocol = spelaion_plugin_host::VERSION;
        Self {
            platform: pins::platform().map(str::to_string),
            version: version_of(env!("CARGO_PKG_VERSION")).unwrap_or((0, 0, 0)),
            protocol: (protocol.major, protocol.minor),
        }
    }
}

/// Who names a build: decides what is asked of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedBy {
    /// The pins compiled into this build: tested with this very SpelaionDB.
    Pins,
    /// The catalogue: tested against `dev`, so its lowest app version is what protects an
    /// older SpelaionDB (ADR-055).
    Catalogue,
}

/// Why a build of a plugin is not started, or not installed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NotUsable {
    #[error("it was withdrawn: {0}")]
    Withdrawn(String),
    #[error("it has no build for this computer")]
    NoBuild,
    #[error("it speaks version {speaks} of the plugin protocol, and this SpelaionDB speaks {ours}")]
    Protocol { speaks: String, ours: String },
    #[error("it needs SpelaionDB {0} or newer")]
    NeedsApp(String),
    #[error("no plugin catalogue this SpelaionDB accepted lists it")]
    Unlisted,
}

/// The build of `entry` this SpelaionDB may use, or why none.
pub(crate) fn usable<'a>(
    entry: &'a PinnedPlugin,
    named_by: NamedBy,
    catalogue: Option<&Catalogue>,
    app: &AppBuild,
) -> Result<&'a PinnedBuild, NotUsable> {
    // A withdrawn build is refused whoever names it: the one pinned into this build too.
    if let Some(reason) =
        catalogue.and_then(|catalogue| catalogue.why_withdrawn(&entry.name, entry.version()))
    {
        return Err(NotUsable::Withdrawn(reason.to_string()));
    }
    let build = app
        .platform
        .as_deref()
        .and_then(|platform| entry.platforms.get(platform))
        .ok_or(NotUsable::NoBuild)?;
    let (major, minor) = entry.protocol();
    if major != app.protocol.0 || minor > app.protocol.1 {
        return Err(NotUsable::Protocol {
            speaks: entry.protocol.clone(),
            ours: format!("{}.{}", app.protocol.0, app.protocol.1),
        });
    }
    if named_by == NamedBy::Catalogue && entry.min_app() > app.version {
        return Err(NotUsable::NeedsApp(entry.min_app.clone()));
    }
    Ok(build)
}

/// A version as it is written: `0.1.0`, whatever spelling it came in.
pub(crate) fn version_text(version: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

/// A build in the store. Being there does not make it a plugin: see the module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledBuild {
    /// The plugin's name: `spelaiondb-plugin-duckdb`.
    pub name: String,
    pub version: String,
    /// Its program.
    pub program: PathBuf,
}

/// Why an archive was not installed.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("no plugin is built for this computer")]
    NoPlatform,
    #[error("{what}: {error}")]
    Io { what: String, error: String },
    #[error(
        "this file is not a plugin of SpelaionDB's catalogue (its SHA-256 is {sha256}). A plugin \
         of someone else's is added another way"
    )]
    NotInCatalogue { sha256: String },
    #[error("this file is {plugin} {version} for {platform}, not for this computer")]
    OtherPlatform {
        plugin: String,
        version: String,
        platform: String,
    },
    #[error(
        "this file is not {plugin} {version}: its SHA-256 is {found} and the catalogue says \
         {expected}"
    )]
    NotTheBuild {
        plugin: String,
        version: String,
        expected: String,
        found: String,
    },
    #[error("{plugin} {version}: the archive has {found} bytes and the catalogue says {expected}")]
    Size {
        plugin: String,
        version: String,
        expected: u64,
        found: u64,
    },
    #[error("{plugin} {version} cannot be installed: {why}")]
    NotUsable {
        plugin: String,
        version: String,
        why: NotUsable,
    },
    #[error("{plugin} {version}: the archive is not what a plugin's release holds: {why}")]
    Archive {
        plugin: String,
        version: String,
        why: String,
    },
    #[error(
        "{plugin} {version}: the program in the archive is not the build that was pinned (its \
         SHA-256 is {found}, the pinned one is {expected})"
    )]
    Program {
        plugin: String,
        version: String,
        expected: String,
        found: String,
    },
}

/// The plugins in the user's personal folder.
#[derive(Debug, Clone)]
pub struct PluginStore {
    folder: PathBuf,
}

impl PluginStore {
    /// The store in `folder`. Nothing is created until something is kept there.
    pub fn at(folder: impl Into<PathBuf>) -> Self {
        Self {
            folder: folder.into(),
        }
    }

    /// The store of a personal folder: its `plugins` folder.
    pub fn in_user_dir(user_dir: &Path) -> Self {
        Self::at(user_dir.join("plugins"))
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    fn catalogue_file(&self) -> PathBuf {
        self.folder.join("catalogue.signed")
    }

    fn ours(&self) -> PathBuf {
        self.folder.join("ours")
    }

    /// Where the program of a build of ours is, installed or not.
    pub fn program(&self, name: &str, version: (u32, u32, u32)) -> PathBuf {
        self.ours()
            .join(name)
            .join(version_text(version))
            .join(program_file_name(name))
    }

    /// The last catalogue accepted, if it is still one this build's key signed.
    pub fn catalogue(&self) -> Option<Catalogue> {
        self.catalogue_with(&CatalogueKey::shipped()?)
    }

    /// [`PluginStore::catalogue`] with `key` in place of the one compiled in (the tests').
    pub fn catalogue_with(&self, key: &CatalogueKey) -> Option<Catalogue> {
        self.kept(key).map(|(catalogue, _)| catalogue)
    }

    /// The catalogue kept, checked again, with its text.
    fn kept(&self, key: &CatalogueKey) -> Option<(Catalogue, Vec<u8>)> {
        let file = self.catalogue_file();
        // Not a file of ours if it is larger than a catalogue and its signature can be.
        let size = std::fs::metadata(&file).ok()?.len();
        if size > (MAX_CATALOGUE_BYTES + 8192) as u64 {
            return None;
        }
        let bytes = std::fs::read(&file).ok()?;
        let (signature, text) = split_signed(&bytes)?;
        let catalogue = Catalogue::verify(key, text, signature).ok()?;
        Some((catalogue, text.to_vec()))
    }

    /// Accepts a catalogue that was fetched, or given as a file with its signature: signed by
    /// the key compiled into this build, and not older than the one kept. It replaces the one
    /// kept.
    pub fn accept_catalogue(
        &self,
        text: &[u8],
        signature: &str,
    ) -> Result<Catalogue, CatalogueError> {
        let key = CatalogueKey::shipped().ok_or(CatalogueError::NoKey)?;
        self.accept_catalogue_with(&key, text, signature)
    }

    /// [`PluginStore::accept_catalogue`] with `key` in place of the one compiled in (the
    /// tests').
    pub fn accept_catalogue_with(
        &self,
        key: &CatalogueKey,
        text: &[u8],
        signature: &str,
    ) -> Result<Catalogue, CatalogueError> {
        let offered = Catalogue::verify(key, text, signature)?;
        if let Some((accepted, kept)) = self.kept(key) {
            if offered.serial() < accepted.serial() {
                return Err(CatalogueError::Older {
                    offered: offered.serial(),
                    accepted: accepted.serial(),
                });
            }
            if offered.serial() == accepted.serial() {
                // A serial names one catalogue.
                return if kept == text {
                    Ok(offered)
                } else {
                    Err(CatalogueError::SameSerial(offered.serial()))
                };
            }
        }
        let io = |e: std::io::Error| CatalogueError::Io(e.to_string());
        std::fs::create_dir_all(&self.folder).map_err(io)?;
        atomic_file::replace(
            &self.catalogue_file(),
            &signed_catalogue(signature, text),
            NewFileMode::OwnerOnly,
        )
        .map_err(io)?;
        Ok(offered)
    }

    /// Accepts a catalogue as it is published and kept: one file, its signature's four lines
    /// and then its text ([`signed_catalogue`]). One file, so a reader never gets the text of
    /// one catalogue with the signature of another.
    pub fn accept_signed_catalogue(&self, signed: &[u8]) -> Result<Catalogue, CatalogueError> {
        let key = CatalogueKey::shipped().ok_or(CatalogueError::NoKey)?;
        self.accept_signed_catalogue_with(&key, signed)
    }

    /// [`PluginStore::accept_signed_catalogue`] with `key` in place of the one compiled in
    /// (the tests').
    pub fn accept_signed_catalogue_with(
        &self,
        key: &CatalogueKey,
        signed: &[u8],
    ) -> Result<Catalogue, CatalogueError> {
        if signed.len() > MAX_CATALOGUE_BYTES + 8192 {
            return Err(CatalogueError::TooLarge(signed.len()));
        }
        let (signature, text) = split_signed(signed)
            .ok_or_else(|| CatalogueError::Signature("it has no signature".into()))?;
        self.accept_catalogue_with(key, text, signature)
    }

    /// The builds in the store, by what their folders are called. Only names a plugin and a
    /// version can have are looked at.
    pub fn installed(&self) -> Vec<InstalledBuild> {
        let mut found = Vec::new();
        for name in folder_names(&self.ours()) {
            if !is_program_name(&name) {
                continue;
            }
            for version in folder_names(&self.ours().join(&name)) {
                let Some(numbers) = version_of(&version) else {
                    continue;
                };
                // One spelling per version: `0.01.0` is not where `0.1.0` is installed.
                if version_text(numbers) != version {
                    continue;
                }
                let program = self.program(&name, numbers);
                if program.is_file() {
                    found.push(InstalledBuild {
                        name: name.clone(),
                        version,
                        program,
                    });
                }
            }
        }
        found.sort_by(|a, b| {
            (a.name.as_str(), version_of(&a.version))
                .cmp(&(b.name.as_str(), version_of(&b.version)))
        });
        found
    }

    /// Installs the archive `archive` as the catalogue entry it is: the one, for this
    /// platform, whose archive has that SHA-256 (ADR-055: a plugin of ours can be installed
    /// from a file). A file the catalogue does not list is not installed.
    pub fn install(
        &self,
        catalogue: &Catalogue,
        app: &AppBuild,
        archive: &Path,
    ) -> Result<InstalledBuild, InstallError> {
        let platform = app.platform.as_deref().ok_or(InstallError::NoPlatform)?;
        let sha256 = hash_of(archive)?;
        let mut elsewhere = None;
        for entry in catalogue.versions() {
            for (target, build) in &entry.platforms {
                if build.archive_sha256 != sha256 {
                    continue;
                }
                if target == platform {
                    return self.install_checked(catalogue, app, entry, build, archive);
                }
                elsewhere = Some((entry, target));
            }
        }
        Err(match elsewhere {
            Some((entry, target)) => InstallError::OtherPlatform {
                plugin: entry.name.clone(),
                version: entry.version.clone(),
                platform: target.clone(),
            },
            None => InstallError::NotInCatalogue { sha256 },
        })
    }

    /// Installs `archive` as the version `version` of the plugin `name`: what a download of
    /// that entry's archive is given to. The file must be that entry's archive.
    pub fn install_as(
        &self,
        catalogue: &Catalogue,
        app: &AppBuild,
        name: &str,
        version: (u32, u32, u32),
        archive: &Path,
    ) -> Result<InstalledBuild, InstallError> {
        let not_usable = |why: NotUsable| InstallError::NotUsable {
            plugin: name.to_string(),
            version: version_text(version),
            why,
        };
        let entry = catalogue
            .version(name, version)
            .ok_or_else(|| not_usable(NotUsable::Unlisted))?;
        let build = usable(entry, NamedBy::Catalogue, Some(catalogue), app).map_err(not_usable)?;
        let found = hash_of(archive)?;
        if found != build.archive_sha256 {
            return Err(InstallError::NotTheBuild {
                plugin: entry.name.clone(),
                version: entry.version.clone(),
                expected: build.archive_sha256.clone(),
                found,
            });
        }
        self.install_checked(catalogue, app, entry, build, archive)
    }

    /// `archive` has the SHA-256 of `build`'s archive. The rest of the checks, and the
    /// install.
    fn install_checked(
        &self,
        catalogue: &Catalogue,
        app: &AppBuild,
        entry: &PinnedPlugin,
        build: &PinnedBuild,
        archive: &Path,
    ) -> Result<InstalledBuild, InstallError> {
        let plugin = entry.name.clone();
        let version = version_text(entry.version());
        usable(entry, NamedBy::Catalogue, Some(catalogue), app).map_err(|why| {
            InstallError::NotUsable {
                plugin: plugin.clone(),
                version: version.clone(),
                why,
            }
        })?;
        let io = |what: String| {
            move |error: std::io::Error| InstallError::Io {
                what,
                error: error.to_string(),
            }
        };
        let found = std::fs::metadata(archive)
            .map_err(io(format!("reading {}", archive.display())))?
            .len();
        if found != build.archive_size {
            return Err(InstallError::Size {
                plugin,
                version,
                expected: build.archive_size,
                found,
            });
        }

        // Only the program is unpacked, to a folder of its own that is removed whatever
        // happens: nothing unchecked is ever left where the builds are.
        let file = program_file_name(&entry.name);
        let work = Work::new(&self.ours()).map_err(io("making room for the plugin".into()))?;
        let unpacked = work.path.join(&file);
        let found = unpack_program(archive, &build.archive, &file, &unpacked).map_err(|why| {
            InstallError::Archive {
                plugin: plugin.clone(),
                version: version.clone(),
                why,
            }
        })?;
        if found != build.program_sha256 {
            return Err(InstallError::Program {
                plugin,
                version,
                expected: build.program_sha256.clone(),
                found,
            });
        }

        // Checked: it goes where that build lives.
        let program = self.program(&entry.name, entry.version());
        let installed = InstalledBuild {
            name: plugin,
            version,
            program: program.clone(),
        };
        // The same build already there is left alone: it may be running.
        if sha256_of(&program).is_ok_and(|there| there == build.program_sha256) {
            return Ok(installed);
        }
        let place = |error: std::io::Error| InstallError::Io {
            what: format!("putting {} in place", program.display()),
            error: error.to_string(),
        };
        if let Some(folder) = program.parent() {
            std::fs::create_dir_all(folder).map_err(place)?;
        }
        match std::fs::remove_file(&program) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(place(error)),
        }
        std::fs::rename(&unpacked, &program).map_err(place)?;
        Ok(installed)
    }

    /// Removes an installed build, stopping its plugin first if it runs: its sessions are lost
    /// (ADR-028). A build that is not there is already removed.
    pub async fn remove(&self, name: &str, version: (u32, u32, u32)) -> std::io::Result<()> {
        if !is_program_name(name) {
            return Err(std::io::Error::other(format!(
                "{name:?} is not a plugin's name"
            )));
        }
        let program = self.program(name, version);
        super::stop(&program).await;
        let folder = self.ours().join(name).join(version_text(version));
        // Windows keeps a program's file locked until its process is gone, a moment after it
        // was stopped.
        let mut tries = 0;
        loop {
            match std::fs::remove_dir_all(&folder) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) if tries == 100 => return Err(error),
                Err(_) => {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
        // The plugin's own folder goes with its last build; one that still holds another
        // stays (the removal fails, and that is the answer).
        let _ = std::fs::remove_dir(self.ours().join(name));
        Ok(())
    }
}

/// The SHA-256 of an archive the user or a download handed over.
fn hash_of(archive: &Path) -> Result<String, InstallError> {
    sha256_of(archive).map_err(|error| InstallError::Io {
        what: format!("reading {}", archive.display()),
        error: error.to_string(),
    })
}

/// The names of the folders in `folder`; none if it cannot be read.
fn folder_names(folder: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect()
}

/// A catalogue and its signature as one file: the signature's four lines (a minisign
/// signature file's), then the catalogue's text byte for byte. How it is kept in the store, so
/// the two are replaced together, and how it is published, so the two are fetched together.
pub fn signed_catalogue(signature: &str, text: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(signature.len() + text.len() + 4);
    for line in signature.lines().take(4) {
        kept.extend_from_slice(line.trim_end().as_bytes());
        kept.push(b'\n');
    }
    kept.extend_from_slice(text);
    kept
}

/// The signature and the text of a kept catalogue.
fn split_signed(kept: &[u8]) -> Option<(&str, &[u8])> {
    let end = kept
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(3)
        .map(|(at, _)| at + 1)?;
    let signature = std::str::from_utf8(&kept[..end]).ok()?;
    Some((signature, &kept[end..]))
}

/// A folder for one install, removed when this value goes.
struct Work {
    path: PathBuf,
}

impl Work {
    fn new(ours: &Path) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Not a name a plugin can have: never taken for an installed build.
        let path = ours.join(format!(
            ".installing-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Unpacks the program `file` of the archive `archive` (a `.tar.gz`, called `archive_name` in
/// its release) to `to`, and returns the SHA-256 of what was written. The archive must be what
/// a plugin's release holds: a few ordinary files at its top, by their own names, the program
/// among them once. Nothing else is written, and nothing is read past the bounds.
fn unpack_program(
    archive: &Path,
    archive_name: &str,
    file: &str,
    to: &Path,
) -> Result<String, String> {
    if !archive_name.ends_with(".tar.gz") {
        return Err(format!(
            "{archive_name} is not a .tar.gz, the only archive this SpelaionDB unpacks"
        ));
    }
    let read = |e: std::io::Error| format!("it cannot be read: {e}");
    let packed = std::fs::File::open(archive).map_err(read)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(packed));
    let mut found = None;
    let mut members = 0usize;
    let mut total = 0u64;
    for entry in tar.entries().map_err(read)? {
        let mut entry = entry.map_err(read)?;
        members += 1;
        if members > MAX_MEMBERS {
            return Err(format!("it has more than {MAX_MEMBERS} files"));
        }
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if !is_plain_name(&name) {
            return Err(format!("{name:?} is not a file at its top"));
        }
        if !entry.header().entry_type().is_file() {
            return Err(format!("{name} is not an ordinary file"));
        }
        let size = entry.size();
        total = total.saturating_add(size);
        if total > MAX_UNPACKED_BYTES {
            return Err(format!(
                "it unpacks to more than {MAX_UNPACKED_BYTES} bytes"
            ));
        }
        if name != file {
            continue;
        }
        if found.is_some() {
            return Err(format!("it holds {file} twice"));
        }
        if size > MAX_PROGRAM_BYTES {
            return Err(format!(
                "{file} has {size} bytes; a program larger than {MAX_PROGRAM_BYTES} is not \
                 installed"
            ));
        }
        let write = |e: std::io::Error| format!("{file} could not be unpacked: {e}");
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)
            .map_err(write)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 16];
        let mut left = MAX_PROGRAM_BYTES;
        loop {
            let read = entry.read(&mut buffer).map_err(write)?;
            if read == 0 {
                break;
            }
            left = left
                .checked_sub(read as u64)
                .ok_or_else(|| format!("{file} is larger than the archive says"))?;
            hasher.update(&buffer[..read]);
            out.write_all(&buffer[..read]).map_err(write)?;
        }
        out.sync_all().map_err(write)?;
        make_executable(&out).map_err(write)?;
        found = Some(
            hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
        );
    }
    found.ok_or_else(|| format!("it has no {file}"))
}

/// A program is run by its owner; anyone may read it, as any installed program.
#[cfg(unix)]
fn make_executable(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

/// A file's own name: no folder, nothing that climbs out of one.
fn is_plain_name(member: &str) -> bool {
    !member.is_empty()
        && member.len() <= 200
        && member != "."
        && member != ".."
        && !member.contains(['/', '\\', ':'])
        && !member.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_build_knows_what_it_is() {
        let app = AppBuild::this();
        assert_ne!(
            app.version,
            (0, 0, 0),
            "the workspace's version must be three numbers: the catalogue's entries are \
             compared with it"
        );
        assert_eq!(app.protocol.0, spelaion_plugin_host::VERSION.major);
        assert_eq!(app.platform.as_deref(), pins::platform());
    }

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
            "./spelaiondb-plugin-duckdb",
            "../spelaiondb-plugin-duckdb",
            "bin/spelaiondb-plugin-duckdb",
            "/usr/bin/x",
            "C:\\Windows\\x.exe",
            "C:x.exe",
            "a\\b",
            "a\nb",
        ] {
            assert!(!is_plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_kept_catalogue_is_its_signature_then_its_text() {
        let signature = "untrusted comment: a\r\nAAAA\r\ntrusted comment: b\r\nBBBB\r\n";
        let text = b"schema_version = 1\nserial = 3\n\n# a comment\n";
        let kept = signed_catalogue(signature, text);
        let (again, same) = split_signed(&kept).expect("four lines and a text");
        assert_eq!(
            again,
            "untrusted comment: a\nAAAA\ntrusted comment: b\nBBBB\n"
        );
        assert_eq!(same, text);
        // Fewer than four lines is not a kept catalogue.
        assert!(split_signed(b"one\ntwo\nthree").is_none());
        assert!(split_signed(b"").is_none());
    }

    #[test]
    fn a_version_has_one_spelling() {
        assert_eq!(version_text((0, 1, 0)), "0.1.0");
        assert_eq!(version_text(version_of("00.01.000").unwrap()), "0.1.0");
    }
}
