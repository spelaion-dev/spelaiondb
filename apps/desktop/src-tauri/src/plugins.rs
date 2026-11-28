//! The Plugins page's commands (ADR-055): a thin adapter over `spelaion_core::plugins`.
//!
//! Every decision is the core's: which build of each plugin is started and what the catalogue
//! offers (`choice::states`), when the catalogue may be read (`fetch::updates_allowed`), what
//! is fetched and from where (`fetch`), what is installed (`installed::PluginStore`). Errors
//! reach the page as the core wrote them.
//!
//! - **No command takes a key or a catalogue's text, and one only takes an address.** They take
//!   a plugin's name, a version the overview listed, an id the page chose, the hash a dialog
//!   showed, or the path of a file the user picked in the app's file dialog. The one address is
//!   [`plugins_inspect_address`]'s: what the user typed or pasted into the Plugins page to add
//!   someone else's plugin (ADR-055, point 5), which nothing else in the app calls and nothing
//!   stores to read again. `scripts/plugin-commands.test.mjs` checks the bindings.
//! - **Other people's plugins** are added only on the user's yes to the dialog
//!   ([`plugins_add_theirs`], given back the hash the dialog showed), are listed apart and as
//!   not ours, are never offered an update, and get nothing from agents: every rule is the
//!   core's (`spelaion_core::plugins::external`).
//! - **Nothing is fetched without the user** but the catalogue, under the "check for updates"
//!   setting (and never with `SPELAION_NO_UPDATES`): when the page opens
//!   ([`plugins_refresh_catalogue`]) and where the app checks for its own updates
//!   ([`refresh_in_background`]).
//! - **The engines change without a restart.** After an install, an update, a removal or a
//!   catalogue accepted, `AppState.engines` is rebuilt with `Engines::builtin_with` and
//!   swapped: the next connection starts the new build, and a connection already open keeps
//!   the plugin process it has (ADR-055, point 7). Removing a build stops its plugin (the
//!   core does it) and its sessions are lost (ADR-028), so the page is asked to confirm first.
//! - `spelaiondb mcp` has none of this: it starts what the app installed.

use spelaion_desktop_macros::command;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::Engines;
use spelaion_core::plugins::catalogue::{CatalogueError, CatalogueKey, MAX_CATALOGUE_BYTES};
use spelaion_core::plugins::choice::{self, PluginState};
use spelaion_core::plugins::external;
use spelaion_core::plugins::fetch::{self, Body, Transport, TransportError};
use spelaion_core::plugins::installed::{AppBuild, PluginStore};
use spelaion_core::plugins::pins::Pins;
use spelaion_https::HttpsTransport;
use tauri::{AppHandle, Manager, State};
use tauri_specta::Event as _;

use crate::{AppState, CmdResult};

/// The engines a connection opened now uses: the driver compiled in, the plugins inside the
/// installer and the ones installed in `user_dir`, under the catalogue kept there.
pub(crate) fn engines_for(user_dir: &Path) -> Arc<Engines> {
    Arc::new(Engines::builtin_with(&PluginStore::in_user_dir(user_dir)))
}

/// What the Plugins page is doing now.
#[derive(Default)]
pub(crate) struct PluginOps {
    /// One change to the plugins at a time (an install, a removal, a catalogue): each ends by
    /// rebuilding the engines from what is on disk, so none swaps in engines another did not
    /// see. A second install waits for the first.
    changing: tokio::sync::Mutex<()>,
    /// Each plugin's install, by plugin name, when it is not idle. Held here, not in the page,
    /// so the Plugins page, the "add connection" picker and a connection that could not open
    /// show the same thing (SDB-373). Changed only by the methods below.
    installs: std::sync::Mutex<HashMap<String, Install>>,
}

/// A plugin's install that is not idle.
enum Install {
    /// Its archive is downloading. `install` is its job's id (`jobs.rs`), to stop it.
    Downloading {
        install: u32,
        received: u64,
        total: u64,
    },
    /// The last install failed or was stopped: why. Until the next install of it, or until a
    /// file installed or a build removed changes what is here.
    Failed { why: String },
}

impl PluginOps {
    /// `name` starts downloading as the install job `install`, of `total` bytes.
    fn begin(&self, name: &str, install: u32, total: u64) {
        self.installs.lock().expect("lock poisoned").insert(
            name.to_string(),
            Install::Downloading {
                install,
                received: 0,
                total,
            },
        );
    }

    /// `received` bytes of the install `install` of `name` have arrived.
    fn progress(&self, name: &str, install: u32, received: u64) {
        if let Some(Install::Downloading {
            install: running,
            received: so_far,
            ..
        }) = self.installs.lock().expect("lock poisoned").get_mut(name)
            && *running == install
        {
            *so_far = received;
        }
    }

    /// The install `install` of `name` ended: idle again when it worked, failed with why when
    /// not. A later install of the same plugin has replaced it, and is left alone.
    fn end(&self, name: &str, install: u32, outcome: &Result<(), String>) {
        let mut installs = self.installs.lock().expect("lock poisoned");
        let ours = match installs.get(name) {
            Some(Install::Downloading {
                install: running, ..
            }) => *running == install,
            Some(Install::Failed { .. }) | None => false,
        };
        if !ours {
            return;
        }
        match outcome {
            Ok(()) => {
                installs.remove(name);
            }
            Err(why) => {
                installs.insert(name.to_string(), Install::Failed { why: why.clone() });
            }
        }
    }

