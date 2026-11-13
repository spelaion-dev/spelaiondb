//! Common driver contract for SpelaionDB (ADR-005).
//!
//! A driver connects to an engine and returns a [`Connection`]. Results travel as Arrow
//! batches ([`ExecEvent::Batch`]) whose schema carries, per column, the common logical type
//! and the engine's native type ([`ColumnMeta`]): the common part is for rendering and
//! sorting, the native part is never lost.

mod column;
mod database_context;
mod dialect;
mod error;
mod filter;
mod introspect;
mod remote;
mod rows;
mod secret;
pub mod thread_cancel;
mod value;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use arrow::array::RecordBatch;
use async_trait::async_trait;
use futures_util::stream::BoxStream;

pub use column::{ColumnMeta, ColumnOrigin, ResultSchema};
pub use database_context::{ContextDialect, DatabaseContext};
pub use dialect::{Dialect, StatementSpan, code_ranges};
pub use error::DriverError;
pub use filter::{LikeFilter, NameFilter, literal_pattern, pattern_matches};
pub use introspect::{
    ColumnInfo, ConstraintInfo, ConstraintKind, ForeignKeyInfo, IndexInfo, ObjectInfo, ObjectKind,
    ReferencingKey, SchemaColumn, SchemaInfo, SchemaObject, SchemaObjectKind, ServerObject,
    ServerObjectKind, TableInfo, check_definition, terminated,
};
pub use remote::{Remote, RemoteCommand, RemoteControl, RemoteProcess};
pub use rows::{TextRows, TextRowsBuilder};
pub use secret::Secret;
pub use value::{LogicalType, Value};

pub type Result<T, E = DriverError> = std::result::Result<T, E>;

/// Stream of events for a running statement.
pub type EventStream = BoxStream<'static, Result<ExecEvent>>;

/// What an engine can do. The UI and the core adapt to this, never to the engine's name.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Capabilities {
    pub transactions: bool,
    pub cancel: bool,
    pub explain: bool,
    pub editable_results: bool,
    /// Several databases/schemas can be browsed within one connection.
    pub schemas: bool,
    /// [`Connection::insert_rows`] works: data can be imported (ADR-038).
    pub insert_rows: bool,
    /// [`Connection::execute_bound`] works: a read can take values bound as parameters (the
    /// data search of ADR-043).
    pub bound_params: bool,
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
    /// How long connecting may take: the TCP connection, TLS and the login. `None` for
    /// [`ConnectParams::DEFAULT_CONNECT_TIMEOUT`] (ADR-032).
    pub connect_timeout: Option<std::time::Duration>,
}

impl ConnectParams {
    pub const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    /// The connect timeout that applies.
    pub fn connect_timeout(&self) -> std::time::Duration {
        self.connect_timeout
            .unwrap_or(Self::DEFAULT_CONNECT_TIMEOUT)
    }
}

/// Connection encryption. Same names and meaning as libpq's `sslmode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
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
    /// Report each column's origin table and column (`ColumnMeta::origin`, ADR-052) even
    /// where that costs a catalog read (PostgreSQL). Engines that get it for free always
    /// report it. The core asks for it when the session masks columns.
    pub origins: bool,
}

/// How an engine-specific plan request treats the original statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanMode {
    /// Compile only; the original statement must not run.
    Estimated,
    /// Execute the statement and return its measured plan.
    Analyze {
        /// The statement produces a user result set before its actual plan (for example,
        /// SELECT without INTO or DML with OUTPUT). This prevents treating that user result
        /// as the appended plan when its column happens to use the engine's plan name.
        query_result: bool,
    },
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            batch_size: 10_000,
            origins: false,
        }
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Notice {
    pub severity: String,
    pub message: String,
}

#[async_trait]
pub trait Driver: Send + Sync {
    /// Stable engine identifier used in the configuration (`engine = "postgres"`).
    fn engine(&self) -> &str;
    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>>;

    /// Creates an empty database for an engine whose databases are files (SQLite, DuckDB).
    /// `database` is what [`ConnectParams::database`] will be: the file's path. An existing
    /// file is never touched. Every other engine refuses.
    async fn create_database(&self, database: &str) -> Result<()> {
        let _ = database;
        Err(DriverError::Other(format!(
            "{} is not a file",
            self.engine()
        )))
    }
}

/// A shared driver is a driver: what holds an `Arc<dyn Driver>` (the core's engines) can be
/// used wherever one is asked for.
#[async_trait]
impl<D: Driver + ?Sized> Driver for Arc<D> {
    fn engine(&self) -> &str {
        (**self).engine()
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        (**self).connect(params).await
    }

    async fn create_database(&self, database: &str) -> Result<()> {
        (**self).create_database(database).await
    }
}

#[async_trait]
pub trait Connection: Send + Sync {
    fn capabilities(&self) -> &Capabilities;

    /// Executes **one** statement. Splitting a script into statements is the core's job,
    /// using the engine's [`Dialect`].
    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream>;

