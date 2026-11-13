//! A plugin's running process: its pipes, the requests waiting for an answer and the result
//! streams being received.

use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use spelaion_driver_api::{DriverError, ExecEvent, Notice, Result};
use spelaion_plugin_protocol::ipc::{BatchDecoder, Decoded, result_schema};
use spelaion_plugin_protocol::{
    Frame, Initialize, InitializeResult, Message, Program, RpcError, SessionRequest, StreamCredit,
    StreamDone, StreamError, StreamId, StreamNotice, VERSION, Version, code, method, read_frame,
    write_frame,
};
use tokio::sync::{mpsc as events, oneshot};

use crate::{LogSink, REDACTED};

/// How long a plugin has to answer `initialize`.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);
/// What is read from or written to a pipe at once when it is there: several small frames.
const PIPE_BUFFER: usize = 64 * 1024;
/// The most of one line of a plugin's standard error that is kept.
const LOG_LINE: usize = 16 * 1024;
/// The most bytes of rows one data frame carries.
pub(crate) const DATA_CHUNK: usize = 16 * 1024 * 1024;

/// A running plugin. Dropping it stops the program.
///
/// Its pipes are read and written on threads of their own, with plain blocking calls: a plugin
/// is shared by everything in this program (ADR-016 needs one DuckDB per file and process), so
/// it cannot belong to the runtime of whoever connected first, which may end long before the
/// others are done. And a frame then costs one read of the pipe, not a trip through a runtime's
/// pool of blocking threads for each of its parts.
pub(crate) struct Process {
    shared: Arc<Shared>,
    info: InitializeResult,
    running: Running,
}

/// The program itself: killed when this value goes, however that happens.
struct Running(Mutex<Option<Child>>);

impl Running {
    /// Stops the program now, and returns once it has ended.
    fn end(&self) {
        let child = self.0.lock().expect("lock poisoned").take();
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.end();
    }
}

/// A result stream being received.
struct Receiving {
    decoder: BatchDecoder,
    events: events::UnboundedSender<Result<ExecEvent>>,
}

/// What the tasks that read and write the pipes share with the connections.
pub(crate) struct Shared {
    name: String,
    /// Frames to write, in sets that go out together: one wake-up and one flush for each.
    out: mpsc::Sender<Vec<Frame>>,
    pending: Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, RpcError>>>>,
    streams: Mutex<HashMap<StreamId, Receiving>>,
    /// Each session's "the connection is known to be closed".
    sessions: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    dead: AtomicBool,
    why: Mutex<String>,
    next_id: AtomicU64,
    next_stream: AtomicU64,
    /// The secrets this plugin was given: taken out of everything it says.
    secrets: Mutex<Vec<String>>,
}

/// Why a call got no result.
enum Failure {
    /// The plugin stopped.
    Lost,
    Rpc(RpcError),
}

impl Process {
    pub(crate) async fn start(program: &Path, log: Option<LogSink>) -> Result<Arc<Self>> {
        let name = program
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.display().to_string());
        let could_not_start = |why: &dyn std::fmt::Display| {
            DriverError::Connection(format!("the plugin {name} could not be started: {why}"))
        };
        // No arguments, and nothing added to the environment: all a plugin learns about a
        // connection arrives in messages (ADR-041).
        let mut command = Command::new(program);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window when the app, which has none, starts a console program.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().map_err(|e| could_not_start(&e))?;
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        // From here the program ends with this value, an early return included.
        let running = Running(Mutex::new(Some(child)));
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            return Err(could_not_start(&"its pipes are missing"));
        };

        let (out, frames) = mpsc::channel();
        let shared = Arc::new(Shared {
            name: name.clone(),
            out,
            pending: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            why: Mutex::new(String::new()),
            next_id: AtomicU64::new(1),
            next_stream: AtomicU64::new(1),
            secrets: Mutex::new(Vec::new()),
        });
        let thread = |what: &str| std::thread::Builder::new().name(format!("plugin {name} {what}"));
        thread("in")
            .spawn(move || write_loop(stdin, frames))
            .map_err(|e| could_not_start(&e))?;
        let reader = shared.clone();
        thread("out")
            .spawn(move || read_loop(stdout, reader))
            .map_err(|e| could_not_start(&e))?;
        let logger = shared.clone();
        thread("log")
            .spawn(move || log_loop(stderr, logger, log))
            .map_err(|e| could_not_start(&e))?;

        let info = shared.initialize().await?;
        Ok(Arc::new(Self {
            shared,
            info,
            running,
        }))
    }

    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    pub(crate) fn info(&self) -> &InitializeResult {
        &self.info
    }

    pub(crate) fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    /// Stops the program now, and returns once it has ended.
    pub(crate) async fn kill(&self, why: &str) {
        // First, so everything that waits fails with this reason and not with "it exited".
        self.shared.die(why);
        self.running.end();
    }
}

