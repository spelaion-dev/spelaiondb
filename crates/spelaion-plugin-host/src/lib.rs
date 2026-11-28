//! The core's side of the plugin protocol (ADR-041, `docs/plugin-protocol.md`): an engine
//! plugin's program, started and talked to, behind the same [`Driver`] and `Connection` a
//! driver compiled into the core has. The session, the result store, the app and the MCP
//! server do not know the difference.
//!
//! - **One process per plugin, per program.** [`Plugin`] starts it the first time one of its
//!   engines connects and every session of those engines uses it. A plugin that stops loses
//!   its sessions (their connections report closed, ADR-028); nothing is re-run, and the next
//!   connection starts it again.
//! - **The password goes in `connect`, over the plugin's standard input, and nowhere else:**
//!   never in its arguments or its environment. Every error, notice and line of the plugin's
//!   standard error has the secrets it was given taken out before anyone sees it.
//! - **A plugin is the wire, not the rules.** Nothing here asks a plugin what may run.
//!
//! Which program may be started (pins, where plugins are found, development builds) is the
//! core's to decide: this crate starts the program it is given.

pub mod conformance;
mod connection;
mod process;
mod usage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use spelaion_driver_api::{ConnectParams, Connection, Driver, DriverError, Result};
use spelaion_plugin_protocol::{CreateDatabase, method};
pub use spelaion_plugin_protocol::{InitializeResult, VERSION, Version};

use connection::PluginConnection;
use process::Process;

/// What a secret becomes in anything a plugin says.
pub const REDACTED: &str = "***";

/// Why a plugin stopped by [`Plugin::stop`] is dead.
const STOPPED: &str = "the plugin was stopped";

/// Where the lines a plugin writes to its standard error go, secrets already taken out.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// What decides, each time a plugin's process is about to start, whether its file may be
/// started (the core's pins, ADR-042). An error is why not: nothing is started then.
pub type StartCheck = Arc<dyn Fn(&Path) -> std::result::Result<(), String> + Send + Sync>;

/// Says whose plugin an error came from (ADR-055, point 5): `attribution` goes before every
/// message, so a report goes to the plugin's author. A cancellation has no message and stays as
/// it is.
pub fn attribute(attribution: Option<&str>, error: DriverError) -> DriverError {
    let Some(label) = attribution else {
        return error;
    };
    let say = |message: String| format!("{label}: {message}");
    match error {
        DriverError::Connection(message) => DriverError::Connection(say(message)),
        DriverError::Database {
            code,
            message,
            detail,
            hint,
            position,
        } => DriverError::Database {
            code,
            message: say(message),
            detail,
            hint,
            position,
        },
        DriverError::Cancelled => DriverError::Cancelled,
        DriverError::Decode { type_name, message } => DriverError::Decode {
            type_name,
            message: say(message),
        },
        DriverError::Other(message) => DriverError::Other(say(message)),
    }
}

/// What a plugin's running process uses (SDB-399), read by the core from the operating
/// system for that one process. A number that cannot be read is `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessUsage {
    pub pid: u32,
    /// Its memory: the working set on Windows, the resident set elsewhere.
    pub memory_bytes: Option<u64>,
    /// The processor time it has taken since it started.
    pub cpu_seconds: Option<f64>,
    /// Its processor use since the previous reading, in percent of one core (more than 100
    /// with several); none at the first reading.
    pub cpu_percent: Option<f64>,
    /// The engines it serves, by the names the configuration uses.
    pub engines: Vec<String>,
    /// The sessions it serves now.
    pub sessions: usize,
    /// How long it has run.
    pub uptime: std::time::Duration,
}

/// Where a plugin's process is in its life. It is started on the first connection and again
/// after it died or was stopped: never on its own, and nothing it ran is run again (ADR-028).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessState {
    /// It has not been started yet.
    NotStarted,
    /// Its file is being checked, or it is being started and asked what it serves.
    Starting,
    Running,
    /// It was asked to end, and has a few seconds to do so before it is killed.
    Stopping,
    /// It ended, could not be started, or was stopped: why.
    Dead {
        why: String,
    },
}

/// What a plugin holds of its process. Changed only by [`Plugin::process`] and
/// [`Plugin::stop`], one at a time; a process that dies on its own is read as dead at once
/// ([`Plugin::state`]) and replaced on the next connection.
enum Slot {
    NotStarted,
    Starting,
    Running(Arc<Process>),
    Stopping,
    Dead { why: String },
}

/// A start or a stop on its way: if it is dropped before it ends (its caller stopped waiting),
/// the slot is not left in its passing state.
struct Passing<'a> {
    slot: &'a std::sync::Mutex<Slot>,
    otherwise: Option<Slot>,
}

