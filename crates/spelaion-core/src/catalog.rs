//! Database engines shown when adding a connection, grouped like other clients do. Only
//! the ones with a driver can be picked; the rest announce what is planned (ADR-005).
//!
//! A server that speaks another engine's protocol is a kind of that engine's connection, not
//! an engine of its own (SDB-169, SDB-170, ADR-042): CockroachDB is a PostgreSQL connection with
//! CockroachDB's port, user and database to start from. What differs in it is read from the
//! server when a session opens (`server_family.rs`), never from the entry it was added from.

use spelaion_config::Engine;

/// Family an engine is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineCategory {
    Relational,
    Analytical,
    NoSql,
}

impl EngineCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineCategory::Relational => "relational",
            EngineCategory::Analytical => "analytical",
            EngineCategory::NoSql => "nosql",
        }
    }
}

/// How a connection reaches the database: over the network, opening a local file, or
/// reading a local folder of files (SDB-316).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineAccess {
    Network,
    File,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    /// Configuration name (`engine = "postgres"`) or, if not supported yet, a stable id.
    pub id: &'static str,
    pub name: &'static str,
    pub category: EngineCategory,
    pub access: EngineAccess,
    pub default_port: Option<u16>,
    /// `Some` when SpelaionDB has a driver for it: it can be used in a connection. For a kind
    /// of another engine's connection (`id` is not `engine`'s name), the engine its
    /// connections are saved as.
    pub engine: Option<Engine>,
    /// What the form starts from for a kind: the server's usual user and database.
    pub default_user: Option<&'static str>,
    pub default_database: Option<&'static str>,
}

impl EngineInfo {
    /// A server reached through another engine's connection (SDB-169).
    pub fn is_kind(&self) -> bool {
        self.engine.is_some_and(|engine| engine.as_str() != self.id)
    }
}

const fn planned(
    id: &'static str,
    name: &'static str,
    category: EngineCategory,
    access: EngineAccess,
    default_port: Option<u16>,
) -> EngineInfo {
    EngineInfo {
        id,
        name,
        category,
        access,
        default_port,
        engine: None,
        default_user: None,
        default_database: None,
    }
}

/// A server that speaks `engine`'s protocol, checked against it (SDB-169).
const fn kind_of(
    engine: Engine,
    id: &'static str,
    name: &'static str,
    category: EngineCategory,
    default_port: u16,
    default_user: Option<&'static str>,
    default_database: Option<&'static str>,
) -> EngineInfo {
    EngineInfo {
        id,
        name,
        category,
        access: EngineAccess::Network,
        default_port: Some(default_port),
        engine: Some(engine),
        default_user,
        default_database,
    }
}

