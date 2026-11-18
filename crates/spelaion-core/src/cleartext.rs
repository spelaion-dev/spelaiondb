//! MySQL's and MariaDB's `mysql_clear_password` authentication plugin (SDB-396) sends the
//! password as it is typed. A ProxySQL or a LDAP/PAM login may need it, so a connection file
//! can allow it (`cleartext_password`), and the driver is told so through
//! `ConnectParams::options`. The core decides whether that may happen: only when what carries
//! the password cannot be read on the way.
//!
//! - TLS whose certificate is checked (`verify-ca` or `verify-full`): `require` encrypts but
//!   would hand the password to whoever answers in the server's place.
//! - An SSH tunnel with at least one hop: the password travels inside SSH up to the last hop.
//!
//! Anything else is refused before the driver is called: a SOCKS or HTTP proxy alone, AWS SSM
//! alone or Kubernetes' port-forward (each carries the bytes as they are as far as this core
//! can tell), and an engine other than MySQL and MariaDB. Every connection reaches its driver
//! through `open::connect_driver`, which asks here first, so no path skips it.

use spelaion_config::{Engine, ResolvedConnection, engine_options};
use spelaion_driver_api::{ConnectParams, TlsMode};

/// Why a connection may not let its driver send the password in clear; `None` when it may, or
/// when it does not ask to.
pub fn refusal(resolved: &ResolvedConnection, params: &ConnectParams) -> Option<String> {
    // The option is looked for in what the driver will receive, whatever its case or value:
    // a driver that read it loosely must still never get it unchecked.
    let asks = params.options.keys().any(|key| {
        key.trim()
            .eq_ignore_ascii_case(engine_options::CLEARTEXT_PASSWORD)
    });
    if !asks {
        return None;
    }
    if !matches!(resolved.engine, Engine::Mysql | Engine::Mariadb) {
        return Some(format!(
            "the cleartext password plugin is MySQL's and MariaDB's, not {}'s",
            resolved.engine.as_str()
        ));
    }
    let verified_tls = matches!(params.tls, TlsMode::VerifyCa | TlsMode::VerifyFull);
    let through_ssh = resolved
        .tunnel
        .as_ref()
        .is_some_and(|tunnel| !tunnel.hops.is_empty());
    if verified_tls || through_ssh {
        return None;
    }
    Some(
        "this connection allows the cleartext password plugin, which sends the password as it is: \
         it opens only over TLS that checks the server's certificate (verify-ca or verify-full) \
         or through an SSH tunnel"
            .into(),
    )
}
