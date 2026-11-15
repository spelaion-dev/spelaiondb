//! Servers that speak PostgreSQL's or MySQL's protocol (SDB-169, SDB-170). A connection of any
//! of them is a PostgreSQL or MySQL connection, served by that engine's plugin; which server it
//! reached is read from the server itself when a session opens (`version()`, and MySQL's
//! `@@version_comment`), never from the connection's file or the plugin.
//!
//! What it decides is which of the barriers the core asks of that engine the server really
//! enforces (ADR-020, ADR-053). A server that accepts a read-only transaction and still writes
//! gets no agents' reads and no streaming reads; one that accepts the session's read-only mode
//! and ignores it gets no read-only connections. A PostgreSQL fork that was never checked here,
//! or a server that does not say it is PostgreSQL, gets neither: the default is closed.
//!
//! MySQL has no such default: the text of a MySQL server names its distributor, not MySQL
//! ("Source distribution", "Percona Server", a cloud's own), so the MySQL family is told apart
//! by the forks' names, and a read-only connection's session mode is read back from every
//! server after it is set ([`read_only_confirmed`]): a fork that accepts it and stays writable
//! (StarRocks) is caught there whatever it calls itself.

use futures_util::StreamExt;
use spelaion_driver_api::{Connection, ExecEvent, ExecOptions, Value};

/// The server a PostgreSQL or MySQL connection reached, as it names itself.
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
    /// A PostgreSQL fork whose barriers were not checked here (its name), or a server whose
    /// version does not start with PostgreSQL's name.
    Unchecked(String),
    /// MySQL, and what does not name itself as one of the forks below (managed MySQL services,
    /// Percona Server).
    MySql,
    MariaDb,
    /// Its MySQL mode: as MySQL, a read-only transaction stops every write but DDL, which
    /// commits it first (checked with 4.3.5).
    OceanBase,
    /// `READ ONLY` answers that it is only a no-op, which a setting can switch on (checked with
    /// 8.5).
    TiDb,
    /// No `START TRANSACTION READ ONLY`; `SET SESSION TRANSACTION READ ONLY` is accepted and
    /// leaves the session writable (checked with the 8.x developer image).
    SingleStore,
    /// As SingleStore (checked with 3.0.5).
    Doris,
}

/// PostgreSQL forks that name themselves in their `version()` and were not checked: their
/// barriers are not assumed to be PostgreSQL's.
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

fn contains(text: &str, name: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&name.to_ascii_lowercase())
}

impl ServerFamily {
    /// Reads a PostgreSQL server's `select version()`.
    pub fn from_version(version: &str) -> Self {
        let version = version.trim();
        let has = |name: &str| contains(version, name);
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

    /// Reads a MySQL server's `select version(), @@version_comment`.
    pub fn from_mysql_version(version: &str, comment: &str) -> Self {
        if contains(version, "TiDB") {
            ServerFamily::TiDb
        } else if contains(version, "OceanBase") {
            ServerFamily::OceanBase
        } else if contains(version, "MariaDB") {
            ServerFamily::MariaDb
        } else if contains(comment, "SingleStore") || contains(comment, "MemSQL") {
            ServerFamily::SingleStore
        } else if contains(comment, "Doris") {
            ServerFamily::Doris
        } else {
            ServerFamily::MySql
        }
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
            ServerFamily::MySql => "MySQL",
            ServerFamily::MariaDb => "MariaDB",
            ServerFamily::OceanBase => "OceanBase",
            ServerFamily::TiDb => "TiDB",
            ServerFamily::SingleStore => "SingleStore",
            ServerFamily::Doris => "Apache Doris",
        }
    }

    fn speaks_mysql(&self) -> bool {
        match self {
            ServerFamily::MySql
            | ServerFamily::MariaDb
            | ServerFamily::OceanBase
            | ServerFamily::TiDb
            | ServerFamily::SingleStore
            | ServerFamily::Doris => true,
            ServerFamily::PostgreSql
            | ServerFamily::CockroachDb
            | ServerFamily::YugabyteDb
            | ServerFamily::RisingWave
            | ServerFamily::Materialize
            | ServerFamily::CrateDb
            | ServerFamily::Unchecked(_) => false,
        }
    }

