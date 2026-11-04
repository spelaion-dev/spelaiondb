//! A program run on another machine (ADR-018): the driver talks to it through its standard
//! input and output. The core provides it (an SSH command on a tunnel's last hop); drivers
//! only see the streams, never SSH.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::Result;

/// Starts programs on the remote machine.
#[async_trait]
pub trait RemoteCommand: Send + Sync {
    /// Runs `command` (a line for the remote shell, already quoted by the driver). Its
    /// standard error comes merged into `stdout`.
    async fn start(&self, command: &str) -> Result<RemoteProcess>;
}

/// A running remote program.
pub struct RemoteProcess {
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    pub control: Arc<dyn RemoteControl>,
}

/// What can be done to a running remote program besides writing to it.
#[async_trait]
pub trait RemoteControl: Send + Sync {
    /// Asks it to stop what it is doing (SIGINT), as Ctrl+C would.
    async fn interrupt(&self) -> Result<()>;
}

/// The remote side of a connection, in [`crate::ConnectParams`].
#[derive(Clone)]
pub struct Remote {
    pub command: Arc<dyn RemoteCommand>,
    /// The program to run there, when it is not the engine's usual name (`sqlite3`).
    pub program: Option<String>,
}

impl fmt::Debug for Remote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Remote")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}
