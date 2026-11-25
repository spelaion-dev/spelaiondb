//! The loop that serves drivers to the core: frames in, frames out.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{FutureExt, StreamExt};
use serde_json::{Value, json};
use spelaion_driver_api::{
    ConnectParams, Connection, Driver, DriverError, EventStream, ExecEvent, ExecOptions,
    SchemaObjectKind, Secret, ServerObjectKind, TextRows,
};
use spelaion_plugin_protocol::ipc::{BatchDecoder, BatchEncoder, Decoded};
use spelaion_plugin_protocol::{
    CATALOG_KINDS, CatalogRequest, Connect, ConnectResult, CreateDatabase, ErrorData, Execute,
    Frame, FrameError, Initialize, InitializeResult, InsertRows, InsertRowsResult, Message,
    Program, RpcError, Run, SessionRequest, StreamCredit, StreamDone, StreamError, StreamId,
    StreamNotice, VERSION, Version, code, method, read_frame, write_frame,
};
use tokio::sync::{Notify, Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;

use crate::Plugin;

/// The most bytes of a result one data frame carries: a larger batch goes in several.
const DATA_CHUNK: usize = 16 * 1024 * 1024;
/// How often the sessions are asked whether their connection is known to be closed.
const CLOSED_CHECK: Duration = Duration::from_millis(500);
/// The most a plugin whose input closed waits for its drivers to close what they keep open
/// before it ends anyway. The core kills a plugin it stops sooner than this (its own grace).
const SHUTDOWN: Duration = Duration::from_secs(30);

/// Serves `plugin` over `reader` and `writer` until `reader` ends. [`crate::run`] does this on
/// the program's standard input and output.
///
/// Each pipe has a thread of its own, with plain blocking calls: a frame then costs one read
/// of the pipe, not a trip through the runtime's pool of blocking threads for each of its
/// parts.
pub async fn serve<R, W>(plugin: Plugin, reader: R, writer: W) -> Result<(), FrameError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    serve_within(plugin, reader, writer, SHUTDOWN).await
}

/// [`serve`], waiting at most `shutdown` for the drivers when the conversation ends. Only the
/// tests give it another limit than [`SHUTDOWN`]: with a paused clock, a test cannot wait for the
/// pipes' threads, which the runtime does not see (SDB-411).
async fn serve_within<R, W>(
    plugin: Plugin,
    reader: R,
    writer: W,
    shutdown: Duration,
) -> Result<(), FrameError>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    let (out, frames) = mpsc::channel(64);
    let spawned = |e: std::io::Error| FrameError::Io(e);
    std::thread::Builder::new()
        .name("plugin out".into())
        .spawn(move || write_loop(writer, frames))
        .map_err(spawned)?;
    let shared = Arc::new(Shared {
        plugin,
        out,
        sessions: Mutex::new(HashMap::new()),
        streams: Mutex::new(HashMap::new()),
        incoming: Mutex::new(HashMap::new()),
        tasks: Mutex::new(JoinSet::new()),
        turns: Mutex::new(HashMap::new()),
        protocol: std::sync::OnceLock::new(),
    });
    let watcher = tokio::spawn(watch_closed(shared.clone()));

    // The reader's thread takes each frame as far as it goes without waiting: rows are kept,
    // what must keep its order (`connect`, `close`, credit) is done there and then, and a
    // request is handed to the runtime. Nothing else wakes up for a frame.
    let runtime = tokio::runtime::Handle::current();
    let (ended, ending) = oneshot::channel();
    let reading = shared.clone();
    std::thread::Builder::new()
        .name("plugin in".into())
        .spawn(move || {
            let _ = ended.send(read_loop(reader, &reading, &runtime));
        })
        .map_err(spawned)?;
    // Without an answer the thread is gone: the conversation is over either way.
    let result = ending.await.unwrap_or(Ok(()));
    watcher.abort();
    shared.end(shutdown).await;
    result
}

/// What is read from or written to a pipe at once when it is there: several small frames.
const PIPE_BUFFER: usize = 64 * 1024;