impl Shared {
    async fn initialize(&self) -> Result<InitializeResult> {
        let hello = Initialize {
            protocols: vec![VERSION],
            host: Program {
                name: "spelaiondb".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        };
        let name = &self.name;
        let expected = format!("this SpelaionDB speaks version {VERSION}");
        let answer = tokio::time::timeout(
            INITIALIZE_TIMEOUT,
            self.call(Vec::new(), method::INITIALIZE, &hello),
        );
        let value = match answer.await {
            Err(_) => {
                self.die("it did not answer");
                return Err(DriverError::Connection(format!(
                    "the plugin {name} did not answer: it may not speak the plugin protocol \
                     ({expected})"
                )));
            }
            Ok(Err(Failure::Lost)) => {
                return Err(DriverError::Connection(format!(
                    "the plugin {name} stopped as it started ({}): it may not speak the plugin \
                     protocol ({expected})",
                    self.why()
                )));
            }
            Ok(Err(Failure::Rpc(error))) => {
                self.die("it refused the protocol");
                let theirs = error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("protocols"))
                    .and_then(|v| serde_json::from_value::<Vec<Version>>(v.clone()).ok())
                    .filter(|_| error.code == code::UNSUPPORTED_PROTOCOL);
                return Err(DriverError::Connection(match theirs {
                    Some(theirs) => format!(
                        "the plugin {name} speaks version {} of the plugin protocol; {expected}",
                        theirs
                            .iter()
                            .map(Version::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    None => format!(
                        "the plugin {name} refused to start: {} ({expected})",
                        self.scrub(&error.message)
                    ),
                }));
            }
            Ok(Ok(value)) => value,
        };
        let info: InitializeResult = serde_json::from_value(value).map_err(|_| {
            self.die("its answer to initialize was not one");
            DriverError::Connection(format!(
                "the plugin {name} did not answer as the plugin protocol says ({expected})"
            ))
        })?;
        // It must have chosen a version that was offered.
        if info.protocol.major != VERSION.major || info.protocol.minor > VERSION.minor {
            self.die("it chose a version that was not offered");
            return Err(DriverError::Connection(format!(
                "the plugin {name} speaks version {} of the plugin protocol; {expected}",
                info.protocol
            )));
        }
        Ok(info)
    }

    /// A request and its answer, as the driver's result.
    pub(crate) async fn request(&self, method: &str, params: impl Serialize) -> Result<Value> {
        self.request_after(Vec::new(), method, params).await
    }

    /// A request and its answer, with `first` written right before it, in the same write: the
    /// rows a request is about.
    pub(crate) async fn request_after(
        &self,
        first: Vec<Frame>,
        method: &str,
        params: impl Serialize,
    ) -> Result<Value> {
        match self.call(first, method, params).await {
            Ok(value) => Ok(value),
            Err(Failure::Lost) => Err(self.lost()),
            Err(Failure::Rpc(error)) => Err(self.scrub_error(error.into_driver_error())),
        }
    }

    async fn call(
        &self,
        mut frames: Vec<Frame>,
        method: &str,
        params: impl Serialize,
    ) -> std::result::Result<Value, Failure> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.pending
            .lock()
            .expect("lock poisoned")
            .insert(id, answer);
        // Forgotten if the caller stops waiting.
        let waiting = Waiting { shared: self, id };
        // After it is registered: a plugin that died in between has already failed the others.
        frames.push(Frame::Message(
            Message::request(id, method, params).to_bytes(),
        ));
        if self.dead.load(Ordering::SeqCst) || self.out.send(frames).is_err() {
            return Err(Failure::Lost);
        }
        let answer = answered.await;
        drop(waiting);
        match answer {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(Failure::Rpc(error)),
            Err(_) => Err(Failure::Lost),
        }
    }

    /// A request whose answer nobody waits for.
    pub(crate) fn tell(&self, method: &str, params: impl Serialize) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(Message::request(id, method, params));
    }