    /// Executes an engine's multi-batch plan protocol as one indivisible request. The core
    /// must validate the statement, read-only policy and confirmations before calling this.
    /// Drivers must restore temporary session settings even on error, cancellation or drop.
    async fn execute_plan(&self, _sql: &str, _mode: PlanMode) -> Result<EventStream> {
        Err(DriverError::Other(
            "this engine has no multi-batch plan protocol".into(),
        ))
    }

    /// Executes **one** read with `params` bound to its placeholders, never written into the
    /// SQL text (ADR-043). Every value is text, and the SQL converts it where it needs another
    /// type. The placeholders are [`bound_placeholder`]'s; a placeholder may appear more than
    /// once. Results and errors are those of [`Connection::execute`]. Engines that cannot bind
    /// parameters say so in [`Capabilities::bound_params`].
    async fn execute_bound(
        &self,
        _sql: &str,
        _params: Vec<String>,
        _opts: &ExecOptions,
    ) -> Result<EventStream> {
        Err(DriverError::Other(
            "this connection cannot bind parameters".into(),
        ))
    }

    /// Handle to cancel the running statement from another task.
    fn cancel_handle(&self) -> Arc<dyn CancelHandle>;

    async fn server_version(&self) -> Result<String>;
    /// Keeps an idle session alive with the engine's cheapest round trip (ADR-028), and says
    /// whether a transaction is open so the core stops sending them. Engines that cannot tell
    /// do not support it: an idle session in a forgotten transaction must not be kept alive.
    async fn keep_alive(&self) -> Result<KeepAlive> {
        Err(DriverError::Other("this engine has no keep-alive".into()))
    }
    /// Whether a transaction is open on this session, as the server says it (ADR-031). Where the
    /// protocol carries it, no round trip is made. Engines that cannot tell do not support it:
    /// the session then stays in auto-commit.
    async fn transaction_state(&self) -> Result<TransactionState> {
        Err(DriverError::Other(
            "this engine cannot tell whether a transaction is open".into(),
        ))
    }
    /// The connection is known to be closed (the server ended it, the network dropped it),
    /// without asking the server.
    fn is_closed(&self) -> bool {
        false
    }
    /// Current database for engines that support changing it within a session.
    async fn current_database(&self) -> Result<Option<String>> {
        Err(DriverError::Other(
            "changing database is not supported".into(),
        ))
    }
    /// A local snapshot for history observers: never executes SQL or changes ROW_COUNT,
    /// diagnostics, transaction state or query results. `None` means unknown/unset.
    async fn database_context(&self) -> Result<Option<String>> {
        Ok(None)
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
    /// The foreign keys of every table (in any schema) that point to `schema.name`, ordered by
    /// referencing schema, table and key name: [`TableInfo::foreign_keys`] seen from the other
    /// side. Read from the catalog, like `describe_table`.
    async fn list_referencing_keys(
        &self,
        _schema: &str,
        _name: &str,
    ) -> Result<Vec<ReferencingKey>> {
        Err(DriverError::Other(
            "this engine cannot list the keys that point to a table".into(),
        ))
    }
    /// Every column of every relation of [`Connection::list_objects`] in `schema`, ordered by
    /// table and then by position: the catalog the metadata search keeps (ADR-043). Drivers
    /// read it with one catalog query; this default describes each relation.
    async fn list_columns(&self, schema: &str) -> Result<Vec<SchemaColumn>> {
        let mut columns = Vec::new();
        for object in self.list_objects(schema).await? {
            let table = self.describe_table(schema, &object.name).await?;
            columns.extend(table.columns.into_iter().map(|c| SchemaColumn {
                table: object.name.clone(),
                name: c.name,
                native_type: c.native_type,
            }));
        }
        Ok(columns)
    }
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
    /// The DDL that creates a relation of [`Connection::list_objects`]: `CREATE TABLE` with
    /// its constraints, then its indexes and comments, or `CREATE VIEW`. Statements end in
    /// `;` and are separated by a blank line.
    async fn relation_definition(&self, _schema: &str, _name: &str) -> Result<String> {
        Err(DriverError::Other(
            "this engine has no table definitions".into(),
        ))
    }
    /// The SQL that calls a procedure or a function of [`Connection::list_schema_objects`]:
    /// its input parameters as `:name` placeholders, a procedure's output ones collected and
    /// shown, a function's result selected (`SELECT f(:a)`, or `SELECT * FROM` one that returns
    /// a table).
    async fn procedure_call(&self, _object: &SchemaObject) -> Result<String> {
        Err(DriverError::Other("this engine has no procedures".into()))
    }
    /// Inserts `rows` into a table (data imports, ADR-038). Every value travels as a bound
    /// parameter or as the data of the engine's bulk path (PostgreSQL's `COPY … FROM STDIN`),
    /// never written into SQL text: only the quoted table and column names are. Values are
    /// text, or `None` for NULL, and the server converts them to each column's type. They come
    /// as columns of text ([`TextRows`]), read in place: the same rows whether the driver is
    /// compiled in or a plugin. It runs inside whatever transaction the session has open and
    /// opens none itself. Returns the rows inserted.
    async fn insert_rows(&self, _target: &InsertTarget, _rows: TextRows) -> Result<u64> {
        Err(DriverError::Other(
            "this connection cannot import rows".into(),
        ))
    }
}

/// The table and columns [`Connection::insert_rows`] writes to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InsertTarget {
    /// Empty for the session's default schema.
    pub schema: String,
    pub table: String,
    /// Every row has one value per column, in this order.
    pub columns: Vec<String>,
}

