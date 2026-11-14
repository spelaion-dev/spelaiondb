//! Servers that speak PostgreSQL's protocol (SDB-169). A connection of any of them is a
//! PostgreSQL connection, served by the PostgreSQL plugin; which server it reached is read
//! from the server itself (`version()`) when a session opens, never from the connection's
//! file or the plugin.
//!
//! What it decides is which of the barriers the core asks of PostgreSQL that server really
//! enforces (ADR-020, ADR-053). A server that accepts `BEGIN READ ONLY` and still writes gets
//! no agents' reads and no streaming reads; one that accepts `SET SESSION CHARACTERISTICS AS
//! TRANSACTION READ ONLY` and ignores it gets no read-only connections. A fork that was never
//! checked here, or a server that does not say what it is, gets neither: the default is
//! closed.

use futures_util::StreamExt;
use spelaion_driver_api::{Connection, ExecEvent, ExecOptions, Value};

/// The server a PostgreSQL connection reached, as its `version()` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFamily {
    /// PostgreSQL itself, and what reports itself as PostgreSQL with no other name in its
    /// version (TimescaleDB, which is an extension, and managed PostgreSQL services).
    PostgreSql,
    CockroachDb,
    YugabyteDb,
    /// `BEGIN READ ONLY` stops writes, but `SET SESSION CHARACTERISTICS AS TRANSACTION READ
    /// ONLY` only answers with a notice that nothing changed (checked with 3.1).
    RisingWave,
    /// `BEGIN READ ONLY` stops `INSERT` but lets `CREATE TABLE` through (checked with 26.50),
    /// and the session's read-only mode is refused.
    Materialize,
    /// No transactions: `BEGIN` is accepted and ignored, `ROLLBACK` is not even understood
    /// (checked with 6.5).
    CrateDb,
    /// A fork whose barriers were not checked here (its name), or a server whose version does
    /// not start with PostgreSQL's name.
    Unchecked(String),
}

/// Forks that name themselves in their `version()` and were not checked: their barriers are
/// not assumed to be PostgreSQL's.
const UNCHECKED_FORKS: &[&str] = &[
    "Redshift",
    "Greenplum",
    "Cloudberry",
    "Greengage",
    "Yellowbrick",
    "openGauss",
    "GaussDB",
    "Kingbase",
];

impl ServerFamily {
    /// Reads `select version()`'s text.
    pub fn from_version(version: &str) -> Self {
        let version = version.trim();
        let has = |name: &str| {
            version
                .to_ascii_lowercase()
                .contains(&name.to_ascii_lowercase())
        };
        if version.starts_with("CockroachDB") {
            return ServerFamily::CockroachDb;
        }
        if version.starts_with("CrateDB") {
            return ServerFamily::CrateDb;
        }
        if !version.starts_with("PostgreSQL ") {
            return ServerFamily::Unchecked(version.split_whitespace().next().unwrap_or("").into());
        }
        // Forks keep PostgreSQL's name first and add their own.
        if has("-YB-") {
            return ServerFamily::YugabyteDb;
        }
        if has("RisingWave") {
            return ServerFamily::RisingWave;
        }
        if has("Materialize") {
            return ServerFamily::Materialize;
        }
        if let Some(fork) = UNCHECKED_FORKS.iter().find(|fork| has(fork)) {
            return ServerFamily::Unchecked((*fork).into());
        }
        ServerFamily::PostgreSql
    }

    /// The name shown in messages.
    pub fn name(&self) -> &str {
        match self {
            ServerFamily::PostgreSql => "PostgreSQL",
            ServerFamily::CockroachDb => "CockroachDB",
            ServerFamily::YugabyteDb => "YugabyteDB",
            ServerFamily::RisingWave => "RisingWave",
            ServerFamily::Materialize => "Materialize",
            ServerFamily::CrateDb => "CrateDB",
            ServerFamily::Unchecked(name) if name.is_empty() => "this server",
            ServerFamily::Unchecked(name) => name,
        }
    }

    /// `BEGIN READ ONLY` makes the server refuse every write in that transaction, DDL
    /// included: agents' reads and the core's streaming reads rely on it.
    pub fn read_only_transactions(&self) -> bool {
        match self {
            ServerFamily::PostgreSql
            | ServerFamily::CockroachDb
            | ServerFamily::YugabyteDb
            | ServerFamily::RisingWave => true,
            ServerFamily::Materialize | ServerFamily::CrateDb | ServerFamily::Unchecked(_) => false,
        }
    }

    /// `SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` makes every transaction of the
    /// session read-only: a read-only connection relies on it (ADR-053).
    pub fn read_only_sessions(&self) -> bool {
        match self {
            ServerFamily::PostgreSql | ServerFamily::CockroachDb | ServerFamily::YugabyteDb => true,
            ServerFamily::RisingWave
            | ServerFamily::Materialize
            | ServerFamily::CrateDb
            | ServerFamily::Unchecked(_) => false,
        }
    }