/// Reads frames until the input closes (`Ok`) or is not the protocol.
fn read_loop<R: Read>(
    reader: R,
    shared: &Arc<Shared>,
    runtime: &tokio::runtime::Handle,
) -> Result<(), FrameError> {
    let mut reader = BufReader::with_capacity(PIPE_BUFFER, reader);
    let mut initialized = false;
    loop {
        let Some(frame) = read_frame(&mut reader)? else {
            return Ok(());
        };
        match frame {
            Frame::Data {
                stream,
                start,
                bytes,
            } => {
                if let Err(error) = shared.rows_arrived(stream, start, bytes) {
                    eprintln!("rows that could not be read: {error}");
                }
            }
            Frame::Message(bytes) => {
                let Ok(message) = Message::from_bytes(&bytes) else {
                    eprintln!("a message that is not JSON-RPC was ignored");
                    continue;
                };
                let Some(name) = message.method.clone() else {
                    // A response: this side asks nothing.
                    continue;
                };
                let Some(id) = message.id else {
                    shared.notified(&name, &message);
                    continue;
                };
                if name == method::INITIALIZE {
                    let answer = match shared.initialize(&message) {
                        Ok(result) => {
                            initialized = true;
                            Message::result(id, result)
                        }
                        Err(error) => Message::error(id, error),
                    };
                    shared.send_now(answer);
                } else if !initialized {
                    let error = RpcError::invalid_request("initialize comes first");
                    shared.send_now(Message::error(id, error));
                } else if name == method::CLOSE {
                    // Here, in the order it arrived, and not in a task that may run before
                    // the `connect` it follows.
                    let answer = match shared.close(&message) {
                        Ok(result) => Message::result(id, result),
                        Err(error) => Message::error(id, error),
                    };
                    shared.send_now(answer);
                } else {
                    if name == method::CONNECT
                        && let Err(error) = shared.reserve(&message)
                    {
                        shared.send_now(Message::error(id, error));
                        continue;
                    }
                    // Taken here, in the order they were read, not in the tasks.
                    let turn = shared.turn_for(&name, &message);
                    shared.spawn(runtime, guarded(shared.clone(), id, name, message, turn));
                }
            }
        }
    }
}

fn write_loop<W: Write>(writer: W, mut frames: mpsc::Receiver<Frame>) {
    let mut writer = BufWriter::with_capacity(PIPE_BUFFER, writer);
    while let Some(frame) = frames.blocking_recv() {
        if write_frame(&mut writer, &frame).is_err() {
            return;
        }
        // Whatever else is waiting goes out before the flush.
        while let Ok(frame) = frames.try_recv() {
            if write_frame(&mut writer, &frame).is_err() {
                return;
            }
        }
        if writer.flush().is_err() {
            return;
        }
    }
}

struct Session {
    connection: Box<dyn Connection>,
    /// `session.closed` was sent.
    closed_sent: AtomicBool,
}

enum Slot {
    /// `connect` is running. A `close` meanwhile removes the slot and tells it to stop: the
    /// driver's attempt is dropped there and then, as it is when the core drops a driver's
    /// `connect` in its own process (a login the user stopped must not stay open).
    Connecting(Arc<Notify>),
    Open(Arc<Session>),
}

/// A result stream being sent.
struct Sending {
    /// Record batches that may still be sent.
    credit: Semaphore,
    /// The core reads no more of it.
    dropped: Notify,
}

/// Rows that arrived for an `insert_rows`.
#[derive(Default)]
struct Incoming {
    decoder: BatchDecoder,
    rows: Vec<TextRows>,
}

struct Shared {
    plugin: Plugin,
    out: mpsc::Sender<Frame>,
    sessions: Mutex<HashMap<u64, Slot>>,
    streams: Mutex<HashMap<StreamId, Arc<Sending>>>,
    incoming: Mutex<HashMap<StreamId, Incoming>>,
    /// The requests being served: each holds what it uses of its session until it ends.
    tasks: Mutex<JoinSet<()>>,
    /// Each session's last `execute` or `cancel` read: the next one waits for its turn.
    turns: Mutex<HashMap<u64, oneshot::Receiver<()>>>,
    /// The version `initialize` chose: what was added after it is never sent (ADR-082).
    protocol: std::sync::OnceLock<Version>,
}

/// A session's `execute` and `cancel` reach its driver in the order they were read (SDB-428).
/// Each request is served in a task of its own, and tasks run in any order: a cancel written
/// right after an execute could reach a driver that numbers its statements (the MySQL and
/// PostgreSQL plugins do, when their `execute` is first polled) before the execute had, and
/// find nothing to stop. So each waits for the one read before it: an execute until it has
/// started its statement (its driver's `execute` polled once), a cancel until it has been
/// delivered. Nothing is retried and no timer is involved.
struct Turn {
    /// The turn before this one; `None` once waited for, or when there was none.
    before: Option<oneshot::Receiver<()>>,
    /// Passes the turn on when dropped, however the request ends.
    _done: oneshot::Sender<()>,
}

impl Turn {
    /// Waits until the request read before this one has had its turn (or ended).
    async fn wait(&mut self) {
        if let Some(before) = self.before.take() {
            let _ = before.await;
        }
    }
}

