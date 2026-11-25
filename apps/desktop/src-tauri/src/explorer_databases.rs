//! The explorer's database level for a PostgreSQL connection that opens several (ADR-074,
//! SDB-504). Which databases there are, which one has a connection of its own and why the list
//! could not be read are the core's answers (`spelaion_core::connection_databases`,
//! `database_owner`); opening one is the core's `open_other_database`, with its refusals. This
//! file only passes them on and keeps the session it opens, as every session is kept.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use specta::Type;
use spelaion_config::{Config, ConnectionKey, SystemBackend, TrustStore, canonical_folder};
use spelaion_core::{ConnectionDatabases, OpenOptions};
use spelaion_desktop_macros::command;
use spelaion_ssh::KnownHosts;
use tauri::{Manager, State};

use crate::dto::ConnectionKeyItem;
use crate::interaction::AppInteraction;
use crate::{AppState, CmdResult, DesktopSession, watch_session};

/// A connection that a database opens only as (ADR-074 point 3).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseOwnerItem {
    pub connection: ConnectionKeyItem,
    pub name: String,
}

/// One database of the connection, its own first.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerDatabaseItem {
    pub name: String,
    /// The connection's own database (`database` in its file).
    pub own: bool,
    /// Another connection has it as its own: its row leads there instead of opening.
    pub owned_by: Option<DatabaseOwnerItem>,
}

/// The databases a connection's explorer shows (`spelaion_core::ConnectionDatabases`).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectionDatabasesItem {
    /// Only its own: the tree has no database level.
    Own,
    /// Several: a database level, its own first.
    Several {
        databases: Vec<ExplorerDatabaseItem>,
    },
    /// Its own only, because the others could not be listed: a database level with its own,
    /// and why.
    OwnOnly {
        databases: Vec<ExplorerDatabaseItem>,
        reason: String,
    },
}

/// The configuration of the open folder a session's connection belongs to (ADR-075): its
/// folder's, or the one shown for a personal connection, which every folder's has. A folder
/// that is not open any more is an error, never another folder's configuration.
async fn folder_config(state: &AppState, folder: Option<&Path>) -> CmdResult<Arc<Config>> {
    let Some(folder) = folder else {
        return Ok(state.config().await);
    };
    state
        .open_configs()
        .await
        .into_iter()
        .find(|config| {
            config
                .workspace_root
                .as_deref()
                .is_some_and(|root| canonical_folder(root) == folder)
        })
        .ok_or_else(|| "the connection's folder is not open any more".to_string())
}

/// The folder of the session `session`'s connection.
async fn session_folder(state: &AppState, session: &str) -> CmdResult<Option<PathBuf>> {
    let sessions = state.sessions.lock().await;
    sessions
        .get(session)
        .map(|s| s.folder.clone())
        .ok_or_else(|| "the connection's explorer session is closed".to_string())
}

/// The databases the explorer shows under the connection whose explorer session is `session`.
#[command]
pub async fn connection_databases(
    session: String,
    state: State<'_, AppState>,
) -> CmdResult<ConnectionDatabasesItem> {
    let open = state.session(&session).await?;
    let own = |name: String| ExplorerDatabaseItem {
        name,
        own: true,
        owned_by: None,
    };
    Ok(match spelaion_core::connection_databases(&open).await {
        ConnectionDatabases::Own(_) => ConnectionDatabasesItem::Own,
        ConnectionDatabases::OwnOnly { database, reason } => ConnectionDatabasesItem::OwnOnly {
            databases: vec![own(database)],
            reason,
        },
        ConnectionDatabases::Several(names) => {
            let folder = session_folder(&state, &session).await?;
            let config = folder_config(&state, folder.as_deref()).await?;
            let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
            let configuration = spelaion_core::Configuration {
                config: &config,
                trust: &trust,
                backend: &SystemBackend,
            };
            // The connection's own database is its saved file's: a tab on another database
            // lists it too, so the tab's picker can go back to it (SDB-505).
            let own_name = config
                .connection(&open.resolved.id)
                .and_then(|entry| entry.file.database.clone())
                .unwrap_or_else(|| open.resolved.params.database.clone());
            let mut names = names;
            if !names.contains(&own_name) {
                names.insert(0, own_name.clone());
            } else if let Some(at) = names.iter().position(|n| *n == own_name) {
                let own = names.remove(at);
                names.insert(0, own);
            }
            let databases = names
                .into_iter()
                .map(|name| {
                    let owned_by = spelaion_core::database_owner(&open, &name, &configuration).map(
                        |(id, name)| DatabaseOwnerItem {
                            connection: ConnectionKeyItem::from(&ConnectionKey::of(&config, &id)),
                            name,
                        },
                    );
                    ExplorerDatabaseItem {
                        own: name == own_name,
                        name,
                        owned_by,
                    }
                })
                .collect();
            ConnectionDatabasesItem::Several { databases }
        }
    })
}