impl<'a> Passing<'a> {
    fn begin(slot: &'a std::sync::Mutex<Slot>, now: Slot, otherwise: Slot) -> Self {
        *slot.lock().expect("lock poisoned") = now;
        Self {
            slot,
            otherwise: Some(otherwise),
        }
    }

    fn end(mut self, slot: Slot) {
        self.otherwise = None;
        *self.slot.lock().expect("lock poisoned") = slot;
    }
}

impl Drop for Passing<'_> {
    fn drop(&mut self) {
        if let Some(otherwise) = self.otherwise.take() {
            *self.slot.lock().expect("lock poisoned") = otherwise;
        }
    }
}

/// An engine plugin: a program that is started when it is first needed and serves every
/// session of its engines until this value is dropped.
#[derive(Clone)]
pub struct Plugin {
    inner: Arc<Inner>,
}

struct Inner {
    program: PathBuf,
    log: Option<LogSink>,
    check: Option<StartCheck>,
    /// Whose plugin it is, when it is not part of SpelaionDB: put before each of its errors
    /// ([`attribute`]).
    attribution: Option<Arc<str>>,
    /// Its process. Never locked across an `.await`.
    slot: std::sync::Mutex<Slot>,
    /// Held while a process starts or stops, so two connections that arrive together start one
    /// process and a stop waits for a start.
    changing: tokio::sync::Mutex<()>,
    next_session: AtomicU64,
}

impl Plugin {
    /// The plugin whose program is `program`. Nothing is started yet.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                program: program.into(),
                log: None,
                check: None,
                attribution: None,
                slot: std::sync::Mutex::new(Slot::NotStarted),
                changing: tokio::sync::Mutex::new(()),
                next_session: AtomicU64::new(1),
            }),
        }
    }

    /// The same plugin, with the lines of its standard error sent to `log`. Only before it is
    /// shared: a plugin that already has drivers keeps the sink it had.
    pub fn with_log(self, log: LogSink) -> Self {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => Self {
                inner: Arc::new(Inner {
                    log: Some(log),
                    ..inner
                }),
            },
            Err(inner) => Self { inner },
        }
    }

    /// The same plugin, started only when `check` accepts its file: asked every time its
    /// process is about to start, never remembered. Only before it is shared, as [`with_log`].
    ///
    /// [`with_log`]: Plugin::with_log
    pub fn with_check(self, check: StartCheck) -> Self {
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => Self {
                inner: Arc::new(Inner {
                    check: Some(check),
                    ..inner
                }),
            },
            Err(inner) => Self { inner },
        }
    }

    /// The same plugin, with `attribution` put before every error it gives ([`attribute`]):
    /// whose it is, for a plugin that is not part of SpelaionDB (ADR-055). Only before it is
    /// shared, as [`with_log`].
    ///
    /// [`with_log`]: Plugin::with_log
    pub fn with_attribution(self, attribution: impl Into<String>) -> Self {
        let attribution: Arc<str> = Arc::from(attribution.into());
        match Arc::try_unwrap(self.inner) {
            Ok(inner) => Self {
                inner: Arc::new(Inner {
                    attribution: Some(attribution),
                    ..inner
                }),
            },
            Err(inner) => Self { inner },
        }
    }

    /// Whose plugin this is, when it is not part of SpelaionDB.
    pub fn attribution(&self) -> Option<&str> {
        self.inner.attribution.as_deref()
    }

    pub fn program(&self) -> &Path {
        &self.inner.program
    }

    /// The driver of one of this plugin's engines, by the name the configuration uses.
    pub fn driver(&self, engine: &str) -> PluginDriver {
        PluginDriver {
            plugin: self.clone(),
            engine: engine.to_string(),
        }
    }

    /// What the plugin said about itself, starting it if it is not running.
    pub async fn describe(&self) -> Result<InitializeResult> {
        Ok(self.process().await?.info().clone())
    }

    /// Where its process is now. A process that died on its own is dead from that moment,
    /// with its reason, not only at the next connection.
    pub fn state(&self) -> ProcessState {
        match &*self.inner.slot.lock().expect("lock poisoned") {
            Slot::NotStarted => ProcessState::NotStarted,
            Slot::Starting => ProcessState::Starting,
            Slot::Running(process) => match process.why_dead() {
                None => ProcessState::Running,
                Some(why) => ProcessState::Dead { why },
            },
            Slot::Stopping => ProcessState::Stopping,
            Slot::Dead { why } => ProcessState::Dead { why: why.clone() },
        }
    }

    /// Whether its process is running now.
    pub async fn is_running(&self) -> bool {
        match self.state() {
            ProcessState::Running => true,
            ProcessState::NotStarted
            | ProcessState::Starting
            | ProcessState::Stopping
            | ProcessState::Dead { .. } => false,
        }
    }

    /// The process, while it runs.
    fn running(&self) -> Option<Arc<Process>> {
        match &*self.inner.slot.lock().expect("lock poisoned") {
            Slot::Running(process) => (!process.is_dead()).then(|| process.clone()),
            Slot::NotStarted | Slot::Starting | Slot::Stopping | Slot::Dead { .. } => None,
        }
    }

    /// What its process uses now, if it runs: never starts it.
    pub async fn usage(&self) -> Option<ProcessUsage> {
        self.running().and_then(|process| process.usage())
    }

    /// Stops the process, and returns once it has ended: its input closes, so it drops its
    /// sessions and closes what its engines keep open, and it is killed if it has not ended
    /// after a few seconds. Its sessions are lost; the next connection starts it again.
    pub async fn stop(&self) {
        let _changing = self.inner.changing.lock().await;
        let Some(process) = self.running() else {
            return;
        };
        let stopped = || Slot::Dead {
            why: STOPPED.into(),
        };
        let stopping = Passing::begin(&self.inner.slot, Slot::Stopping, stopped());
        process.kill(STOPPED).await;
        stopping.end(stopped());
    }

    /// The running process, started if there is none or it died. Its file is checked every
    /// time a process is about to start (ADR-042).
    async fn process(&self) -> Result<Arc<Process>> {
        let _changing = self.inner.changing.lock().await;
        if let Some(process) = self.running() {
            return Ok(process);
        }
        let starting = Passing::begin(&self.inner.slot, Slot::Starting, Slot::NotStarted);
        if let Some(check) = self.inner.check.clone() {
            // Reading a whole program to hash it is not for the runtime's own threads.
            let program = self.inner.program.clone();
            let checked = tokio::task::spawn_blocking(move || check(&program))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            if let Err(why) = checked {
                let why = format!("the plugin {} was not started: {why}", self.name());
                starting.end(Slot::Dead { why: why.clone() });
                return Err(attribute(self.attribution(), DriverError::Connection(why)));
            }
        }
        match Process::start(
            &self.inner.program,
            self.inner.log.clone(),
            self.inner.attribution.clone(),
        )
        .await
        {
            Ok(process) => {
                starting.end(Slot::Running(process.clone()));
                Ok(process)
            }
            Err(error) => {
                let error = attribute(self.attribution(), error);
                starting.end(Slot::Dead {
                    why: error.to_string(),
                });
                Err(error)
            }
        }
    }

    fn name(&self) -> String {
        self.inner
            .program
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.inner.program.display().to_string())
    }
}

