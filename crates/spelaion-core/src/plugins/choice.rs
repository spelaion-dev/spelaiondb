//! Which build of each of our plugins is started (ADR-042, ADR-055), and what the Plugins page
//! says about each.
//!
//! A build can be on this machine in two places: next to the app (inside the installer) or in
//! the user's store (installed from the Plugins page). For each plugin, **the one started is
//! the newest build on this machine that**
//!
//! - the pins compiled into this build or the accepted catalogue name, for this platform: its
//!   file is then started only while it has the SHA-256 they give;
//! - speaks a plugin protocol version the core speaks;
//! - this SpelaionDB is not too old for (what the catalogue's entry asks; a pinned build was
//!   tested with this very SpelaionDB);
//! - **was not withdrawn**, wherever it is: a build inside the installer too.
//!
//! A build that fails one of them is never started, and it is not skipped silently: when no
//! build of a plugin is left, its engines answer with the reason ([`PluginState::message`])
//! and do not fall back to anything else.
//!
//! Nothing here reads a setting, a variable or a repository file: what is started is decided
//! by the pins, the signed catalogue and which files are there.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use spelaion_driver_api::{ConnectParams, Connection, Driver, DriverError};

use super::catalogue::Catalogue;
use super::installed::{AppBuild, NamedBy, NotUsable, PluginStore, usable, version_text};
use super::pins::{PinnedPlugin, Pins, program_file_name, version_of};

/// Where a build is on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Next to the app: it came inside the installer.
    Shipped,
    /// In the user's store: installed from the Plugins page.
    Installed,
}

/// The build of a plugin that is started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub version: String,
    pub source: Source,
    pub program: PathBuf,
    /// The SHA-256 its file must have, checked before every start.
    pub sha256: String,
    /// The engines it serves.
    pub engines: Vec<String>,
}

/// A build on this machine that is not started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub version: String,
    pub source: Source,
    /// The engines it would serve, when anything says which.
    pub engines: Vec<String>,
    pub why: NotUsable,
}

/// The newest build the catalogue offers for this computer and this SpelaionDB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offered {
    pub version: String,
    pub engines: Vec<String>,
    /// The archive's size in bytes: what a download will fetch.
    pub archive_size: u64,
    /// Where its archive is: a release of the plugin's own repository.
    pub address: String,
}

/// One plugin of ours, as this machine has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginState {
    /// The plugin's name: `spelaiondb-plugin-duckdb`.
    pub name: String,
    /// The build that is started, if any.
    pub chosen: Option<Chosen>,
    /// The builds on this machine that are not started, newest first.
    pub refused: Vec<Refused>,
    /// What the catalogue offers, if it offers anything usable here.
    pub offered: Option<Offered>,
}

impl PluginState {
    /// The offered build, when it is newer than the one started: an update. Installing it is
    /// the user's click.
    pub fn update(&self) -> Option<&Offered> {
        let chosen = version_of(&self.chosen.as_ref()?.version)?;
        self.offered
            .as_ref()
            .filter(|offered| version_of(&offered.version).is_some_and(|v| v > chosen))
    }

    /// Why no build of this plugin is started although one is on this machine, as its engines
    /// say it; `None` when one is started or none is here.
    pub fn message(&self) -> Option<String> {
        if self.chosen.is_some() {
            return None;
        }
        let refused = self.refused.first()?;
        let mut message = format!(
            "{} {} is not started: {}.",
            self.name, refused.version, refused.why
        );
        if let Some(offered) = &self.offered {
            message.push_str(&format!(
                " Version {} can be installed in Settings > Plugins.",
                offered.version
            ));
        }
        Some(message)
    }
}