    pub(crate) fn notify(&self, method: &str, params: impl Serialize) {
        self.send(Message::notification(method, params));
    }

    fn send(&self, message: Message) -> bool {
        self.out
            .send(vec![Frame::Message(message.to_bytes())])
            .is_ok()
    }

    /// The frames of rows for the plugin, on `stream`.
    pub(crate) fn rows_frames(stream: StreamId, start: Vec<u8>, rows: Vec<u8>) -> Vec<Frame> {
        let mut frames = vec![Frame::Data {
            stream,
            start: true,
            bytes: start,
        }];
        if rows.len() <= DATA_CHUNK {
            // As they are: the usual case, and no copy of them is made.
            frames.push(Frame::Data {
                stream,
                start: false,
                bytes: rows,
            });
            return frames;
        }
        frames.extend(rows.chunks(DATA_CHUNK).map(|chunk| Frame::Data {
            stream,
            start: false,
            bytes: chunk.to_vec(),
        }));
        frames
    }

    pub(crate) fn next_stream(&self) -> StreamId {
        self.next_stream.fetch_add(1, Ordering::Relaxed)
    }

    /// Registers a result stream before its statement is sent, so nothing of it is missed.
    pub(crate) fn receive(&self, stream: StreamId) -> events::UnboundedReceiver<Result<ExecEvent>> {
        let (events, receiver) = events::unbounded_channel();
        self.streams.lock().expect("lock poisoned").insert(
            stream,
            Receiving {
                decoder: BatchDecoder::new(),
                events,
            },
        );
        receiver
    }

    /// Stops receiving `stream`. `tell`: the plugin is told too, because it may still be
    /// sending.
    pub(crate) fn forget_stream(&self, stream: StreamId, tell: bool) {
        self.streams.lock().expect("lock poisoned").remove(&stream);
        if tell {
            self.notify(method::STREAM_DROP, StreamCredit { stream, batches: 0 });
        }
    }

    pub(crate) fn credit(&self, stream: StreamId, batches: u32) {
        self.notify(method::STREAM_CREDIT, StreamCredit { stream, batches });
    }

    pub(crate) fn add_session(&self, session: u64) -> Arc<AtomicBool> {
        let closed = Arc::new(AtomicBool::new(false));
        self.sessions
            .lock()
            .expect("lock poisoned")
            .insert(session, closed.clone());
        closed
    }

    pub(crate) fn forget_session(&self, session: u64) {
        self.sessions
            .lock()
            .expect("lock poisoned")
            .remove(&session);
    }

    /// A secret given to the plugin: never shown in anything it says.
    pub(crate) fn add_secret(&self, secret: &str) {
        if secret.is_empty() {
            return;
        }
        let mut secrets = self.secrets.lock().expect("lock poisoned");
        if !secrets.iter().any(|s| s == secret) {
            secrets.push(secret.to_string());
            // The longest first, so one that contains another is taken out whole.
            secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        }
    }

