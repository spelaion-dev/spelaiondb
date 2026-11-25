//! Importing connections from other clients (DBeaver, DataGrip, TablePlus, MySQL Workbench, pgAdmin, Beekeeper Studio…): a thin adapter.
//! The core (`spelaion-config`'s `import.rs` and each client's reader) reads the client's files,
//! finds the connections that already exist and writes the drafts; these commands carry the
//! user's choices to it and its answers back. No secret ever travels to the UI: the files are
//! read again on import, so passwords stay in Rust on their way to the keychain. The one secret
//! that comes from the UI is the password of an encrypted export (TablePlus's), typed by the
//! user and passed on to the core with each read.

use spelaion_desktop_macros::command;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_config::{
    Config, DuplicateAction, ImportClient, ImportPlaces, ImportRead, ImportReadError,
    ImportSourceState, ImportedConnection, SavedSecrets, Source, SystemBackend, TargetFolder,
};
use spelaion_driver_api::Secret;
use tauri::State;

use crate::dto::TlsModeItem;
use crate::{AppState, CmdResult, check_setup_before_saving, source_of};

/// Each connection's startup SQL checked by the core before it is shown or saved (SDB-104,
/// ADR-037): where the read-only barrier applies, what could write is taken out and listed.
fn vet_setups(
    state: &AppState,
    config: &Config,
    connections: &mut [ImportedConnection],
) -> CmdResult<()> {
    let trust = spelaion_config::TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let types = spelaion_config::ConnectionTypes::load(config, &trust);
    for connection in connections {
        let kind = types.resolve(Some(connection.env.as_str()));
        spelaion_core::vet_imported_setup(connection, &kind);
    }
    Ok(())
}

fn client_named(name: &str) -> CmdResult<ImportClient> {
    match name {
        "dbeaver" => Ok(ImportClient::Dbeaver),
        "datagrip" => Ok(ImportClient::Datagrip),
        "tableplus" => Ok(ImportClient::Tableplus),
        "heidisql" => Ok(ImportClient::Heidisql),
        "workbench" => Ok(ImportClient::Workbench),
        "pgadmin" => Ok(ImportClient::Pgadmin),
        "beekeeper" => Ok(ImportClient::Beekeeper),
        other => Err(format!("unknown client: {other}")),
    }
}

/// Reads a client's connections off the UI thread, with the passwords it saved in its files
/// (the scan shows whether one comes with each connection; the import stores them). The core
/// decides where the client is read from (`spelaion-config`'s `import_sources.rs`).
async fn read_blocking(
    client: ImportClient,
    path: Option<String>,
    password: Option<String>,
) -> Result<ImportRead, ImportReadError> {
    let password = password.filter(|p| !p.is_empty()).map(Secret::new);
    let chosen = path.filter(|p| !p.trim().is_empty()).map(PathBuf::from);
    tokio::task::spawn_blocking(move || {
        spelaion_config::read_import(
            &ImportPlaces::this_system(),
            client,
            chosen.as_deref(),
            password.as_ref(),
            SavedSecrets::Read,
        )
    })
    .await
    .map_err(|e| ImportReadError::Other(e.to_string()))?
}

/// What the import's list shows for a client before it is picked (SDB-446), as the core decided
/// it (`ImportSourceState`). A folder or file the test chose stands for the client's own place.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ClientSourceItem {
    /// Connections found where the client keeps them: how many, and where.
    Found { place: String, count: u32 },
    /// The client keeps nothing where it would on this computer.
    NotFound,
    /// Nothing this computer reads on its own: an export file is needed.
    NeedsFile,
    /// An encrypted export: counted once opened with its password.
    Encrypted { place: String },
    /// Reading failed: why (the other clients are not held).
    Failed { reason: String },
}

impl From<ImportSourceState> for ClientSourceItem {
    fn from(state: ImportSourceState) -> Self {
        match state {
            ImportSourceState::Found { place, count } => Self::Found {
                place,
                count: u32::try_from(count).unwrap_or(u32::MAX),
            },
            ImportSourceState::NotFound => Self::NotFound,
            ImportSourceState::NeedsFile => Self::NeedsFile,
            ImportSourceState::Encrypted { place } => Self::Encrypted { place },
            ImportSourceState::Failed { reason } => Self::Failed { reason },
        }
    }
}