impl InsertTarget {
    /// `schema.table` quoted for `dialect` (no schema when it is empty, or `main` on SQLite and
    /// DuckDB).
    pub fn qualified(&self, dialect: &dyn Dialect) -> String {
        let main = matches!(dialect.name(), "sqlite" | "duckdb") && self.schema == "main";
        if self.schema.is_empty() || main {
            dialect.quote_identifier(&self.table)
        } else {
            format!(
                "{}.{}",
                dialect.quote_identifier(&self.schema),
                dialect.quote_identifier(&self.table)
            )
        }
    }

    /// `(a, b, c)` quoted for `dialect`.
    pub fn column_list(&self, dialect: &dyn Dialect) -> String {
        let names: Vec<String> = self
            .columns
            .iter()
            .map(|c| dialect.quote_identifier(c))
            .collect();
        format!("({})", names.join(", "))
    }

    /// Every row has one value per column.
    pub fn check_rows(&self, rows: &TextRows) -> Result<()> {
        if rows.width() == self.columns.len() {
            Ok(())
        } else {
            Err(DriverError::Other(format!(
                "a row has {} values for {} columns",
                rows.width(),
                self.columns.len()
            )))
        }
    }

    /// `rows` cut into the multi-row statements an engine accepts: at most `max_values` bound
    /// values (its parameter limit), `max_rows` rows (its `VALUES` limit) and about
    /// `max_bytes` of data each (its packet size), and always at least one row.
    pub fn statement_chunks(
        &self,
        rows: &TextRows,
        max_values: usize,
        max_rows: usize,
        max_bytes: usize,
    ) -> Vec<TextRows> {
        let width = self.columns.len().max(1);
        let per_statement = (max_values / width).clamp(1, max_rows.max(1));
        let mut chunks = Vec::new();
        // The chunk being filled: where it starts, and its bytes so far.
        let mut start = 0usize;
        let mut bytes = 0usize;
        for row in 0..rows.len() {
            let size = rows.row_bytes(row);
            let filled = row - start;
            if filled > 0 && (filled >= per_statement || bytes + size > max_bytes) {
                chunks.push(rows.slice(start, filled));
                start = row;
                bytes = 0;
            }
            bytes += size;
        }
        if start < rows.len() {
            chunks.push(rows.slice(start, rows.len() - start));
        }
        chunks
    }

    /// `INSERT INTO t (a, b) VALUES (…), (…)` for `rows` rows, with `placeholder(n)` (from 1)
    /// for each value: the multi-row statements of the engines that bind parameters.
    pub fn insert_sql(
        &self,
        dialect: &dyn Dialect,
        rows: usize,
        placeholder: impl Fn(usize) -> String,
    ) -> String {
        let width = self.columns.len();
        let mut sql = format!(
            "INSERT INTO {} {} VALUES ",
            self.qualified(dialect),
            self.column_list(dialect)
        );
        for row in 0..rows {
            if row > 0 {
                sql.push_str(", ");
            }
            sql.push('(');
            for column in 0..width {
                if column > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&placeholder(row * width + column + 1));
            }
            sql.push(')');
        }
        sql
    }
}

/// The `n`th (from 1) placeholder of [`Connection::execute_bound`] for an engine: `$1`
/// (PostgreSQL, DuckDB), `@P1` (SQL Server), `?1` (SQLite) and `@spelaion_p1` for MySQL and
/// MariaDB, whose driver binds the value to that user variable with a prepared `SET` and then
/// runs the read with the text protocol, so its values decode as every other result's.
pub fn bound_placeholder(engine: &str, n: usize) -> String {
    match engine {
        "sqlserver" => format!("@P{n}"),
        "sqlite" => format!("?{n}"),
        "mysql" | "mariadb" => format!("@spelaion_p{n}"),
        _ => format!("${n}"),
    }
}

/// `:name` for a parameter, with anything but letters, digits and `_` turned into `_` (and
/// SQL Server's leading `@` left out); `:argN` for one without a name.
pub fn parameter_placeholder(name: &str, position: usize) -> String {
    let name: String = name
        .trim_start_matches('@')
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        format!(":arg{position}")
    } else {
        format!(":{name}")
    }
}

/// What [`Connection::keep_alive`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KeepAlive {
    /// The server answered and no transaction is open.
    Alive,
    /// A transaction is open: the core sends no more keep-alives until the session is used.
    InTransaction,
}

/// What [`Connection::transaction_state`] found (ADR-031).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    /// No transaction is open: each statement commits as it runs.
    Idle,
    /// A transaction is open: its changes wait for `COMMIT` or `ROLLBACK`.
    Open,
    /// A transaction is open but failed: the engine will only roll it back.
    Failed,
}

#[async_trait]
pub trait CancelHandle: Send + Sync {
    async fn cancel(&self) -> Result<()>;
}
