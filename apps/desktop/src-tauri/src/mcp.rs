//! The MCP server page (ADR-033): a thin adapter. The core finds the clients' files, computes
//! and writes the change, and decides what agents may do; these commands only carry it.

use std::path::PathBuf;

use spelaion_config::TrustStore;
use spelaion_config::mcp_clients::{
    self, CliSource, ClientEnv, InstallState, McpClient, Os, ServerCommand,
};
use tauri::State;

use crate::dto::*;
use crate::{AppState, CmdResult};

/// The `spelaiondb` the clients will run: `SPELAION_CLI`, the one installed next to the app,
/// or the first on the PATH.
fn cli() -> Option<(PathBuf, CliSource)> {
    let exe = std::env::current_exe().ok();
    let chosen = std::env::var_os(mcp_clients::CLI_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    mcp_clients::find_cli(
        exe.as_deref(),
        chosen.as_deref(),
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("APPIMAGE").is_some(),
        Os::current(),
    )
}

fn env() -> CmdResult<ClientEnv> {
    ClientEnv::system().ok_or_else(|| "the home folder cannot be found".to_string())
}

fn client(id: &str) -> CmdResult<McpClient> {
    McpClient::parse(id).ok_or_else(|| format!("unknown AI client {id:?}"))
}

/// The command to install: only with a `spelaiondb` that exists.
fn installable() -> CmdResult<ServerCommand> {
    cli()
        .map(|(path, _)| ServerCommand::new(&path))
        .ok_or_else(|| "the spelaiondb command was not found".to_string())
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> CmdResult<T> + Send + 'static,
) -> CmdResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}

/// Every AI client: its file, whether our server is there, and its manual configuration.
#[tauri::command]
#[specta::specta]
pub async fn mcp_clients() -> CmdResult<McpClientsItem> {
    blocking(|| {
        let env = env()?;
        let found = cli();
        // Without the binary, the manual configuration names the bare command.
        let command = found.as_ref().map_or_else(
            || ServerCommand::new(std::path::Path::new("spelaiondb")),
            |(path, _)| ServerCommand::new(path),
        );
        let clients = McpClient::ALL
            .into_iter()
            .map(|client| {
                let path = mcp_clients::config_path(client, &env);
                let (state, problem) = match mcp_clients::install_state(client, &path, &command) {
                    InstallState::NoFile => (McpInstallStateItem::NoFile, None),
                    InstallState::NotInstalled => (McpInstallStateItem::NotInstalled, None),
                    InstallState::Installed => (McpInstallStateItem::Installed, None),
                    InstallState::Different => (McpInstallStateItem::Different, None),
                    InstallState::Unreadable(why) => (McpInstallStateItem::Unreadable, Some(why)),
                };
                McpClientItem {
                    id: client.id().into(),
                    name: client.name().into(),
                    path: path.display().to_string(),
                    state,
                    problem,
                    manual_config: mcp_clients::manual_config(client, &command),
                    manual_language: match client {
                        McpClient::Codex => McpConfigLanguageItem::Toml,
                        McpClient::ClaudeDesktop
                        | McpClient::ClaudeCode
                        | McpClient::Cursor
                        | McpClient::Windsurf
                        | McpClient::Antigravity => McpConfigLanguageItem::Json,
                    },
                    add_command: (client == McpClient::ClaudeCode)
                        .then(|| mcp_clients::claude_code_command(&command, Os::current())),
                }
            })
            .collect();
        Ok(McpClientsItem {
            cli: found.map(|(path, source)| McpCliItem {
                path: path.display().to_string(),
                source: source.into(),
            }),
            clients,
        })
    })
    .await
}

/// The change that installs our server in one client, to show before writing it.
#[tauri::command]
#[specta::specta]
pub async fn mcp_plan_install(client_id: String) -> CmdResult<McpInstallPlanItem> {
    blocking(move || {
        let client = client(&client_id)?;
        let path = mcp_clients::config_path(client, &env()?);
        let plan =
            mcp_clients::plan_install(client, &path, &installable()?).map_err(|e| e.to_string())?;
        Ok(McpInstallPlanItem {
            client: client.id().into(),
            path: path.display().to_string(),
            creates: plan.before.is_none(),
            fingerprint: plan.fingerprint(),
            diff: plan.diff().into_iter().map(McpDiffLineItem::from).collect(),
        })
    })
    .await
}

