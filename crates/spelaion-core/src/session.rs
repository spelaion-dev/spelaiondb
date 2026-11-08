//! A session is a connection with its own state (transaction, `SET`, temporary tables).
//! In the app there is one per editor tab (ADR-004).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use spelaion_config::Safeguards;
use spelaion_driver_api::{
    Connection, Dialect, DriverError, ExecEvent, ExecOptions, Notice, ResultSchema, StatementSpan,
};

use crate::health::{Health, SessionHealth, Tracked};
use crate::results::{ResultId, ResultStore};
pub use crate::script::{
    AnswerError, ErrorAnswer, ErrorTicket, OnError, ScriptOptions, ScriptStop, ScriptSummary,
};
use crate::script::{Pause, Resume};
use crate::transaction::{
    self, StatementKind, TransactionError, TransactionOutcome, TransactionState, TransactionStatus,
};

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
        /// The whole statement, with the manual-commit `BEGIN` sent before it.
        elapsed: Duration,
        /// Until the engine's first answer (its columns, its row count or its error).
        execution: Duration,
        /// From that first answer until its last row.
        fetch: Duration,
    },
    /// The statement failed. What follows depends on the run's [`OnError`]: the script stops,
    /// goes on, or pauses with [`CoreEvent::ErrorPaused`].
    StatementFailed {
        index: usize,
        error: StatementError,
    },
    /// The run waits for [`Session::answer_error`] with this ticket (the [`OnError::Ask`]
    /// mode): stop, skip the failed statement or skip every later error. Cancelling or closing
    /// the session stops it.
    ErrorPaused {
        index: usize,
        ticket: ErrorTicket,
        /// Statements after the failed one that have not run.
        remaining: usize,
    },
    ScriptFinished {
        executed: usize,
        /// At least one statement failed.
        failed: bool,
        summary: ScriptSummary,
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
    /// The user cancelled the statement.
    pub cancelled: bool,
    /// The statement ran longer than the run's statement timeout and was cancelled.
    pub timed_out: bool,
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
            timed_out: false,
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
    idle_close: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    dialect: Arc<dyn Dialect>,
    results: Arc<ResultStore>,
    options: ExecOptions,
    /// Held while a script runs: one at a time, and no Commit or Rollback meanwhile.
    control: Arc<tokio::sync::Mutex<()>>,
    transaction: Arc<tokio::sync::watch::Sender<TransactionStatus>>,
    /// The run that waits for an answer after an error ([`OnError::Ask`]), if any.
    pause: Arc<Pause>,
    /// How many times the user asked to cancel: a run that sees it change stops.
    cancels: Arc<AtomicU64>,
}

