//! The database a statement runs in, where the engine has one to tell apart from the connection
//! (SDB-32): what the query history and the MCP server's audit record beside each statement.
//!
//! On the engines whose sessions switch database (`USE`), it is the session's current one, read
//! from the driver's local snapshot (`Connection::database_context`, which runs no SQL); on
//! PostgreSQL it is the connection's own database (agents never open another, ADR-074); every
//! other engine has none to record (a file, a single catalog, or no databases at all).

use spelaion_config::Engine;

use crate::open::OpenConnection;

/// Where an engine's statement database comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatabaseSource {
    /// The session's current database: it changes with `USE` (MySQL, MariaDB, SQL Server,
    /// DuckDB's attached catalogs).
    Session,
    /// The connection's configured database, which a session never leaves.
    Connection,
    /// Nothing to record.
    None,
}

/// Where `engine`'s statement database comes from.
pub fn database_source(engine: Engine) -> DatabaseSource {
    match engine {
        Engine::Mysql | Engine::Mariadb | Engine::Sqlserver | Engine::Duckdb => {
            DatabaseSource::Session
        }
        Engine::Postgres => DatabaseSource::Connection,
        Engine::Sqlite
        | Engine::Mongodb
        | Engine::Clickhouse
        | Engine::Redis
        | Engine::Trino
        | Engine::Presto
        | Engine::Oracle
        | Engine::Libsql
        | Engine::Cassandra
        | Engine::Tdengine
        | Engine::Flightsql
        | Engine::Dremio
        | Engine::Gizmosql
        | Engine::Neo4j
        | Engine::Hive
        | Engine::Spark
        | Engine::Kyuubi
        | Engine::Impala
        | Engine::Databend
        | Engine::Spanner
        | Engine::Elasticsearch
        | Engine::Opensearch
        | Engine::Solr
        | Engine::Meilisearch => DatabaseSource::None,
    }
}

/// Whether `engine`'s sessions switch database, so a statement's database is read from the
/// session rather than the connection.
pub fn switches_database(engine: Engine) -> bool {
    database_source(engine) == DatabaseSource::Session
}

/// The database a statement just run on `open` ran in, or `None` where there is none to record
/// (or the driver does not know). Runs no SQL.
pub async fn statement_database(open: &OpenConnection) -> Option<String> {
    let database = match database_source(open.resolved.engine) {
        DatabaseSource::Session => open
            .session
            .connection()
            .database_context()
            .await
            .ok()
            .flatten(),
        DatabaseSource::Connection => Some(open.resolved.params.database.clone()),
        DatabaseSource::None => None,
    };
    database.filter(|database| !database.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_engines_that_switch_database_read_it_from_the_session() {
        let session: Vec<Engine> = Engine::ALL
            .into_iter()
            .filter(|engine| switches_database(*engine))
            .collect();
        assert_eq!(
            session,
            [
                Engine::Mysql,
                Engine::Mariadb,
                Engine::Duckdb,
                Engine::Sqlserver
            ]
        );
        assert_eq!(
            database_source(Engine::Postgres),
            DatabaseSource::Connection
        );
        assert_eq!(database_source(Engine::Sqlite), DatabaseSource::None);
    }
}