    /// What was here changed (a file installed, a build removed): old failures no longer say
    /// anything about it.
    fn forget_failures(&self) {
        self.installs
            .lock()
            .expect("lock poisoned")
            .retain(|_, install| match install {
                Install::Downloading { .. } => true,
                Install::Failed { .. } => false,
            });
    }

    /// The install of `name`, as the page sees it.
    fn of(&self, name: &str) -> PluginInstall {
        match self.installs.lock().expect("lock poisoned").get(name) {
            None => PluginInstall::Idle,
            Some(Install::Downloading {
                install,
                received,
                total,
                ..
            }) => PluginInstall::Downloading {
                install: *install,
                received: *received as f64,
                total: *total as f64,
            },
            Some(Install::Failed { why }) => PluginInstall::Failed { why: why.clone() },
        }
    }
}

/// A plugin's install, as every view shows it (SDB-373).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PluginInstall {
    /// No install of it is running, and the last one did not fail.
    Idle,
    /// Its archive is downloading: bytes received of the total the catalogue gives (0 when it
    /// gives none). `install` is its job's id: `cancelJob(install, "pluginInstall")` stops it.
    #[serde(rename_all = "camelCase")]
    Downloading {
        install: u32,
        received: f64,
        total: f64,
    },
    /// The last install failed or was stopped, in the core's words.
    #[serde(rename_all = "camelCase")]
    Failed { why: String },
}

/// What a plugin's engines get on this machine: the core's one answer
/// (`PluginState::availability`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum PluginAvailability {
    /// A build is started: its engines use it.
    InUse,
    /// Builds are here and none may be started (`message` says why).
    Refused,
    /// The pinned build should be next to the app and is not (`message` says which file).
    Missing,
    /// Nothing of it is here: it needs installing.
    NotInstalled,
}

impl From<&choice::Availability> for PluginAvailability {
    fn from(availability: &choice::Availability) -> Self {
        match availability {
            choice::Availability::InUse(_) => Self::InUse,
            choice::Availability::Refused(_) => Self::Refused,
            choice::Availability::Missing(_) => Self::Missing,
            choice::Availability::NotInstalled => Self::NotInstalled,
        }
    }
}

/// Where a build is on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum PluginSource {
    /// Next to the app: it came inside the installer.
    Shipped,
    /// In the user's personal folder: installed from the Plugins page.
    Installed,
}

impl From<choice::Source> for PluginSource {
    fn from(source: choice::Source) -> Self {
        match source {
            choice::Source::Shipped => Self::Shipped,
            choice::Source::Installed => Self::Installed,
        }
    }
}

/// The build of a plugin that its engines start.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PluginInUse {
    pub version: String,
    pub source: PluginSource,
    /// The engines it serves, by the name the configuration uses (`duckdb`).
    pub engines: Vec<String>,
}

/// A build on this machine that is not started, and why (the core's words).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PluginRefused {
    pub version: String,
    pub source: PluginSource,
    pub engines: Vec<String>,
    pub why: String,
}

/// The newest build the catalogue offers for this computer and this SpelaionDB.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PluginOffered {
    pub version: String,
    pub engines: Vec<String>,
    /// What installing it downloads, in bytes.
    pub archive_size: f64,
    /// Where it is downloaded from, to show: a release of the plugin's repository. Written by
    /// the core; no command takes it back.
    pub address: String,
}

/// A build pinned into this SpelaionDB whose program is not next to the app.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PluginMissing {
    pub version: String,
    /// The file that is not there.
    pub program: String,
    pub engines: Vec<String>,
}

/// One plugin of ours, as this machine has it.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PluginItem {
    /// The plugin's name: `spelaiondb-plugin-duckdb`.
    pub name: String,
    /// Every engine anything here says it serves.
    pub engines: Vec<String>,
    /// What its engines get: the core's one answer.
    pub availability: PluginAvailability,
    /// Its install, wherever it was started from.
    pub install: PluginInstall,
    /// The build started for its engines, if any.
    pub in_use: Option<PluginInUse>,
    /// The builds on this machine that are not started, newest first.
    pub refused: Vec<PluginRefused>,
    /// What the catalogue offers, if anything usable here.
    pub offered: Option<PluginOffered>,
    /// The offered version, when it is newer than the one in use: an update.
    pub update: Option<String>,
    /// The pinned build, when it should have come inside the installer and is not there.
    pub missing: Option<PluginMissing>,
    /// Why its engines start nothing although a build is here, as they say it.
    pub message: Option<String>,
    /// The versions in the user's personal folder: what `pluginsRemove` can remove.
    pub installed: Vec<String>,
    /// Sessions open now on its engines (editor tabs and the explorer's, one per connection).
    pub open_sessions: u32,
}

/// What the Plugins page shows.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PluginsOverview {
    /// Whether this SpelaionDB can accept a catalogue at all: it carries the catalogue's key.
    pub accepts_catalogue: bool,
    /// Whether the catalogue may be read now: the "check for updates" setting, unless
    /// `SPELAION_NO_UPDATES` is set.
    pub updates_allowed: bool,
    /// The serial of the catalogue accepted last, if any.
    pub catalogue_serial: Option<f64>,
    pub plugins: Vec<PluginItem>,
    /// The plugins of other people's the user added, which are not part of SpelaionDB.
    pub theirs: Vec<TheirPluginItem>,
}

