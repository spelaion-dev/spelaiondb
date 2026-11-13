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

pub mod duckdb;
pub mod mysql;
pub mod postgres;
pub mod sqlite;
pub mod sqlserver;

pub use duckdb::DuckdbDialect;
pub use mysql::MysqlDialect;
pub use postgres::PostgresDialect;
pub use sqlite::SqliteDialect;
pub use sqlserver::SqlServerDialect;

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
        ] {
            let found = for_engine(engine).unwrap_or_else(|| panic!("{engine} has no dialect"));
            assert_eq!(found.name(), dialect, "{engine}");
        }
    }

    #[test]
    fn an_unknown_engine_has_no_dialect() {
        for engine in ["", "oracle", "Postgres", "postgres ", "mysql;", "sqlite\0"] {
            assert!(for_engine(engine).is_none(), "{engine:?}");
        }
    }
}
