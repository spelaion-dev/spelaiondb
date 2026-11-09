//! Importing connections from other clients (DBeaver, DataGrip, TablePlus…): a thin adapter.
//! The core (`spelaion-config`'s `import.rs` and each client's reader) reads the client's files,
//! finds the connections that already exist and writes the drafts; these commands carry the
//! user's choices to it and its answers back. No secret ever travels to the UI: the files are
//! read again on import, so passwords stay in Rust on their way to the keychain. The one secret
//! that comes from the UI is the password of an encrypted export (TablePlus's), typed by the
//! user and passed on to the core with each read.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_config::{
    Config, DuplicateAction, ImportClient, ImportedConnection, Source, SystemBackend,
    TableplusError, TargetFolder, TrustStore,
};
use spelaion_driver_api::Secret;
use tauri::State;

use crate::dto::TlsModeItem;
use crate::{AppState, CmdResult, source_of};

fn client_named(name: &str) -> CmdResult<ImportClient> {
    match name {
        "dbeaver" => Ok(ImportClient::Dbeaver),
        "datagrip" => Ok(ImportClient::Datagrip),
        "tableplus" => Ok(ImportClient::Tableplus),
        other => Err(format!("unknown client: {other}")),
    }
}

/// Where JetBrains' configuration folders are: `SPELAION_JETBRAINS_DIR` when set (the smoke
/// test points it at its own files, never the user's), otherwise the system's.
fn jetbrains_dir() -> Option<PathBuf> {
    std::env::var_os("SPELAION_JETBRAINS_DIR")
        .map(PathBuf::from)
        .or_else(spelaion_config::jetbrains_dir)
}

/// Where TablePlus keeps its connections: `SPELAION_TABLEPLUS_DIR` when set (the smoke test
/// points it at the repository's fixtures, never the user's), otherwise the system's.
fn tableplus_dir() -> Option<PathBuf> {
    std::env::var_os("SPELAION_TABLEPLUS_DIR")
        .map(PathBuf::from)
        .or_else(spelaion_config::tableplus_dir)
}

/// Why a client's files could not be read.
enum ReadError {
    /// An encrypted export without its password (`wrong`: the one given does not open it).
    Password {
        wrong: bool,
    },
    Other(String),
}

impl From<&str> for ReadError {
    fn from(message: &str) -> Self {
        ReadError::Other(message.to_string())
    }
}

impl From<ReadError> for String {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::Password { wrong: false } => "the file needs its export password".into(),
            ReadError::Password { wrong: true } => {
                "the password does not open the export file".into()
            }
            ReadError::Other(message) => message,
        }
    }
}

fn other(e: impl std::fmt::Display) -> ReadError {
    ReadError::Other(e.to_string())
}

/// Reads a client's connections from the folder or file chosen, or from where the client keeps
/// them on this system. `password` opens an encrypted export.
fn read(
    client: ImportClient,
    path: Option<String>,
    password: Option<Secret>,
) -> Result<(PathBuf, Vec<ImportedConnection>), ReadError> {
    let chosen = path.filter(|p| !p.trim().is_empty()).map(PathBuf::from);
    match client {
        ImportClient::Dbeaver => {
            let dir = match chosen {
                Some(dir) => dir,
                None => spelaion_config::dbeaver_workspace()
                    .ok_or("DBeaver's folder is unknown on this system")?,
            };
            let found = spelaion_config::read_dbeaver(&dir).map_err(other)?;
            Ok((dir, found))
        }
        ImportClient::Datagrip => {
            let jetbrains = jetbrains_dir();
            let config_dirs = jetbrains
                .as_deref()
                .map(spelaion_config::datagrip_dirs)
                .unwrap_or_default();
            let dir = match chosen {
                Some(dir) => dir,
                None => jetbrains.ok_or("JetBrains' folder is unknown on this system")?,
            };
            let found = spelaion_config::read_datagrip(&dir, &config_dirs).map_err(other)?;
            Ok((dir, found))
        }
        ImportClient::Tableplus => {
            let dir = match chosen {
                Some(dir) => dir,
                None => tableplus_dir().ok_or("TablePlus's folder is unknown on this system")?,
            };
            let found =
                spelaion_config::read_tableplus(&dir, password.as_ref()).map_err(|e| match e {
                    TableplusError::NeedsPassword(_) => ReadError::Password { wrong: false },
                    TableplusError::WrongPassword(_) => ReadError::Password { wrong: true },
                    TableplusError::NotFound(_) | TableplusError::Unreadable { .. } => other(e),
                })?;
            Ok((dir, found))
        }
    }
}

