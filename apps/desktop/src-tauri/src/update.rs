//! Automatic updates (ADR-021): Tauri's updater plugin behind our own commands. Nothing is
//! downloaded or installed until the user asks; the signature is checked by the plugin
//! against the public key in `tauri.conf.json`.
//!
//! On Linux the app only replaces itself when it runs as an AppImage. A `.deb` or `.rpm`
//! install belongs to the system's package manager, and a plain binary (built with
//! `--no-bundle`, unpacked from a tarball, a distribution's own package) is not something the
//! updater knows how to replace: those are told that a new version exists and where it is,
//! and `update_install` refuses them (SDB-198).

use serde::Serialize;
use specta::Type;
use tauri::utils::config::BundleType;
use tauri::{AppHandle, State};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Mutex;

use crate::CmdResult;

/// Where the app looks for `latest.json`: the latest public release (ADR-021). Releases are
/// published as "latest" (not as pre-releases), which this address follows.
const ENDPOINT: &str =
    "https://github.com/spelaion-dev/spelaiondb/releases/latest/download/latest.json";

/// The page of the latest release, for installs the app cannot update itself. Fixed here, so
/// the web view never chooses what is opened.
const RELEASES_PAGE: &str = "https://github.com/spelaion-dev/spelaiondb/releases/latest";

/// Set (to anything) to switch updates off: the app says they are not available and makes
/// no request. For tests and packaged builds whose updates come from elsewhere. It can only
/// switch them off, never point them somewhere else.
const SWITCH_OFF: &str = "SPELAION_NO_UPDATES";

/// The update found by the last check, kept for `update_install`.
#[derive(Default)]
pub struct PendingUpdate(Mutex<Option<Update>>);

/// How this copy of the app gets a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum UpdateMethod {
    /// The app downloads, verifies and installs it itself ("Install and restart"): Windows,
    /// macOS and a Linux AppImage.
    InApp,
    /// Installed from a `.deb` package: the new `.deb` is installed with the package manager.
    Deb,
    /// Installed from an `.rpm` package: the new `.rpm` is installed with the package manager.
    Rpm,
    /// A Linux binary that is none of those: the new version is downloaded by hand.
    ReleasesPage,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateStatus {
    /// Updates are switched off in this run (`SPELAION_NO_UPDATES`).
    Unavailable,
    UpToDate,
    #[serde(rename_all = "camelCase")]
    Available {
        version: String,
        current_version: String,
        notes: Option<String>,
        method: UpdateMethod,
    },
}

/// Decides how a copy of the app is updated. `bundle` is the bundle type Tauri's bundler
/// patched into this binary (`None` when it was built without bundling); `appimage` says
/// whether the AppImage runtime started it (`APPIMAGE` names an existing file).
///
/// On Linux the updater plugin writes the new AppImage over `APPIMAGE`, or over the running
/// executable when that is unset, for any bundle type it does not know: only a binary that is
/// both bundled as an AppImage and running as one may replace itself. Other systems are not
/// changed: their installers are always ones the plugin can run.
fn update_method(linux: bool, bundle: Option<BundleType>, appimage: bool) -> UpdateMethod {
    if !linux {
        return UpdateMethod::InApp;
    }
    match bundle {
        Some(BundleType::AppImage) if appimage => UpdateMethod::InApp,
        Some(BundleType::Deb) => UpdateMethod::Deb,
        Some(BundleType::Rpm) => UpdateMethod::Rpm,
        _ => UpdateMethod::ReleasesPage,
    }
}

/// Why `update_install` refuses an install it cannot replace, or `None` when it can.
fn refusal(method: UpdateMethod) -> Option<String> {
    let how = "download the new version from the releases page";
    match method {
        UpdateMethod::InApp => None,
        UpdateMethod::Deb => Some(format!(
            "this copy of SpelaionDB was installed from a .deb package, so the app does not \
             replace it: {how} and install the .deb with your package manager"
        )),
        UpdateMethod::Rpm => Some(format!(
            "this copy of SpelaionDB was installed from an .rpm package, so the app does not \
             replace it: {how} and install the .rpm with your package manager"
        )),
        UpdateMethod::ReleasesPage => Some(format!(
            "this copy of SpelaionDB is not an AppImage or a package, so the app cannot \
             replace it: {how}"
        )),
    }
}

