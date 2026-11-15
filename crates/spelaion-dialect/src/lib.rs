//! Each engine's SQL dialect: how a script is split into statements, how a name is quoted and
//! which parts of a script are code ([`Dialect`], ADR-005).
//!
//! ADR-041: a driver is the wire, not the rules. What decides whether something may run is the
//! core's, compiled into it, and is never asked of the program that talks to the server; the
//! dialects are the part of those rules that reads each engine's SQL text. They live here, in a
//! crate with no network or native dependency, and the core takes an engine's dialect from
//! [`for_engine`], never from its driver. A driver uses its engine's module only for what it
//! writes itself (quoting the names of its catalog queries, the words it must recognise in a
//! statement it is about to send).

use std::sync::Arc;

use spelaion_driver_api::Dialect;

pub mod clickhouse;
pub mod cql;
pub mod duckdb;
pub mod mongodb;
pub mod mysql;
pub mod neo4j;
pub mod oracle;
pub mod postgres;
pub mod redis;
pub mod sqlite;
pub mod sqlserver;
pub mod standard;
pub mod tdengine;
pub mod trino;

pub use clickhouse::ClickhouseDialect;
pub use cql::CqlDialect;
pub use duckdb::DuckdbDialect;
pub use mongodb::MongodbDialect;
pub use mysql::MysqlDialect;
pub use neo4j::Neo4jDialect;
pub use oracle::OracleDialect;
pub use postgres::PostgresDialect;
pub use redis::RedisDialect;
pub use sqlite::SqliteDialect;
pub use sqlserver::SqlServerDialect;
pub use standard::StandardDialect;
pub use tdengine::TdengineDialect;
pub use trino::TrinoDialect;

/// The dialect of an engine, by the name the configuration uses (`engine = "postgres"`).
/// `None` for an engine the core has no dialect for: nothing is split, classified or guarded
/// for it, so nothing that depends on those may be offered on it (ADR-041).
pub fn for_engine(engine: &str) -> Option<Arc<dyn Dialect>> {
    match engine {
        "postgres" => Some(Arc::new(PostgresDialect)),
        // One dialect for both servers; a session's `sql_mode` gives its own
        // (`Dialect::with_sql_mode`).
        "mysql" | "mariadb" => Some(Arc::new(MysqlDialect)),
        "sqlite" => Some(Arc::new(SqliteDialect)),
        "sqlserver" => Some(Arc::new(SqlServerDialect)),
        "duckdb" => Some(Arc::new(DuckdbDialect)),
        // Not SQL: the core reads it and sends requests (ADR-056).
        "mongodb" => Some(Arc::new(MongodbDialect)),
        "clickhouse" => Some(Arc::new(ClickhouseDialect)),
        "redis" => Some(Arc::new(RedisDialect)),
        // One dialect for both: Presto is where Trino came from, and their grammar is one.
        "trino" | "presto" => Some(Arc::new(TrinoDialect)),
        "oracle" => Some(Arc::new(OracleDialect)),
        // Cypher, read by the core (ADR-056).
        "neo4j" => Some(Arc::new(Neo4jDialect)),
        // libSQL's server reads SQL as SQLite does (ADR-058).
        "libsql" => Some(Arc::new(SqliteDialect)),
        // Shaped like SQL, but its own: sent as text, read here (ADR-056).
        "cassandra" => Some(Arc::new(CqlDialect)),
        // Shaped like SQL, but its own: sent as text, read here (ADR-056).
        "tdengine" => Some(Arc::new(TdengineDialect)),
        // Flight SQL: standard SQL under each engine's own name, which no barrier answers to.
        "flightsql" => Some(Arc::new(StandardDialect::named("flightsql"))),
        "dremio" => Some(Arc::new(StandardDialect::named("dremio"))),
        "gizmosql" => Some(Arc::new(StandardDialect::named("gizmosql"))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_engine_of_the_configuration_has_its_dialect() {
        for (engine, dialect) in [
            ("postgres", "postgres"),
            ("mysql", "mysql"),
            ("mariadb", "mysql"),
            ("sqlite", "sqlite"),
            ("sqlserver", "sqlserver"),
            ("duckdb", "duckdb"),
            ("mongodb", "mongodb"),
            ("clickhouse", "clickhouse"),
            ("redis", "redis"),
            ("trino", "trino"),
            ("presto", "trino"),
            ("oracle", "oracle"),
            ("cassandra", "cassandra"),
            ("tdengine", "tdengine"),
            ("flightsql", "flightsql"),
            ("dremio", "dremio"),
            ("gizmosql", "gizmosql"),
            ("neo4j", "neo4j"),
        ] {
            let found = for_engine(engine).unwrap_or_else(|| panic!("{engine} has no dialect"));
            assert_eq!(found.name(), dialect, "{engine}");
        }
    }

    #[test]
    fn only_a_language_that_is_not_sql_reads_requests() {
        // ADR-056: a SQL engine's statements are sent as text; only MongoDB's and Redis's become requests.
        for engine in [
            "postgres",
            "mysql",
            "mariadb",
            "sqlite",
            "sqlserver",
            "duckdb",
        ] {
            let dialect = for_engine(engine).unwrap();
            assert!(!dialect.reads_requests(), "{engine}");
            assert!(dialect.to_request("select 1", "db").is_none(), "{engine}");
        }
        let mongodb = for_engine("mongodb").unwrap();
        assert!(mongodb.reads_requests());
        assert!(matches!(
            mongodb.to_request("db.c.find()", "db"),
            Some(Ok(_))
        ));
        let redis = for_engine("redis").unwrap();
        assert!(redis.reads_requests());
        assert!(matches!(redis.to_request("GET k", "0"), Some(Ok(_))));
    }

    #[test]
    fn an_unknown_engine_has_no_dialect() {
        for engine in [
            "",
            "snowflake",
            "Oracle",
            "Postgres",
            "postgres ",
            "mysql;",
            "sqlite\0",
        ] {
            assert!(for_engine(engine).is_none(), "{engine:?}");
        }
    }
}