/// What a plugin the user added is now (the core's `ExternalState`).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TheirPluginState {
    /// Its program is the file the user trusted: its engine uses it.
    Trusted,
    /// Its program changed since: refused until trusted again. `found` is its SHA-256 now,
    /// what `pluginsTrustAgain` is given back once the user agreed.
    #[serde(rename_all = "camelCase")]
    Changed { found: String },
    /// Its program is gone.
    Missing,
    /// It cannot be read or started here, in the core's words.
    #[serde(rename_all = "camelCase")]
    Unusable { why: String },
}

/// A plugin of someone else's the user added: not part of SpelaionDB, and not supported.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TheirPluginItem {
    pub name: String,
    pub version: Option<String>,
    /// Who it says it is from.
    pub author: Option<String>,
    pub description: Option<String>,
    /// Where its author says to look for it: shown as text, never opened by the app.
    pub homepage: Option<String>,
    /// The engine it adds, as connections name it, and as people write it.
    pub engine: Option<String>,
    pub engine_label: Option<String>,
    /// The SHA-256 of the program the user trusted.
    pub sha256: Option<String>,
    /// The file or address it was added from: shown, never read again.
    pub source: Option<String>,
    pub state: TheirPluginState,
    /// Why its engine does not open, when it does not (the core's words).
    pub refused: Option<String>,
    /// Sessions open now on its engine.
    pub open_sessions: u32,
}

/// Where an archive waiting for the user's answer came from (the core's `Origin`).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PendingOriginItem {
    #[serde(rename_all = "camelCase")]
    File { path: String },
    /// The whole address the user gave, its host and, when it was a release manifest, the
    /// archive's own address and host; `arrived_from` is where the archive's bytes finally
    /// came from after a redirect, when that was elsewhere.
    #[serde(rename_all = "camelCase")]
    Address {
        address: String,
        host: String,
        archive: Option<String>,
        archive_host: Option<String>,
        arrived_from: Option<String>,
    },
}

/// What the dialog shows before someone else's plugin is added: what it says it is, who it
/// says it is from, the engine it adds, where it came from and the hash of what arrived.
/// Nothing in it was run or unpacked.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PendingPluginItem {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub engine: String,
    pub engine_label: String,
    /// The core's dialect its scripts are read with; none for standard SQL.
    pub dialect: Option<String>,
    /// The SHA-256 of the archive that arrived: what `pluginsAddTheirs` is given back.
    pub archive_sha256: String,
    pub archive_size: f64,
    /// The SHA-256 of its program, checked before every start.
    pub program_sha256: String,
    pub origin: PendingOriginItem,
    /// The version of the same plugin it replaces, if one was added before.
    pub replaces: Option<String>,
}

impl From<external::Pending> for PendingPluginItem {
    fn from(pending: external::Pending) -> Self {
        let manifest = pending.manifest;
        Self {
            name: manifest.name,
            version: manifest.version,
            author: manifest.author,
            description: manifest.description,
            homepage: manifest.homepage,
            engine: manifest.engine,
            engine_label: manifest.engine_label,
            dialect: manifest.dialect,
            archive_sha256: pending.archive_sha256,
            archive_size: pending.archive_size as f64,
            program_sha256: pending.program_sha256,
            origin: match pending.origin {
                external::Origin::File { path } => PendingOriginItem::File { path },
                external::Origin::Address {
                    address,
                    host,
                    archive,
                    archive_host,
                    arrived_from,
                } => PendingOriginItem::Address {
                    address,
                    host,
                    archive,
                    archive_host,
                    arrived_from,
                },
            },
            replaces: pending.replaces,
        }
    }
}

fn their_item(
    plugin: &external::ExternalPlugin,
    sessions: &HashMap<String, u32>,
) -> TheirPluginItem {
    let manifest = plugin.manifest.as_ref();
    let engine = manifest.map(|manifest| manifest.engine.clone());
    TheirPluginItem {
        name: plugin.name.clone(),
        version: manifest.map(|manifest| manifest.version.clone()),
        author: manifest.map(|manifest| manifest.author.clone()),
        description: manifest.and_then(|manifest| manifest.description.clone()),
        homepage: manifest.and_then(|manifest| manifest.homepage.clone()),
        engine_label: manifest.map(|manifest| manifest.engine_label.clone()),
        open_sessions: engine
            .as_ref()
            .and_then(|engine| sessions.get(&external_key(engine)).copied())
            .unwrap_or(0),
        engine,
        sha256: plugin.sha256.clone(),
        source: plugin.source.clone(),
        state: match &plugin.state {
            external::ExternalState::Trusted => TheirPluginState::Trusted,
            external::ExternalState::Changed { found } => TheirPluginState::Changed {
                found: found.clone(),
            },
            external::ExternalState::Missing => TheirPluginState::Missing,
            external::ExternalState::Unusable(why) => {
                TheirPluginState::Unusable { why: why.clone() }
            }
        },
        refused: plugin.refusal(),
    }
}

/// How `sessions_by_engine` counts the sessions of an engine of someone else's plugin: apart
/// from ours, whose names it can never have.
fn external_key(engine: &str) -> String {
    format!("external:{engine}")
}

/// What removing a build did.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PluginRemoval {
    /// Nothing was removed: the build is running, and removing it stops it and loses its
    /// sessions. Call again with `confirmed` once the user agreed.
    #[serde(rename_all = "camelCase")]
    NeedsConfirmation { open_sessions: u32 },
    /// Removed; the engines were swapped.
    #[serde(rename_all = "camelCase")]
    Removed { overview: PluginsOverview },
}

