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
    CancelHandle, Capabilities, ConnectParams, Connection, Dialect, Driver, DriverError,
    EventStream, ExecEvent, ExecOptions, LikeFilter, NameFilter, ObjectInfo, Result, SchemaInfo,
    SchemaObject, TableInfo,
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
    Schemas(LikeFilter, Reply<Vec<SchemaInfo>>),
    Objects(String, LikeFilter, Reply<Vec<ObjectInfo>>),
    Describe(String, String, Reply<TableInfo>),
    SchemaObjects(String, LikeFilter, Reply<Vec<SchemaObject>>),
    Definition(Box<SchemaObject>, Reply<String>),
}

/// Loop of the connection thread. Ends when the `DuckdbConnection` is dropped.
fn actor(conn: &duckdb::Connection, mut inbox: mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = inbox.blocking_recv() {
        match command {
            Command::Execute {
                sql,
                batch_size,
                events,
            } => {
                if let Err(e) = exec::execute(conn, &sql, batch_size, &events) {
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
            Command::Objects(schema, like, reply) => {
                let _ = reply.send(introspect::list_objects(conn, &schema, &like));
            }
            Command::Describe(schema, name, reply) => {
                let _ = reply.send(introspect::describe_table(conn, &schema, &name));
            }
            Command::SchemaObjects(schema, like, reply) => {
                let _ = reply.send(objects::list_schema_objects(conn, &schema, &like));
            }
            Command::Definition(object, reply) => {
                let _ = reply.send(objects::object_definition(conn, &object));
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

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        let schema = schema.to_string();
        self.ask(|reply| Command::SchemaObjects(schema, LikeFilter::default(), reply))
            .await
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        let object = Box::new(object.clone());
        self.ask(|reply| Command::Definition(object, reply)).await
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
