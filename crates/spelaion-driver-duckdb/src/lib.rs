//! DuckDB driver for SpelaionDB.
//!
//! DuckDB is embedded and synchronous, like SQLite: each connection lives in its own thread
//! (an actor) and the rest of the program talks to it through channels.
//!
//! A DuckDB file can be opened only once per process (a second open fails on the file lock),
//! while the app opens several connections to the same database (one per tab, one for the
//! explorer). So the first connection to a file opens the database and keeps a root handle;
//! the others are clones of it. The file is closed when the last connection closes.

mod dialect;
mod exec;
mod format;
mod introspect;
mod objects;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use duckdb::{AccessMode, Config, InterruptHandle};
use spelaion_driver_api::{
    CancelHandle, Capabilities, ConnectParams, Connection, ContextDialect, DatabaseContext,
    Dialect, Driver, DriverError, EventStream, ExecEvent, ExecOptions, InsertTarget, LikeFilter,
    NameFilter, ObjectInfo, ReferencingKey, Result, SchemaInfo, SchemaObject, TableInfo,
    TransactionState,
};
use tokio::sync::{mpsc, oneshot};

pub use dialect::DuckdbDialect;

/// A database opened in this process: the handle the connections are cloned from.
type Root = Arc<Mutex<duckdb::Connection>>;

struct Opened {
    read_only: bool,
    root: Root,
    clients: Arc<AtomicUsize>,
    actors: usize,
}

/// Open databases by file.
static OPEN: LazyLock<(Mutex<HashMap<PathBuf, Opened>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashMap::new()), Condvar::new()));

/// Tracks connections visible to callers, independently of their closing threads.
struct ClientLease(Arc<AtomicUsize>);

impl Drop for ClientLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Keeps the root alive until the actor has closed its cloned connection.
struct ActorLease(PathBuf);

impl Drop for ActorLease {
    fn drop(&mut self) {
        if let Ok(mut open) = OPEN.0.lock() {
            if let Some(entry) = open.get_mut(&self.0) {
                entry.actors -= 1;
                if entry.actors == 0 {
                    open.remove(&self.0);
                }
            }
            OPEN.1.notify_all();
        }
    }
}

/// Creates an empty DuckDB database at `path`. An existing file is never touched.
pub fn create_database(path: &Path) -> Result<()> {
    if path.exists() {
        return Err(DriverError::Other(format!(
            "{} already exists: a new database never replaces a file",
            path.display()
        )));
    }
    let conn = duckdb::Connection::open(path)
        .map_err(|e| DriverError::Other(format!("could not create {}: {e}", path.display())))?;
    // Writes the header, so the file is a database before anything else uses it.
    conn.execute_batch("checkpoint").map_err(exec::map_error)?;
    Ok(())
}

/// DuckDB driver. `ConnectParams::database` is the file path (or `:memory:`).
#[derive(Debug, Default, Clone)]
pub struct DuckdbDriver {
    /// Create the file if it does not exist. Off by default: opening a mistyped path must not
    /// silently create an empty database.
    pub create_if_missing: bool,
}