impl Shared {
    /// Serves a request in a task of its own, kept until it ends so the conversation's end can
    /// stop it.
    fn spawn(
        &self,
        runtime: &tokio::runtime::Handle,
        request: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let mut tasks = self.tasks.lock().expect("lock poisoned");
        // What already ended is let go of here, so the set holds only what still runs.
        while tasks.try_join_next().is_some() {}
        tasks.spawn_on(request, runtime);
    }

    /// The conversation is over: every request still being served stops, every session's
    /// connection is dropped, and then each driver closes what it keeps open past them (a
    /// DuckDB file shared by several sessions writes what it holds), for at most `shutdown`
    /// ([`SHUTDOWN`] outside the tests). Only then may the program end: ending it while a
    /// driver still writes a file can leave that file corrupt.
    async fn end(&self, shutdown: Duration) {
        let mut tasks = std::mem::take(&mut *self.tasks.lock().expect("lock poisoned"));
        // Stopped, and waited for: a request that is dropped drops what it held of its session.
        tasks.shutdown().await;
        let sessions: Vec<Slot> = self
            .sessions
            .lock()
            .expect("lock poisoned")
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        drop(sessions);
        self.streams.lock().expect("lock poisoned").clear();
        self.incoming.lock().expect("lock poisoned").clear();
        let drivers = self.plugin.drivers.clone();
        let closed = tokio::time::timeout(shutdown, async move {
            for driver in &drivers {
                driver.shutdown().await;
            }
        })
        .await;
        if closed.is_err() {
            eprintln!(
                "the drivers had not closed everything after {} seconds: the plugin ends anyway",
                shutdown.as_secs_f32()
            );
        }
    }

    async fn send(&self, message: Message) {
        let _ = self.out.send(Frame::Message(message.to_bytes())).await;
    }

    /// The same from the reader's thread, which is not in the runtime.
    fn send_now(&self, message: Message) {
        let _ = self.out.blocking_send(Frame::Message(message.to_bytes()));
    }

    async fn notify(&self, method: &str, params: impl serde::Serialize) {
        self.send(Message::notification(method, params)).await;
    }

    fn initialize(&self, message: &Message) -> Result<InitializeResult, RpcError> {
        let hello: Initialize = message.params()?;
        // The highest minor of this major the core speaks, capped at ours: within a major
        // version changes only add.
        let Some(theirs) = hello
            .protocols
            .iter()
            .filter(|v| v.major == VERSION.major)
            .max()
        else {
            return Err(RpcError {
                code: code::UNSUPPORTED_PROTOCOL,
                message: format!("this plugin speaks version {VERSION} of the protocol"),
                data: Some(json!({ "protocols": [VERSION] })),
            });
        };
        let chosen = (*theirs).min(VERSION);
        let _ = self.protocol.set(chosen);
        Ok(InitializeResult {
            protocol: chosen,
            plugin: Program {
                name: self.plugin.name.clone(),
                version: self.plugin.version.clone(),
            },
            engines: self
                .plugin
                .drivers
                .iter()
                .map(|d| d.engine().to_string())
                .collect(),
            credit: self.plugin.credit.max(1),
        })
    }

    /// A notification from the core.
    fn notified(&self, name: &str, message: &Message) {
        let Ok(params) = message.params::<StreamCredit>() else {
            return;
        };
        let sending = self
            .streams
            .lock()
            .expect("lock poisoned")
            .get(&params.stream)
            .cloned();
        let Some(sending) = sending else {
            return;
        };
        match name {
            method::STREAM_CREDIT => sending.credit.add_permits(params.batches as usize),
            method::STREAM_DROP => sending.dropped.notify_one(),
            // One this version does not know.
            _ => {}
        }
    }

    fn rows_arrived(
        &self,
        stream: StreamId,
        start: bool,
        bytes: Vec<u8>,
    ) -> Result<(), DriverError> {
        let mut incoming = self.incoming.lock().expect("lock poisoned");
        if start {
            incoming.insert(stream, Incoming::default());
        }
        let entry = incoming.entry(stream).or_default();
        let decoded = entry
            .decoder
            .push(bytes)
            .map_err(|e| DriverError::Other(e.to_string()))?;
        for item in decoded {
            if let Decoded::Batch(batch) = item {
                entry.rows.push(TextRows::from_batch(&batch)?);
            }
        }
        Ok(())
    }

    /// A session's place, taken as its `connect` arrives: a `close` that follows finds it,
    /// however soon it comes.
    fn reserve(&self, message: &Message) -> Result<(), RpcError> {
        let request: SessionRequest = message.params()?;
        let mut sessions = self.sessions.lock().expect("lock poisoned");
        if sessions.contains_key(&request.session) {
            return Err(RpcError::invalid_request(format!(
                "session {} is already open",
                request.session
            )));
        }
        sessions.insert(request.session, Slot::Connecting(Arc::new(Notify::new())));
        Ok(())
    }