/// One running plugin process, as Settings › Plugins shows it (SDB-399). The numbers are the
/// core's, read from the operating system for that process.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PluginProcessItem {
    /// The plugin's name: its program's, `spelaiondb-plugin-duckdb`.
    pub name: String,
    pub pid: u32,
    pub memory_bytes: Option<f64>,
    /// Since the previous reading, in percent of one core; none at the first.
    pub cpu_percent: Option<f64>,
    pub cpu_seconds: Option<f64>,
    pub sessions: u32,
    pub uptime_seconds: f64,
    pub engines: Vec<String>,
}

/// What restarting a plugin did.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PluginRestart {
    /// Nothing was stopped: these sessions (tabs, by id) have a transaction open, which the
    /// restart would roll back (ADR-031). Call again with `confirmed` once the user agreed.
    #[serde(rename_all = "camelCase")]
    NeedsConfirmation { sessions: Vec<String> },
    /// Stopped: its sessions are lost and the next connection starts it again.
    Restarted,
    /// It was not running (it ended meanwhile).
    NotRunning,
}

/// The plugin's name from its program: the file name without `.exe`.
fn plugin_name(program: &Path) -> String {
    program
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.display().to_string())
}

/// The plugin processes running now, with what each uses (SDB-399). Nothing is started.
#[command]
pub async fn plugin_processes() -> CmdResult<Vec<PluginProcessItem>> {
    Ok(spelaion_core::plugins::running()
        .await
        .into_iter()
        .map(|plugin| PluginProcessItem {
            name: plugin_name(&plugin.program),
            pid: plugin.usage.pid,
            memory_bytes: plugin.usage.memory_bytes.map(|bytes| bytes as f64),
            cpu_percent: plugin.usage.cpu_percent,
            cpu_seconds: plugin.usage.cpu_seconds,
            sessions: u32::try_from(plugin.usage.sessions).unwrap_or(u32::MAX),
            uptime_seconds: plugin.usage.uptime.as_secs_f64(),
            engines: plugin.usage.engines,
        })
        .collect())
}

/// Restarts the plugin `name` (SDB-399): its process is stopped, given `STOP_GRACE` to close
/// its files, and the next connection starts it again, checked again. Its sessions are lost
/// (ADR-028): each tab says so at once, and nothing is run again. When a session of it has a
/// transaction open, nothing is stopped until `confirmed` (ADR-031).
#[command]
pub async fn plugin_restart(
    state: State<'_, AppState>,
    name: String,
    confirmed: bool,
) -> CmdResult<PluginRestart> {
    let _changing = state.plugins.changing.lock().await;
    let Some(plugin) = spelaion_core::plugins::running()
        .await
        .into_iter()
        .find(|plugin| plugin_name(&plugin.program) == name)
    else {
        return Ok(PluginRestart::NotRunning);
    };
    let engines = plugin.usage.engines.clone();
    let of_plugin = |session: &crate::DesktopSession| {
        engines
            .iter()
            .any(|engine| engine == session.open.resolved.engine.as_str())
    };
    if !confirmed {
        let mut open: Vec<String> = state
            .sessions
            .lock()
            .await
            .iter()
            .filter(|(_, session)| {
                of_plugin(session) && session.open.session.transaction_status().is_open()
            })
            .map(|(id, _)| id.clone())
            .collect();
        if !open.is_empty() {
            open.sort();
            return Ok(PluginRestart::NeedsConfirmation { sessions: open });
        }
    }
    spelaion_core::plugins::stop(&plugin.program).await;
    // Their connections are closed now: each tab is told at once (the health watcher sends
    // `SessionLost`), rather than at its next keep-alive.
    let reason = format!("the plugin {name} was restarted");
    for session in state.sessions.lock().await.values() {
        if of_plugin(session) {
            session.open.session.mark_lost(reason.clone());
        }
    }
    Ok(PluginRestart::Restarted)
}

/// A plugin's install changed: it started, more of it arrived (a few times a second at most),
/// it failed or was stopped, or it ended (idle again; the overview has changed then). Every
/// view that shows the plugin reads it, wherever the install was started (SDB-373).
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstallChanged {
    /// The plugin's name.
    pub name: String,
    pub install: PluginInstall,
}

/// A catalogue was accepted without the page asking (where the app checks for its own
/// updates): what the page shows may have changed.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct PluginsChanged {
    pub catalogue_serial: f64,
}

fn store_of(state: &AppState) -> PluginStore {
    PluginStore::in_user_dir(&state.user_dir)
}

/// Whether the catalogue may be read now (the core's rule). A setting that cannot be read
/// counts as off.
async fn updates_allowed(state: &AppState) -> bool {
    let setting = state.history.update_check().await.unwrap_or(false);
    fetch::updates_allowed(setting, std::env::var_os(fetch::NO_UPDATES))
}

/// Rebuilds the engines from what is on disk now and swaps them in.
fn swap_engines(state: &AppState) {
    let engines = engines_for(&state.user_dir);
    *state.engines.write().expect("lock poisoned") = engines;
}

