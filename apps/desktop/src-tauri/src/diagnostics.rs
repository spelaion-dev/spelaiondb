//! "Copy diagnostic info" and a failed screen's "Copy report" (SDB-398): this adapter gathers
//! what the report is made of (the version, the plugins, the engines in use, the log) and the
//! core writes and scrubs it (`spelaion_core::diagnostics`). The page only copies the text the
//! core returns. Nothing is sent anywhere; the issue page is opened only on the user's click,
//! at a fixed address that carries no text of the report.

use serde::Deserialize;
use specta::Type;
use spelaion_core::AppLog;
use spelaion_core::diagnostics::{
    DiagnosticsInput, LOG_LINES, PluginLine, ScreenFailure, Scrubber, log_tail, report,
};
use spelaion_desktop_macros::command;
use tauri::State;

use crate::plugins::{PluginSource, overview, sessions_by_engine};
use crate::{AppState, CmdResult};

/// The new-issue page with the bug report form: a fixed address, never with report text in it.
const NEW_ISSUE_PAGE: &str =
    "https://github.com/spelaion-dev/spelaiondb/issues/new?template=bug_report.yml";

/// A screen that failed, as the page's error boundary saw it.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ScreenFailureItem {
    /// Which screen: `explorer`, `tab`, `settings`…
    pub screen: String,
    /// The error's name and message.
    pub error: String,
    /// The components' stack.
    pub stack: String,
}

/// The diagnostic report, scrubbed by the core: the app's version and system, its plugins and
/// builds, the engines in use (never a connection's name or host) and the app log's last lines,
/// and the failed screen when there is one.
#[command]
pub async fn diagnostic_info(
    failure: Option<ScreenFailureItem>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let plugins = overview(&state)
        .await
        .plugins
        .into_iter()
        .map(|plugin| {
            let state = match (&plugin.in_use, &plugin.missing, &plugin.message) {
                (Some(build), _, _) => match build.source {
                    PluginSource::Shipped => "in use (came with the app)".to_string(),
                    PluginSource::Installed => {
                        "in use (installed from the Plugins page)".to_string()
                    }
                },
                (None, Some(_), _) => "the build that comes with the app is missing".to_string(),
                (None, None, Some(message)) => message.clone(),
                (None, None, None) => "not installed".to_string(),
            };
            PluginLine {
                name: plugin.name,
                version: plugin.in_use.map(|build| build.version),
                state,
            }
        })
        .collect();
    let mut engines_in_use: Vec<(String, u32)> =
        sessions_by_engine(&state).await.into_iter().collect();
    engines_in_use.sort();
    let input = DiagnosticsInput {
        app_version: app.package_info().version.to_string(),
        plugins,
        engines_in_use,
        failure: failure.map(|failure| ScreenFailure {
            screen: failure.screen,
            error: failure.error,
            stack: failure.stack,
        }),
    };
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(std::path::PathBuf::from);
    let user_name = std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).ok();
    let config = state.config.read().await;
    let scrubber = Scrubber::new(&config, home.as_deref(), user_name.as_deref());
    let log = log_tail(&AppLog::in_user_dir(&state.user_dir), LOG_LINES);
    Ok(report(&input, &log, &scrubber))
}

/// Opens the new-issue page in the system's browser, on the user's click. Only that fixed
/// address can be opened, and it carries nothing of the report.
#[command]
pub fn open_issue_page() -> CmdResult<()> {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(program)
        .arg(NEW_ISSUE_PAGE)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not open the browser: {e}; the page is {NEW_ISSUE_PAGE}"))?;
    // The opener may stay with the browser it started: reap it without waiting here.
    std::thread::spawn(move || child.wait());
    Ok(())
}