    /// `close`: the session is gone. One that was still connecting stops, and drops what it
    /// has.
    fn close(&self, message: &Message) -> Result<Value, RpcError> {
        let request: SessionRequest = message.params()?;
        let slot = self
            .sessions
            .lock()
            .expect("lock poisoned")
            .remove(&request.session);
        if let Some(Slot::Connecting(given_up)) = slot {
            given_up.notify_one();
        }
        self.turns
            .lock()
            .expect("lock poisoned")
            .remove(&request.session);
        Ok(json!({}))
    }

    /// The turn of an `execute` or a `cancel` read now, for its session (SDB-428); `None` for
    /// any other request, which keeps running alongside.
    fn turn_for(&self, name: &str, message: &Message) -> Option<Turn> {
        if name != method::EXECUTE && name != method::CANCEL {
            return None;
        }
        // A request without its session fails in its task, with no turn to wait for.
        let session = message
            .params
            .as_ref()
            .and_then(|params| params.get("session"))
            .and_then(Value::as_u64)?;
        let (done, next) = oneshot::channel();
        let before = self
            .turns
            .lock()
            .expect("lock poisoned")
            .insert(session, next);
        Some(Turn {
            before,
            _done: done,
        })
    }

    /// The core chose `version` or a later one in `initialize`.
    fn speaks(&self, version: Version) -> bool {
        self.protocol.get().is_some_and(|chosen| *chosen >= version)
    }

    fn session(&self, id: u64) -> Result<Arc<Session>, RpcError> {
        match self.sessions.lock().expect("lock poisoned").get(&id) {
            Some(Slot::Open(session)) => Ok(session.clone()),
            Some(Slot::Connecting(_)) | None => Err(RpcError::invalid_request(format!(
                "there is no session {id}"
            ))),
        }
    }

    /// Says once that a session's connection is known to be closed.
    async fn report_closed(&self, id: u64) {
        let Ok(session) = self.session(id) else {
            return;
        };
        if session.connection.is_closed() && !session.closed_sent.swap(true, Ordering::SeqCst) {
            self.notify(method::SESSION_CLOSED, SessionRequest { session: id })
                .await;
        }
    }
}

/// A connection found closed while nothing is asked of it is still reported.
async fn watch_closed(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(CLOSED_CHECK);
    loop {
        tick.tick().await;
        let ids: Vec<u64> = shared
            .sessions
            .lock()
            .expect("lock poisoned")
            .keys()
            .copied()
            .collect();
        for id in ids {
            shared.report_closed(id).await;
        }
    }
}

/// Runs a request so that a driver that panics still answers: the core never waits for ever.
async fn guarded(shared: Arc<Shared>, id: u64, name: String, message: Message, turn: Option<Turn>) {
    let stream = message
        .params
        .as_ref()
        .and_then(|p| p.get("stream"))
        .and_then(Value::as_u64);
    let is_execute = name == method::EXECUTE;
    // In this task, not one more: a request wakes up as little as it can.
    let served = AssertUnwindSafe(handle(shared.clone(), id, name, message, turn))
        .catch_unwind()
        .await;
    if served.is_err() {
        let error = DriverError::Other("the plugin failed while it ran the request".into());
        shared
            .send(Message::error(id, RpcError::from(&error)))
            .await;
        if let (true, Some(stream)) = (is_execute, stream) {
            shared
                .streams
                .lock()
                .expect("lock poisoned")
                .remove(&stream);
            let error = ErrorData::from(&error);
            shared
                .notify(method::STREAM_ERROR, StreamError { stream, error })
                .await;
            shared
                .notify(method::STREAM_END, StreamCredit { stream, batches: 0 })
                .await;
        }
    }
}

async fn handle(
    shared: Arc<Shared>,
    id: u64,
    name: String,
    message: Message,
    mut turn: Option<Turn>,
) {
    if name == method::EXECUTE {
        execute(&shared, id, &message, turn).await;
        return;
    }
    if let Some(turn) = turn.as_mut() {
        // A cancel: the execute read before it has started its statement.
        turn.wait().await;
    }
    let result = if name == method::CONNECT {
        connect(&shared, &message).await
    } else if name == method::CREATE_DATABASE {
        create_database(&shared, &message).await
    } else {
        match message.params::<SessionRequest>() {
            Ok(request) => {
                let result = on_session(&shared, &name, request.session, &message).await;
                // Before the answer, so the core knows why it failed.
                shared.report_closed(request.session).await;
                result
            }
            Err(error) => Err(error),
        }
    };
    let answer = match result {
        Ok(result) => Message::result(id, result),
        Err(error) => Message::error(id, error),
    };
    shared.send(answer).await;
}

