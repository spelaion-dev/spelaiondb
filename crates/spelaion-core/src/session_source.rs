//! A session made from another one (SDB-426): a tab opened from the explorer, a details or
//! diagram tab, the process monitor. Such a session reuses the source session's resolved
//! connection and the password the user typed for it, so nothing is asked again. That holds
//! only while the connection is what the configuration says: once the user saves an edit (its
//! TLS mode, the databases it shows, its session setup, masking…), the next session must be
//! opened from the saved file, never from the resolution made before it.
//!
//! The app keeps, with each session, the [`ConnectionDefinition`] it was opened from (the
//! files the resolver reads, without secrets), and asks here before making a session from it:
//! - the same definition now: reuse the source's resolution ([`SourceSession::Current`]);
//! - another one: resolve the saved file again ([`SourceSession::Changed`]). The password typed
//!   for the source goes with it only to the same login on the same route
//!   ([`carried_password`]); anything else opens as a new connection does, asking again.
//!
//! Nothing the user ran is run again (ADR-028): only a new session is opened, with the
//! connection's setup (ADR-037).

use spelaion_config::{ConnectionDefinition, Password, ResolvedConnection};
use spelaion_driver_api::Secret;

/// Whether a session opened from `opened` is still what the configuration says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceSession {
    /// Nothing it is opened from changed: its resolution can be reused.
    Current,
    /// The connection was edited, removed, or something it is opened from changed: a new
    /// session is opened from the saved file.
    Changed,
}

/// Compares the definition a session was opened from with the one the configuration has now
/// (`None`: the connection is gone).
pub fn source_session(
    opened: &ConnectionDefinition,
    now: Option<&ConnectionDefinition>,
) -> SourceSession {
    match now {
        Some(now) if now == opened => SourceSession::Current,
        Some(_) | None => SourceSession::Changed,
    }
}

/// The password typed for `old`'s session, if it may be used to open `new` (the connection
/// resolved again after an edit): only when `new` still asks for one and logs in as the same
/// user on the same server through the same route. A password is never sent anywhere it was
/// not typed for; `None` means `new` opens as a new connection does (its keychain entry, its
/// variable, or asking).
pub fn carried_password(
    old: &ResolvedConnection,
    new: &ResolvedConnection,
    typed: Option<&Secret>,
) -> Option<Secret> {
    let typed = typed?;
    let both_ask = matches!(old.password, Password::Prompt { .. })
        && matches!(new.password, Password::Prompt { .. });
    (both_ask && same_login(old, new)).then(|| typed.clone())
}

/// The same engine, server, user and route (tunnel hops, proxy, SSM, Kubernetes).
fn same_login(old: &ResolvedConnection, new: &ResolvedConnection) -> bool {
    let target = |c: &ResolvedConnection| {
        (
            c.engine,
            c.params.host.clone(),
            c.params.port,
            c.params.user.clone(),
        )
    };
    let route = |c: &ResolvedConnection| {
        c.tunnel.as_ref().map(|t| {
            (
                t.hops
                    .iter()
                    .map(|h| (h.host.clone(), h.port, h.user.clone()))
                    .collect::<Vec<_>>(),
                t.proxy
                    .as_ref()
                    .map(|p| (p.kind, p.host.clone(), p.port, p.user.clone())),
                t.ssm.clone(),
            )
        })
    };
    let kubernetes = |c: &ResolvedConnection| {
        c.kubernetes
            .as_ref()
            .map(|k| (k.context.clone(), k.namespace.clone(), k.resource.clone()))
    };
    target(old) == target(new) && route(old) == route(new) && kubernetes(old) == kubernetes(new)
}