/// Writes the change the user was shown, and nothing else (the core checks the fingerprint).
#[tauri::command]
#[specta::specta]
pub async fn mcp_apply_install(
    client_id: String,
    fingerprint: String,
) -> CmdResult<McpInstallDoneItem> {
    blocking(move || {
        let client = client(&client_id)?;
        let path = mcp_clients::config_path(client, &env()?);
        let backup = mcp_clients::apply_shown(client, &path, &installable()?, &fingerprint)
            .map_err(|e| e.to_string())?;
        Ok(McpInstallDoneItem {
            backup: backup.map(|b| b.display().to_string()),
        })
    })
    .await
}

#[tauri::command]
#[specta::specta]
pub async fn mcp_settings(state: State<'_, AppState>) -> CmdResult<McpSettingsItem> {
    let (read_only, format) = state
        .history
        .mcp_settings()
        .await
        .map_err(|e| e.to_string())?;
    Ok(McpSettingsItem {
        read_only,
        output_format: format.into(),
    })
}

/// Read-only mode for every agent (ADR-033): `spelaiondb mcp` reads it on every call.
#[tauri::command]
#[specta::specta]
pub async fn set_mcp_read_only(read_only: bool, state: State<'_, AppState>) -> CmdResult<()> {
    state
        .history
        .set_mcp_read_only(read_only)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
#[specta::specta]
pub async fn set_mcp_output_format(
    format: ToolOutputItem,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .history
        .set_mcp_output_format(format.into())
        .await
        .map_err(|e| e.to_string())
}

/// What agents can do on each connection, as the core decides it.
#[tauri::command]
#[specta::specta]
pub async fn mcp_access(state: State<'_, AppState>) -> CmdResult<Vec<McpAccessItem>> {
    state.reload().await;
    let (all_read_only, _) = state
        .history
        .mcp_settings()
        .await
        .map_err(|e| e.to_string())?;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let config = state.config.read().await;
    let trusted = config
        .workspace_root
        .as_deref()
        .is_some_and(|root| trust.is_trusted(root));
    let types = spelaion_config::ConnectionTypes::load(&config, &trust);
    Ok(config
        .connections
        .iter()
        .map(|entry| {
            McpAccessItem::new(
                entry,
                spelaion_core::agent_view(entry, trusted, all_read_only),
                entry
                    .file
                    .env
                    .as_deref()
                    .and_then(|env| types.get(env))
                    .map(ConnectionTypeItem::from),
            )
        })
        .collect())
}

/// The MCP activity the filter keeps, newest first (ADR-033); secrets masked by the core.
#[tauri::command]
#[specta::specta]
pub async fn audit_events(
    filter: AuditFilterItem,
    offset: u32,
    limit: u32,
    state: State<'_, AppState>,
) -> CmdResult<AuditPageItem> {
    let (records, total) = state
        .history
        .audit_events(filter.into(), limit.min(1000) as usize, offset as usize)
        .await
        .map_err(|e| e.to_string())?;
    Ok(AuditPageItem {
        events: records.into_iter().map(AuditEventItem::from).collect(),
        total,
    })
}

#[tauri::command]
#[specta::specta]
pub async fn audit_tools(state: State<'_, AppState>) -> CmdResult<Vec<String>> {
    state.history.audit_tools().await.map_err(|e| e.to_string())
}

/// The latest `spelaiondb mcp` processes and their calls.
#[tauri::command]
#[specta::specta]
pub async fn audit_sessions(state: State<'_, AppState>) -> CmdResult<Vec<AuditSessionItem>> {
    state
        .history
        .audit_sessions(200)
        .await
        .map(|sessions| sessions.into_iter().map(AuditSessionItem::from).collect())
        .map_err(|e| e.to_string())
}

/// Writes the activity the filter keeps to a file the user chose; the core masks secrets and
/// writes it. Returns how many events were written.
#[tauri::command]
#[specta::specta]
pub async fn export_audit(
    filter: AuditFilterItem,
    format: AuditFormatItem,
    path: String,
    state: State<'_, AppState>,
) -> CmdResult<u32> {
    let entries = state
        .history
        .audit_all(filter.into())
        .await
        .map_err(|e| e.to_string())?;
    let format = match format {
        AuditFormatItem::Csv => spelaion_core::audit::AuditFormat::Csv,
        AuditFormatItem::Json => spelaion_core::audit::AuditFormat::Json,
    };
    let count = entries.len();
    tokio::task::spawn_blocking(move || {
        spelaion_core::audit::export_audit(&entries, format, std::path::Path::new(&path))
            .map_err(|e| format!("could not write {path}: {e}"))
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Empties the MCP activity log, leaving a record that it was cleared (done by the core).
#[tauri::command]
#[specta::specta]
pub async fn clear_audit(state: State<'_, AppState>) -> CmdResult<u32> {
    state
        .history
        .clear_audit()
        .await
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
        .map_err(|e| e.to_string())
}
