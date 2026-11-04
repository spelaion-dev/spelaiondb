//! Names of the keychain entries used by `password = { secret = "keychain" }`.
//!
//! Repository entries include the repository's canonical path, so two projects with a
//! connection (or tunnel) of the same name never share a password: otherwise the password
//! of one project's server could be sent to another project's server. The path cannot be
//! chosen by the repository's content, unlike a name written in its files. Personal
//! entries start with `~/`. Moving a repository to another folder only means its passwords
//! are asked (and can be remembered) again.

use crate::load::{Config, ConnectionId, Source};
use crate::trust::canonical;

fn scope(config: &Config, source: Source) -> String {
    match source {
        Source::User => "~".to_string(),
        Source::Workspace => match &config.workspace_root {
            Some(root) => format!("repo:{}", canonical(root).display()),
            // Not reachable for loaded repository entries; kept distinct from personal ones.
            None => "repo:".to_string(),
        },
    }
}

/// Entry of a connection's password: `~/<id>/password` or `repo:<root>/<id>/password`.
pub fn connection_account(config: &Config, id: &ConnectionId) -> String {
    format!("{}/{}/password", scope(config, id.source), id.path)
}

/// Entry of a tunnel hop's password (`hop` counts from 0; the name counts from 1):
/// `~/tunnel/<name>/<n>/password` or `repo:<root>/tunnel/<name>/<n>/password`.
pub fn tunnel_account(config: &Config, source: Source, tunnel: &str, hop: usize) -> String {
    format!(
        "{}/tunnel/{tunnel}/{}/password",
        scope(config, source),
        hop + 1
    )
}