/// How many sessions are open on each engine.
pub(crate) async fn sessions_by_engine(state: &AppState) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for session in state.sessions.lock().await.values() {
        let resolved = &session.open.resolved;
        let key = match (resolved.engine, &resolved.external_engine) {
            (spelaion_config::Engine::External, Some(engine)) => external_key(engine),
            (engine, _) => engine.as_str().to_string(),
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

/// Every plugin of ours as this machine has it: the one place the adapter asks the core.
fn plugin_states(
    store: &PluginStore,
    catalogue: Option<&spelaion_core::plugins::catalogue::Catalogue>,
) -> Vec<PluginState> {
    choice::states(
        Pins::shipped(),
        spelaion_core::plugins::shipped_folder().as_deref(),
        Some(store),
        catalogue,
        &AppBuild::this(),
    )
}

/// Tells every view that `name`'s install is now what `PluginOps` holds.
fn install_changed(app: &AppHandle, ops: &PluginOps, name: &str) {
    let _ = PluginInstallChanged {
        name: name.to_string(),
        install: ops.of(name),
    }
    .emit(app);
}

pub(crate) async fn overview(state: &AppState) -> PluginsOverview {
    let store = store_of(state);
    let catalogue = store.catalogue();
    let states = plugin_states(&store, catalogue.as_ref());
    let installed = store.installed();
    let sessions = sessions_by_engine(state).await;
    // Hashing each program of theirs is not for the runtime's own threads.
    let theirs_store = store.theirs();
    let theirs = tokio::task::spawn_blocking(move || theirs_store.plugins(&AppBuild::this()))
        .await
        .unwrap_or_default();
    PluginsOverview {
        theirs: theirs
            .iter()
            .map(|plugin| their_item(plugin, &sessions))
            .collect(),
        accepts_catalogue: CatalogueKey::shipped().is_some(),
        updates_allowed: updates_allowed(state).await,
        catalogue_serial: catalogue
            .as_ref()
            .map(|catalogue| catalogue.serial() as f64),
        plugins: states
            .iter()
            .map(|plugin| {
                // The pinned build is the installer's, even once unpacked into the store
                // (ADR-057): it is not offered for removal, since the next start would only
                // install it again.
                let pinned = Pins::shipped()
                    .plugins()
                    .iter()
                    .find(|pin| pin.name == plugin.name)
                    .map(|pin| pin.version.as_str());
                let versions = installed
                    .iter()
                    .filter(|build| build.name == plugin.name)
                    .filter(|build| Some(build.version.as_str()) != pinned)
                    .map(|build| build.version.clone())
                    .collect();
                item(plugin, versions, &sessions, state.plugins.of(&plugin.name))
            })
            .collect(),
    }
}

fn item(
    state: &PluginState,
    installed: Vec<String>,
    sessions: &HashMap<String, u32>,
    install: PluginInstall,
) -> PluginItem {
    let engines: BTreeSet<String> = state
        .chosen
        .iter()
        .flat_map(|build| build.engines.iter())
        .chain(state.refused.iter().flat_map(|build| build.engines.iter()))
        .chain(state.offered.iter().flat_map(|build| build.engines.iter()))
        .chain(state.missing.iter().flat_map(|build| build.engines.iter()))
        .cloned()
        .collect();
    let open_sessions = engines
        .iter()
        .map(|engine| sessions.get(engine).copied().unwrap_or(0))
        .sum();
    PluginItem {
        name: state.name.clone(),
        engines: engines.into_iter().collect(),
        availability: (&state.availability()).into(),
        install,
        in_use: state.chosen.as_ref().map(|build| PluginInUse {
            version: build.version.clone(),
            source: build.source.into(),
            engines: build.engines.clone(),
        }),
        refused: state
            .refused
            .iter()
            .map(|build| PluginRefused {
                version: build.version.clone(),
                source: build.source.into(),
                engines: build.engines.clone(),
                why: build.why.to_string(),
            })
            .collect(),
        offered: state.offered.as_ref().map(|build| PluginOffered {
            version: build.version.clone(),
            engines: build.engines.clone(),
            archive_size: build.archive_size as f64,
            address: build.address.clone(),
        }),
        update: state.update().map(|build| build.version.clone()),
        missing: state.missing.as_ref().map(|build| PluginMissing {
            version: build.version.clone(),
            program: build.program.display().to_string(),
            engines: build.engines.clone(),
        }),
        message: state.message(),
        installed,
        open_sessions,
    }
}

/// Every plugin of ours: the build in use, what is refused and why, what the catalogue offers.
/// Reads only this machine: it fetches nothing.
#[command]
pub async fn plugins_overview(state: State<'_, AppState>) -> CmdResult<PluginsOverview> {
    Ok(overview(&state).await)
}

/// Reads the published catalogue and accepts it, if the "check for updates" setting allows it
/// and this SpelaionDB has the catalogue's key (else the core's error says which). For when
/// the Plugins page opens. The engines are swapped when it is accepted.
#[command]
pub async fn plugins_refresh_catalogue(state: State<'_, AppState>) -> CmdResult<PluginsOverview> {
    let allowed = updates_allowed(&state).await;
    let transport = HttpsTransport::new().map_err(|error| error.0)?;
    let _changing = state.plugins.changing.lock().await;
    let store = store_of(&state);
    fetch::refresh_catalogue(&store, &transport, allowed)
        .await
        .map_err(|error| error.to_string())?;
    swap_engines(&state);
    Ok(overview(&state).await)
}

/// Reads the catalogue where the app checks for its own updates, in the background: only when
/// it may be read and this SpelaionDB has the key. A failure stays quiet, as the update check's
/// does at startup; an accepted catalogue swaps the engines and sends `PluginsChanged`.
pub(crate) fn refresh_in_background(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if CatalogueKey::shipped().is_none() || !updates_allowed(&state).await {
            return;
        }
        let Ok(transport) = HttpsTransport::new() else {
            return;
        };
        let _changing = state.plugins.changing.lock().await;
        match fetch::refresh_catalogue(&store_of(&state), &transport, true).await {
            Ok(catalogue) => {
                swap_engines(&state);
                let _ = PluginsChanged {
                    catalogue_serial: catalogue.serial() as f64,
                }
                .emit(&app);
            }
            Err(error) => eprintln!("the plugin catalogue was not read: {error}"),
        }
    });
}

