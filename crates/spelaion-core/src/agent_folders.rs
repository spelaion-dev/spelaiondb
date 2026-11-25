//! What agents can do on the connections of every folder the app has open, as the MCP page shows
//! it (ADR-033, ADR-075 point 9).
//!
//! An MCP server serves one folder (the one it is started in, or `--workspace`) and the personal
//! connections; it never reads what the app has open. So the page shows each open folder's
//! connections under that folder, each decided exactly as a server started in that folder
//! decides it (its trust, its types, its per-connection override), and the personal connections
//! once. Nothing here changes what a server lists.

use std::path::PathBuf;
use std::sync::Arc;

use spelaion_config::{
    Config, ConnectionEntry, ConnectionKey, ConnectionType, ConnectionTypes, Source, TrustStore,
    canonical_folder,
};

use crate::{
    AgentView, QueryHistory, agent_view_with_override, agent_workspace, all_agents_read_only,
};

/// Whose connections a group holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFolder {
    /// An open repository folder: its canonical root (a connection's folder, and what the
    /// audit log records), the path it was opened as, and whether it is trusted (ADR-022).
    Repository {
        root: PathBuf,
        path: PathBuf,
        trusted: bool,
    },
    /// The personal connections, which every server serves whatever its folder.
    Personal,
}

/// One connection, with what agents can do there as its own folder decides it.
#[derive(Debug, Clone)]
pub struct ConnectionAgents {
    /// Its identity: its folder and its id (ADR-075).
    pub key: ConnectionKey,
    pub entry: ConnectionEntry,
    pub view: AgentView,
    /// Its type, from its folder's types (ADR-029).
    pub kind: Option<ConnectionType>,
    /// The store key of its per-connection override (its folder's, empty for a personal one);
    /// `None` when the folder cannot be resolved, so the override cannot be read or set.
    pub override_scope: Option<String>,
    /// The override as read; `None` when it could not be read (agents are then read-only).
    pub read_only_override: Option<bool>,
}

/// The connections of one open folder, or the personal ones.
#[derive(Debug, Clone)]
pub struct FolderAgents {
    pub folder: AgentFolder,
    pub connections: Vec<ConnectionAgents>,
}

/// Every open repository folder's connections, each under its folder in the order they were
/// opened, then the personal connections. `open` is each open folder loaded alone with the
/// personal layer; `shown` is the configuration the page shows (the last folder opened, or the
/// personal layer alone), whose types a personal connection's row shows, as before several
/// folders could be open. Folders that are not repositories have no connections of their own
/// and no group.
pub async fn agents_by_folder(
    open: &[Arc<Config>],
    shown: &Config,
    trust: &TrustStore,
    history: &QueryHistory,
) -> Vec<FolderAgents> {
    // Asked as the server asks it: a setting that cannot be read makes every agent read-only,
    // so the page shows what a running server decides instead of an error.
    let all_read_only = all_agents_read_only(false, history.mcp_settings().await.map(|(on, _)| on));
    let mut groups = Vec::with_capacity(open.len() + 1);
    for config in open {
        let Some(root) = config.workspace_root.as_deref() else {
            continue;
        };
        let trusted = trust.is_trusted(root);
        let connections = connections_of(
            config,
            Source::Workspace,
            trusted,
            all_read_only,
            trust,
            history,
        )
        .await;
        groups.push(FolderAgents {
            folder: AgentFolder::Repository {
                root: canonical_folder(root),
                path: root.to_path_buf(),
                trusted,
            },
            connections,
        });
    }
    groups.push(FolderAgents {
        folder: AgentFolder::Personal,
        // Trust is a repository's: it does not apply to personal connections.
        connections: connections_of(shown, Source::User, true, all_read_only, trust, history).await,
    });
    groups
}

async fn connections_of(
    config: &Config,
    source: Source,
    trusted: bool,
    all_read_only: bool,
    trust: &TrustStore,
    history: &QueryHistory,
) -> Vec<ConnectionAgents> {
    let types = ConnectionTypes::load(config, trust);
    let override_scope = agent_workspace(config, source);
    let mut connections = Vec::new();
    for entry in config.connections.iter().filter(|e| e.id.source == source) {
        let read_only_override = match &override_scope {
            Some(scope) => history
                .mcp_connection_read_only(scope.clone(), entry.id.to_string())
                .await
                .ok(),
            None => None,
        };
        let view = agent_view_with_override(
            entry,
            trusted,
            all_read_only,
            read_only_override.unwrap_or(true),
        );
        connections.push(ConnectionAgents {
            key: ConnectionKey::of(config, &entry.id),
            entry: entry.clone(),
            view,
            kind: entry
                .file
                .env
                .as_deref()
                .and_then(|env| types.get(env))
                .cloned(),
            override_scope: override_scope.clone(),
            read_only_override,
        });
    }
    connections
}