/// All paths share session serialization and state observation. Only editor SQL manages
/// transactions; core metadata is not work the user ran in their open transaction.
#[derive(Clone, Copy)]
enum RunKind {
    User,
    Generated,
    Metadata,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.pause.resume(Resume::Closed);
        if let Some(task) = self.keep_alive.lock().expect("lock poisoned").take() {
            task.abort();
        }
        if let Some(task) = self.idle_close.lock().expect("lock poisoned").take() {
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
            idle_close: std::sync::Mutex::new(None),
            dialect,
            results,
            options: ExecOptions::default(),
            control: Arc::new(tokio::sync::Mutex::new(())),
            transaction: Arc::new(tokio::sync::watch::Sender::new(TransactionStatus::default())),
            pause: Arc::new(Pause::default()),
            cancels: Arc::new(AtomicU64::new(0)),
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

    /// Marks the session [`SessionHealth::IdleClosed`] once it has gone `after` without use
    /// (ADR-029): never while a transaction is open, and never on an engine that cannot tell.
    /// The owner then closes it; nothing reconnects. Replaces a previous one.
    pub fn close_when_idle(&self, after: std::time::Duration) {
        let task = self.tracked.close_when_idle(after);
        if let Some(previous) = self.idle_close.lock().expect("lock poisoned").replace(task) {
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

    /// Cancels the running statement, and the run that sent it: a cancelled script always
    /// stops, whatever its [`OnError`]. A run waiting for an answer after an error stops at
    /// once, without asking the engine anything.
    pub async fn cancel(&self) -> Result<(), DriverError> {
        self.cancels.fetch_add(1, Ordering::SeqCst);
        if self.pause.resume(Resume::Cancelled) {
            return Ok(());
        }
        self.connection.cancel_handle().cancel().await
    }

    /// Answers the run that paused after an error ([`CoreEvent::ErrorPaused`]). The ticket
    /// must be the one it paused with: an answer to an older pause, or after the run stopped,
    /// is refused.
    pub fn answer_error(
        &self,
        ticket: ErrorTicket,
        answer: ErrorAnswer,
    ) -> Result<(), AnswerError> {
        self.pause.answer(ticket, answer)
    }

    /// Stops a run that waits for an answer, as when its tab closes: nothing else runs.
    /// Returns whether one was waiting.
    pub fn stop_paused_run(&self) -> bool {
        self.pause.resume(Resume::Closed)
    }

    /// What the session shows about its transaction (ADR-031). Changes are sent to every
    /// receiver.
    pub fn transaction(&self) -> tokio::sync::watch::Receiver<TransactionStatus> {
        self.transaction.subscribe()
    }

    /// The last [`TransactionStatus`] read; also after the session was lost, to say whether it
    /// had a transaction open.
    pub fn transaction_status(&self) -> TransactionStatus {
        *self.transaction.borrow()
    }

    /// Makes this an editor session that follows its transaction, under its connection type's
    /// safeguards: auto-commit unless the type asks for manual commit, and smart commit when it
    /// asks for it (ADR-031). An engine that cannot tell whether a transaction is open stays in
    /// auto-commit and says so ([`TransactionStatus::supported`]).
    pub async fn follow_transactions(&self, safeguards: &Safeguards) -> TransactionStatus {
        let _running = self.control.lock().await;
        let status = match self.connection.transaction_state().await {
            Ok(state) => transaction::initial_status(safeguards, state),
            Err(_) => TransactionStatus::default(),
        };
        self.transaction.send_replace(status);
        status
    }

    /// Whether closing the session (closing its tab, switching its connection, quitting) must
    /// ask first: a transaction with work in it is open. Asks the driver once more unless a
    /// script is running, when the last state read decides.
    pub async fn close_needs_confirmation(&self) -> bool {
        let status = self.transaction_status();
        if !status.supported {
            return false;
        }
        let Ok(_running) = self.control.try_lock() else {
            return status.is_open();
        };
        match self.connection.transaction_state().await {
            Ok(state) => {
                self.transaction.send_modify(|s| s.observe(state));
                state == TransactionState::Open
            }
            // A session that cannot answer any more has nothing left to commit.
            Err(_) => false,
        }
    }

    /// Turns auto-commit on or off. Turning it on is refused while a transaction is open: the
    /// user commits or rolls back first, nothing is committed by a toggle. Turning it off sends
    /// nothing until the next statement.
    pub async fn set_auto_commit(&self, on: bool) -> Result<TransactionStatus, TransactionError> {
        let Ok(_running) = self.control.try_lock() else {
            return Err(TransactionError::Busy);
        };
        if !self.transaction_status().supported {
            return Err(TransactionError::Unsupported);
        }
        let state = self
            .connection
            .transaction_state()
            .await
            .map_err(|e| TransactionError::Failed(e.to_string()))?;
        if on && state != TransactionState::Idle {
            self.transaction.send_modify(|s| s.observe(state));
            return Err(TransactionError::Open);
        }
        self.transaction.send_modify(|s| {
            s.observe(state);
            s.auto_commit = on;
            s.smart_switched = false;
        });
        Ok(self.transaction_status())
    }

    /// Commits the open transaction.
    pub async fn commit(&self) -> Result<TransactionOutcome, TransactionError> {
        self.end_transaction(true).await
    }

    /// Rolls back the open transaction.
    pub async fn rollback(&self) -> Result<TransactionOutcome, TransactionError> {
        self.end_transaction(false).await
    }

    async fn end_transaction(&self, commit: bool) -> Result<TransactionOutcome, TransactionError> {
        let Ok(_running) = self.control.try_lock() else {
            return Err(TransactionError::Busy);
        };
        if !self.transaction_status().supported {
            return Err(TransactionError::Unsupported);
        }
        let failed = |e: DriverError| TransactionError::Failed(e.to_string());
        let before = self.connection.transaction_state().await.map_err(failed)?;
        if before == TransactionState::Idle {
            self.transaction.send_modify(|s| s.observe(before));
            return Err(TransactionError::NotOpen);
        }
        let engine = self.dialect.name();
        let sql = if commit {
            transaction::commit_statement(engine)
        } else {
            transaction::rollback_statement(engine)
        };
        let mut notices = run_control(&*self.connection, sql, &self.options)
            .await
            .map_err(failed)?;
        if commit && before == TransactionState::Failed {
            notices.push(transaction::failed_commit_notice());
        }
        // MyISAM ignores transactions: the server's warning that its changes stayed comes with
        // the ROLLBACK's notices (ADR-031), as every MySQL warning does (ADR-035).
        let after = self.connection.transaction_state().await.map_err(failed)?;
        self.transaction.send_modify(|s| s.observe(after));
        Ok(TransactionOutcome {
            status: self.transaction_status(),
            notices,
        })
    }

    /// Splits the script with the engine's dialect and runs its statements in order,
    /// stopping at the first error. Rows are stored in the [`ResultStore`]. On an editor
    /// session that follows its transaction (ADR-031), manual commit opens one before a
    /// statement that runs while none is open, smart commit switches to manual before the
    /// first write, and the state is read after every statement.
    pub fn run_script(&self, script: &str) -> BoxStream<'static, CoreEvent> {
        self.run(script, RunKind::User, ScriptOptions::default())
    }

    /// [`Session::run_script`] with its error mode and statement timeout (ADR-035). Whatever
    /// the mode, a cancelled run stops, and a failed statement that aborted or rolled back the
    /// transaction it ran in stops the run: going on would only run statements that fail, or
    /// run them without the ones before.
    pub fn run_script_with(
        &self,
        script: &str,
        options: ScriptOptions,
    ) -> BoxStream<'static, CoreEvent> {
        self.run(script, RunKind::User, options)
    }

    /// Runs statements the core wrote itself (table edits, object actions, pages, the process
    /// list) as written: never preceded by the manual-commit `BEGIN`, never switching smart
    /// commit. The transaction state is still read afterwards.
    pub(crate) fn run_generated(&self, script: &str) -> BoxStream<'static, CoreEvent> {
        self.run(script, RunKind::Generated, ScriptOptions::default())
    }