/// Installs the newest build the accepted catalogue offers of the plugin `name`: an install or
/// an update, on the user's click. Its job's ticket goes to `job` at once (`cancel_job` stops
/// it), and its progress to every view through `PluginInstallChanged`. The next connection of
/// its engines starts the new build; connections already open keep theirs.
#[command]
pub async fn plugins_install(
    app: AppHandle,
    state: State<'_, AppState>,
    job: tauri::ipc::Channel<crate::jobs::JobTicket>,
    name: String,
) -> CmdResult<PluginsOverview> {
    // What the download will fetch, from the catalogue's entry for what it offers.
    let total = {
        let store = store_of(&state);
        let catalogue = store.catalogue();
        plugin_states(&store, catalogue.as_ref())
            .into_iter()
            .find(|plugin| plugin.name == name)
            .and_then(|plugin| plugin.offered)
            .map_or(0, |offered| offered.archive_size)
    };
    let running = crate::jobs::start(&state, crate::jobs::JobKind::PluginInstall, &job);
    let install = running.id();
    state.plugins.begin(&name, install, total);
    install_changed(&app, &state.plugins, &name);
    let work = async {
        let transport = HttpsTransport::new().map_err(|error| error.0)?;
        let _changing = state.plugins.changing.lock().await;
        let store = store_of(&state);
        let catalogue = store
            .catalogue()
            .ok_or_else(|| fetch::FetchError::NotOffered(name.clone()).to_string())?;
        let report = progress_reporter(app.clone(), name.clone(), install);
        let transport = Counting {
            inner: transport,
            report,
        };
        fetch::install_offered(&store, &transport, &catalogue, &AppBuild::this(), &name)
            .await
            .map_err(|error| error.to_string())?;
        swap_engines(&state);
        Ok::<_, String>(())
    };
    // Stopping drops the download: the core leaves nothing of it behind.
    let result = tokio::select! {
        result = work => result,
        () = running.cancel().cancelled() => Err(format!("installing {name} was stopped")),
    };
    drop(running);
    state.plugins.end(&name, install, &result);
    install_changed(&app, &state.plugins, &name);
    result?;
    Ok(overview(&state).await)
}

/// Installs a file the user picked in the app's file dialog: a plugin's archive (installed only
/// as the accepted catalogue's entry with its SHA-256) or, for a file whose name ends in
/// `.signed`, a catalogue with its signature (accepted only with our signature, not older than
/// the one kept). The engines are swapped.
#[command]
pub async fn plugins_install_file(
    app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> CmdResult<PluginsOverview> {
    let path = Path::new(&path);
    let _changing = state.plugins.changing.lock().await;
    let store = store_of(&state);
    let is_catalogue = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".signed"));
    if is_catalogue {
        let size = std::fs::metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len();
        // Not read whole if it cannot be a catalogue and its signature.
        if size > (MAX_CATALOGUE_BYTES + 8192) as u64 {
            return Err(
                CatalogueError::TooLarge(usize::try_from(size).unwrap_or(usize::MAX)).to_string(),
            );
        }
        let signed = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        store
            .accept_signed_catalogue(&signed)
            .map_err(|error| error.to_string())?;
    } else {
        let Some(catalogue) = store.catalogue() else {
            return Err(match CatalogueKey::shipped() {
                None => CatalogueError::NoKey.to_string(),
                Some(_) => "no plugin catalogue has been accepted yet: check for updates, or \
                            install the catalogue's .signed file first"
                    .to_string(),
            });
        };
        store
            .install(&catalogue, &AppBuild::this(), path)
            .map_err(|error| error.to_string())?;
    }
    swap_engines(&state);
    forget_failures(&app, &state.plugins);
    Ok(overview(&state).await)
}

/// What is here changed: installs that failed before no longer say anything about it.
fn forget_failures(app: &AppHandle, ops: &PluginOps) {
    let failed: Vec<String> = ops
        .installs
        .lock()
        .expect("lock poisoned")
        .iter()
        .filter_map(|(name, install)| match install {
            Install::Failed { .. } => Some(name.clone()),
            Install::Downloading { .. } => None,
        })
        .collect();
    ops.forget_failures();
    for name in failed {
        install_changed(app, ops, &name);
    }
}