/// A schema object kind a core of protocol 1.2 or older can read: it would fail to read the
/// whole list for one kind it does not know (ADR-082).
fn schema_kind_before_1_3(kind: SchemaObjectKind) -> bool {
    match kind {
        SchemaObjectKind::Function
        | SchemaObjectKind::Procedure
        | SchemaObjectKind::Aggregate
        | SchemaObjectKind::Sequence
        | SchemaObjectKind::Trigger
        | SchemaObjectKind::Enum
        | SchemaObjectKind::Domain
        | SchemaObjectKind::Type
        | SchemaObjectKind::Event
        | SchemaObjectKind::Synonym => true,
        SchemaObjectKind::Sink | SchemaObjectKind::Connection | SchemaObjectKind::Secret => false,
    }
}

/// The same for a server object kind.
fn server_kind_before_1_3(kind: ServerObjectKind) -> bool {
    match kind {
        ServerObjectKind::Role
        | ServerObjectKind::User
        | ServerObjectKind::Login
        | ServerObjectKind::Tablespace
        | ServerObjectKind::Extension => true,
        ServerObjectKind::Cluster | ServerObjectKind::ClusterReplica => false,
    }
}

fn driver_error(error: DriverError) -> RpcError {
    RpcError::from(&error)
}

fn needs<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, RpcError> {
    value.as_deref().ok_or_else(|| RpcError {
        code: code::INVALID_PARAMS,
        message: format!("`{name}` is missing"),
        data: None,
    })
}

/// The driver of one of this plugin's engines.
fn driver_of(shared: &Shared, engine: &str) -> Result<Arc<dyn Driver>, RpcError> {
    shared
        .plugin
        .drivers
        .iter()
        .find(|d| d.engine() == engine)
        .cloned()
        .ok_or_else(|| RpcError::invalid_request(format!("this plugin has no engine {engine:?}")))
}

async fn create_database(shared: &Shared, message: &Message) -> Result<Value, RpcError> {
    let request: CreateDatabase = message.params()?;
    let driver = driver_of(shared, &request.engine)?;
    driver
        .create_database(&request.database)
        .await
        .map_err(driver_error)?;
    Ok(json!({}))
}

/// `connect`. The session's place was taken when the request arrived (`reserve`).
async fn connect(shared: &Shared, message: &Message) -> Result<Value, RpcError> {
    let session = message.params::<SessionRequest>()?.session;
    let result = connecting(shared, message).await;
    if result.is_err() {
        // Its place is given back, unless a `close` already took it.
        let mut sessions = shared.sessions.lock().expect("lock poisoned");
        if matches!(sessions.get(&session), Some(Slot::Connecting(_))) {
            sessions.remove(&session);
        }
    }
    result
}

async fn connecting(shared: &Shared, message: &Message) -> Result<Value, RpcError> {
    let request: Connect = message.params()?;
    let driver = driver_of(shared, &request.engine)?;
    let closed = || RpcError::invalid_request("the session was closed");
    let given_up = match shared
        .sessions
        .lock()
        .expect("lock poisoned")
        .get(&request.session)
    {
        Some(Slot::Connecting(given_up)) => given_up.clone(),
        Some(Slot::Open(_)) | None => return Err(closed()),
    };
    let params = ConnectParams {
        host: request.host,
        port: request.port,
        user: request.user,
        password: request.password.map(Secret::new),
        database: request.database,
        application_name: request.application_name,
        tls: request.tls,
        ca_certificate: request.ca_certificate,
        client_certificate: request.client_certificate,
        client_key: request.client_key,
        read_only: request.read_only,
        connect_addr: request.connect_addr,
        // A program on another machine is the core's to start (ADR-018): never a plugin's.
        remote: None,
        connect_timeout: request.connect_timeout_ms.map(Duration::from_millis),
        options: request.options,
    };
    let connected = tokio::select! {
        biased;
        // The attempt ends here: whatever it had open is closed with it.
        () = given_up.notified() => return Err(closed()),
        connected = driver.connect(&params) => connected,
    };
    let mut sessions = shared.sessions.lock().expect("lock poisoned");
    match connected {
        Ok(connection) => {
            // Closed while it was connecting: the connection is dropped here.
            if !matches!(sessions.get(&request.session), Some(Slot::Connecting(_))) {
                return Err(closed());
            }
            let capabilities = connection.capabilities().clone();
            sessions.insert(
                request.session,
                Slot::Open(Arc::new(Session {
                    connection,
                    closed_sent: AtomicBool::new(false),
                })),
            );
            serde_json::to_value(ConnectResult { capabilities }).map_err(|e| RpcError {
                code: code::DRIVER,
                message: e.to_string(),
                data: None,
            })
        }
        Err(error) => Err(driver_error(error)),
    }
}

