//! The other databases of a PostgreSQL connection (ADR-074): which ones it opens, and opening a
//! session on one of them. Every decision is here; the explorer and the tabs only ask.
//!
//! - **What it opens** ([`connection_databases`]): its own database, and the others its file
//!   names (`databases`) or every one the server lists (`all_databases`), read from the
//!   driver's `list_databases` on its own session and narrowed to the file's names, compared
//!   exactly (PostgreSQL's names are case-sensitive). A plugin that cannot list them leaves the
//!   connection with its own database, with the reason.
//! - **Opening one** ([`open_other_database`]): a new session, never a switch (point 4). It is
//!   the connection's own resolved definition with only `ConnectParams.database` changed: the
//!   same host, user, password, TLS and route (the same tunnel of the pool, point 5), the same
//!   read-only mode, masking, connection type and setup statements, through
//!   [`crate::open_connection`] like every session. Its default schema is not set (it is the
//!   connection's own database's): it starts with the server's `search_path`.
//! - **Refused**, each with its own reason ([`OtherDatabaseError`]): an engine or a server that
//!   is not PostgreSQL itself (point 7), a pooler (point 7), a file that names no other
//!   database, a name the file does not name (exactly), a name the server does not list, and a
//!   database another connection with the same login has as its own (point 3), which is named
//!   so the user opens that one instead.
//!
//! Agents never come here (point 8): the MCP server opens the file's `database` only.

use std::sync::Arc;

use spelaion_config::{
    Config, ConnectionId, Engine, OtherDatabases, ResolvedConnection, Resolver, SecretBackend,
    TrustStore, engine_options,
};

use crate::open::{Engines, OpenConnection, OpenError, OpenOptions, open_connection};
use crate::{ResultStore, ServerFamily};

/// The databases a connection opens, its own first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionDatabases {
    /// Only its own: its file names no other, or its engine opens none.
    Own(String),
    /// Its own, then the others it opens that the server lists, in the server's order.
    Several(Vec<String>),
    /// Only its own, because the others could not be listed (an older plugin, a refusal of the
    /// server's), with the reason to show.
    OwnOnly { database: String, reason: String },
}

impl ConnectionDatabases {
    /// Every name it opens, its own first.
    pub fn names(&self) -> Vec<String> {
        match self {
            Self::Own(database) | Self::OwnOnly { database, .. } => vec![database.clone()],
            Self::Several(names) => names.clone(),
        }
    }
}

/// Why a session on another database was not opened.
#[derive(Debug, thiserror::Error)]
pub enum OtherDatabaseError {
    /// Only a PostgreSQL connection opens other databases.
    #[error("only a PostgreSQL connection opens other databases of its server")]
    NotPostgres,
    /// The server is not PostgreSQL itself: its other databases are not checked yet.
    #[error(
        "this server is {family}, not PostgreSQL itself: other databases of its server are not opened yet"
    )]
    OtherFamily { family: String },
    /// A pooler routes by its own list of names, not the server's.
    #[error(
        "this connection goes through a pooler, which routes by its own list of names: it opens only its own database"
    )]
    Pooler,
    /// The file names no other database.
    #[error(
        "this connection opens only its own database: name the others in its `databases`, or set `all_databases`"
    )]
    NoneNamed,
    /// Not one of the names the file lists (compared exactly: PostgreSQL's names are
    /// case-sensitive).
    #[error("{database} is not one of the databases this connection names")]
    NotNamed { database: String },
    /// Not a database the server lists for this user (a template, one that refuses
    /// connections, one without `CONNECT`, or none of that name).
    #[error("{database} is not a database this server lists for this user")]
    NotListed { database: String },
    /// Another connection with the same login has it as its own database: it opens only as
    /// that connection, with that connection's settings.
    #[error("{database} has a connection of its own, {name}: open it there")]
    HasOwnConnection {
        database: String,
        connection: ConnectionId,
        name: String,
    },
    /// The server's list could not be read, so the name could not be checked.
    #[error("the server's databases could not be listed: {0}")]
    Unlisted(String),
    #[error("{0}")]
    Open(#[from] OpenError),
}