/// Whether the AppImage runtime started this process: Tauri reads `APPIMAGE` at startup,
/// and it must name an existing file by an absolute path.
fn running_as_appimage(app: &AppHandle) -> bool {
    #[cfg(target_os = "linux")]
    {
        use tauri::Manager;
        app.env().appimage.is_some_and(|path| {
            let path = std::path::Path::new(&path);
            path.is_absolute() && path.is_file()
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = app;
        false
    }
}

fn this_install(app: &AppHandle) -> UpdateMethod {
    update_method(
        cfg!(target_os = "linux"),
        tauri::utils::platform::bundle_type(),
        running_as_appimage(app),
    )
}

#[tauri::command]
#[specta::specta]
pub async fn update_check(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> CmdResult<UpdateStatus> {
    if std::env::var_os(SWITCH_OFF).is_some() {
        return Ok(UpdateStatus::Unavailable);
    }
    let endpoint = ENDPOINT.parse().map_err(|e| format!("{ENDPOINT}: {e}"))?;
    let update = app
        .updater_builder()
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let method = this_install(&app);
    let status = match &update {
        Some(update) => UpdateStatus::Available {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
            method,
        },
        None => UpdateStatus::UpToDate,
    };
    // Only an update this copy can install is kept for `update_install`.
    *pending.0.lock().await = update.filter(|_| method == UpdateMethod::InApp);
    Ok(status)
}

/// Downloads and installs the update the last check found, then restarts the app. Open tabs
/// come back with their SQL, not executed (AGENTS.md). Refused for an install the app cannot
/// replace (see `update_method`), whatever the web view asks.
#[tauri::command]
#[specta::specta]
pub async fn update_install(app: AppHandle, pending: State<'_, PendingUpdate>) -> CmdResult<()> {
    if let Some(reason) = refusal(this_install(&app)) {
        return Err(reason);
    }
    let Some(update) = pending.0.lock().await.take() else {
        return Err("there is no update to install: check for updates first".into());
    };
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())?;
    app.restart()
}

/// Opens the latest release's page in the system's browser, for an install that is updated by
/// hand or by the package manager. Only that fixed address can be opened.
#[tauri::command]
#[specta::specta]
pub fn update_open_releases_page() -> CmdResult<()> {
    open_in_browser(RELEASES_PAGE)
}

#[cfg(target_os = "linux")]
fn open_in_browser(url: &str) -> CmdResult<()> {
    let mut child = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not run xdg-open: {e}; the page is {url}"))?;
    // xdg-open may stay with the browser it started: reap it without waiting here.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Only Linux installs are sent to the releases page (`update_method`).
#[cfg(not(target_os = "linux"))]
fn open_in_browser(url: &str) -> CmdResult<()> {
    Err(format!("open {url} in a browser"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Option<BundleType>; 8] = [
        None,
        Some(BundleType::Deb),
        Some(BundleType::Rpm),
        Some(BundleType::AppImage),
        Some(BundleType::Msi),
        Some(BundleType::Nsis),
        Some(BundleType::App),
        Some(BundleType::Dmg),
    ];

    #[test]
    fn only_an_appimage_running_as_one_replaces_itself_on_linux() {
        for bundle in ALL {
            for appimage in [false, true] {
                let method = update_method(true, bundle.clone(), appimage);
                let expected = bundle == Some(BundleType::AppImage) && appimage;
                assert_eq!(
                    method == UpdateMethod::InApp,
                    expected,
                    "{bundle:?}, APPIMAGE set: {appimage}"
                );
            }
        }
    }

    #[test]
    fn packages_go_to_their_package_manager_whatever_appimage_says() {
        for appimage in [false, true] {
            assert_eq!(
                update_method(true, Some(BundleType::Deb), appimage),
                UpdateMethod::Deb
            );
            assert_eq!(
                update_method(true, Some(BundleType::Rpm), appimage),
                UpdateMethod::Rpm
            );
        }
    }

    #[test]
    fn a_plain_or_extracted_binary_is_sent_to_the_releases_page() {
        // `tauri build --no-bundle`, a tarball, or a distribution's own package: no bundle type,
        // even when something exported `APPIMAGE`.
        assert_eq!(update_method(true, None, false), UpdateMethod::ReleasesPage);
        assert_eq!(update_method(true, None, true), UpdateMethod::ReleasesPage);
        // An AppImage's binary run outside its runtime (`--appimage-extract`).
        assert_eq!(
            update_method(true, Some(BundleType::AppImage), false),
            UpdateMethod::ReleasesPage
        );
    }

    #[test]
    fn other_systems_keep_updating_in_the_app() {
        for bundle in ALL {
            for appimage in [false, true] {
                assert_eq!(
                    update_method(false, bundle.clone(), appimage),
                    UpdateMethod::InApp
                );
            }
        }
    }

    #[test]
    fn install_is_refused_unless_the_app_can_replace_itself() {
        assert_eq!(refusal(UpdateMethod::InApp), None);
        let deb = refusal(UpdateMethod::Deb).unwrap();
        assert!(
            deb.contains(".deb") && deb.contains("package manager"),
            "{deb}"
        );
        let rpm = refusal(UpdateMethod::Rpm).unwrap();
        assert!(
            rpm.contains(".rpm") && rpm.contains("package manager"),
            "{rpm}"
        );
        let plain = refusal(UpdateMethod::ReleasesPage).unwrap();
        assert!(plain.contains("releases page"), "{plain}");
    }

    #[test]
    fn the_method_reaches_the_web_view_in_camel_case() {
        let status = UpdateStatus::Available {
            version: "0.8.0".into(),
            current_version: "0.7.99".into(),
            notes: None,
            method: UpdateMethod::ReleasesPage,
        };
        let json = serde_json::to_value(status).unwrap();
        assert_eq!(json["kind"], "available");
        assert_eq!(json["method"], "releasesPage");
        assert_eq!(serde_json::to_value(UpdateMethod::InApp).unwrap(), "inApp");
    }
}