/// Removes the build `version` of the plugin `name` from the user's personal folder (one the
/// overview lists in `installed`). If that build is running, nothing is removed until
/// `confirmed`: removing it stops it, and its sessions are lost (ADR-028). The engines are
/// swapped, so the next connection starts whatever build is left.
#[command]
pub async fn plugins_remove(
    app: AppHandle,
    state: State<'_, AppState>,
    name: String,
    version: String,
    confirmed: bool,
) -> CmdResult<PluginRemoval> {
    let _changing = state.plugins.changing.lock().await;
    let store = store_of(&state);
    // Only a build the store lists, by the name and version it lists.
    let build = store
        .installed()
        .into_iter()
        .find(|build| build.name == name && build.version == version)
        .ok_or_else(|| format!("{name} {version} is not installed"))?;
    let numbers = version_numbers(&build.version)
        .ok_or_else(|| format!("{name} {version} is not installed"))?;
    if !confirmed && spelaion_core::plugins::is_running(&build.program).await {
        let sessions = sessions_by_engine(&state).await;
        let catalogue = store.catalogue();
        let open_sessions = plugin_states(&store, catalogue.as_ref())
            .iter()
            .find(|plugin| plugin.name == name)
            .map(|plugin| {
                item(plugin, Vec::new(), &sessions, state.plugins.of(&name)).open_sessions
            })
            .unwrap_or(0);
        return Ok(PluginRemoval::NeedsConfirmation { open_sessions });
    }
    store
        .remove(&build.name, numbers)
        .await
        .map_err(|error| format!("{name} {version} could not be removed: {error}"))?;
    swap_engines(&state);
    forget_failures(&app, &state.plugins);
    Ok(PluginRemoval::Removed {
        overview: overview(&state).await,
    })
}

/// Reads a plugin's archive the user picked in the app's file dialog, to add someone else's
/// plugin: what the dialog shows. Nothing in it is run or unpacked (the core's rule).
#[command]
pub async fn plugins_inspect_file(
    state: State<'_, AppState>,
    path: String,
) -> CmdResult<PendingPluginItem> {
    let theirs = store_of(&state).theirs();
    tokio::task::spawn_blocking(move || theirs.inspect_file(Path::new(&path), &AppBuild::this()))
        .await
        .map_err(|error| error.to_string())?
        .map(PendingPluginItem::from)
        .map_err(|error| error.to_string())
}

/// Reads, once, the address the user typed or pasted into the Plugins page to add someone
/// else's plugin: the `https://` address of its archive, or of its release manifest. What the
/// dialog shows; nothing that arrives is run or unpacked, every redirect stays on HTTPS and
/// the download is bounded (the core's rules). **The only command that takes an address**:
/// nothing but that field of the Plugins page calls it. Its ticket goes to `job`, and
/// `cancel_job` stops the download.
#[command]
pub async fn plugins_inspect_address(
    state: State<'_, AppState>,
    job: tauri::ipc::Channel<crate::jobs::JobTicket>,
    address: String,
) -> CmdResult<PendingPluginItem> {
    let running = crate::jobs::start(&state, crate::jobs::JobKind::PluginInstall, &job);
    let work = async {
        let transport = HttpsTransport::new().map_err(|error| error.0)?;
        store_of(&state)
            .theirs()
            .inspect_address(&transport, &address, &AppBuild::this())
            .await
            .map(PendingPluginItem::from)
            .map_err(|error| error.to_string())
    };
    // Stopping drops the download: the core leaves nothing of it behind.
    let result = tokio::select! {
        result = work => result,
        () = running.cancel().cancelled() => Err("reading the address was stopped".to_string()),
    };
    drop(running);
    result
}

/// Adds the plugin of someone else's waiting with `sha256`, the hash the dialog showed: **the
/// user's yes**. The core adds it only if what waits still has that hash, and pins its program.
/// A plugin of the same name is replaced (stopped first, its sessions lost). The engines are
/// swapped.
#[command]
pub async fn plugins_add_theirs(
    state: State<'_, AppState>,
    sha256: String,
) -> CmdResult<PluginsOverview> {
    let _changing = state.plugins.changing.lock().await;
    store_of(&state)
        .theirs()
        .add(&sha256, &AppBuild::this())
        .await
        .map_err(|error| error.to_string())?;
    swap_engines(&state);
    Ok(overview(&state).await)
}

/// Drops the archive waiting with `sha256`: the user said no to the dialog.
#[command]
pub async fn plugins_discard_theirs(state: State<'_, AppState>, sha256: String) -> CmdResult<()> {
    store_of(&state).theirs().discard(&sha256);
    Ok(())
}

/// Trusts again the program of the plugin `name` whose file changed, as it is now: only if its
/// SHA-256 is still `sha256`, the hash the dialog showed (**the user's yes**). The engines are
/// swapped.
#[command]
pub async fn plugins_trust_again(
    state: State<'_, AppState>,
    name: String,
    sha256: String,
) -> CmdResult<PluginsOverview> {
    let _changing = state.plugins.changing.lock().await;
    let theirs = store_of(&state).theirs();
    tokio::task::spawn_blocking(move || theirs.trust_again(&name, &sha256, &AppBuild::this()))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    swap_engines(&state);
    Ok(overview(&state).await)
}

/// Removes the plugin of someone else's `name` and its pin. If sessions are open on its engine,
/// nothing is removed until `confirmed`: removing it stops it and they are lost (ADR-028). The
/// engines are swapped.
#[command]
pub async fn plugins_remove_theirs(
    state: State<'_, AppState>,
    name: String,
    confirmed: bool,
) -> CmdResult<PluginRemoval> {
    let _changing = state.plugins.changing.lock().await;
    if !confirmed {
        let open_sessions = overview(&state)
            .await
            .theirs
            .iter()
            .find(|plugin| plugin.name == name)
            .map(|plugin| plugin.open_sessions)
            .unwrap_or(0);
        if open_sessions > 0 {
            return Ok(PluginRemoval::NeedsConfirmation { open_sessions });
        }
    }
    store_of(&state)
        .theirs()
        .remove(&name)
        .await
        .map_err(|error| error.to_string())?;
    swap_engines(&state);
    Ok(PluginRemoval::Removed {
        overview: overview(&state).await,
    })
}