    pub(crate) fn scrub(&self, text: &str) -> String {
        let secrets = self.secrets.lock().expect("lock poisoned");
        let mut text = text.to_string();
        for secret in secrets.iter() {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), REDACTED);
            }
        }
        text
    }

    fn scrub_error(&self, error: DriverError) -> DriverError {
        let clean = |text: String| self.scrub(&text);
        match error {
            DriverError::Connection(message) => DriverError::Connection(clean(message)),
            DriverError::Database {
                code,
                message,
                detail,
                hint,
                position,
            } => DriverError::Database {
                code: code.map(clean),
                message: clean(message),
                detail: detail.map(clean),
                hint: hint.map(clean),
                position,
            },
            DriverError::Cancelled => DriverError::Cancelled,
            DriverError::Decode { type_name, message } => DriverError::Decode {
                type_name: clean(type_name),
                message: clean(message),
            },
            DriverError::Other(message) => DriverError::Other(clean(message)),
        }
    }

    fn why(&self) -> String {
        self.why.lock().expect("lock poisoned").clone()
    }

    /// The error of everything that was waiting on a plugin that stopped.
    pub(crate) fn lost(&self) -> DriverError {
        DriverError::Connection(format!(
            "the engine's plugin ({}) stopped: {}",
            self.name,
            self.why()
        ))
    }

    /// The plugin is gone: every request fails, every result ends with an error and every
    /// session's connection is closed. Nothing is retried.
    pub(crate) fn die(&self, why: &str) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.why.lock().expect("lock poisoned") = why.to_string();
        // Dropping the senders fails the requests that wait.
        self.pending.lock().expect("lock poisoned").clear();
        let streams: Vec<Receiving> = self
            .streams
            .lock()
            .expect("lock poisoned")
            .drain()
            .map(|(_, receiving)| receiving)
            .collect();
        for receiving in streams {
            let _ = receiving.events.send(Err(self.lost()));
        }
        for closed in self.sessions.lock().expect("lock poisoned").values() {
            closed.store(true, Ordering::SeqCst);
        }
    }

    /// One frame from the plugin. An error is why the conversation cannot go on.
    fn arrived(&self, frame: Frame) -> std::result::Result<(), String> {
        match frame {
            Frame::Data {
                stream,
                start,
                bytes,
            } => {
                let mut streams = self.streams.lock().expect("lock poisoned");
                // A stream nobody reads any more: what was already on its way is dropped.
                let Some(receiving) = streams.get_mut(&stream) else {
                    return Ok(());
                };
                if start {
                    receiving.decoder = BatchDecoder::new();
                }
                match receiving.decoder.push(bytes) {
                    Ok(items) => {
                        for item in items {
                            let event = match item {
                                Decoded::Schema(schema) => {
                                    result_schema(schema).map(|s| ExecEvent::Columns(Arc::new(s)))
                                }
                                Decoded::Batch(batch) => Ok(ExecEvent::Batch(batch)),
                            };
                            let _ = receiving.events.send(event);
                        }
                    }
                    Err(error) => {
                        let _ = receiving.events.send(Err(DriverError::Other(format!(
                            "the plugin sent rows that cannot be read: {error}"
                        ))));
                    }
                }
                Ok(())
            }
            Frame::Message(bytes) => {
                let message = Message::from_bytes(&bytes)
                    .map_err(|_| "it sent a message that is not JSON-RPC".to_string())?;
                match (&message.method, message.id) {
                    (Some(name), None) => self.notified(name, &message),
                    // This side answers nothing: a plugin asks the core for nothing.
                    (Some(name), Some(id)) => {
                        self.send(Message::error(id, RpcError::method_not_found(name)));
                    }
                    (None, Some(id)) => {
                        let waiting = self.pending.lock().expect("lock poisoned").remove(&id);
                        if let Some(waiting) = waiting {
                            let _ = waiting.send(match message.error {
                                Some(error) => Err(error),
                                None => Ok(message.result.unwrap_or(Value::Null)),
                            });
                        }
                    }
                    (None, None) => {}
                }
                Ok(())
            }
        }
    }

    fn notified(&self, name: &str, message: &Message) {
        match name {
            method::SESSION_CLOSED => {
                if let Ok(params) = message.params::<SessionRequest>()
                    && let Some(closed) = self
                        .sessions
                        .lock()
                        .expect("lock poisoned")
                        .get(&params.session)
                {
                    closed.store(true, Ordering::SeqCst);
                }
            }
            method::STREAM_NOTICE => {
                if let Ok(params) = message.params::<StreamNotice>() {
                    self.event(
                        params.stream,
                        Ok(ExecEvent::Notice(Notice {
                            severity: self.scrub(&params.severity),
                            message: self.scrub(&params.message),
                        })),
                    );
                }
            }
            method::STREAM_DONE => {
                if let Ok(params) = message.params::<StreamDone>() {
                    self.event(
                        params.stream,
                        Ok(ExecEvent::Done {
                            rows_affected: params.rows_affected,
                        }),
                    );
                }
            }
            method::STREAM_ERROR => {
                if let Ok(params) = message.params::<StreamError>() {
                    self.event(params.stream, Err(self.scrub_error(params.error.into())));
                }
            }
            method::STREAM_END => {
                if let Ok(params) = message.params::<StreamCredit>() {
                    // Dropping the sender ends the stream for its reader.
                    self.streams
                        .lock()
                        .expect("lock poisoned")
                        .remove(&params.stream);
                }
            }
            // One this version does not know.
            _ => {}
        }
    }

    fn event(&self, stream: StreamId, event: Result<ExecEvent>) {
        if let Some(receiving) = self.streams.lock().expect("lock poisoned").get(&stream) {
            let _ = receiving.events.send(event);
        }
    }
}

