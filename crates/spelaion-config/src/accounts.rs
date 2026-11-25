//! Names of the keychain entries used by `password = { secret = "keychain" }`.
//!
//! Repository entries include the repository's canonical path, so two projects with a
//! connection (or tunnel) of the same name never share a password: otherwise the password
//! of one project's server could be sent to another project's server. The path cannot be
//! chosen by the repository's content, unlike a name written in its files. Personal
//! entries start with `~/`. Moving a repository to another folder only means its passwords
//! are asked (and can be remembered) again.

use std::path::PathBuf;

use crate::identity::{ConnectionKey, canonical_folder, keychain_scope};
use crate::load::{Config, ConnectionId, Source};

fn scope(config: &Config, source: Source) -> String {
    match source {
        Source::User => keychain_scope(None),
        // Not reachable without a root for loaded repository entries; an empty folder is
        // kept distinct from personal ones.
        Source::Workspace => keychain_scope(Some(
            &config
                .workspace_root
                .as_deref()
                .map_or_else(PathBuf::new, canonical_folder),
        )),
    }
}

/// Entry of a connection's password: `~/<id>/password` or `repo:<root>/<id>/password`, from
/// its identity (ADR-075).
pub fn connection_account(config: &Config, id: &ConnectionId) -> String {
    ConnectionKey::of(config, id).keychain_account()
}

/// Entry of a tunnel proxy's password (ADR-027): `~/tunnel/<name>/proxy/password` or
/// `repo:<root>/tunnel/<name>/proxy/password`.
pub fn proxy_account(config: &Config, source: Source, tunnel: &str) -> String {
    format!("{}/tunnel/{tunnel}/proxy/password", scope(config, source))
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