    /// Reads internal session metadata without opening a managed transaction or counting it
    /// as a user statement. Still serialized, tracked, cancellable, and observed afterwards.
    /// Callers supply only fixed metadata SQL written by the core.
    pub(crate) fn run_metadata(&self, script: &str) -> BoxStream<'static, CoreEvent> {
        self.run(script, RunKind::Metadata, ScriptOptions::default())
    }

    fn run(
        &self,
        script: &str,
        kind: RunKind,
        run_options: ScriptOptions,
    ) -> BoxStream<'static, CoreEvent> {
        // This run is requested now, even if history or another run delays its first poll.
        // A cancel during those waits stops it; a later, newly requested run gets a new epoch.
        let cancels_at_start = self.cancels.load(Ordering::SeqCst);
        let (managed, count_statement) = match kind {
            RunKind::User => (true, true),
            RunKind::Generated => (false, true),
            RunKind::Metadata => (false, false),
        };
        let script = script.to_string();
        let spans = self.dialect.split_statements(&script);
        let engine = self.dialect.name();
        let connection = self.connection.clone();
        let results = self.results.clone();
        let options = self.options.clone();
        let control = self.control.clone();
        let transaction = self.transaction.clone();
        let pause = self.pause.clone();
        let cancels = self.cancels.clone();
        let mut health = self.health.subscribe();

        Box::pin(async_stream::stream! {
            // One script at a time; Commit, Rollback and the toggle wait for none.
            let _running = control.lock_owned().await;
            let cancelled = || cancels.load(Ordering::SeqCst) != cancels_at_start;
            let follow = transaction.borrow().supported;
            let mut on_error = run_options.on_error;
            let mut summary = ScriptSummary { statements: spans.len(), ..Default::default() };
            let total = spans.len();
            for (index, span) in spans.into_iter().enumerate() {
                if cancelled() {
                    summary.stop = Some(ScriptStop::Cancelled { index });
                    break;
                }
                yield CoreEvent::StatementStarted { index, span };
                // A stream consumer may await history/context work after this yield. A cancel
                // during that pause must stop before BEGIN or the user's SQL reaches a driver.
                if cancelled() {
                    summary.stop = Some(ScriptStop::Cancelled { index });
                    break;
                }
                let started = Instant::now();
                summary.executed += 1;
                let text = span.text(&script);
                let kind = transaction::statement_kind(engine, text);

                // Before: smart commit, then the manual-commit BEGIN.
                let mut before = None;
                if follow {
                    let mut status = *transaction.borrow();
                    if managed
                        && status.auto_commit
                        && status.smart_commit
                        && kind == StatementKind::Other
                        && transaction::is_write(engine, text)
                    {
                        status.auto_commit = false;
                        status.smart_switched = true;
                        transaction.send_replace(status);
                    }
                    if managed
                        && !status.auto_commit
                        && kind == StatementKind::Other
                        && status.state == Some(TransactionState::Idle)
                    {
                        if let Err(e) = run_control(&*connection, transaction::begin_statement(engine), &options).await {
                            let mut error = StatementError::from_driver(e, &script, span);
                            error.message = format!("could not open a transaction: {}", error.message);
                            error.script_offset = None;
                            summary.failed += 1;
                            if index + 1 < total {
                                summary.stop = Some(ScriptStop::Failed { index });
                            }
                            yield CoreEvent::StatementFailed { index, error };
                            break;
                        }
                        status.state = Some(TransactionState::Open);
                        status.statements = 0;
                        transaction.send_replace(status);
                    }
                    before = Some(status);
                }

                // The statement, with its timeout: once it fires the statement is cancelled
                // and its error says it timed out.
                let deadline = run_options
                    .statement_timeout
                    .map(|timeout| tokio::time::Instant::now() + timeout);
                let mut timed_out = false;
                let sent = Instant::now();
                let mut answered: Option<Instant> = None;
                let mut failure = None;
                let mut rows_affected = None;
                let mut notices = Vec::new();
                let execute = connection.execute(text, &options);
                tokio::pin!(execute);
                let opened = loop {
                    match until(&mut execute, deadline, timed_out).await {
                        Waited::Ready(opened) => break opened,
                        Waited::TimedOut => {
                            timed_out = true;
                            let _ = connection.cancel_handle().cancel().await;
                        }
                    }
                };
                match opened {
                    Err(e) => {
                        answered = Some(Instant::now());
                        failure = Some(StatementError::from_driver(e, &script, span));
                    }
                    Ok(mut events) => {
                        let mut current: Option<ResultId> = None;
                        loop {
                            let event = match until(events.next(), deadline, timed_out).await {
                                Waited::Ready(event) => event,
                                Waited::TimedOut => {
                                    timed_out = true;
                                    let _ = connection.cancel_handle().cancel().await;
                                    continue;
                                }
                            };
                            let Some(event) = event else { break };
                            answered.get_or_insert_with(Instant::now);
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
                                Ok(ExecEvent::Notice(notice)) => notices.push(notice),
                                Ok(ExecEvent::Done { rows_affected: rows }) => {
                                    if let Some(result) = current.take() {
                                        results.complete(result);
                                    }
                                    rows_affected = rows;
                                }
                                Err(e) => {
                                    if let Some(result) = current.take() {
                                        results.complete(result);
                                    }
                                    failure = Some(StatementError::from_driver(e, &script, span));
                                    break;
                                }
                            }
                        }
                    }
                }
                let finished = Instant::now();
                let answered = answered.unwrap_or(finished);
                let execution = answered.saturating_duration_since(sent);
                let fetch = finished.saturating_duration_since(answered);
                summary.execution += execution;
                summary.fetch += fetch;
                let user_cancelled = cancelled();
                if let Some(error) = failure.as_mut() {
                    if user_cancelled {
                        // Some engines report a cancel as an ordinary error (PostgreSQL's 57014).
                        error.cancelled = true;
                    } else if timed_out {
                        error.cancelled = false;
                        error.timed_out = true;
                        let limit = run_options.statement_timeout.unwrap_or_default();
                        error.message = format!(
                            "the statement ran longer than its timeout ({}) and was cancelled: {}",
                            seconds(limit),
                            error.message
                        );
                    }
                }
                for notice in notices {
                    yield CoreEvent::Notice { index, notice };
                }

                // After: what the driver says now.
                let mut after = None;
                if let Some(before) = before
                    && let Ok(state) = connection.transaction_state().await
                {
                    after = Some(state);
                    let had_work = before.state == Some(TransactionState::Open) && before.statements > 0;
                    let ended = state == TransactionState::Idle && kind != StatementKind::End;
                    // MySQL commits the open transaction when a new one begins: the state
                    // stays open, so only the statement tells.
                    let restarted = engine == "mysql" && kind == StatementKind::Begin && failure.is_none();
                    if had_work && (ended || restarted) {
                        let notice = transaction::ended_by_statement(engine, failure.is_some());
                        yield CoreEvent::Notice { index, notice };
                    }
                    transaction.send_modify(|status| {
                        status.observe(state);
                        if state == TransactionState::Open {
                            if restarted {
                                status.statements = 0;
                            }
                            if count_statement && !kind.is_control() {
                                status.statements += 1;
                            }
                        }
                    });
                }

                let Some(error) = failure else {
                    summary.succeeded += 1;
                    yield CoreEvent::StatementFinished {
                        index,
                        rows_affected,
                        elapsed: started.elapsed(),
                        execution,
                        fetch,
                    };
                    continue;
                };
                summary.failed += 1;
                yield CoreEvent::StatementFailed { index, error };
                let remaining = total - index - 1;
                if remaining == 0 {
                    break;
                }
                if user_cancelled {
                    summary.stop = Some(ScriptStop::Cancelled { index });
                    break;
                }
                if on_error == OnError::Stop {
                    summary.stop = Some(ScriptStop::Failed { index });
                    break;
                }
                // Going on must respect the transaction the statement ran in. A session that
                // does not follow its transaction is still asked whether one was aborted.
                if after.is_none() && !follow {
                    after = connection.transaction_state().await.ok();
                }
                if after == Some(TransactionState::Failed) {
                    summary.stop = Some(ScriptStop::TransactionAborted { index });
                    break;
                }
                if before.is_some_and(|b| b.in_transaction())
                    && after == Some(TransactionState::Idle)
                    && kind != StatementKind::End
                {
                    summary.stop = Some(ScriptStop::TransactionRolledBack { index });
                    break;
                }
                if on_error == OnError::Ask {
                    let (ticket, answer) = pause.wait();
                    yield CoreEvent::ErrorPaused { index, ticket, remaining };
                    let lost = health.wait_for(|h| *h != SessionHealth::Alive);
                    let resume = tokio::select! {
                        answer = answer => answer.unwrap_or(Resume::Closed),
                        _ = lost => Resume::Closed,
                    };
                    pause.clear(ticket);
                    match resume {
                        Resume::Answer(ErrorAnswer::Skip) => {}
                        Resume::Answer(ErrorAnswer::SkipAll) => on_error = OnError::Continue,
                        Resume::Answer(ErrorAnswer::Stop) => {
                            summary.stop = Some(ScriptStop::Failed { index });
                            break;
                        }
                        Resume::Cancelled => {
                            summary.stop = Some(ScriptStop::Cancelled { index });
                            break;
                        }
                        Resume::Closed => {
                            summary.stop = Some(ScriptStop::Closed { index });
                            break;
                        }
                    }
                }
            }
            yield CoreEvent::ScriptFinished {
                executed: summary.executed,
                failed: summary.failed > 0,
                summary,
            };
        })
    }
}