/// A request whose caller may stop waiting: its place is given up then.
struct Waiting<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.shared
            .pending
            .lock()
            .expect("lock poisoned")
            .remove(&self.id);
    }
}

fn write_loop(stdin: ChildStdin, frames: mpsc::Receiver<Vec<Frame>>) {
    let mut stdin = BufWriter::with_capacity(PIPE_BUFFER, stdin);
    while let Ok(set) = frames.recv() {
        let mut write = |set: Vec<Frame>| {
            set.iter()
                .all(|frame| write_frame(&mut stdin, frame).is_ok())
        };
        if !write(set) {
            return;
        }
        // Whatever else is waiting goes out before the flush.
        while let Ok(set) = frames.try_recv() {
            if !write(set) {
                return;
            }
        }
        if stdin.flush().is_err() {
            return;
        }
    }
    // Every sender is gone: closing its input is how the plugin is told to end.
}

/// However the thread ends, nothing waits on the plugin for ever.
struct Gone(Arc<Shared>);

impl Drop for Gone {
    fn drop(&mut self) {
        self.0.die("it stopped");
    }
}

/// Reads what the plugin says to its end: what it had already written when it exited is still
/// heard.
fn read_loop(stdout: impl Read, shared: Arc<Shared>) {
    let gone = Gone(shared);
    let shared = &gone.0;
    let mut stdout = BufReader::with_capacity(PIPE_BUFFER, stdout);
    let why = loop {
        match read_frame(&mut stdout) {
            Ok(Some(frame)) => {
                if let Err(why) = shared.arrived(frame) {
                    break why;
                }
            }
            Ok(None) => break "it exited".to_string(),
            Err(error) => break format!("its output is not the plugin protocol ({error})"),
        }
    };
    shared.die(&why);
}

/// Reads the plugin's standard error to its end, so it never blocks on a full pipe, and hands
/// each line, bounded and with the secrets taken out, to the sink.
fn log_loop(mut stderr: impl Read, shared: Arc<Shared>, log: Option<LogSink>) {
    let mut line: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 8192];
    let emit = |line: &mut Vec<u8>| {
        if let Some(log) = &log {
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end_matches(['\r', '\n']);
            if !text.is_empty() {
                log(&shared.scrub(text));
            }
        }
        line.clear();
    };
    loop {
        let read = match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        for &byte in &buffer[..read] {
            if byte == b'\n' {
                emit(&mut line);
            } else if line.len() < LOG_LINE {
                line.push(byte);
            }
        }
    }
    emit(&mut line);
}
