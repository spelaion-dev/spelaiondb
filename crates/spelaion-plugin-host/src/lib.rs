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

/// Where the lines a plugin writes to its standard error go, secrets already taken out.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// An engine plugin: a program that is started when it is first needed and serves every
/// session of its engines until this value is dropped.
#[derive(Clone)]
pub struct Plugin {
    inner: Arc<Inner>,
}

struct Inner {
    program: PathBuf,
    log: Option<LogSink>,
    /// The running process, if any. The lock is held while one starts, so two connections
    /// that arrive together start one process.
    running: tokio::sync::Mutex<Option<Arc<Process>>>,
    next_session: AtomicU64,
}

impl Plugin {
    /// The plugin whose program is `program`. Nothing is started yet.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                program: program.into(),
                log: None,
                running: tokio::sync::Mutex::new(None),
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

    /// Whether its process is running now.
    pub async fn is_running(&self) -> bool {
        self.inner
            .running
            .lock()
            .await
            .as_ref()
            .is_some_and(|p| !p.is_dead())
    }

    /// Stops the process. Its sessions are lost; the next connection starts it again.
    pub async fn stop(&self) {
        if let Some(process) = self.inner.running.lock().await.take() {
            process.kill("the plugin was stopped").await;
        }
    }

    /// The running process, started if there is none or it died.
    async fn process(&self) -> Result<Arc<Process>> {
        let mut running = self.inner.running.lock().await;
        if let Some(process) = running.as_ref()
            && !process.is_dead()
        {
            return Ok(process.clone());
        }
        let process = Process::start(&self.inner.program, self.inner.log.clone()).await?;
        *running = Some(process.clone());
        Ok(process)
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
            return Err(DriverError::Other(format!(
                "the plugin {} does not serve the engine {:?}",
                self.plugin.name(),
                self.engine
            )));
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