/// What a client has on this computer, for the import's list (SDB-446): counted by the core
/// without reading any saved password, keychain or encrypted export, off the UI thread. The page
/// asks for every client at once and fills each row as it answers.
#[command]
pub async fn client_import_source(
    client: String,
    path: Option<String>,
) -> CmdResult<ClientSourceItem> {
    let client = client_named(&client)?;
    let chosen = path.filter(|p| !p.trim().is_empty()).map(PathBuf::from);
    let state = tokio::task::spawn_blocking(move || {
        spelaion_config::import_source(&ImportPlaces::this_system(), client, chosen.as_deref())
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(state.into())
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
    pub engine_kind: Option<String>,
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
    /// The session setup it brings (SQL run on every new session, ADR-037), already checked by
    /// the core; empty for none.
    pub setup: String,
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
        engine_kind: c.engine_kind.clone(),
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
        setup: c.session.setup.clone(),
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
    /// Nothing was read: nothing was chosen, and the client keeps nothing this system can read
    /// on its own, so the user chooses a folder or file first.
    pub needs_choice: bool,
    /// At least one connection can be imported with a saved password.
    pub can_save_passwords: bool,
    /// The client keeps its connections somewhere this system reads when nothing is chosen
    /// (its folders, or HeidiSQL's registry on Windows), so the UI can offer to go back to it.
    pub default_source: bool,
}

/// The connections of a client under a folder or in a file (where the client keeps them if
/// none), with the connections each would duplicate; without secrets. An encrypted export
/// (TablePlus's) is opened with `file_password`; without it, or with a wrong one, the scan
/// says so instead of failing.
#[command]
pub async fn scan_connections(
    client: String,
    path: Option<String>,
    file_password: Option<String>,
    repository: Option<String>,
    state: State<'_, AppState>,
) -> CmdResult<ImportScan> {
    let client = client_named(&client)?;
    let default_source = ImportPlaces::this_system().has_default(client);
    let shown = path.clone().unwrap_or_default();
    let unread = |needs_password: bool, wrong_password: bool, needs_choice: bool| ImportScan {
        dir: shown.clone(),
        connections: Vec::new(),
        needs_password,
        wrong_password,
        needs_choice,
        can_save_passwords: false,
        default_source,
    };
    let ImportRead {
        place,
        connections: mut found,
    } = match read_blocking(client, path, file_password).await {
        Ok(read) => read,
        Err(ImportReadError::NeedsPassword) => return Ok(unread(true, false, false)),
        Err(ImportReadError::WrongPassword) => return Ok(unread(true, true, false)),
        Err(ImportReadError::NoDefault) => return Ok(unread(false, false, true)),
        Err(e @ (ImportReadError::NotFound(_) | ImportReadError::Other(_))) => {
            return Err(e.to_string());
        }
    };
    // Duplicates are looked for in the folder they would go into (ADR-075, ADR-039).
    let config = state.folder_config(repository.as_deref()).await?;
    vet_setups(&state, &config, &mut found)?;
    Ok(ImportScan {
        dir: place,
        connections: found.iter().map(|c| item(&config, c)).collect(),
        needs_password: false,
        wrong_password: false,
        needs_choice: false,
        can_save_passwords: found
            .iter()
            .any(ImportedConnection::has_importable_password),
        default_source,
    })
}

/// One connection to import, and what to do if it already exists.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportChoice {
    pub id: String,
    pub action: ImportAction,
}

/// What to do with a connection that already exists. Any other word is refused when the
/// command is called, never read as one of these (SDB-376).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ImportAction {
    /// Import it as a copy when the name is taken.
    Import,
    Replace,
    Skip,
}

impl From<ImportAction> for DuplicateAction {
    fn from(action: ImportAction) -> Self {
        match action {
            ImportAction::Import => Self::Import,
            ImportAction::Replace => Self::Replace,
            ImportAction::Skip => Self::Skip,
        }
    }
}

/// Where the connections go.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportTarget {
    pub personal: bool,
    /// The open folder repository connections go into (its root, ADR-075); none: the shown
    /// one. Duplicates are looked for there.
    #[serde(default)]
    pub repository: Option<String>,
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
    /// Imported or replaced connections that brought setup statements (SDB-99): SQL that runs on
    /// every new session, which the user should know about.
    pub with_setup: Vec<String>,
}