/// Every engine in the "add connection" picker, in display order within each category.
pub fn engine_catalog() -> Vec<EngineInfo> {
    use EngineAccess::{File, Network};
    use EngineCategory::{Analytical, NoSql, Relational};
    let supported = |engine: Engine, name, category, access| EngineInfo {
        id: engine.as_str(),
        name,
        category,
        access,
        default_port: engine.default_port(),
        engine: Some(engine),
        default_user: None,
        default_database: None,
    };
    vec![
        supported(Engine::Postgres, "PostgreSQL", Relational, Network),
        supported(Engine::Sqlite, "SQLite", Relational, File),
        supported(Engine::Mysql, "MySQL", Relational, Network),
        supported(Engine::Mariadb, "MariaDB", Relational, Network),
        // MySQL's protocol, checked with each server's image (`testing/docker/mysql-family`).
        kind_of(
            Engine::Mysql,
            "tidb",
            "TiDB",
            Relational,
            4000,
            Some("root"),
            None,
        ),
        kind_of(
            Engine::Mysql,
            "oceanbase",
            "OceanBase",
            Relational,
            2881,
            Some("root"),
            None,
        ),
        kind_of(
            Engine::Mysql,
            "singlestore",
            "SingleStore",
            Analytical,
            3306,
            Some("root"),
            None,
        ),
        supported(Engine::Sqlserver, "SQL Server", Relational, Network),
        supported(Engine::Oracle, "Oracle", Relational, Network),
        // SQLite's SQL over HTTP: libSQL's server and Turso (ADR-058).
        supported(Engine::Libsql, "libSQL / Turso", Relational, Network),
        // PostgreSQL's protocol, checked with each server's image (`testing/docker/pg-family`).
        kind_of(
            Engine::Postgres,
            "cockroachdb",
            "CockroachDB",
            Relational,
            26257,
            Some("root"),
            Some("defaultdb"),
        ),
        kind_of(
            Engine::Postgres,
            "yugabytedb",
            "YugabyteDB",
            Relational,
            5433,
            Some("yugabyte"),
            Some("yugabyte"),
        ),
        kind_of(
            Engine::Postgres,
            "timescaledb",
            "TimescaleDB",
            Analytical,
            5432,
            Some("postgres"),
            Some("postgres"),
        ),
        supported(Engine::Duckdb, "DuckDB", Analytical, File),
        // CSV, TSV and Parquet files read through DuckDB: a DuckDB connection whose target is a
        // folder, saved with `file_folder = true` (SDB-316).
        EngineInfo {
            id: "file-folder",
            name: "CSV / Parquet folder",
            category: Analytical,
            access: EngineAccess::Folder,
            default_port: None,
            engine: Some(Engine::Duckdb),
            default_user: None,
            default_database: None,
        },
        supported(Engine::Clickhouse, "ClickHouse", Analytical, Network),
        supported(Engine::Trino, "Trino", Analytical, Network),
        supported(Engine::Presto, "Presto", Analytical, Network),
        supported(Engine::Flightsql, "Arrow Flight SQL", Analytical, Network),
        supported(Engine::Dremio, "Dremio", Analytical, Network),
        supported(Engine::Gizmosql, "GizmoSQL", Analytical, Network),
        planned("snowflake", "Snowflake", Analytical, Network, None),
        planned("bigquery", "BigQuery", Analytical, Network, None),
        supported(Engine::Mongodb, "MongoDB", NoSql, Network),
        supported(Engine::Redis, "Redis", NoSql, Network),
        // Redis's protocol, checked with its image (`testing/docker`).
        kind_of(Engine::Redis, "valkey", "Valkey", NoSql, 6379, None, None),
        supported(Engine::Cassandra, "Cassandra", NoSql, Network),
        // Cassandra's protocol, checked with its image (`testing/docker`).
        kind_of(
            Engine::Cassandra,
            "scylladb",
            "ScyllaDB",
            NoSql,
            9042,
            None,
            None,
        ),
        supported(Engine::Tdengine, "TDengine", Analytical, Network),
        // Cypher over the HTTP Query API (ADR-056).
        supported(Engine::Neo4j, "Neo4j", NoSql, Network),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::Engines;

    #[test]
    fn every_supported_engine_has_a_driver_and_ids_are_unique() {
        let catalog = engine_catalog();
        let ids: HashSet<_> = catalog.iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), catalog.len());
        let engines = Engines::builtin();
        for info in &catalog {
            if let Some(engine) = info.engine {
                assert!(engines.get(engine).is_some(), "{} has no driver", info.id);
                assert_eq!(info.access != EngineAccess::Network, engine.is_file());
                if info.access == EngineAccess::Folder {
                    assert_eq!((info.id, engine), ("file-folder", Engine::Duckdb));
                } else if info.is_kind() {
                    assert!(info.default_port.is_some(), "{}", info.id);
                } else {
                    assert_eq!(info.default_port, engine.default_port());
                    assert_eq!((info.default_user, info.default_database), (None, None));
                }
            }
        }
        // Every engine of the configuration format appears exactly once as itself.
        for engine in [
            Engine::Postgres,
            Engine::Sqlite,
            Engine::Mysql,
            Engine::Mariadb,
            Engine::Sqlserver,
            Engine::Duckdb,
            Engine::Mongodb,
            Engine::Redis,
            Engine::Cassandra,
            Engine::Tdengine,
            Engine::Neo4j,
        ] {
            assert_eq!(
                catalog
                    .iter()
                    .filter(|e| e.engine == Some(engine) && !e.is_kind())
                    .count(),
                1
            );
        }
    }

    /// SDB-169, SDB-170: the servers checked against PostgreSQL's and MySQL's protocols are
    /// kinds of those engines' connections, starting from their own port.
    #[test]
    fn protocol_kinds_are_their_engines_connections() {
        let catalog = engine_catalog();
        let kinds: Vec<_> = catalog
            .iter()
            .filter(|e| e.is_kind() && e.access == EngineAccess::Network)
            .map(|e| (e.id, e.engine, e.default_port))
            .collect();
        assert_eq!(
            kinds,
            [
                ("tidb", Some(Engine::Mysql), Some(4000)),
                ("oceanbase", Some(Engine::Mysql), Some(2881)),
                ("singlestore", Some(Engine::Mysql), Some(3306)),
                ("cockroachdb", Some(Engine::Postgres), Some(26257)),
                ("yugabytedb", Some(Engine::Postgres), Some(5433)),
                ("timescaledb", Some(Engine::Postgres), Some(5432)),
                ("valkey", Some(Engine::Redis), Some(6379)),
                ("scylladb", Some(Engine::Cassandra), Some(9042)),
            ]
        );
    }

    /// ADR-041: the core reads SQL with its own dialect; a driver has none to give.
    #[test]
    fn every_supported_engine_has_the_cores_dialect() {
        for info in engine_catalog() {
            let Some(engine) = info.engine else { continue };
            let dialect =
                crate::dialect_of(engine).unwrap_or_else(|| panic!("{} has no dialect", info.id));
            // MariaDB reads SQL as MySQL does and Presto as Trino does; every other engine's dialect
            // bears its name.
            let expected = match engine {
                Engine::Mariadb => "mysql",
                // libSQL's server reads SQL as SQLite does (ADR-058).
                Engine::Libsql => "sqlite",
                Engine::Presto => "trino",
                other => other.as_str(),
            };
            assert_eq!(dialect.name(), expected, "{}", info.id);
        }
    }
}