/// Every request that names a session, except `execute`.
async fn on_session(
    shared: &Shared,
    name: &str,
    id: u64,
    message: &Message,
) -> Result<Value, RpcError> {
    let session = shared.session(id)?;
    let connection = &session.connection;
    let request: CatalogRequest = message.params()?;
    let value = match name {
        method::CANCEL => {
            connection
                .cancel_handle()
                .cancel()
                .await
                .map_err(driver_error)?;
            json!({})
        }
        method::INSERT_ROWS => {
            let request: InsertRows = message.params()?;
            let rows = shared
                .incoming
                .lock()
                .expect("lock poisoned")
                .remove(&request.stream)
                .map(|incoming| incoming.rows)
                .unwrap_or_default();
            // One batch, as the core sends them: used as it arrived.
            let rows =
                TextRows::concat(request.target.columns.len(), &rows).map_err(driver_error)?;
            let rows = connection
                .insert_rows(&request.target, rows)
                .await
                .map_err(driver_error)?;
            json!(InsertRowsResult { rows })
        }
        method::SERVER_VERSION => {
            json!({ "version": connection.server_version().await.map_err(driver_error)? })
        }
        method::KEEP_ALIVE => {
            json!({ "state": connection.keep_alive().await.map_err(driver_error)? })
        }
        method::TRANSACTION_STATE => {
            json!({ "state": connection.transaction_state().await.map_err(driver_error)? })
        }
        method::CURRENT_DATABASE => {
            json!({ "database": connection.current_database().await.map_err(driver_error)? })
        }
        method::DATABASE_CONTEXT => {
            json!({ "database": connection.database_context().await.map_err(driver_error)? })
        }
        method::SELECT_DATABASE => {
            connection
                .select_database(needs(&request.database, "database")?)
                .await
                .map_err(driver_error)?;
            json!({})
        }
        method::LIST_DATABASES => {
            json!({ "databases": connection.list_databases().await.map_err(driver_error)? })
        }
        method::LIST_SCHEMAS => {
            let schemas = match &request.filter {
                Some(filter) => connection.list_schemas_matching(filter).await,
                None => connection.list_schemas().await,
            };
            json!({ "schemas": schemas.map_err(driver_error)? })
        }
        method::LIST_OBJECTS => {
            let schema = needs(&request.schema, "schema")?;
            let objects = match &request.filter {
                Some(filter) => connection.list_objects_matching(schema, filter).await,
                None => connection.list_objects(schema).await,
            };
            json!({ "objects": objects.map_err(driver_error)? })
        }
        method::DESCRIBE_TABLE => {
            let table = connection
                .describe_table(
                    needs(&request.schema, "schema")?,
                    needs(&request.name, "name")?,
                )
                .await
                .map_err(driver_error)?;
            json!({ "table": table })
        }
        method::LIST_REFERENCING_KEYS => {
            let keys = connection
                .list_referencing_keys(
                    needs(&request.schema, "schema")?,
                    needs(&request.name, "name")?,
                )
                .await
                .map_err(driver_error)?;
            json!({ "keys": keys })
        }
        method::LIST_COLUMNS => {
            let columns = connection
                .list_columns(needs(&request.schema, "schema")?)
                .await
                .map_err(driver_error)?;
            json!({ "columns": columns })
        }
        method::LIST_SCHEMA_OBJECTS => {
            let schema = needs(&request.schema, "schema")?;
            let objects = match &request.filter {
                Some(filter) => {
                    connection
                        .list_schema_objects_matching(schema, filter)
                        .await
                }
                None => connection.list_schema_objects(schema).await,
            };
            let mut objects = objects.map_err(driver_error)?;
            if !shared.speaks(CATALOG_KINDS) {
                objects.retain(|o| schema_kind_before_1_3(o.kind));
            }
            json!({ "objects": objects })
        }
        method::LIST_SERVER_OBJECTS => {
            let mut objects = connection
                .list_server_objects()
                .await
                .map_err(driver_error)?;
            if !shared.speaks(CATALOG_KINDS) {
                objects.retain(|o| server_kind_before_1_3(o.kind));
            }
            json!({ "objects": objects })
        }
        method::SERVER_OBJECT_DEFINITION => {
            let object = request.server_object.as_ref().ok_or_else(|| RpcError {
                code: code::INVALID_PARAMS,
                message: "`server_object` is missing".into(),
                data: None,
            })?;
            let sql = connection.server_object_definition(object).await;
            json!({ "sql": sql.map_err(driver_error)? })
        }
        method::OBJECT_DEFINITION | method::PROCEDURE_CALL => {
            let object = request.object.as_ref().ok_or_else(|| RpcError {
                code: code::INVALID_PARAMS,
                message: "`object` is missing".into(),
                data: None,
            })?;
            let sql = if name == method::OBJECT_DEFINITION {
                connection.object_definition(object).await
            } else {
                connection.procedure_call(object).await
            };
            json!({ "sql": sql.map_err(driver_error)? })
        }
        method::OBJECT_PROPERTIES => {
            let properties = connection
                .object_properties(
                    needs(&request.schema, "schema")?,
                    needs(&request.name, "name")?,
                )
                .await
                .map_err(driver_error)?;
            json!({ "properties": properties })
        }
        method::TABLE_PARTITIONS => {
            let partitioning = connection
                .table_partitions(
                    needs(&request.schema, "schema")?,
                    needs(&request.name, "name")?,
                )
                .await
                .map_err(driver_error)?;
            json!({ "partitioning": partitioning })
        }
        method::OBJECT_DEPENDENCIES => {
            let dependencies = connection
                .object_dependencies(
                    needs(&request.schema, "schema")?,
                    needs(&request.name, "name")?,
                )
                .await
                .map_err(driver_error)?;
            json!({ "dependencies": dependencies })
        }
        method::RELATION_DEFINITION => {
            let sql = connection
                .relation_definition(
                    needs(&request.schema, "schema")?,
                    needs(&request.name, "name")?,
                )
                .await
                .map_err(driver_error)?;
            json!({ "sql": sql })
        }
        other => return Err(RpcError::method_not_found(other)),
    };
    Ok(value)
}