/// What [`until`] got.
enum Waited<T> {
    Ready(T),
    /// The deadline passed first; the future is still there to be awaited again.
    TimedOut,
}

/// Awaits `future` until `deadline`, unless the deadline already fired (`fired`): then it
/// waits for as long as it takes (the engine is answering the cancel).
async fn until<F: std::future::Future>(
    future: F,
    deadline: Option<tokio::time::Instant>,
    fired: bool,
) -> Waited<F::Output> {
    match deadline {
        Some(deadline) if !fired => tokio::select! {
            output = future => Waited::Ready(output),
            () = tokio::time::sleep_until(deadline) => Waited::TimedOut,
        },
        _ => Waited::Ready(future.await),
    }
}

/// A duration as the editor writes it: "1.5 s", "30 s".
fn seconds(duration: Duration) -> String {
    let secs = duration.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{secs:.0} s")
    } else {
        format!("{secs:.1} s")
    }
}

/// Runs a transaction statement the core sends itself (`BEGIN`, `COMMIT`, `ROLLBACK`) and
/// returns the engine's notices.
async fn run_control(
    connection: &dyn Connection,
    sql: &str,
    options: &ExecOptions,
) -> Result<Vec<Notice>, DriverError> {
    let mut events = connection.execute(sql, options).await?;
    let mut notices = Vec::new();
    while let Some(event) = events.next().await {
        match event? {
            ExecEvent::Notice(notice) => notices.push(notice),
            ExecEvent::Columns(_) | ExecEvent::Batch(_) | ExecEvent::Done { .. } => {}
        }
    }
    Ok(notices)
}
