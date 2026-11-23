//! The warning about a connection whose TLS does not check the server (SDB-395): `prefer` and
//! `require` encrypt without verifying who answers, and `disable` does not encrypt at all, so on
//! a network anyone between the app and the server can read or impersonate it. The default
//! mode is `prefer`, which makes this easy to leave on by mistake.
//!
//! The rule is the core's, from the connection as configured: the form asks it with what is
//! typed, the connection test with the resolved connection, the explorer with each connection
//! file and its network profile. React only shows the answer.
//!
//! - Only a database server warns: a file (SQLite, DuckDB, a folder of files, a SQLite file on
//!   a server reached over SSH) has no TLS.
//! - `verify-ca` and `verify-full` never warn.
//! - A host that is this machine (`localhost`, a `.localhost` name, a loopback address, a Unix
//!   socket's path) never warns: nothing crosses a network.
//! - Through an SSH tunnel, AWS SSM or a Kubernetes port-forward the warning is calmer: the
//!   traffic is encrypted to a machine next to the server, and only the last stretch is not
//!   checked. A host that is that machine itself does not warn. A proxy alone (SOCKS5, HTTP)
//!   encrypts nothing: it warns as a direct connection does.
//! - A SQL Server LocalDB instance (ADR-068) is reached through its pipe on this machine, whose
//!   login goes without TLS: only a SQL login warns, since its password crosses that pipe
//!   unencrypted; the Windows account sends none.

use std::net::IpAddr;

use spelaion_config::{Config, ConnectionEntry, Engine, ResolvedConnection, TlsModeConfig};
use spelaion_driver_api::TlsMode;

/// What the connection's TLS leaves unchecked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsRisk {
    /// `disable`: nothing is encrypted.
    Unencrypted,
    /// `prefer` or `require`: encrypted, but the server's certificate is not checked.
    Unverified,
    /// A SQL login to a LocalDB instance (ADR-068): the login goes through the instance's pipe
    /// on this machine without TLS, so its password crosses that pipe unencrypted. With the
    /// Windows account (no user) no password travels and nothing warns.
    LocalDbPassword,
}

/// How the connection reaches its server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsRoute {
    /// Straight to the host, or through a proxy, which encrypts nothing.
    Direct,
    /// Through an SSH tunnel, AWS SSM or a Kubernetes port-forward: encrypted up to a machine
    /// next to the server.
    Tunneled,
}

/// A warning to show, with what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsWarning {
    pub risk: TlsRisk,
    /// The TLS mode as the connection file writes it (`prefer`, `require`, `disable`).
    pub mode: &'static str,
    /// The host the database is reached at, as configured.
    pub host: String,
    pub route: TlsRoute,
}

/// Whether `host` names this machine: nothing it is sent to crosses a network.
pub fn is_local_host(host: &str) -> bool {
    let host = host.trim();
    // A driver's default host is this machine; a path is a Unix socket.
    if host.is_empty() || host.starts_with('/') {
        return true;
    }
    let lower = host.to_ascii_lowercase();
    let name = lower.trim_end_matches('.');
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    let address = name
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(name);
    match address.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

fn mode_name(mode: TlsMode) -> &'static str {
    match mode {
        TlsMode::Disable => "disable",
        TlsMode::Prefer => "prefer",
        TlsMode::Require => "require",
        TlsMode::VerifyCa => "verify-ca",
        TlsMode::VerifyFull => "verify-full",
    }
}

/// The rule itself: a server engine, a mode that does not check the server, and a host that is
/// not this machine.
pub fn tls_warning(
    engine: Engine,
    host: &str,
    mode: TlsMode,
    route: TlsRoute,
) -> Option<TlsWarning> {
    if engine.is_file() || is_local_host(host) {
        return None;
    }
    let risk = match mode {
        TlsMode::Disable => TlsRisk::Unencrypted,
        TlsMode::Prefer | TlsMode::Require => TlsRisk::Unverified,
        TlsMode::VerifyCa | TlsMode::VerifyFull => return None,
    };
    Some(TlsWarning {
        risk,
        mode: mode_name(mode),
        host: host.trim().to_string(),
        route,
    })
}