async fn execute(shared: &Arc<Shared>, id: u64, message: &Message, mut turn: Option<Turn>) {
    if let Some(turn) = turn.as_mut() {
        // A cancel read before this execute is delivered first: it cannot reach this statement.
        turn.wait().await;
    }
    let request: Execute = match message.params() {
        Ok(request) => request,
        Err(error) => {
            shared.send(Message::error(id, error)).await;
            return;
        }
    };
    let stream = request.stream;
    // Before the statement starts: credit and a drop may arrive while it does.
    let sending = Arc::new(Sending {
        credit: Semaphore::new(shared.plugin.credit.max(1) as usize),
        dropped: Notify::new(),
    });
    shared
        .streams
        .lock()
        .expect("lock poisoned")
        .insert(stream, sending.clone());

    let started = match shared.session(request.session) {
        Ok(session) => {
            let options = ExecOptions {
                batch_size: request.batch_size.max(1),
                origins: request.origins,
            };
            let connection = &session.connection;
            let sql = &request.sql;
            let mut starting: std::pin::Pin<Box<dyn Future<Output = _> + Send + '_>> = match request
                .run
            {
                Run::Statement => Box::pin(connection.execute(sql, &options)),
                Run::Bound { params } => Box::pin(connection.execute_bound(sql, params, &options)),
                Run::Plan(mode) => Box::pin(connection.execute_plan(sql, mode.into())),
                Run::Request { request } => Box::pin(connection.execute_request(request, &options)),
            };
            // Polled once before a cancel read after it may reach the driver: a driver numbers
            // its statement there. Then the turn passes, and the statement goes on.
            let first = first_poll(&mut starting).await;
            drop(turn.take());
            let started = match first {
                Some(started) => started,
                None => starting.await,
            };
            started.map_err(driver_error)
        }
        Err(error) => Err(error),
    };
    // A session that is not there: the turn passes here.
    drop(turn);
    let events = match started {
        Ok(events) => events,
        Err(error) => {
            shared
                .streams
                .lock()
                .expect("lock poisoned")
                .remove(&stream);
            shared.report_closed(request.session).await;
            shared.send(Message::error(id, error)).await;
            return;
        }
    };
    shared.send(Message::result(id, json!({}))).await;
    send_events(shared, stream, &sending, events).await;
    shared
        .streams
        .lock()
        .expect("lock poisoned")
        .remove(&stream);
    shared.report_closed(request.session).await;
    shared
        .notify(method::STREAM_END, StreamCredit { stream, batches: 0 })
        .await;
}

/// Polls `future` once: its output if it is ready then, `None` if it waits.
async fn first_poll<F: Future + Unpin>(future: &mut F) -> Option<F::Output> {
    std::future::poll_fn(
        |context| match std::pin::Pin::new(&mut *future).poll(context) {
            std::task::Poll::Ready(output) => std::task::Poll::Ready(Some(output)),
            std::task::Poll::Pending => std::task::Poll::Ready(None),
        },
    )
    .await
}

