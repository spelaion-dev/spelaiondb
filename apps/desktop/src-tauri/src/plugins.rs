//! The Plugins page's commands (ADR-055): a thin adapter over `spelaion_core::plugins`.
//!
//! Every decision is the core's: which build of each plugin is started and what the catalogue
//! offers (`choice::states`), when the catalogue may be read (`fetch::updates_allowed`), what
//! is fetched and from where (`fetch`), what is installed (`installed::PluginStore`). Errors
//! reach the page as the core wrote them.
//!
//! - **No command takes an address, a key or a catalogue's text.** They take a plugin's name,
//!   a version the overview listed, an id the page chose, or the path of a file the user
//!   picked in the app's file dialog. `scripts/plugin-commands.test.mjs` checks the bindings.
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
use spelaion_core::plugins::fetch::{self, Body, Transport, TransportError};
use spelaion_core::plugins::installed::{AppBuild, PluginStore};
use spelaion_core::plugins::pins::Pins;
use spelaion_https::HttpsTransport;
use tauri::{AppHandle, Manager, State};
use tauri_specta::Event as _;
use tokio::sync::Notify;

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
    /// The installs running, by the id the page gave them, to stop them.
    installs: std::sync::Mutex<HashMap<u32, Arc<Notify>>>,
}

impl PluginOps {
    fn start(&self, install: u32) -> Arc<Notify> {
        let stop = Arc::new(Notify::new());
        self.installs
            .lock()
            .expect("lock poisoned")
            .insert(install, stop.clone());
        stop
    }

    fn stop(&self, install: u32) {
        if let Some(stop) = self
            .installs
            .lock()
            .expect("lock poisoned")
            .remove(&install)
        {
            // Kept for the install if it is not waiting yet.
            stop.notify_one();
        }
    }

    fn end(&self, install: u32) {
        self.installs
            .lock()
            .expect("lock poisoned")
            .remove(&install);
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

/// How far an install's download is. Sent a few times a second at most.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct PluginInstallProgress {
    /// The id the page gave `pluginsInstall`.
    pub install: u32,
    pub received: f64,
    /// The archive's size, from the catalogue.
    pub total: f64,
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
        *counts
            .entry(session.open.resolved.engine.as_str().to_string())
            .or_insert(0) += 1;
    }
    counts
}

pub(crate) async fn overview(state: &AppState) -> PluginsOverview {
    let store = store_of(state);
    let catalogue = store.catalogue();
    let shipped = spelaion_core::plugins::shipped_folder();
    let states = choice::states(
        Pins::shipped(),
        shipped.as_deref(),
        Some(&store),
        catalogue.as_ref(),
        &AppBuild::this(),
    );
    let installed = store.installed();
    let sessions = sessions_by_engine(state).await;
    PluginsOverview {
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
                item(plugin, versions, &sessions)
            })
            .collect(),
    }
}

fn item(
    state: &PluginState,
    installed: Vec<String>,
    sessions: &HashMap<String, u32>,
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
/// an update, on the user's click. `install` is an id the page chooses, for
/// `pluginsCancelInstall` and the `PluginInstallProgress` events. The next connection of its
/// engines starts the new build; connections already open keep theirs.
#[command]
pub async fn plugins_install(
    app: AppHandle,
    state: State<'_, AppState>,
    install: u32,
    name: String,
) -> CmdResult<PluginsOverview> {
    let stop = state.plugins.start(install);
    let work = async {
        let transport = HttpsTransport::new().map_err(|error| error.0)?;
        let _changing = state.plugins.changing.lock().await;
        let store = store_of(&state);
        let catalogue = store
            .catalogue()
            .ok_or_else(|| fetch::FetchError::NotOffered(name.clone()).to_string())?;
        let report = progress_reporter(app.clone(), install, &store, &catalogue, &name);
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
        _ = stop.notified() => Err(format!("installing {name} was stopped")),
    };
    state.plugins.end(install);
    result?;
    Ok(overview(&state).await)
}

/// Stops the install `install`, if it runs. Nothing of it is kept.
#[command]
pub fn plugins_cancel_install(state: State<'_, AppState>, install: u32) {
    state.plugins.stop(install);
}

/// Installs a file the user picked in the app's file dialog: a plugin's archive (installed only
/// as the accepted catalogue's entry with its SHA-256) or, for a file whose name ends in
/// `.signed`, a catalogue with its signature (accepted only with our signature, not older than
/// the one kept). The engines are swapped.
#[command]
pub async fn plugins_install_file(
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
    Ok(overview(&state).await)
}

/// Removes the build `version` of the plugin `name` from the user's personal folder (one the
/// overview lists in `installed`). If that build is running, nothing is removed until
/// `confirmed`: removing it stops it, and its sessions are lost (ADR-028). The engines are
/// swapped, so the next connection starts whatever build is left.
#[command]
pub async fn plugins_remove(
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
        let states = choice::states(
            Pins::shipped(),
            spelaion_core::plugins::shipped_folder().as_deref(),
            Some(&store),
            catalogue.as_ref(),
            &AppBuild::this(),
        );
        let open_sessions = states
            .iter()
            .find(|plugin| plugin.name == name)
            .map(|plugin| item(plugin, Vec::new(), &sessions).open_sessions)
            .unwrap_or(0);
        return Ok(PluginRemoval::NeedsConfirmation { open_sessions });
    }
    store
        .remove(&build.name, numbers)
        .await
        .map_err(|error| format!("{name} {version} could not be removed: {error}"))?;
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

/// Sends `PluginInstallProgress` for `install`, against the archive size the catalogue gives
/// for what it offers of `name`.
fn progress_reporter(
    app: AppHandle,
    install: u32,
    store: &PluginStore,
    catalogue: &spelaion_core::plugins::catalogue::Catalogue,
    name: &str,
) -> Report {
    let total = choice::states(
        &Pins::none(),
        None,
        Some(store),
        Some(catalogue),
        &AppBuild::this(),
    )
    .into_iter()
    .find(|state| state.name == name)
    .and_then(|state| state.offered)
    .map(|offered| offered.archive_size as f64)
    .unwrap_or(0.0);
    Arc::new(move |received| {
        let _ = PluginInstallProgress {
            install,
            received: received as f64,
            total,
        }
        .emit(&app);
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
