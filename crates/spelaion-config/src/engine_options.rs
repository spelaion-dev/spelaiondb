//! The engine's own connection settings a connection file asks for (SDB-396), as the driver
//! receives them in `ConnectParams::options` (ADR-056): never a secret.
//!
//! - `pooler`: PostgreSQL behind a pooler (`session` or `transaction`). In `transaction`, the
//!   driver relies on no prepared statement and no session state.
//! - `cleartext_password`: MySQL and MariaDB may answer the `mysql_clear_password`
//!   authentication plugin (`on`). The core refuses such a connection unless TLS is verified or
//!   it goes through an SSH tunnel (`spelaion-core`'s `cleartext.rs`), so this is only ever
//!   set on a route the password cannot be read on.

use std::collections::BTreeMap;

use crate::model::{ConnectionFile, Engine};

/// The option that names a PostgreSQL pooler's mode.
pub const POOLER: &str = "pooler";
/// The option that allows MySQL's cleartext password plugin.
pub const CLEARTEXT_PASSWORD: &str = "cleartext_password";

/// The options `file` gives its driver.
pub fn of(file: &ConnectionFile) -> BTreeMap<String, String> {
    let mut options = BTreeMap::new();
    if file.engine == Engine::Postgres
        && let Some(pooler) = file.pooler
    {
        options.insert(POOLER.to_string(), pooler.as_str().to_string());
    }
    if file.cleartext_password && matches!(file.engine, Engine::Mysql | Engine::Mariadb) {
        options.insert(CLEARTEXT_PASSWORD.to_string(), "on".to_string());
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(toml: &str) -> ConnectionFile {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn only_the_engine_s_own_options_are_given() {
        let pg = file("schema_version = 1\nengine = \"postgres\"\npooler = \"transaction\"\n");
        assert_eq!(of(&pg).get(POOLER).map(String::as_str), Some("transaction"));
        let mysql = file("schema_version = 1\nengine = \"mysql\"\ncleartext_password = true\n");
        assert_eq!(
            of(&mysql).get(CLEARTEXT_PASSWORD).map(String::as_str),
            Some("on")
        );
        assert!(of(&file("schema_version = 1\nengine = \"mysql\"\n")).is_empty());
        // Another engine's option is not passed on (the loader refuses such a file anyway).
        let odd = file("schema_version = 1\nengine = \"sqlserver\"\ncleartext_password = true\n");
        assert!(of(&odd).is_empty());
    }
}