/// Sends a statement's events in the order the driver gives them, a batch only when the core
/// has given credit for it, until the stream ends or the core drops it.
async fn send_events(
    shared: &Shared,
    stream: StreamId,
    sending: &Sending,
    mut events: EventStream,
) {
    let mut encoder: Option<BatchEncoder> = None;
    loop {
        let event = tokio::select! {
            biased;
            _ = sending.dropped.notified() => return,
            event = events.next() => event,
        };
        let Some(event) = event else {
            return;
        };
        let failed = match event {
            Ok(ExecEvent::Columns(schema)) => match BatchEncoder::new(&schema.arrow) {
                Ok((new, bytes)) => {
                    encoder = Some(new);
                    send_data(shared, stream, true, bytes).await;
                    None
                }
                Err(error) => Some(DriverError::Other(error.to_string())),
            },
            Ok(ExecEvent::Batch(batch)) => {
                let permit = tokio::select! {
                    biased;
                    _ = sending.dropped.notified() => return,
                    permit = sending.credit.acquire() => permit,
                };
                let Ok(permit) = permit else {
                    return;
                };
                // The core gives it back when it has consumed the batch.
                permit.forget();
                match encoder.as_mut() {
                    Some(encoder) => match encoder.encode(&batch) {
                        Ok(bytes) => {
                            send_data(shared, stream, false, bytes).await;
                            None
                        }
                        Err(error) => Some(DriverError::Other(error.to_string())),
                    },
                    None => Some(DriverError::Other(
                        "the driver sent rows before their columns".into(),
                    )),
                }
            }
            Ok(ExecEvent::Notice(notice)) => {
                let notice = StreamNotice {
                    stream,
                    severity: notice.severity,
                    message: notice.message,
                };
                shared.notify(method::STREAM_NOTICE, notice).await;
                None
            }
            Ok(ExecEvent::Done { rows_affected }) => {
                let done = StreamDone {
                    stream,
                    rows_affected,
                };
                shared.notify(method::STREAM_DONE, done).await;
                None
            }
            Err(error) => Some(error),
        };
        if let Some(error) = failed {
            let error = ErrorData::from(&error);
            shared
                .notify(method::STREAM_ERROR, StreamError { stream, error })
                .await;
        }
    }
}

async fn send_data(shared: &Shared, stream: StreamId, start: bool, bytes: Vec<u8>) {
    if bytes.len() <= DATA_CHUNK {
        let frame = Frame::Data {
            stream,
            start,
            bytes,
        };
        let _ = shared.out.send(frame).await;
        return;
    }
    for (i, chunk) in bytes.chunks(DATA_CHUNK).enumerate() {
        let frame = Frame::Data {
            stream,
            start: start && i == 0,
            bytes: chunk.to_vec(),
        };
        let _ = shared.out.send(frame).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// A driver whose shutdown never ends.
    struct Hangs;

    #[async_trait]
    impl Driver for Hangs {
        fn engine(&self) -> &str {
            "hangs"
        }

        async fn connect(
            &self,
            _: &ConnectParams,
        ) -> spelaion_driver_api::Result<Box<dyn Connection>> {
            Err(DriverError::Other("not here".into()))
        }

        async fn shutdown(&self) {
            std::future::pending::<()>().await;
        }
    }

    /// An input that takes a moment of real time to end, as a pipe under load does.
    struct EndsLate;

    impl std::io::Read for EndsLate {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(300));
            Ok(0)
        }
    }

    /// With the clock in real time and a short limit: a paused clock would jump to the test's
    /// own timeout while the input's thread, which the runtime does not see, is still reading
    /// (SDB-411: with `EndsLate` that failed every time; with an empty input, now and then in CI).
    #[tokio::test]
    async fn a_driver_that_never_closes_does_not_keep_the_plugin_for_ever() {
        let limit = Duration::from_millis(200);
        for (name, input) in [
            (
                "an input already closed",
                Box::new(std::io::empty()) as Box<dyn Read + Send>,
            ),
            ("an input that ends late", Box::new(EndsLate)),
        ] {
            // The conversation ends, and the plugin waits for its driver only as long as it
            // may, even with nobody left to kill it.
            let plugin = Plugin::new("hangs", "0", vec![Arc::new(Hangs)]);
            let started = std::time::Instant::now();
            let served = tokio::time::timeout(
                Duration::from_secs(20),
                serve_within(plugin, input, std::io::sink(), limit),
            )
            .await;
            assert!(matches!(served, Ok(Ok(()))), "{name}: {served:?}");
            assert!(
                started.elapsed() >= limit,
                "{name}: the driver was not waited for"
            );
        }
    }
}
