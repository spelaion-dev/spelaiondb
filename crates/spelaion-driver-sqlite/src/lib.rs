//! SQLite driver for SpelaionDB.
//!
//! `rusqlite` is synchronous: each connection lives in its own thread (an actor) and the rest
//! of the program talks to it through channels. That way a long query never blocks the async
//! runtime.

mod catalog;
mod dialect;
mod exec;
mod introspect;
mod remote;

use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::{InterruptHandle, OpenFlags};
use spelaion_driver_api::{
    CancelHandle, Capabilities, ConnectParams, Connection, Dialect, Driver, DriverError,
    EventStream, ExecEvent, ExecOptions, ObjectInfo, Result, SchemaInfo, TableInfo,
};
use tokio::sync::{mpsc, oneshot};

pub use dialect::SqliteDialect;

/// Creates an empty SQLite database at `path`. An existing file is never touched: the path
/// is reserved with an exclusive create before SQLite writes its header, so a race with
/// another program fails instead of overwriting.
pub fn create_database(path: &std::path::Path) -> Result<()> {
    let failed = |e: &dyn std::fmt::Display| {
        DriverError::Other(format!("could not create {}: {e}", path.display()))
    };
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| match e.kind() {
            // Our own words: the system's message comes in the system's language.
            std::io::ErrorKind::AlreadyExists => DriverError::Other(format!(
                "{} already exists: a new database never replaces a file",
                path.display()
            )),
            _ => failed(&e),
        })?;
    let written = rusqlite::Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    // Setting a header field writes page 1, so the file is a database and not 0 bytes.
    .and_then(|conn| conn.execute_batch("PRAGMA user_version = 0;"));
    if let Err(e) = written {
        // The file is ours and empty: do not leave it behind.
        let _ = std::fs::remove_file(path);
        return Err(failed(&e));
    }
    Ok(())
}

/// SQLite driver. `ConnectParams::database` is the file path (or `:memory:`).
#[derive(Debug, Default, Clone)]
pub struct SqliteDriver {
    /// Create the file if it does not exist. Off by default: opening a mistyped path must not
    /// silently create an empty database.
    pub create_if_missing: bool,
}

#[async_trait]
impl Driver for SqliteDriver {
    fn engine(&self) -> &'static str {
        "sqlite"
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(SqliteDialect)
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        // A file on a server: the server's sqlite3, through the tunnel (ADR-018).
        if let Some(remote) = &params.remote {
            return remote::connect(remote, &params.database, params.read_only).await;
        }
        let mut flags = OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        if params.read_only {
            flags |= OpenFlags::SQLITE_OPEN_READ_ONLY;
        } else {
            flags |= OpenFlags::SQLITE_OPEN_READ_WRITE;
            if self.create_if_missing || params.database == ":memory:" {
                flags |= OpenFlags::SQLITE_OPEN_CREATE;
            }
        }
        let path = params.database.clone();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (commands, inbox) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name(format!("sqlite {path}"))
            .spawn(move || {
                let conn = match rusqlite::Connection::open_with_flags(&path, flags) {
                    Ok(conn) => conn,
                    Err(e) => {
                        let _ = ready_tx.send(Err(exec::map_error(e)));
                        return;
                    }
                };
                // Foreign keys come disabled for historical compatibility.
                if let Err(e) = conn.execute_batch("pragma foreign_keys = on") {
                    let _ = ready_tx.send(Err(exec::map_error(e)));
                    return;
                }
                let _ = ready_tx.send(Ok(Arc::new(conn.get_interrupt_handle())));
                actor(conn, inbox);
            })
            .map_err(|e| DriverError::Other(format!("could not spawn the SQLite thread: {e}")))?;

        let interrupt = ready_rx.await.map_err(|_| {
            DriverError::Connection("the SQLite thread ended while opening".into())
        })??;
        Ok(Box::new(SqliteConnection {
            commands,
            cancel: Arc::new(SqliteCancel(interrupt)),
            capabilities: Capabilities {
                transactions: true,
                cancel: true,
                explain: true,
                editable_results: false, // comes in v0.2
                schemas: true,           // main, temp and attached databases (ATTACH)
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
    Schemas(Reply<Vec<SchemaInfo>>),
    Objects(String, Reply<Vec<ObjectInfo>>),
    Describe(String, String, Reply<TableInfo>),
}

/// Loop of the connection thread. Ends when the `SqliteConnection` is dropped.
fn actor(conn: rusqlite::Connection, mut inbox: mpsc::UnboundedReceiver<Command>) {
    while let Some(command) = inbox.blocking_recv() {
        match command {
            Command::Execute {
                sql,
                batch_size,
                events,
            } => {
                if let Err(e) = exec::execute(&conn, &sql, batch_size, &events) {
                    let _ = events.blocking_send(Err(e));
                }
            }
            Command::Version(reply) => {
                let _ = reply.send(
                    conn.query_row("select sqlite_version()", [], |r| r.get(0))
                        .map_err(exec::map_error),
                );
            }
            Command::Schemas(reply) => {
                let _ = reply.send(introspect::list_schemas(&conn));
            }
            Command::Objects(schema, reply) => {
                let _ = reply.send(introspect::list_objects(&conn, &schema));
            }
            Command::Describe(schema, name, reply) => {
                let _ = reply.send(introspect::describe_table(&conn, &schema, &name));
            }
        }
    }
}

struct SqliteConnection {
    commands: mpsc::UnboundedSender<Command>,
    cancel: Arc<SqliteCancel>,
    capabilities: Capabilities,
}

impl SqliteConnection {
    async fn ask<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.commands.send(make(tx)).map_err(|_| closed())?;
        rx.await.map_err(|_| closed())?
    }
}

fn closed() -> DriverError {
    DriverError::Connection("the SQLite connection is closed".into())
}

#[async_trait]
impl Connection for SqliteConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        // Bounded channel: if the UI does not consume, the SQLite thread waits instead of filling memory.
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
        self.ask(Command::Schemas).await
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        let schema = schema.to_string();
        self.ask(|reply| Command::Objects(schema, reply)).await
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.ask(|reply| Command::Describe(schema, name, reply))
            .await
    }
}

struct SqliteCancel(Arc<InterruptHandle>);

#[async_trait]
impl CancelHandle for SqliteCancel {
    async fn cancel(&self) -> Result<()> {
        self.0.interrupt();
        Ok(())
    }
}
