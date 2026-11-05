//! A session is a connection with its own state (transaction, `SET`, temporary tables).
//! In the app there is one per editor tab (ADR-004).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use spelaion_driver_api::{
    Connection, Dialect, DriverError, ExecEvent, ExecOptions, Notice, ResultSchema, StatementSpan,
};

use crate::health::{Health, SessionHealth, Tracked};
use crate::results::{ResultId, ResultStore};

/// What happens while a script runs, in order. `index` is the position of the statement
/// within the script.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    StatementStarted {
        index: usize,
        span: StatementSpan,
    },
    ResultStarted {
        index: usize,
        result: ResultId,
        schema: Arc<ResultSchema>,
    },
    /// More rows have arrived; `total_rows` is the result's running total.
    Rows {
        index: usize,
        result: ResultId,
        total_rows: usize,
    },
    Notice {
        index: usize,
        notice: Notice,
    },
    StatementFinished {
        index: usize,
        rows_affected: Option<u64>,
        elapsed: Duration,
    },
    /// The statement failed and the script stops here.
    StatementFailed {
        index: usize,
        error: StatementError,
    },
    ScriptFinished {
        executed: usize,
        failed: bool,
    },
    /// The query still ran, but its history could not be saved.
    HistoryWarning {
        message: String,
    },
}

/// A statement error, ready to display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementError {
    pub message: String,
    pub code: Option<String>,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// Error position in bytes **within the full script**, to underline it in the editor.
    pub script_offset: Option<usize>,
    pub cancelled: bool,
}

impl StatementError {
    fn from_driver(error: DriverError, script: &str, span: StatementSpan) -> Self {
        let mut out = Self {
            message: error.to_string(),
            code: None,
            detail: None,
            hint: None,
            script_offset: None,
            cancelled: matches!(error, DriverError::Cancelled),
        };
        if let DriverError::Database {
            code,
            detail,
            hint,
            position,
            ..
        } = error
        {
            out.code = code;
            out.detail = detail;
            out.hint = hint;
            // The engine gives the position in characters (1-based) within the statement.
            out.script_offset = position.and_then(|p| {
                span.text(script)
                    .char_indices()
                    .nth((p as usize).checked_sub(1)?)
                    .map(|(byte, _)| span.start + byte)
            });
        }
        out
    }
}

pub struct Session {
    /// Every use goes through [`Tracked`] (ADR-028).
    connection: Arc<dyn Connection>,
    tracked: Arc<Tracked>,
    health: Health,
    keep_alive: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    dialect: Arc<dyn Dialect>,
    results: Arc<ResultStore>,
    options: ExecOptions,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = self.keep_alive.lock().expect("lock poisoned").take() {
            task.abort();
        }
    }
}

impl Session {
    pub fn new(
        connection: Box<dyn Connection>,
        dialect: Arc<dyn Dialect>,
        results: Arc<ResultStore>,
    ) -> Self {
        let tracked = Arc::new(Tracked::new(Arc::from(connection)));
        let health = tracked.health().clone();
        Self {
            connection: tracked.clone(),
            tracked,
            health,
            keep_alive: std::sync::Mutex::new(None),
            dialect,
            results,
            options: ExecOptions::default(),
        }
    }

    /// Whether the connection still works. Once lost it stays lost: reconnecting means a
    /// new session, and nothing is re-run (ADR-028).
    pub fn health(&self) -> tokio::sync::watch::Receiver<SessionHealth> {
        self.health.subscribe()
    }

    /// Marks the session lost, for what the session cannot see itself (its tunnel went down).
    pub fn mark_lost(&self, reason: impl Into<String>) {
        self.health.lose(reason);
    }

    pub(crate) fn health_handle(&self) -> &Health {
        &self.health
    }

    /// Keeps the session alive while it is idle: the engine's cheapest round trip after every
    /// `interval` without use, none while something runs or a transaction is open. Replaces
    /// a previous one. Agents' short sessions do not use it.
    pub fn start_keep_alive(&self, interval: std::time::Duration) {
        let task = self.tracked.keep_alive(interval);
        if let Some(previous) = self.keep_alive.lock().expect("lock poisoned").replace(task) {
            previous.abort();
        }
    }

    pub fn with_options(mut self, options: ExecOptions) -> Self {
        self.options = options;
        self
    }

    pub fn connection(&self) -> &Arc<dyn Connection> {
        &self.connection
    }

    pub fn dialect(&self) -> &Arc<dyn Dialect> {
        &self.dialect
    }

    pub fn results(&self) -> &Arc<ResultStore> {
        &self.results
    }

    /// Cancels the running statement. The script stops with `StatementFailed { cancelled }`.
    pub async fn cancel(&self) -> Result<(), DriverError> {
        self.connection.cancel_handle().cancel().await
    }

    /// Splits the script with the engine's dialect and runs its statements in order,
    /// stopping at the first error. Rows are stored in the [`ResultStore`].
    pub fn run_script(&self, script: &str) -> BoxStream<'static, CoreEvent> {
        let script = script.to_string();
        let spans = self.dialect.split_statements(&script);
        let connection = self.connection.clone();
        let results = self.results.clone();
        let options = self.options.clone();

        Box::pin(async_stream::stream! {
            let mut executed = 0;
            for (index, span) in spans.into_iter().enumerate() {
                yield CoreEvent::StatementStarted { index, span };
                let started = Instant::now();
                executed += 1;

                let mut events = match connection.execute(span.text(&script), &options).await {
                    Ok(events) => events,
                    Err(e) => {
                        yield CoreEvent::StatementFailed { index, error: StatementError::from_driver(e, &script, span) };
                        yield CoreEvent::ScriptFinished { executed, failed: true };
                        return;
                    }
                };

                let mut current: Option<ResultId> = None;
                while let Some(event) = events.next().await {
                    match event {
                        Ok(ExecEvent::Columns(schema)) => {
                            // A batch may return several result sets (SQL Server).
                            if let Some(previous) = current {
                                results.complete(previous);
                            }
                            let result = results.create(schema.clone());
                            current = Some(result);
                            yield CoreEvent::ResultStarted { index, result, schema };
                        }
                        Ok(ExecEvent::Batch(batch)) => {
                            let Some(result) = current else { continue };
                            if let Some(total_rows) = results.append(result, batch) {
                                yield CoreEvent::Rows { index, result, total_rows };
                            }
                        }
                        Ok(ExecEvent::Notice(notice)) => yield CoreEvent::Notice { index, notice },
                        Ok(ExecEvent::Done { rows_affected }) => {
                            if let Some(result) = current {
                                results.complete(result);
                            }
                            yield CoreEvent::StatementFinished { index, rows_affected, elapsed: started.elapsed() };
                        }
                        Err(e) => {
                            if let Some(result) = current {
                                results.complete(result);
                            }
                            yield CoreEvent::StatementFailed { index, error: StatementError::from_driver(e, &script, span) };
                            yield CoreEvent::ScriptFinished { executed, failed: true };
                            return;
                        }
                    }
                }
            }
            yield CoreEvent::ScriptFinished { executed, failed: false };
        })
    }
}