/// One engine of a [`Plugin`], as a driver.
#[derive(Clone)]
pub struct PluginDriver {
    plugin: Plugin,
    engine: String,
}

impl PluginDriver {
    pub fn plugin(&self) -> &Plugin {
        &self.plugin
    }

    /// The plugin's process, started if need be, once it is known to serve this engine.
    async fn process(&self) -> Result<Arc<Process>> {
        let process = self.plugin.process().await?;
        if !process.info().engines.iter().any(|e| e == &self.engine) {
            return Err(attribute(
                self.plugin.attribution(),
                DriverError::Other(format!(
                    "the plugin {} does not serve the engine {:?}",
                    self.plugin.name(),
                    self.engine
                )),
            ));
        }
        Ok(process)
    }
}

#[async_trait]
impl Driver for PluginDriver {
    fn engine(&self) -> &str {
        &self.engine
    }

    async fn create_database(&self, database: &str) -> Result<()> {
        let process = self.process().await?;
        let request = CreateDatabase {
            engine: self.engine.clone(),
            database: database.to_string(),
        };
        process
            .shared()
            .request(method::CREATE_DATABASE, request)
            .await?;
        Ok(())
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        if params.remote.is_some() {
            // A program on another machine runs through the core's own tunnel (ADR-018), which
            // cannot be handed to another process (ADR-041).
            return Err(DriverError::Other(
                "a database on another machine cannot be opened through a plugin".into(),
            ));
        }
        let process = self.process().await?;
        let session = self
            .plugin
            .inner
            .next_session
            .fetch_add(1, Ordering::Relaxed);
        let connection = PluginConnection::open(process, session, &self.engine, params).await?;
        Ok(Box::new(connection))
    }
}