    /// Why a read that relies on `BEGIN READ ONLY` is refused here, if it is.
    pub fn read_only_transactions_refusal(&self) -> Option<String> {
        (!self.read_only_transactions()).then(|| {
            format!(
                "{} does not enforce read-only transactions (BEGIN READ ONLY): \
                 SpelaionDB cannot keep this read from writing",
                self.name()
            )
        })
    }

    /// Why a read-only connection is refused here, if it is.
    pub fn read_only_sessions_refusal(&self) -> Option<String> {
        (!self.read_only_sessions()).then(|| {
            format!(
                "read-only mode is not available on {}: it does not enforce a read-only session \
                 (SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY)",
                self.name()
            )
        })
    }
}

/// Asks the server what it is. A server that does not answer is not recognised: it gets no
/// barrier (`Unchecked`).
pub(crate) async fn read_family(connection: &dyn Connection) -> ServerFamily {
    match read_version(connection).await {
        Some(version) => ServerFamily::from_version(&version),
        None => ServerFamily::Unchecked(String::new()),
    }
}

async fn read_version(connection: &dyn Connection) -> Option<String> {
    let mut events = connection
        .execute("select version()", &ExecOptions::default())
        .await
        .ok()?;
    let mut version = None;
    let mut logical = None;
    while let Some(event) = events.next().await {
        match event.ok()? {
            ExecEvent::Columns(schema) => logical = schema.columns.first().map(|c| c.logical),
            ExecEvent::Batch(batch) if version.is_none() && batch.num_rows() > 0 => {
                version = match Value::from_array(batch.column(0).as_ref(), 0, logical?)? {
                    Value::Text(text) => Some(text),
                    _ => None,
                };
            }
            _ => {}
        }
    }
    version
}

#[cfg(test)]
mod tests {
    use super::ServerFamily::{self, *};

    /// `version()` as each server answered it.
    #[test]
    fn servers_are_recognised_from_their_version() {
        for (version, family) in [
            (
                "PostgreSQL 17.6 on x86_64-pc-linux-musl, compiled by gcc (Alpine 14.2.0) 14.2.0, 64-bit",
                PostgreSql,
            ),
            // TimescaleDB is an extension: the server is PostgreSQL.
            (
                "PostgreSQL 17.11 on x86_64-pc-linux-musl, compiled by gcc (Alpine 14.2.0) 14.2.0, 64-bit",
                PostgreSql,
            ),
            (
                "CockroachDB CCL v24.3.37 (x86_64-pc-linux-gnu, built 2026/10/21 02:10:26, go1.22.12 X:nocoverageredesign)",
                CockroachDb,
            ),
            (
                "PostgreSQL 15.12-YB-2026.1.3.0-b0 on x86_64-pc-linux-gnu, compiled by clang version 21.1.1, 64-bit",
                YugabyteDb,
            ),
            (
                "PostgreSQL 13.14.0-RisingWave-3.1.1 (04db7efd159bfe2941cd0aeaf1a42288d49a70ff)",
                RisingWave,
            ),
            (
                "PostgreSQL 9.5 on x86_64-unknown-linux-gnu (Materialize 26.50.1)",
                Materialize,
            ),
            (
                "CrateDB 6.5.1 (built 62722e3/NA, Linux 6.6.87.1-microsoft-standard-WSL2 amd64, OpenJDK 64-Bit Server VM 26.0.2.1+1)",
                CrateDb,
            ),
            // As their documentation shows them: recognised, never checked here.
            (
                "PostgreSQL 8.0.2 on i686-pc-linux-gnu, compiled by GCC gcc (GCC) 3.4.2 20041017 (Red Hat 3.4.2-6.fc3), Redshift 1.0.77467",
                Unchecked("Redshift".into()),
            ),
            (
                "PostgreSQL 12.12 (Greenplum Database 7.1.0 build commit:e7c2b1f14bb42a1018ac57d14f4436880e0a0515) on x86_64-pc-linux-gnu",
                Unchecked("Greenplum".into()),
            ),
            ("", Unchecked(String::new())),
            ("MySQL 8.4", Unchecked("MySQL".into())),
        ] {
            assert_eq!(ServerFamily::from_version(version), family, "{version}");
        }
    }

    #[test]
    fn only_checked_servers_get_barriers() {
        for (family, transactions, sessions) in [
            (PostgreSql, true, true),
            (CockroachDb, true, true),
            (YugabyteDb, true, true),
            (RisingWave, true, false),
            (Materialize, false, false),
            (CrateDb, false, false),
            (Unchecked("Redshift".into()), false, false),
            (Unchecked(String::new()), false, false),
        ] {
            assert_eq!(family.read_only_transactions(), transactions, "{family:?}");
            assert_eq!(family.read_only_sessions(), sessions, "{family:?}");
            assert_eq!(
                family.read_only_transactions_refusal().is_some(),
                !transactions
            );
            assert_eq!(family.read_only_sessions_refusal().is_some(), !sessions);
        }
        assert!(
            Unchecked(String::new())
                .read_only_sessions_refusal()
                .unwrap()
                .contains("this server")
        );
    }
}
