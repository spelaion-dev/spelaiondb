//! Database engines shown when adding a connection, grouped like other clients do. Only
//! the ones with a driver can be picked; the rest announce what is planned (ADR-005).

use spelunk_config::Engine;

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

/// How a connection reaches the database: over the network, or opening a local file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineAccess {
    Network,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    /// Configuration name (`engine = "postgres"`) or, if not supported yet, a stable id.
    pub id: &'static str,
    pub name: &'static str,
    pub category: EngineCategory,
    pub access: EngineAccess,
    pub default_port: Option<u16>,
    /// `Some` when SpelunkDB has a driver for it: it can be used in a connection.
    pub engine: Option<Engine>,
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
    };
    vec![
        supported(Engine::Postgres, "PostgreSQL", Relational, Network),
        supported(Engine::Sqlite, "SQLite", Relational, File),
        planned("mysql", "MySQL", Relational, Network, Some(3306)),
        planned("mariadb", "MariaDB", Relational, Network, Some(3306)),
        planned("sqlserver", "SQL Server", Relational, Network, Some(1433)),
        planned("oracle", "Oracle", Relational, Network, Some(1521)),
        planned(
            "cockroachdb",
            "CockroachDB",
            Relational,
            Network,
            Some(26257),
        ),
        planned("duckdb", "DuckDB", Analytical, File, None),
        planned("clickhouse", "ClickHouse", Analytical, Network, Some(8123)),
        planned("snowflake", "Snowflake", Analytical, Network, None),
        planned("bigquery", "BigQuery", Analytical, Network, None),
        planned("mongodb", "MongoDB", NoSql, Network, Some(27017)),
        planned("redis", "Redis", NoSql, Network, Some(6379)),
        planned("cassandra", "Cassandra", NoSql, Network, Some(9042)),
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
                assert_eq!(info.default_port, engine.default_port());
                assert_eq!(info.access == EngineAccess::File, engine == Engine::Sqlite);
            }
        }
        // Both engines of the configuration format appear exactly once.
        for engine in [Engine::Postgres, Engine::Sqlite] {
            assert_eq!(
                catalog.iter().filter(|e| e.engine == Some(engine)).count(),
                1
            );
        }
    }
}