/// Imports the chosen connections (and their SSH tunnels) as SpelaionDB files. The client's
/// files are read again here, so passwords never leave Rust: with `save_passwords` the ones
/// the client saved go to the keychain (those it keeps in the system keychain are only read
/// now), otherwise they are asked when connecting.
#[command]
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
            .map(|c| DuplicateAction::from(c.action))
    };
    let save_passwords = target.save_passwords;
    let found = read_blocking(client, path, file_password)
        .await
        .map_err(|e| e.to_string())?
        .connections;
    let mut chosen: Vec<ImportedConnection> = found
        .into_iter()
        .filter(|c| action_of(&c.id).is_some_and(|a| a != DuplicateAction::Skip))
        .collect();
    let repository = target.repository.clone();
    vet_setups(
        &state,
        &*state.folder_config(repository.as_deref()).await?,
        &mut chosen,
    )?;
    let mut report = ImportReport {
        imported: Vec::new(),
        replaced: Vec::new(),
        tunnels: Vec::new(),
        skipped: Vec::new(),
        password_errors: Vec::new(),
        with_setup: Vec::new(),
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
        let brings_setup = !plan.connection.session.setup.is_empty();
        let outcome = import_one(
            &state,
            Destination {
                source,
                repository: repository.as_deref(),
            },
            client,
            connection,
            plan,
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
                if brings_setup {
                    report.with_setup.push(name.clone());
                }
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
/// Where an imported connection goes: personal or a repository's, and which open folder's
/// (ADR-075; `None`: the shown one).
#[derive(Clone, Copy)]
struct Destination<'a> {
    source: Source,
    repository: Option<&'a str>,
}

async fn import_one(
    state: &AppState,
    destination: Destination<'_>,
    client: ImportClient,
    connection: &ImportedConnection,
    plan: spelaion_config::ImportPlan,
    action: DuplicateAction,
    tunnel_name: &str,
) -> Result<Done, String> {
    let Destination { source, repository } = destination;
    let mut created_tunnel = false;
    if let Some((tunnel, passwords)) = &plan.tunnel {
        let exists = state
            .folder_config(repository)
            .await?
            .tunnels
            .iter()
            .any(|t| t.source == source && t.name == tunnel_name);
        if !exists {
            let config = state.folder_config(repository).await?;
            let file = tunnel.to_file(&config).map_err(|e| e.to_string())?;
            let name = spelaion_config::add_tunnel(&config, tunnel, passwords, &SystemBackend)
                .map_err(|e| e.to_string())?;
            if source == Source::Workspace {
                state.change_trust(|trust| trust.accept_tunnel_write(&config, &name, &file))?;
            }
            created_tunnel = true;
        }
        state.reload().await;
    }
    if action == DuplicateAction::Replace {
        let replaced = {
            let config = state.folder_config(repository).await?;
            match spelaion_config::find_duplicate(&config, connection, source) {
                Some(existing) => {
                    let id = existing.id.clone();
                    let name = existing.name();
                    let draft = spelaion_config::replacement_draft(existing, &plan.connection);
                    // As the form checks a setup before saving: the type or read-only mode of
                    // the connection it replaces may restrict it further.
                    check_setup_before_saving(
                        state,
                        &config,
                        &draft.to_file(&config).map_err(|e| e.to_string())?,
                    )?;
                    let written = spelaion_config::update_connection(
                        &config,
                        &id,
                        &draft,
                        plan.password.as_ref(),
                        &SystemBackend,
                    )
                    .map_err(|e| e.to_string())?;
                    if source == Source::Workspace {
                        state.change_trust(|trust| {
                            trust.accept_connection_edit(&config, &id, &written)
                        })?;
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
            let config = state.folder_config(repository).await?;
            let file = draft.to_file(&config).map_err(|e| e.to_string())?;
            check_setup_before_saving(state, &config, &file)?;
            let result = spelaion_config::add_connection(
                &config,
                &draft,
                plan.password.as_ref(),
                &SystemBackend,
            );
            if let Ok(id) = &result
                && source == Source::Workspace
            {
                state.change_trust(|trust| trust.accept_connection_write(&config, id, &file))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(action: &str) -> Result<ImportChoice, serde_json::Error> {
        serde_json::from_str(&format!(r#"{{"id": "a", "action": "{action}"}}"#))
    }

    #[test]
    fn each_action_reads_as_itself() {
        for (text, action) in [
            ("import", DuplicateAction::Import),
            ("replace", DuplicateAction::Replace),
            ("skip", DuplicateAction::Skip),
        ] {
            let read = choice(text).expect("a known action");
            assert_eq!(DuplicateAction::from(read.action), action, "{text}");
        }
    }

    /// An action this build does not know (a typo, a newer page) is refused, never taken for
    /// "import".
    #[test]
    fn an_unknown_action_is_refused() {
        for text in ["Replace", "overwrite", ""] {
            assert!(choice(text).is_err(), "{text:?}");
        }
    }
}