/// The three numbers of a version the store wrote (always `a.b.c`).
fn version_numbers(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split('.').map(|part| part.parse::<u32>().ok());
    let numbers = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(numbers)
}

/// What sends an install's progress: the bytes received so far.
type Report = Arc<dyn Fn(u64) + Send + Sync>;

/// Notes how much of the install `install` of `name` has arrived, and tells every view.
fn progress_reporter(app: AppHandle, name: String, install: u32) -> Report {
    Arc::new(move |received| {
        let state = app.state::<AppState>();
        state.plugins.progress(&name, install, received);
        install_changed(&app, &state.plugins, &name);
    })
}

/// A transport that reports how many bytes arrived: the install's progress. It moves the
/// bytes as they are.
struct Counting {
    inner: HttpsTransport,
    report: Report,
}

#[async_trait::async_trait]
impl Transport for Counting {
    async fn get(&self, address: &str) -> Result<Box<dyn Body>, TransportError> {
        let inner = self.inner.get(address).await?;
        (self.report)(0);
        Ok(Box::new(CountingBody {
            inner,
            report: self.report.clone(),
            received: 0,
            reported: 0,
        }))
    }
}

struct CountingBody {
    inner: Box<dyn Body>,
    report: Report,
    received: u64,
    reported: u64,
}

/// How many bytes between two progress events.
const REPORT_EVERY: u64 = 256 << 10;

#[async_trait::async_trait]
impl Body for CountingBody {
    fn address(&self) -> String {
        self.inner.address()
    }

    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let chunk = self.inner.chunk().await?;
        match &chunk {
            Some(bytes) => {
                self.received = self.received.saturating_add(bytes.len() as u64);
                if self.received - self.reported >= REPORT_EVERY {
                    self.reported = self.received;
                    (self.report)(self.received);
                }
            }
            None if self.reported != self.received => {
                self.reported = self.received;
                (self.report)(self.received);
            }
            None => {}
        }
        Ok(chunk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUCKDB: &str = "spelaiondb-plugin-duckdb";

    fn downloading(install: u32, received: f64, total: f64) -> PluginInstall {
        PluginInstall::Downloading {
            install,
            received,
            total,
        }
    }

    #[test]
    fn an_install_downloads_then_is_idle_or_failed_with_why() {
        let ops = PluginOps::default();
        assert_eq!(ops.of(DUCKDB), PluginInstall::Idle);
        ops.begin(DUCKDB, 1, 1000);
        assert_eq!(ops.of(DUCKDB), downloading(1, 0.0, 1000.0));
        ops.progress(DUCKDB, 1, 400);
        assert_eq!(ops.of(DUCKDB), downloading(1, 400.0, 1000.0));
        // Another plugin is not touched.
        assert_eq!(ops.of("spelaiondb-plugin-mysql"), PluginInstall::Idle);
        ops.end(DUCKDB, 1, &Ok(()));
        assert_eq!(ops.of(DUCKDB), PluginInstall::Idle);

        ops.begin(DUCKDB, 2, 1000);
        ops.end(DUCKDB, 2, &Err("the archive's SHA-256 is wrong".into()));
        assert_eq!(
            ops.of(DUCKDB),
            PluginInstall::Failed {
                why: "the archive's SHA-256 is wrong".into()
            }
        );
        // Progress of an install that ended changes nothing.
        ops.progress(DUCKDB, 2, 999);
        assert!(matches!(ops.of(DUCKDB), PluginInstall::Failed { .. }));
        // The next install starts clean.
        ops.begin(DUCKDB, 3, 1000);
        assert_eq!(ops.of(DUCKDB), downloading(3, 0.0, 1000.0));
    }

    #[test]
    fn a_later_install_is_never_overwritten_by_an_earlier_one_ending() {
        let ops = PluginOps::default();
        ops.begin(DUCKDB, 1, 10);
        ops.begin(DUCKDB, 2, 10);
        ops.progress(DUCKDB, 1, 9);
        assert_eq!(ops.of(DUCKDB), downloading(2, 0.0, 10.0));
        ops.end(DUCKDB, 1, &Err("stopped".into()));
        assert_eq!(ops.of(DUCKDB), downloading(2, 0.0, 10.0));
        ops.end(DUCKDB, 1, &Ok(()));
        assert_eq!(ops.of(DUCKDB), downloading(2, 0.0, 10.0));
    }

    #[test]
    fn failures_are_forgotten_once_things_change_and_a_download_is_not() {
        let ops = PluginOps::default();
        ops.begin(DUCKDB, 7, 10);
        ops.begin("spelaiondb-plugin-mysql", 8, 10);
        ops.end(DUCKDB, 7, &Err("installing it was stopped".into()));
        ops.forget_failures();
        assert_eq!(ops.of(DUCKDB), PluginInstall::Idle);
        assert_eq!(
            ops.of("spelaiondb-plugin-mysql"),
            downloading(8, 0.0, 10.0),
            "a download running is not forgotten"
        );
    }
}