    /// The engine's read-only transaction stops the writes the core relies on it for (every
    /// write on PostgreSQL; every write but DDL on MySQL, whose DDL the classifier refuses
    /// before it is sent, ADR-020): agents' reads and the core's streaming reads rely on it.
    pub fn read_only_transactions(&self) -> bool {
        match self {
            ServerFamily::PostgreSql
            | ServerFamily::CockroachDb
            | ServerFamily::YugabyteDb
            | ServerFamily::RisingWave
            | ServerFamily::MySql
            | ServerFamily::MariaDb
            | ServerFamily::OceanBase => true,
            ServerFamily::Materialize
            | ServerFamily::CrateDb
            | ServerFamily::Unchecked(_)
            | ServerFamily::TiDb
            | ServerFamily::SingleStore
            | ServerFamily::Doris => false,
        }
    }

    /// The session's read-only mode makes every transaction of the session read-only: a
    /// read-only connection relies on it (ADR-053).
    pub fn read_only_sessions(&self) -> bool {
        match self {
            ServerFamily::PostgreSql
            | ServerFamily::CockroachDb
            | ServerFamily::YugabyteDb
            | ServerFamily::MySql
            | ServerFamily::MariaDb
            | ServerFamily::OceanBase => true,
            ServerFamily::RisingWave
            | ServerFamily::Materialize
            | ServerFamily::CrateDb
            | ServerFamily::Unchecked(_)
            | ServerFamily::TiDb
            | ServerFamily::SingleStore
            | ServerFamily::Doris => false,
        }
    }

    fn transaction_statement(&self) -> &'static str {
        if self.speaks_mysql() {
            "START TRANSACTION READ ONLY"
        } else {
            "BEGIN READ ONLY"
        }
    }

    fn session_statement(&self) -> &'static str {
        if self.speaks_mysql() {
            "SET SESSION TRANSACTION READ ONLY"
        } else {
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"
        }
    }

    /// Why a read that relies on the read-only transaction is refused here, if it is.
    pub fn read_only_transactions_refusal(&self) -> Option<String> {
        (!self.read_only_transactions()).then(|| {
            format!(
                "{} does not enforce read-only transactions ({}): \
                 SpelaionDB cannot keep this read from writing",
                self.name(),
                self.transaction_statement()
            )
        })
    }

    /// Why a read-only connection is refused here, if it is.
    pub fn read_only_sessions_refusal(&self) -> Option<String> {
        (!self.read_only_sessions()).then(|| {
            format!(
                "read-only mode is not available on {}: it does not enforce a read-only session \
                 ({})",
                self.name(),
                self.session_statement()
            )
        })
    }
}

/// Asks the server what it is, by the dialect its connection reads SQL with; `None` for an
/// engine that is not PostgreSQL's or MySQL's. A server that does not answer is not
/// recognised: on PostgreSQL it gets no barrier (`Unchecked`), on MySQL it is taken for MySQL
/// and its session mode is still read back.
pub(crate) async fn read_family(
    connection: &dyn Connection,
    dialect: &str,
) -> Option<ServerFamily> {
    match dialect {
        "postgres" => Some(match first_row(connection, "select version()").await {
            Some(row) => ServerFamily::from_version(row.first().map_or("", String::as_str)),
            None => ServerFamily::Unchecked(String::new()),
        }),
        "mysql" => {
            let row = first_row(connection, "select version(), @@version_comment")
                .await
                .unwrap_or_default();
            let column = |i: usize| row.get(i).map_or("", String::as_str);
            Some(ServerFamily::from_mysql_version(column(0), column(1)))
        }
        _ => None,
    }
}

/// After a read-only connection's session was made read-only, whether the server says it is:
/// a server that accepted the statement and kept the session writable (StarRocks) is caught
/// here, whatever it calls itself. `true` for an engine with no such mode.
pub(crate) async fn read_only_confirmed(connection: &dyn Connection, dialect: &str) -> bool {
    match dialect {
        "postgres" => first_row(
            connection,
            "select current_setting('transaction_read_only')",
        )
        .await
        .is_some_and(|row| row.first().is_some_and(|v| v == "on")),
        "mysql" => {
            // `transaction_read_only` since MySQL 5.7.20 and MariaDB 11.1; `tx_read_only` before.
            let row = match first_row(connection, "select @@session.transaction_read_only").await {
                Some(row) => Some(row),
                None => first_row(connection, "select @@session.tx_read_only").await,
            };
            row.is_some_and(|row| row.first().is_some_and(|v| v == "1"))
        }
        _ => true,
    }
}