/// The rule for a LocalDB instance (ADR-068), which has no network and no TLS inside its pipe:
/// `None` when `host` names none, otherwise its warning, if any. Only a SQL login warns, since
/// its password crosses the pipe unencrypted; the Windows account sends none.
fn localdb_warning(engine: Engine, host: &str, user: &str) -> Option<Option<TlsWarning>> {
    spelaion_config::localdb_instance(engine, Some(host))?;
    Some((!user.trim().is_empty()).then(|| TlsWarning {
        risk: TlsRisk::LocalDbPassword,
        mode: "none",
        host: host.trim().to_string(),
        route: TlsRoute::Direct,
    }))
}

/// The warning for a resolved connection (the connection test, an open session): its host and
/// TLS mode as resolved, before any tunnel points it elsewhere.
pub fn connection_tls_warning(resolved: &ResolvedConnection) -> Option<TlsWarning> {
    if resolved.file_folder.is_some() || resolved.sqlite3.is_some() {
        return None;
    }
    if let Some(warning) = localdb_warning(
        resolved.engine,
        &resolved.params.host,
        &resolved.params.user,
    ) {
        return warning;
    }
    let tunneled = resolved.kubernetes.is_some()
        || resolved
            .tunnel
            .as_ref()
            .is_some_and(|tunnel| !tunnel.hops.is_empty() || tunnel.ssm.is_some());
    tls_warning(
        resolved.engine,
        &resolved.params.host,
        resolved.params.tls,
        if tunneled {
            TlsRoute::Tunneled
        } else {
            TlsRoute::Direct
        },
    )
}

/// The driver's mode for a file's mode.
pub fn tls_mode_of(mode: TlsModeConfig) -> TlsMode {
    match mode {
        TlsModeConfig::Disable => TlsMode::Disable,
        TlsModeConfig::Prefer => TlsMode::Prefer,
        TlsModeConfig::Require => TlsMode::Require,
        TlsModeConfig::VerifyCa => TlsMode::VerifyCa,
        TlsModeConfig::VerifyFull => TlsMode::VerifyFull,
    }
}

/// How a connection with this network profile (`tunnel`, looked up in `config` from `source`)
/// and Kubernetes forward reaches its server. A profile that is not found counts as a tunnel:
/// such a connection cannot open, and the explorer says why elsewhere.
fn route_of(
    config: &Config,
    source: spelaion_config::Source,
    tunnel: Option<&str>,
    kubernetes: bool,
) -> TlsRoute {
    let tunneled = kubernetes
        || tunnel.is_some_and(|name| {
            config.tunnel_for(source, name).is_none_or(|profile| {
                !profile.file.hops.is_empty()
                    || profile.file.ssm.is_some()
                    || profile.file.kubernetes.is_some()
            })
        });
    if tunneled {
        TlsRoute::Tunneled
    } else {
        TlsRoute::Direct
    }
}

/// The warning for a connection file as written, for the explorer, which never resolves a
/// connection (resolving reads secrets and may run programs). A host written with a variable
/// is not judged here: the connection test, which resolves it, says.
pub fn file_tls_warning(config: &Config, entry: &ConnectionEntry) -> Option<TlsWarning> {
    let file = &entry.file;
    if file.file_folder || file.sqlite3.is_some() {
        return None;
    }
    draft_tls_warning(
        config,
        entry.id.source,
        file.engine,
        file.host.as_deref().unwrap_or_default(),
        file.user.as_deref().unwrap_or_default(),
        tls_mode_of(file.tls.mode),
        file.tunnel.as_deref(),
        file.kubernetes.is_some(),
    )
}

/// The warning for what the add / edit connection form holds: its engine, host, user and TLS
/// mode, and the network profile and Kubernetes forward it names. `source` is where the
/// connection is (or will be) saved, which decides where its profile is looked up.
#[allow(clippy::too_many_arguments)]
pub fn draft_tls_warning(
    config: &Config,
    source: spelaion_config::Source,
    engine: Engine,
    host: &str,
    user: &str,
    mode: TlsMode,
    tunnel: Option<&str>,
    kubernetes: bool,
) -> Option<TlsWarning> {
    if host.contains("${") {
        return None;
    }
    if let Some(warning) = localdb_warning(engine, host, user) {
        return warning;
    }
    let tunnel = tunnel.map(str::trim).filter(|name| !name.is_empty());
    tls_warning(
        engine,
        host,
        mode,
        route_of(config, source, tunnel, kubernetes),
    )
}