/// The databases `own` opens: its own, and the others its file names or the server lists.
pub async fn connection_databases(own: &OpenConnection) -> ConnectionDatabases {
    let database = own.resolved.params.database.clone();
    if own.resolved.engine != Engine::Postgres
        || matches!(own.resolved.other_databases, OtherDatabases::None)
    {
        return ConnectionDatabases::Own(database);
    }
    if let Err(refusal) = server_refusal(own) {
        return ConnectionDatabases::OwnOnly {
            database,
            reason: refusal.to_string(),
        };
    }
    let listed = match own.session.connection().list_databases().await {
        Ok(listed) => listed,
        Err(e) => {
            return ConnectionDatabases::OwnOnly {
                database,
                reason: format!("the server's databases could not be listed: {e}"),
            };
        }
    };
    let mut names = vec![database.clone()];
    names.extend(
        listed
            .into_iter()
            .filter(|name| *name != database && named(&own.resolved.other_databases, name)),
    );
    ConnectionDatabases::Several(names)
}

/// The configuration a connection was resolved from: its other connections are read from it,
/// to keep a database that has a connection of its own to that connection (ADR-074 point 3).
pub struct Configuration<'a> {
    pub config: &'a Config,
    pub trust: &'a TrustStore,
    pub backend: &'a dyn SecretBackend,
}

/// Opens a session on `database` of `own`'s connection, as ADR-074 allows, or says why not.
/// `configuration` is what `own` was resolved from. `options`
/// are those of a new session of `own` (its typed password, its tunnels), and `results` the
/// store its results go to. Asking for its own database opens it as the connection does.
pub async fn open_other_database(
    own: &OpenConnection,
    database: &str,
    configuration: &Configuration<'_>,
    engines: &Engines,
    options: OpenOptions,
    results: Arc<ResultStore>,
) -> Result<OpenConnection, OtherDatabaseError> {
    if database == own.resolved.params.database {
        return Ok(open_connection(own.resolved.clone(), engines, options, results).await?);
    }
    server_refusal(own)?;
    match &own.resolved.other_databases {
        OtherDatabases::None => return Err(OtherDatabaseError::NoneNamed),
        OtherDatabases::Named(names) if !names.iter().any(|n| n == database) => {
            return Err(OtherDatabaseError::NotNamed {
                database: database.to_string(),
            });
        }
        OtherDatabases::Named(_) | OtherDatabases::All => {}
    }
    if let Some((connection, name)) = owner(configuration, &own.resolved, database) {
        return Err(OtherDatabaseError::HasOwnConnection {
            database: database.to_string(),
            connection,
            name,
        });
    }
    // Only what the server lists for this user: never a template or a database that refuses
    // connections, even under `all_databases`.
    let listed = own
        .session
        .connection()
        .list_databases()
        .await
        .map_err(|e| OtherDatabaseError::Unlisted(e.to_string()))?;
    if !listed.iter().any(|n| n == database) {
        return Err(OtherDatabaseError::NotListed {
            database: database.to_string(),
        });
    }
    Ok(open_connection(
        on_database(&own.resolved, database),
        engines,
        options,
        results,
    )
    .await?)
}

/// The connection's resolved definition on `database`: only the database changes, and the
/// default schema, which belongs to its own database (ADR-074 point 4), is left unset.
pub(crate) fn on_database(own: &ResolvedConnection, database: &str) -> ResolvedConnection {
    let mut resolved = own.clone();
    resolved.params.database = database.to_string();
    resolved.session.schema = None;
    resolved
}

/// What refuses any other database of `own`'s connection: its engine, a pooler, its server.
fn server_refusal(own: &OpenConnection) -> Result<(), OtherDatabaseError> {
    // The family was asked to the server when the connection opened, never taken from the
    // file (SDB-169).
    refusal(
        own.resolved.engine,
        own.resolved
            .params
            .options
            .contains_key(engine_options::POOLER),
        own.session.server_family(),
    )
}