/// The state of every plugin of ours that the pins, the catalogue or the store know.
///
/// `shipped` is the folder the app is in; `store` the user's; `catalogue` the last one
/// accepted. Without a catalogue only the pinned builds can be started.
pub fn states(
    pins: &Pins,
    shipped: Option<&Path>,
    store: Option<&PluginStore>,
    catalogue: Option<&Catalogue>,
    app: &AppBuild,
) -> Vec<PluginState> {
    let installed = store.map(PluginStore::installed).unwrap_or_default();
    let listed: &[PinnedPlugin] = catalogue.map(Catalogue::versions).unwrap_or_default();
    let names: BTreeSet<&str> = pins
        .plugins()
        .iter()
        .chain(listed)
        .map(|plugin| plugin.name.as_str())
        .chain(installed.iter().map(|build| build.name.as_str()))
        .collect();

    names
        .into_iter()
        .map(|name| {
            let pin = pins.plugins().iter().find(|plugin| plugin.name == name);
            // The engines of a build nothing describes: those of the newest that is described.
            let known_engines = || {
                listed
                    .iter()
                    .filter(|plugin| plugin.name == name)
                    .chain(pin)
                    .max_by_key(|plugin| plugin.version())
                    .map(|plugin| plugin.engines.clone())
                    .unwrap_or_default()
            };

            // Every build on this machine, with whether it may be started.
            let mut here: Vec<Here> = Vec::new();
            if let (Some(folder), Some(pin)) = (shipped, pin) {
                let program = folder.join(program_file_name(name));
                if program.is_file() {
                    match usable(pin, NamedBy::Pins, catalogue, app) {
                        Ok(build) => here.push((
                            pin.version(),
                            Ok(Chosen {
                                version: version_text(pin.version()),
                                source: Source::Shipped,
                                program,
                                sha256: build.program_sha256.clone(),
                                engines: pin.engines.clone(),
                            }),
                        )),
                        // Pinned for other platforms only: there is no pin here, and a file by
                        // that name is not a plugin (ADR-042).
                        Err(NotUsable::NoBuild) => {}
                        Err(why) => here.push((
                            pin.version(),
                            Err(Refused {
                                version: version_text(pin.version()),
                                source: Source::Shipped,
                                engines: pin.engines.clone(),
                                why,
                            }),
                        )),
                    }
                }
            }
            for build in installed.iter().filter(|build| build.name == name) {
                let Some(version) = version_of(&build.version) else {
                    continue;
                };
                // Who names this build: the catalogue, or the pins (the build inside the
                // installer, installed again from a file).
                let named = catalogue
                    .and_then(|catalogue| catalogue.version(name, version))
                    .map(|entry| (entry, NamedBy::Catalogue))
                    .or_else(|| {
                        pin.filter(|pin| pin.version() == version)
                            .map(|pin| (pin, NamedBy::Pins))
                    });
                let state = match named {
                    Some((entry, named_by)) => match usable(entry, named_by, catalogue, app) {
                        Ok(pinned) => Ok(Chosen {
                            version: version_text(version),
                            source: Source::Installed,
                            program: build.program.clone(),
                            sha256: pinned.program_sha256.clone(),
                            engines: entry.engines.clone(),
                        }),
                        Err(why) => Err(Refused {
                            version: version_text(version),
                            source: Source::Installed,
                            engines: entry.engines.clone(),
                            why,
                        }),
                    },
                    // Nothing names it: a file in the store is not a plugin. It may still be
                    // one we withdrew and stopped listing.
                    None => Err(Refused {
                        version: version_text(version),
                        source: Source::Installed,
                        engines: known_engines(),
                        why: catalogue
                            .and_then(|catalogue| catalogue.why_withdrawn(name, version))
                            .map(|reason| NotUsable::Withdrawn(reason.to_string()))
                            .unwrap_or(NotUsable::Unlisted),
                    }),
                };
                here.push((version, state));
            }
            // Newest first; of one version in both places, the one inside the installer.
            here.sort_by(|(a, _), (b, _)| b.cmp(a));

            let mut chosen = None;
            let mut refused = Vec::new();
            for (_, state) in here {
                match state {
                    Ok(build) if chosen.is_none() => chosen = Some(build),
                    // An older build that could be started: there, and not the one in use.
                    Ok(_) => {}
                    Err(build) => refused.push(build),
                }
            }

            let offered = listed
                .iter()
                .filter(|plugin| plugin.name == name)
                .filter_map(|plugin| {
                    let build = usable(plugin, NamedBy::Catalogue, catalogue, app).ok()?;
                    Some((plugin, build))
                })
                .max_by_key(|(plugin, _)| plugin.version())
                .map(|(plugin, build)| Offered {
                    version: version_text(plugin.version()),
                    engines: plugin.engines.clone(),
                    archive_size: build.archive_size,
                    address: plugin.archive_address(build),
                });

            PluginState {
                name: name.to_string(),
                chosen,
                refused,
                offered,
            }
        })
        .collect()
}

/// A build on this machine: its version, and whether it may be started.
type Here = ((u32, u32, u32), Result<Chosen, Refused>);

/// An engine whose plugin is on this machine and may not be started: it says why, instead of
/// "engine not available" or an older behaviour.
pub(crate) struct RefusedPlugin {
    pub(crate) engine: String,
    pub(crate) why: String,
}

#[async_trait::async_trait]
impl Driver for RefusedPlugin {
    fn engine(&self) -> &str {
        &self.engine
    }

    async fn connect(&self, _params: &ConnectParams) -> Result<Box<dyn Connection>, DriverError> {
        Err(DriverError::Connection(self.why.clone()))
    }

    async fn create_database(&self, _database: &str) -> Result<(), DriverError> {
        Err(DriverError::Connection(self.why.clone()))
    }
}