/// Opens the database or clones the connection that already has it open.
fn open(
    path: &str,
    read_only: bool,
    create: bool,
) -> Result<(Option<ActorLease>, duckdb::Connection, Option<ClientLease>)> {
    if path == ":memory:" {
        let conn = duckdb::Connection::open_in_memory().map_err(exec::map_error)?;
        return Ok((None, conn, None));
    }
    let file = Path::new(path);
    if !file.exists() && !(create && !read_only) {
        return Err(DriverError::Connection(format!("{path} does not exist")));
    }
    // The same file through another spelling (`./x.duckdb`) is the same lock. A file about to
    // be created is named by its canonical folder, as it will be once it exists.
    let key = std::fs::canonicalize(file)
        .or_else(|_| {
            let folder = file
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::canonicalize(folder).map(|f| f.join(file.file_name().unwrap_or_default()))
        })
        .unwrap_or_else(|_| file.to_path_buf());
    let mut open = OPEN
        .0
        .lock()
        .map_err(|_| DriverError::Other("the DuckDB registry is poisoned".into()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while let Some(entry) = open.get(&key) {
        if entry.read_only == read_only {
            break;
        }
        if entry.clients.load(Ordering::Acquire) > 0 {
            return Err(DriverError::Connection(format!(
                "{path} is already open {} in SpelaionDB; DuckDB opens a file in one mode at a time",
                if entry.read_only {
                    "read-only"
                } else {
                    "for writing"
                }
            )));
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(DriverError::Connection(format!(
                "{path} is still closing; try opening it again"
            )));
        };
        open = OPEN
            .1
            .wait_timeout(open, remaining)
            .map_err(|_| DriverError::Other("the DuckDB registry is poisoned".into()))?
            .0;
    }
    if let Entry::Vacant(entry) = open.entry(key.clone()) {
        let config = Config::default()
            .access_mode(if read_only {
                AccessMode::ReadOnly
            } else {
                AccessMode::ReadWrite
            })
            .map_err(exec::map_error)?;
        let root = duckdb::Connection::open_with_flags(file, config).map_err(exec::map_error)?;
        entry.insert(Opened {
            read_only,
            root: Arc::new(Mutex::new(root)),
            clients: Arc::new(AtomicUsize::new(0)),
            actors: 0,
        });
    }
    let entry = open.get_mut(&key).expect("database was inserted");
    let conn = match entry.root.lock() {
        Ok(root) => root.try_clone().map_err(exec::map_error),
        Err(_) => Err(DriverError::Other("a DuckDB connection is poisoned".into())),
    };
    let conn = match conn {
        Ok(conn) => conn,
        Err(error) => {
            if entry.actors == 0 {
                open.remove(&key);
            }
            return Err(error);
        }
    };
    entry.actors += 1;
    entry.clients.fetch_add(1, Ordering::AcqRel);
    let client = ClientLease(Arc::clone(&entry.clients));
    Ok((Some(ActorLease(key)), conn, Some(client)))
}

#[async_trait]
impl Driver for DuckdbDriver {
    fn engine(&self) -> &'static str {
        "duckdb"
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(DuckdbDialect)
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let path = params.database.clone();
        let read_only = params.read_only;
        let create = self.create_if_missing;
        let (ready_tx, ready_rx) = oneshot::channel();
        let (commands, inbox) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name(format!("duckdb {path}"))
            .spawn(move || {
                let (actor_lease, conn, client_lease) = match open(&path, read_only, create) {
                    Ok(opened) => opened,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok((conn.interrupt_handle(), client_lease)));
                actor(&conn, inbox);
                // The connection goes before the root, so the root is the last to close the file.
                drop(conn);
                drop(actor_lease);
            })
            .map_err(|e| DriverError::Other(format!("could not spawn the DuckDB thread: {e}")))?;

        let (interrupt, client_lease) = ready_rx.await.map_err(|_| {
            DriverError::Connection("the DuckDB thread ended while opening".into())
        })??;
        Ok(Box::new(DuckdbConnection {
            _lease: client_lease,
            commands,
            cancel: Arc::new(DuckdbCancel(interrupt)),
            capabilities: Capabilities {
                transactions: true,
                cancel: true,
                explain: true,
                editable_results: false,
                schemas: true, // main and attached databases (ATTACH)
                insert_rows: true,
            },
        }))
    }
}

type Reply<T> = oneshot::Sender<Result<T>>;

enum Command {
    Execute {
        sql: String,
        batch_size: usize,
        events: mpsc::Sender<Result<ExecEvent>>,
    },
    Version(Reply<String>),
    CurrentDatabase(Reply<String>),
    DatabaseContext(Reply<Option<String>>),
    Schemas(LikeFilter, Reply<Vec<SchemaInfo>>),
    Objects(String, LikeFilter, Reply<Vec<ObjectInfo>>),
    Describe(String, String, Reply<TableInfo>),
    Referencing(String, String, Reply<Vec<ReferencingKey>>),
    SchemaObjects(String, LikeFilter, Reply<Vec<SchemaObject>>),
    Definition(Box<SchemaObject>, Reply<String>),
    RelationDefinition(String, String, Reply<String>),
    /// Whether a transaction is open (ADR-031).
    TransactionState(Reply<TransactionState>),
    /// Rows of an import (ADR-038), through DuckDB's appender.
    Append(InsertTarget, Vec<Vec<Option<String>>>, Reply<u64>),
}

/// Loop of the connection thread. Ends when the `DuckdbConnection` is dropped.
fn actor(conn: &duckdb::Connection, mut inbox: mpsc::UnboundedReceiver<Command>) {
    // Initialization happens before the actor accepts any user SQL.
    let mut context = DatabaseContext::new(
        ContextDialect::Duckdb,
        introspect::current_database(conn).ok(),
    );
    while let Some(command) = inbox.blocking_recv() {
        match command {
            Command::Execute {
                sql,
                batch_size,
                events,
            } => {
                let result = exec::execute(conn, &sql, batch_size, &events);
                context.observe(&sql, result.is_ok());
                if result.is_err() && !matches!(&result, Err(DriverError::Database { .. })) {
                    context.refresh(None);
                }
                if let Err(e) = result {
                    let _ = events.blocking_send(Err(e));
                }
            }
            Command::Version(reply) => {
                let _ = reply.send(
                    conn.query_row("select version()", [], |r| r.get(0))
                        .map_err(exec::map_error),
                );
            }
            Command::Schemas(like, reply) => {
                let _ = reply.send(introspect::list_schemas(conn, &like));
            }
            Command::CurrentDatabase(reply) => {
                let result = introspect::current_database(conn);
                context.refresh(result.as_ref().ok().cloned());
                let _ = reply.send(result);
            }
            Command::DatabaseContext(reply) => {
                let _ = reply.send(Ok(context.snapshot()));
            }
            Command::Objects(schema, like, reply) => {
                let _ = reply.send(introspect::list_objects(conn, &schema, &like));
            }
            Command::Describe(schema, name, reply) => {
                let _ = reply.send(introspect::describe_table(conn, &schema, &name));
            }
            Command::Referencing(schema, name, reply) => {
                let _ = reply.send(introspect::list_referencing_keys(conn, &schema, &name));
            }
            Command::SchemaObjects(schema, like, reply) => {
                let _ = reply.send(objects::list_schema_objects(conn, &schema, &like));
            }
            Command::Definition(object, reply) => {
                let _ = reply.send(objects::object_definition(conn, &object));
            }
            Command::RelationDefinition(schema, name, reply) => {
                let _ = reply.send(objects::relation_definition(conn, &schema, &name));
            }
            Command::TransactionState(reply) => {
                let _ = reply.send(transaction_state(conn));
            }
            Command::Append(target, rows, reply) => {
                let _ = reply.send(append(conn, &target, rows));
            }
        }
    }
}

/// DuckDB's appender, given the table's columns by name: values arrive as text and DuckDB casts
/// them to each column's type. It writes in the connection's open transaction, so a rollback
/// takes the rows back.
fn append(
    conn: &duckdb::Connection,
    target: &InsertTarget,
    rows: Vec<Vec<Option<String>>>,
) -> Result<u64> {
    let schema = if target.schema.is_empty() {
        "main"
    } else {
        target.schema.as_str()
    };
    let columns: Vec<&str> = target.columns.iter().map(String::as_str).collect();
    let mut appender = conn
        .appender_with_columns_to_db(&target.table, schema, &columns)
        .map_err(exec::map_error)?;
    let count = rows.len() as u64;
    for row in rows {
        appender
            .append_row(duckdb::appender_params_from_iter(row))
            .map_err(exec::map_error)?;
    }
    appender.flush().map_err(exec::map_error)?;
    Ok(count)
}

/// duckdb-rs does not expose DuckDB's auto-commit flag (its `is_autocommit` always says yes),
/// so the transaction id answers: outside a transaction every statement is a transaction of its
/// own with a new id; inside one, both statements see the same. In-process, so it costs
/// microseconds. A transaction that failed refuses any statement until it is rolled back.
fn transaction_state(conn: &duckdb::Connection) -> Result<TransactionState> {
    let id = || conn.query_row("select txid_current()", [], |r| r.get::<_, u64>(0));
    match (id(), id()) {
        (Ok(first), Ok(second)) => Ok(if first == second {
            TransactionState::Open
        } else {
            TransactionState::Idle
        }),
        (Err(e), _) | (_, Err(e)) => {
            if e.to_string().contains("transaction is aborted") {
                Ok(TransactionState::Failed)
            } else {
                Err(exec::map_error(e))
            }
        }
    }
}

struct DuckdbConnection {
    // Drop the client lease before closing the command channel.
    _lease: Option<ClientLease>,
    commands: mpsc::UnboundedSender<Command>,
    cancel: Arc<DuckdbCancel>,
    capabilities: Capabilities,
}

impl DuckdbConnection {
    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.commands.send(make(tx)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }
}

fn closed() -> DriverError {
    DriverError::Connection("the DuckDB connection is closed".into())
}

#[async_trait]
impl Connection for DuckdbConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        // Bounded channel: if the UI does not consume, the DuckDB thread waits instead of filling memory.
        let (events, rx) = mpsc::channel(4);
        self.commands
            .send(Command::Execute {
                sql: sql.to_string(),
                batch_size: opts.batch_size.max(1),
                events,
            })
            .map_err(|_| closed())?;
        Ok(Box::pin(futures_util::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|event| (event, rx)) },
        )))
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    async fn server_version(&self) -> Result<String> {
        self.ask(Command::Version).await
    }

    async fn current_database(&self) -> Result<Option<String>> {
        self.ask(Command::CurrentDatabase).await.map(Some)
    }
    async fn database_context(&self) -> Result<Option<String>> {
        self.ask(Command::DatabaseContext).await
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        self.ask(Command::TransactionState).await
    }

    /// DuckDB's appender: no SQL at all, the values are the appender's data.
    async fn insert_rows(
        &self,
        target: &InsertTarget,
        rows: Vec<Vec<Option<String>>>,
    ) -> Result<u64> {
        target.check_rows(&rows)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let target = target.clone();
        self.ask(|reply| Command::Append(target, rows, reply)).await
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        self.ask(|reply| Command::Schemas(LikeFilter::default(), reply))
            .await
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        let schema = schema.to_string();
        self.ask(|reply| Command::Objects(schema, LikeFilter::default(), reply))
            .await
    }

    // Narrowed in the catalog query, then matched exactly (ADR-025).
    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let like = filter.server_side();
        let mut schemas = self.ask(|reply| Command::Schemas(like, reply)).await?;
        schemas.retain(|s| filter.matches(&s.name));
        Ok(schemas)
    }

    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let (schema, like) = (schema.to_string(), filter.server_side());
        let mut objects = self
            .ask(|reply| Command::Objects(schema, like, reply))
            .await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let (schema, like) = (schema.to_string(), filter.server_side());
        let mut objects = self
            .ask(|reply| Command::SchemaObjects(schema, like, reply))
            .await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.ask(|reply| Command::Describe(schema, name, reply))
            .await
    }

    async fn list_referencing_keys(&self, schema: &str, name: &str) -> Result<Vec<ReferencingKey>> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.ask(|reply| Command::Referencing(schema, name, reply))
            .await
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        let schema = schema.to_string();
        self.ask(|reply| Command::SchemaObjects(schema, LikeFilter::default(), reply))
            .await
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        let object = Box::new(object.clone());
        self.ask(|reply| Command::Definition(object, reply)).await
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.ask(|reply| Command::RelationDefinition(schema, name, reply))
            .await
    }
}

struct DuckdbCancel(Arc<InterruptHandle>);

#[async_trait]
impl CancelHandle for DuckdbCancel {
    async fn cancel(&self) -> Result<()> {
        self.0.interrupt();
        Ok(())
    }
}
