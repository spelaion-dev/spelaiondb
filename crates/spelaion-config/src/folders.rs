//! What one connection reads of the folders a window holds open (ADR-075): each folder is one
//! `Config`, loaded alone with the personal layer. A folder's connection reads its own folder;
//! a personal connection reads every open folder, and only to tighten. The resolver
//! (`resolve.rs`) and a connection's definition (`definition.rs`) both ask here, so what a
//! session is opened from and what is compared before reusing it are always the same folders
//! and the same network profile.

use crate::load::{Config, Source, TunnelEntry};

/// The folders a connection from `source` reads beyond its own file, each once (the same
/// root twice is one folder), in the order they were opened: `own` alone for a folder's
/// connection; every open one for a personal connection, or `own` when none are given.
pub(crate) fn folders_for<'a>(
    own: &'a Config,
    open: &[&'a Config],
    source: Source,
) -> Vec<&'a Config> {
    let all: Vec<&'a Config> = match source {
        Source::Workspace => vec![own],
        Source::User if open.is_empty() => vec![own],
        Source::User => open.to_vec(),
    };
    let mut folders: Vec<&'a Config> = Vec::new();
    for config in all {
        if !folders
            .iter()
            .any(|other| other.workspace_root == config.workspace_root)
        {
            folders.push(config);
        }
    }
    folders
}

/// Where the network profile a connection names is.
pub(crate) enum Profile<'a> {
    /// In this folder's configuration, whose root its paths, its trust and its keychain names
    /// belong to (`own` for a personal profile).
    Found(&'a Config, &'a TunnelEntry),
    /// Nowhere the connection may look.
    Missing,
    /// A personal connection's: no personal profile has the name and several open folders do.
    /// It is refused rather than one of them picked.
    Several(Vec<&'a Config>),
}

/// The network profile `name` of a connection from `source` loaded in `own`. A folder's
/// connection: its own folder's, then a personal one (`Config::tunnel_for`). A personal
/// connection: a personal one, else the one open folder's that has it.
pub(crate) fn profile<'a>(
    own: &'a Config,
    open: &[&'a Config],
    source: Source,
    name: &str,
) -> Profile<'a> {
    if source == Source::Workspace {
        return match own.tunnel_for(Source::Workspace, name) {
            Some(tunnel) => Profile::Found(own, tunnel),
            None => Profile::Missing,
        };
    }
    if let Some(tunnel) = own
        .tunnels
        .iter()
        .find(|t| t.source == Source::User && t.name == name)
    {
        return Profile::Found(own, tunnel);
    }
    let mut found: Vec<(&'a Config, &'a TunnelEntry)> = folders_for(own, open, Source::User)
        .into_iter()
        .filter_map(|config| {
            config
                .tunnels
                .iter()
                .find(|t| t.source == Source::Workspace && t.name == name)
                .map(|tunnel| (config, tunnel))
        })
        .collect();
    match found.len() {
        0 => Profile::Missing,
        1 => {
            let (config, tunnel) = found.remove(0);
            Profile::Found(config, tunnel)
        }
        _ => Profile::Several(found.into_iter().map(|(config, _)| config).collect()),
    }
}