async fn read_blocking(
    client: ImportClient,
    path: Option<String>,
    password: Option<String>,
) -> Result<(PathBuf, Vec<ImportedConnection>), ReadError> {
    let password = password.filter(|p| !p.is_empty()).map(Secret::new);
    tokio::task::spawn_blocking(move || read(client, path, password))
        .await
        .map_err(other)?
}

/// A connection that already exists, by name.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ExistingItem {
    pub id: String,
    pub name: String,
}

/// A connection found in another client's files. No secret travels: only whether a password
/// comes with it.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportItem {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// `postgres`, `mysql`, `mariadb`, `sqlserver`, `sqlite`, `duckdb`; `None` when SpelaionDB
    /// cannot open it.
    pub engine: Option<String>,
    /// Why it cannot be imported, if it cannot.
    pub unsupported: Option<String>,
    /// `host:port/database`, or the file for SQLite and DuckDB.
    pub target: String,
    pub user: String,
    pub env: String,
    pub has_password: bool,
    /// `user@host:port` of its SSH tunnel.
    pub ssh: Option<String>,
    /// `host:port` of its SOCKS proxy.
    pub proxy: Option<String>,
    /// Its TLS mode, when it sets one.
    pub tls: Option<TlsModeItem>,
    pub read_only: bool,
    /// The connection it would duplicate among the personal connections, and among the
    /// project's.
    pub existing_personal: Option<ExistingItem>,
    pub existing_project: Option<ExistingItem>,
    /// What the client has that SpelaionDB does not import.
    pub unmapped: Vec<String>,
}

fn existing(config: &Config, c: &ImportedConnection, source: Source) -> Option<ExistingItem> {
    spelaion_config::find_duplicate(config, c, source).map(|entry| ExistingItem {
        id: entry.id.to_string(),
        name: entry.name(),
    })
}

fn item(config: &Config, c: &ImportedConnection) -> ImportItem {
    let file = c.engine.as_ref().is_ok_and(|e| e.is_file());
    ImportItem {
        id: c.id.clone(),
        name: c.name.clone(),
        folder: c.folder.clone(),
        engine: c.engine.as_ref().ok().map(|e| e.as_str().to_string()),
        unsupported: c.engine.as_ref().err().cloned(),
        target: if file {
            c.database.clone()
        } else {
            let port = c.port.map(|p| format!(":{p}")).unwrap_or_default();
            let database = if c.database.is_empty() {
                String::new()
            } else {
                format!("/{}", c.database)
            };
            format!("{}{port}{database}", c.host)
        },
        user: c.user.clone(),
        env: c.env.clone(),
        has_password: c.has_password(),
        ssh: c.ssh.as_ref().map(|s| {
            let port = s.port.map(|p| format!(":{p}")).unwrap_or_default();
            format!("{}@{}{port}", s.user, s.host)
        }),
        proxy: c
            .proxy
            .as_ref()
            .map(|p| format!("{}:{}", p.host, p.port.unwrap_or(1080))),
        tls: c.tls.as_ref().map(|t| t.mode.into()),
        read_only: c.read_only,
        existing_personal: existing(config, c, Source::User),
        existing_project: existing(config, c, Source::Workspace),
        unmapped: c.unmapped.clone(),
    }
}

/// What a scan found, and where.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportScan {
    pub dir: String,
    pub connections: Vec<ImportItem>,
    /// The file is an encrypted export: its password is needed to read it.
    pub needs_password: bool,
    /// The password given does not open it.
    pub wrong_password: bool,
}