fn refusal(
    engine: Engine,
    pooled: bool,
    family: Option<&ServerFamily>,
) -> Result<(), OtherDatabaseError> {
    if engine != Engine::Postgres {
        return Err(OtherDatabaseError::NotPostgres);
    }
    if pooled {
        return Err(OtherDatabaseError::Pooler);
    }
    match family {
        Some(ServerFamily::PostgreSql) => Ok(()),
        Some(family) => Err(OtherDatabaseError::OtherFamily {
            family: family.name().to_string(),
        }),
        None => Err(OtherDatabaseError::OtherFamily {
            family: "a server whose kind could not be read".into(),
        }),
    }
}

/// Whether the file's choice takes `name` (exactly).
fn named(choice: &OtherDatabases, name: &str) -> bool {
    match choice {
        OtherDatabases::None => false,
        OtherDatabases::Named(names) => names.iter().any(|n| n == name),
        OtherDatabases::All => true,
    }
}

/// Another connection of the configuration, with the same login as `own` (engine, server,
/// user and route), whose own database is `database`. One that does not resolve (a repository
/// change not reviewed yet, a variable not defined) is compared by what its file says (engine,
/// host, port, user): when in doubt, the database stays with it.
fn owner(
    configuration: &Configuration<'_>,
    own: &ResolvedConnection,
    database: &str,
) -> Option<(ConnectionId, String)> {
    let resolver = Resolver::new(
        configuration.config,
        configuration.trust,
        configuration.backend,
    );
    configuration
        .config
        .connections
        .iter()
        .filter(|entry| entry.id != own.id && entry.file.engine == own.engine)
        .find(|entry| match resolver.resolve(&entry.id) {
            Ok(other) => {
                other.params.database == database && crate::session_source::same_login(own, &other)
            }
            Err(_) => {
                let file = &entry.file;
                file.database.as_deref() == Some(database)
                    && file.host.as_deref() == Some(own.params.host.as_str())
                    && file.port.unwrap_or(own.params.port) == own.params.port
                    && file.user.as_deref() == Some(own.params.user.as_str())
            }
        })
        .map(|entry| (entry.id.clone(), entry.name()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_postgresql_itself_without_a_pooler_opens_other_databases() {
        let postgres = ServerFamily::PostgreSql;
        assert!(refusal(Engine::Postgres, false, Some(&postgres)).is_ok());
        assert!(matches!(
            refusal(Engine::Mysql, false, Some(&ServerFamily::MySql)),
            Err(OtherDatabaseError::NotPostgres)
        ));
        assert!(matches!(
            refusal(Engine::Postgres, true, Some(&postgres)),
            Err(OtherDatabaseError::Pooler)
        ));
        for family in [
            ServerFamily::CockroachDb,
            ServerFamily::YugabyteDb,
            ServerFamily::RisingWave,
            ServerFamily::Materialize,
            ServerFamily::CrateDb,
            ServerFamily::Unchecked("Greenplum".into()),
        ] {
            let refused = refusal(Engine::Postgres, false, Some(&family));
            assert!(
                matches!(&refused, Err(OtherDatabaseError::OtherFamily { family: f }) if f == family.name()),
                "{family:?}: {refused:?}"
            );
        }
        // A server whose kind could not be read is not taken for PostgreSQL.
        assert!(matches!(
            refusal(Engine::Postgres, false, None),
            Err(OtherDatabaseError::OtherFamily { .. })
        ));
    }

    #[test]
    fn the_files_names_are_compared_exactly() {
        let list = OtherDatabases::Named(vec!["billing".into()]);
        assert!(named(&list, "billing"));
        assert!(!named(&list, "Billing"));
        assert!(!named(&list, "billing "));
        assert!(named(&OtherDatabases::All, "anything"));
        assert!(!named(&OtherDatabases::None, "billing"));
    }
}