/// Moves the editor tab `session` to `database` of its connection (ADR-074 point 4, SDB-505):
/// on PostgreSQL a session never changes database, so this is a new session on that database,
/// opened through the core (`open_other_database`, with its refusals) with the password typed
/// for the tab, which then takes the tab's place; the old one is closed (its transaction, which
/// the page asked about, rolls back). Refused while a run goes on. Nothing the user ran is run
/// again; results the old session read stay, and are no longer editable (ADR-061: they are of
/// that session). Answers the new session's server version.
#[command]
pub async fn change_tab_database(
    session: String,
    database: String,
    window: tauri::Window,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let app = window.app_handle().clone();
    let Some(folder) = state.folder.settled() else {
        return Err("the folder is changing; select a connection again".into());
    };
    let (old, password, history_workspace, connection_folder, definition) = {
        let sessions = state.sessions.lock().await;
        let tab = sessions.get(&session).ok_or("this tab is not connected")?;
        (
            tab.open.clone(),
            tab.password.clone(),
            tab.history_workspace.clone(),
            tab.folder.clone(),
            tab.definition.clone(),
        )
    };
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::for_window(
            app.clone(),
            state.pending.clone(),
            window.label(),
        )),
        password: password.clone(),
        read_only_session: false,
    };
    // Its own folder's configuration, an `Arc` so no lock is held while the session opens (a
    // tunnel may ask the user).
    let config = folder_config(&state, connection_folder.as_deref()).await?;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let configuration = spelaion_core::Configuration {
        config: &config,
        trust: &trust,
        backend: &SystemBackend,
    };
    let opened = spelaion_core::change_session_database(
        &old,
        &database,
        &configuration,
        &state.engines(),
        options,
        state.results.clone(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let server_version = match opened.session.connection().server_version().await {
        Ok(version) => version,
        Err(error) => {
            opened.close().await;
            return Err(error.to_string());
        }
    };
    opened
        .session
        .follow_transactions(&opened.resolved.safeguards())
        .await;
    // Whether it is on another database than the connection's own, as its saved file says.
    let other_database = config
        .connection(&opened.resolved.id)
        .and_then(|entry| entry.file.database.as_deref())
        != Some(opened.resolved.params.database.as_str());
    let replaced = {
        let mut sessions = state.sessions.lock().await;
        if !state.folder.still(folder)
            || !sessions
                .get(&session)
                .is_some_and(|s| Arc::ptr_eq(&s.open, &old))
        {
            drop(sessions);
            opened.close().await;
            return Err("the tab or the folder changed while connecting".into());
        }
        let opened = Arc::new(opened);
        watch_session(&app, session.clone(), &opened);
        sessions.insert(
            session,
            DesktopSession {
                window: window.label().to_owned(),
                open: opened,
                password,
                history_workspace,
                folder: connection_folder,
                definition,
                other_database,
                explorer_parent: None,
            },
        )
    };
    drop(old);
    if let Some(replaced) = replaced {
        replaced.open.session.stop_paused_run();
        if let Ok(open) = Arc::try_unwrap(replaced.open) {
            open.close().await;
        }
    }
    Ok(server_version)
}

/// Opens `database` of the connection whose explorer session is `source` as the session
/// `session` (ADR-074 point 4): a new session, never a switch, with the connection's settings and
/// the password typed for `source`. What the core refuses comes back as its reason.
#[command]
pub async fn connect_database(
    source: String,
    session: String,
    database: String,
    window: tauri::Window,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let app = window.app_handle().clone();
    if source == session {
        return Err("the source and target sessions must differ".into());
    }
    let Some(folder) = state.folder.settled() else {
        return Err("the folder is changing; select a connection again".into());
    };
    let (own, password, history_workspace, connection_folder, definition) = {
        let sessions = state.sessions.lock().await;
        let source = sessions
            .get(&source)
            .ok_or("the connection's explorer session is closed")?;
        (
            source.open.clone(),
            source.password.clone(),
            source.history_workspace.clone(),
            source.folder.clone(),
            source.definition.clone(),
        )
    };
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::for_window(
            app.clone(),
            state.pending.clone(),
            window.label(),
        )),
        password: password.clone(),
        read_only_session: false,
    };
    // Its own folder's configuration, an `Arc` so no lock is held while the session opens (a
    // tunnel may ask the user).
    let config = folder_config(&state, connection_folder.as_deref()).await?;
    let trust = TrustStore::load(&state.user_dir).map_err(|e| e.to_string())?;
    let configuration = spelaion_core::Configuration {
        config: &config,
        trust: &trust,
        backend: &SystemBackend,
    };
    let opened = spelaion_core::open_other_database(
        &own,
        &database,
        &configuration,
        &state.engines(),
        options,
        state.results.clone(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut sessions = state.sessions.lock().await;
    if !state.folder.still(folder)
        || !sessions
            .get(&source)
            .is_some_and(|s| Arc::ptr_eq(&s.open, &own))
        || sessions.contains_key(&session)
    {
        drop(sessions);
        opened.close().await;
        return Err("the connection or the folder changed while connecting".into());
    }
    let opened = Arc::new(opened);
    watch_session(&app, session.clone(), &opened);
    sessions.insert(
        session,
        DesktopSession {
            window: window.label().to_owned(),
            open: opened,
            password,
            history_workspace,
            folder: connection_folder,
            definition,
            other_database: true,
            explorer_parent: Some(source),
        },
    );
    Ok(())
}