/// The first row of `sql`, each value as text (NULL as empty); `None` if it fails or is empty.
async fn first_row(connection: &dyn Connection, sql: &str) -> Option<Vec<String>> {
    let mut events = connection
        .execute(sql, &ExecOptions::default())
        .await
        .ok()?;
    let mut row = None;
    let mut logicals = Vec::new();
    while let Some(event) = events.next().await {
        match event.ok()? {
            ExecEvent::Columns(schema) => {
                logicals = schema.columns.iter().map(|c| c.logical).collect();
            }
            ExecEvent::Batch(batch) if row.is_none() && batch.num_rows() > 0 => {
                let mut values = Vec::with_capacity(batch.num_columns());
                for (i, logical) in logicals.iter().enumerate().take(batch.num_columns()) {
                    values.push(
                        match Value::from_array(batch.column(i).as_ref(), 0, *logical)? {
                            Value::Null => String::new(),
                            Value::Text(text) => text,
                            other => other.to_string(),
                        },
                    );
                }
                row = Some(values);
            }
            _ => {}
        }
    }
    row
}

#[cfg(test)]
mod tests {
    use super::ServerFamily::{self, *};

    /// `version()` as each server answered it.
    #[test]
    fn postgres_servers_are_recognised_from_their_version() {
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

    /// `version()` and `@@version_comment` as each server answered them.
    #[test]
    fn mysql_servers_are_recognised_from_their_version_and_comment() {
        for (version, comment, family) in [
            ("8.4.13", "MySQL Community Server - GPL", MySql),
            // Managed MySQL and Percona name their distributor, not MySQL.
            ("8.0.39", "Source distribution", MySql),
            (
                "8.0.36-28",
                "Percona Server (GPL), Release 28, Revision 47601f19",
                MySql,
            ),
            (
                "11.4.13-MariaDB-ubu2404",
                "mariadb.org binary distribution",
                MariaDb,
            ),
            ("5.5.5-10.11.6-MariaDB", "MariaDB Server", MariaDb),
            (
                "8.0.11-TiDB-v8.5.4",
                "TiDB Server (Apache License 2.0) Community Edition, MySQL 8.0 compatible",
                TiDb,
            ),
            (
                "5.7.25-OceanBase_CE-v4.3.5.6",
                "OceanBase_CE 4.3.5.6 (r106000012026040916-560923b02bdb9ed88dbd7940ac97a2e183c09b29) (Built Apr  9 2026 17:14:55)",
                OceanBase,
            ),
            (
                "5.7.32",
                "SingleStoreDB source distribution (compatible; MySQL Enterprise & MySQL Commercial)",
                SingleStore,
            ),
            ("5.7.99", "Doris version doris-3.0.5-rc01-e277cfb83f", Doris),
            // StarRocks does not say what it is: its session mode is read back instead.
            ("8.0.33", "3.4.5-1207fa7", MySql),
            ("", "", MySql),
        ] {
            assert_eq!(
                ServerFamily::from_mysql_version(version, comment),
                family,
                "{version} / {comment}"
            );
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
            (MySql, true, true),
            (MariaDb, true, true),
            (OceanBase, true, true),
            (TiDb, false, false),
            (SingleStore, false, false),
            (Doris, false, false),
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
        // Each family's refusal names its own engine's statement.
        assert!(
            TiDb.read_only_transactions_refusal()
                .unwrap()
                .contains("START TRANSACTION READ ONLY")
        );
        assert!(
            Materialize
                .read_only_transactions_refusal()
                .unwrap()
                .contains("BEGIN READ ONLY")
        );
        assert!(
            SingleStore
                .read_only_sessions_refusal()
                .unwrap()
                .contains("SET SESSION TRANSACTION READ ONLY")
        );
    }
}
