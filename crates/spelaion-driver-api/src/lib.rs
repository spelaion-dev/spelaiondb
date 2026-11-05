//! Common driver contract for SpelaionDB (ADR-005).
//!
//! A driver connects to an engine and returns a [`Connection`]. Results travel as Arrow
//! batches ([`ExecEvent::Batch`]) whose schema carries, per column, the common logical type
//! and the engine's native type ([`ColumnMeta`]): the common part is for rendering and
//! sorting, the native part is never lost.

mod column;
mod dialect;
mod error;
mod filter;
mod introspect;
mod remote;
mod secret;
mod value;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use arrow::array::RecordBatch;
use async_trait::async_trait;
use futures_util::stream::BoxStream;

pub use column::{ColumnMeta, ResultSchema};
pub use dialect::{Dialect, StatementSpan};
pub use error::DriverError;
pub use filter::{NameFilter, literal_pattern, pattern_matches};
pub use introspect::{
    ColumnInfo, ConstraintInfo, ConstraintKind, ForeignKeyInfo, IndexInfo, ObjectInfo, ObjectKind,
    SchemaInfo, SchemaObject, SchemaObjectKind, ServerObject, ServerObjectKind, TableInfo,
    check_definition, terminated,
};
pub use remote::{Remote, RemoteCommand, RemoteControl, RemoteProcess};
pub use secret::Secret;
pub use value::{LogicalType, Value};

pub type Result<T, E = DriverError> = std::result::Result<T, E>;

/// Stream of events for a running statement.
pub type EventStream = BoxStream<'static, Result<ExecEvent>>;

/// What an engine can do. The UI and the core adapt to this, never to the engine's name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub transactions: bool,
    pub cancel: bool,
    pub explain: bool,
    pub editable_results: bool,
    /// Several databases/schemas can be browsed within one connection.
    pub schemas: bool,
}

/// Fully resolved connection parameters (variables and secrets already substituted).
#[derive(Debug, Clone, Default)]
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<Secret>,
    /// Database name, or file path for embedded engines (SQLite).
    pub database: String,
    /// Name the application reports to the server.
    pub application_name: Option<String>,
    pub tls: TlsMode,
    /// PEM certificate of the authority that signs the server's certificate, when it is not
    /// in the system store.
    pub ca_certificate: Option<PathBuf>,
    /// PEM certificate (chain) the client presents to log in without a password (ADR-022),
    /// with its private key. Both or neither.
    pub client_certificate: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    /// Open read-only when the engine supports it natively (SQLite).
    pub read_only: bool,
    /// Actual address to connect to, if it is not `host:port` (for example, the local end
    /// of an SSH tunnel). `host` is still used to verify the TLS certificate.
    pub connect_addr: Option<SocketAddr>,
    /// The database is on another machine and is reached by running a program there
    /// (SQLite on a server, ADR-018); `database` is its path on that machine.
    pub remote: Option<Remote>,
}

/// Connection encryption. Same names and meaning as libpq's `sslmode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsMode {
    /// No encryption.
    Disable,
    /// Encrypt if the server supports it, without verifying its certificate.
    #[default]
    Prefer,
    /// Encryption required, certificate not verified (protects from eavesdropping, not from
    /// impersonation).
    Require,
    /// Encryption required and certificate signed by a trusted authority.
    VerifyCa,
    /// Like `VerifyCa`, and the certificate name must also match the host.
    VerifyFull,
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Rows per Arrow batch.
    pub batch_size: usize,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self { batch_size: 10_000 }
    }
}

/// Event produced while executing a statement, in this order:
/// `Columns` (only if it returns rows) → `Batch`* → `Notice`* → `Done`. A statement that
/// returns several result sets (a SQL Server batch or procedure) repeats `Columns` →
/// `Batch`* for each one before `Done`.
#[derive(Debug, Clone)]
pub enum ExecEvent {
    Columns(Arc<ResultSchema>),
    Batch(RecordBatch),
    Notice(Notice),
    /// `rows_affected` is `None` for statements without a row count (DDL, `SET`, `BEGIN`…).
    Done {
        rows_affected: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub severity: String,
    pub message: String,
}

#[async_trait]
pub trait Driver: Send + Sync {
    /// Stable engine identifier used in the configuration (`engine = "postgres"`).
    fn engine(&self) -> &'static str;
    fn dialect(&self) -> Arc<dyn Dialect>;
    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>>;
}

#[async_trait]
pub trait Connection: Send + Sync {
    fn capabilities(&self) -> &Capabilities;

    /// Executes **one** statement. Splitting a script into statements is the core's job,
    /// using the engine's [`Dialect`].
    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream>;

    /// Handle to cancel the running statement from another task.
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;

    async fn server_version(&self) -> Result<String>;
    /// Current database for engines that support changing it within a session.
    async fn current_database(&self) -> Result<Option<String>> {
        Err(DriverError::Other(
            "changing database is not supported".into(),
        ))
    }
    /// Change this session's database without changing the saved connection.
    async fn select_database(&self, _database: &str) -> Result<()> {
        Err(DriverError::Other(
            "changing database is not supported".into(),
        ))
    }
    /// The databases [`Connection::select_database`] can switch to.
    async fn list_databases(&self) -> Result<Vec<String>> {
        Err(DriverError::Other(
            "changing database is not supported".into(),
        ))
    }
    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>>;
    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>>;
    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo>;
    /// Functions, sequences, triggers, types… of a schema (ADR-024), ordered by name. Engines
    /// without any return none.
    async fn list_schema_objects(&self, _schema: &str) -> Result<Vec<SchemaObject>> {
        Ok(Vec::new())
    }
    /// Roles, logins, tablespaces, extensions… (ADR-024). Engines without any return none.
    async fn list_server_objects(&self) -> Result<Vec<ServerObject>> {
        Ok(Vec::new())
    }
    /// [`Connection::list_schemas`] narrowed to what `filter` keeps (ADR-025). Drivers may
    /// narrow it in their catalog query too; the result must be the same.
    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let mut schemas = self.list_schemas().await?;
        schemas.retain(|s| filter.matches(&s.name));
        Ok(schemas)
    }
    /// [`Connection::list_objects`] narrowed to what `filter` keeps (ADR-025).
    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let mut objects = self.list_objects(schema).await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }
    /// [`Connection::list_schema_objects`] narrowed to what `filter` keeps (ADR-025).
    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let mut objects = self.list_schema_objects(schema).await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }
    /// The DDL of an object from [`Connection::list_schema_objects`], ending in `;`.
    async fn object_definition(&self, _object: &SchemaObject) -> Result<String> {
        Err(DriverError::Other(
            "this engine has no object definitions".into(),
        ))
    }
}

#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> Result<()>;
}