/// The connections of a client under a folder or in a file (where the client keeps them if
/// none), with the connections each would duplicate; without secrets. An encrypted export
/// (TablePlus's) is opened with `file_password`; without it, or with a wrong one, the scan
/// says so instead of failing.
#[tauri::command]
#[specta::specta]
pub async fn scan_connections(
    client: String,
    path: Option<String>,
    file_password: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<ImportScan> {
    let client = client_named(&client)?;
    let shown = path.clone().unwrap_or_default();
    let (dir, found) = match read_blocking(client, path, file_password).await {
        Ok(read) => read,
        Err(ReadError::Password { wrong }) => {
            return Ok(ImportScan {
                dir: shown,
                connections: Vec::new(),
                needs_password: true,
                wrong_password: wrong,
            });
        }
        Err(e) => return Err(e.into()),
    };
    let config = state.config.read().await;
    Ok(ImportScan {
        dir: dir.display().to_string(),
        connections: found.iter().map(|c| item(&config, c)).collect(),
        needs_password: false,
        wrong_password: false,
    })
}

/// One connection to import, and what to do if it already exists.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportChoice {
    pub id: String,
    /// `import` (as a copy when the name is taken), `replace` or `skip`.
    pub action: String,
}

/// Where the connections go.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportTarget {
    pub personal: bool,
    pub save_passwords: bool,
    /// A folder to put them in; empty for the top level.
    pub folder: String,
    /// Keep the client's folders under it.
    pub keep_client_folders: bool,
}

/// One connection that could not be imported, and why.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ImportSkip {
    pub name: String,
    pub reason: String,
}

/// The outcome of an import: the names given to the new connections (`(DataGrip)` is added
/// when the name was taken), the ones replaced, and those left out.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub replaced: Vec<String>,
    pub tunnels: Vec<String>,
    pub skipped: Vec<ImportSkip>,
    /// Connections imported without the password the client kept in the keychain, and why.
    pub password_errors: Vec<ImportSkip>,
}

/// Imports the chosen connections (and their SSH tunnels) as SpelaionDB files. The client's
/// files are read again here, so passwords never leave Rust: with `save_passwords` the ones
/// the client saved go to the keychain (those it keeps in the system keychain are only read
/// now), otherwise they are asked when connecting.
#[tauri::command]
#[specta::specta]
pub async fn import_connections(
    client: String,
    path: Option<String>,
    file_password: Option<String>,
    choices: Vec<ImportChoice>,
    target: ImportTarget,
    state: State<'_, AppState>,
) -> CmdResult<ImportReport> {
    let client = client_named(&client)?;
    let folder = TargetFolder {
        folder: target.folder.clone(),
        keep_client_folders: target.keep_client_folders,
    };
    folder.check()?;
    let action_of = |id: &str| {
        choices
            .iter()
            .find(|c| c.id == id)
            .map(|c| match c.action.as_str() {
                "replace" => DuplicateAction::Replace,
                "skip" => DuplicateAction::Skip,
                _ => DuplicateAction::Import,
            })
    };
    let save_passwords = target.save_passwords;
    let (_, found) = read_blocking(client, path, file_password).await?;
    let mut chosen: Vec<ImportedConnection> = found
        .into_iter()
        .filter(|c| action_of(&c.id).is_some_and(|a| a != DuplicateAction::Skip))
        .collect();
    let mut report = ImportReport {
        imported: Vec::new(),
        replaced: Vec::new(),
        tunnels: Vec::new(),
        skipped: Vec::new(),
        password_errors: Vec::new(),
    };
    if save_passwords {
        let (read, failed) = tokio::task::spawn_blocking(move || {
            let failed = spelaion_config::read_stored_passwords(&mut chosen, &SystemBackend);
            (chosen, failed)
        })
        .await
        .map_err(|e| e.to_string())?;
        chosen = read;
        report.password_errors = failed
            .into_iter()
            .map(|(name, reason)| ImportSkip { name, reason })
            .collect();
    }
    let source = source_of(target.personal);
    for connection in &chosen {
        let engine = match &connection.engine {
            Ok(engine) => *engine,
            Err(reason) => {
                report.skipped.push(ImportSkip {
                    name: connection.name.clone(),
                    reason: reason.clone(),
                });
                continue;
            }
        };
        let action = action_of(&connection.id).unwrap_or(DuplicateAction::Import);
        // One network profile per bastion and user (and proxy): connections through the same
        // one share it.
        let tunnel_name = connection.tunnel_name().unwrap_or_default();
        let plan = spelaion_config::import_plan(
            connection,
            engine,
            source,
            save_passwords,
            &tunnel_name,
            &folder,
        );
        let outcome = import_one(
            &state,
            client,
            connection,
            plan,
            source,
            action,
            &tunnel_name,
        )
        .await;
        match outcome {
            Ok(Done {
                name,
                replaced,
                created_tunnel,
            }) => {
                if replaced {
                    report.replaced.push(name);
                } else {
                    report.imported.push(name);
                }
                if created_tunnel {
                    report.tunnels.push(tunnel_name);
                }
            }
            Err(reason) => report.skipped.push(ImportSkip {
                name: connection.name.clone(),
                reason,
            }),
        }
    }
    Ok(report)
}