/// [`draft_tls_warning`] for the form's whole draft.
pub fn connection_draft_tls_warning(
    config: &Config,
    draft: &spelaion_config::ConnectionDraft,
) -> Option<TlsWarning> {
    if draft.file_folder || !draft.sqlite3.trim().is_empty() {
        return None;
    }
    draft_tls_warning(
        config,
        draft.source,
        draft.engine,
        &draft.host,
        &draft.user,
        tls_mode_of(draft.tls_mode),
        Some(&draft.tunnel),
        draft.kubernetes.is_some(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_machine_is_local_and_every_other_host_is_not() {
        for host in [
            "",
            "localhost",
            "LOCALHOST",
            "localhost.",
            "db.localhost",
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "::1",
            "[::1]",
            "::",
            "::ffff:127.0.0.1",
            "/var/run/postgresql",
            " localhost ",
        ] {
            assert!(is_local_host(host), "{host:?}");
        }
        for host in [
            "db.example.com",
            "localhost.example.com",
            "mylocalhost",
            "10.0.0.5",
            "192.168.1.10",
            "172.16.0.1",
            "128.0.0.1",
            "::ffff:10.0.0.1",
            "fe80::1",
            "[2001:db8::1]",
            "host.docker.internal",
        ] {
            assert!(!is_local_host(host), "{host:?}");
        }
    }

    #[test]
    fn modes_that_do_not_check_the_server_warn_on_every_server_engine() {
        for engine in &Engine::ALL {
            for (mode, risk) in [
                (TlsMode::Disable, Some(TlsRisk::Unencrypted)),
                (TlsMode::Prefer, Some(TlsRisk::Unverified)),
                (TlsMode::Require, Some(TlsRisk::Unverified)),
                (TlsMode::VerifyCa, None),
                (TlsMode::VerifyFull, None),
            ] {
                let warning = tls_warning(*engine, "db.example.com", mode, TlsRoute::Direct);
                if engine.is_file() {
                    assert_eq!(warning, None, "{engine:?}: a file has no TLS");
                } else {
                    assert_eq!(
                        warning.as_ref().map(|w| w.risk),
                        risk,
                        "{engine:?} {mode:?}"
                    );
                }
                if let Some(warning) = warning {
                    assert_eq!(warning.host, "db.example.com");
                    assert_eq!(warning.mode, mode_name(mode));
                }
                // This machine never warns, whatever the mode.
                assert_eq!(
                    tls_warning(*engine, "127.0.0.1", mode, TlsRoute::Direct),
                    None
                );
            }
        }
    }

    /// LocalDB (ADR-068): no TLS inside its pipe, so a SQL login's password crosses it
    /// unencrypted and says so; the Windows account sends no password, and nothing warns.
    #[test]
    fn a_localdb_sql_login_is_told_its_password_crosses_the_pipe_unencrypted() {
        let host = "(localdb)\\MSSQLLocalDB";
        for mode in [TlsMode::Disable, TlsMode::Prefer, TlsMode::Require] {
            assert_eq!(
                localdb_warning(Engine::Sqlserver, host, ""),
                Some(None),
                "{mode:?}"
            );
            let warning = localdb_warning(Engine::Sqlserver, host, "app")
                .flatten()
                .unwrap();
            assert_eq!(warning.risk, TlsRisk::LocalDbPassword);
            assert_eq!(warning.host, host);
        }
        // Not LocalDB: the ordinary rule decides.
        assert_eq!(
            localdb_warning(Engine::Sqlserver, "db.internal", "app"),
            None
        );
        assert_eq!(localdb_warning(Engine::Postgres, host, "app"), None);
    }

    #[test]
    fn a_tunnel_makes_the_warning_calmer_and_a_host_on_the_tunnels_end_silences_it() {
        let warning = tls_warning(
            Engine::Postgres,
            "db.internal",
            TlsMode::Require,
            TlsRoute::Tunneled,
        )
        .unwrap();
        assert_eq!(warning.route, TlsRoute::Tunneled);
        assert_eq!(warning.mode, "require");
        assert_eq!(
            tls_warning(
                Engine::Mysql,
                "localhost",
                TlsMode::Prefer,
                TlsRoute::Tunneled
            ),
            None
        );
    }
}