struct Done {
    name: String,
    replaced: bool,
    created_tunnel: bool,
}

/// Creates the tunnel (unless one with its name exists), then the connection: a new one,
/// trying `name (Client)`, `name (Client 2)`… when the name is taken, or the existing one it
/// duplicates, edited in place, for `replace`.
async fn import_one(
    state: &AppState,
    client: ImportClient,
    connection: &ImportedConnection,
    plan: spelaion_config::ImportPlan,
    source: Source,
    action: DuplicateAction,
    tunnel_name: &str,
) -> Result<Done, String> {
    let mut created_tunnel = false;
    if let Some((tunnel, passwords)) = &plan.tunnel {
        let exists = state
            .config
            .read()
            .await
            .tunnels
            .iter()
            .any(|t| t.source == source && t.name == tunnel_name);
        if !exists {
            let config = state.config.read().await;
            let file = tunnel.to_file(&config).map_err(|e| e.to_string())?;
            let name = spelaion_config::add_tunnel(&config, tunnel, passwords, &SystemBackend)
                .map_err(|e| e.to_string())?;
            if source == Source::Workspace {
                TrustStore::load(&state.user_dir)
                    .and_then(|mut trust| trust.accept_tunnel_write(&config, &name, &file))
                    .map_err(|e| e.to_string())?;
            }
            created_tunnel = true;
        }
        state.reload().await;
    }
    if action == DuplicateAction::Replace {
        let replaced = {
            let config = state.config.read().await;
            match spelaion_config::find_duplicate(&config, connection, source) {
                Some(existing) => {
                    let id = existing.id.clone();
                    let name = existing.name();
                    let draft = spelaion_config::replacement_draft(existing, &plan.connection);
                    let written = spelaion_config::update_connection(
                        &config,
                        &id,
                        &draft,
                        plan.password.as_ref(),
                        &SystemBackend,
                    )
                    .map_err(|e| e.to_string())?;
                    if source == Source::Workspace {
                        TrustStore::load(&state.user_dir)
                            .and_then(|mut trust| {
                                trust.accept_connection_edit(&config, &id, &written)
                            })
                            .map_err(|e| e.to_string())?;
                    }
                    Some(name)
                }
                // Nothing to replace any more: it is imported as a new one.
                None => None,
            }
        };
        if let Some(name) = replaced {
            state.reload().await;
            return Ok(Done {
                name,
                replaced: true,
                created_tunnel,
            });
        }
    }
    let mut draft = plan.connection;
    let base = draft.name.clone();
    for name in spelaion_config::import_names(&base, client) {
        draft.name = name;
        let added = {
            let config = state.config.read().await;
            let file = draft.to_file(&config).map_err(|e| e.to_string())?;
            let result = spelaion_config::add_connection(
                &config,
                &draft,
                plan.password.as_ref(),
                &SystemBackend,
            );
            if let Ok(id) = &result
                && source == Source::Workspace
            {
                TrustStore::load(&state.user_dir)
                    .and_then(|mut trust| trust.accept_connection_write(&config, id, &file))
                    .map_err(|e| e.to_string())?;
            }
            result
        };
        match added {
            Ok(_) => {
                state.reload().await;
                return Ok(Done {
                    name: draft.name,
                    replaced: false,
                    created_tunnel,
                });
            }
            Err(e) if e.to_string().contains("already exists") => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("every name tried is taken".into())
}
