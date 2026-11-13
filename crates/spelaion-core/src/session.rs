//! A session is a connection with its own state (transaction, `SET`, temporary tables).
//! In the app there is one per editor tab (ADR-004).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use spelaion_config::Safeguards;
use spelaion_driver_api::{
    Connection, Dialect, DriverError, ExecEvent, ExecOptions, Notice, ResultSchema, StatementSpan,
};

use crate::health::{Health, SessionHealth, Tracked};
use crate::masking::MaskPolicy;
use crate::results::{ResultId, ResultSource, ResultStore};
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
    /// Unique in this process: a result read through this session remembers it (SDB-137).
    id: u64,
    /// Every use goes through [`Tracked`] (ADR-028).
    connection: Arc<dyn Connection>,
    tracked: Arc<Tracked>,
    health: Health,
    keep_alive: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    idle_close: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The driver's dialect, as the session's `sql_mode` reads text (SDB-161).
    dialect: Arc<crate::sql_mode::SessionDialect>,
    results: Arc<ResultStore>,
    options: ExecOptions,
    /// Held while a script runs: one at a time, and no Commit or Rollback meanwhile.
    control: Arc<tokio::sync::Mutex<()>>,
    /// Runs the user sees as running (their scripts, the core's generated statements such as a
    /// table's page) that hold `control` or wait for it. Commit, Rollback and the auto-commit
    /// switch are refused as busy while there is one; the core's own short reads (the editor's
    /// schema, a catalog) only make them wait (SDB-277).
    user_runs: Arc<AtomicUsize>,
    transaction: Arc<tokio::sync::watch::Sender<TransactionStatus>>,
    /// The run that waits for an answer after an error ([`OnError::Ask`]), if any.
    pause: Arc<Pause>,
    /// How many times the user asked to cancel: a run that sees it change stops.
    cancels: Arc<AtomicU64>,
    /// Cancels still on their way to the engine: a new run, or a commit, waits for them before
    /// it sends anything (SDB-266).
    cancel_gate: CancelGate,
    /// The connection's column masking (ADR-051), applied to every result this session stores
    /// from a run, and by the core's own reads that do not go through the result store.
    masking: Arc<MaskPolicy>,
    /// The connection is read-only (ADR-053): a run refuses, before anything is sent, a script
    /// with a statement that is not a read, a session setting or transaction control.
    read_only: bool,
}

/// How long Commit, Rollback or the auto-commit switch wait for one of the core's own reads
/// that holds the session (SDB-277) before they say it is busy.
const CORE_READ_WAIT: Duration = Duration::from_secs(10);

/// A run the user sees, counted in [`Session::user_runs`] from when it asks for the session
/// until it lets go of it (or is dropped).
struct UserRun(Arc<AtomicUsize>);

impl UserRun {
    fn new(count: Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for UserRun {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// All paths share session serialization and state observation. Only editor SQL manages
/// transactions; core metadata is not work the user ran in their open transaction.
#[derive(Clone, Copy)]
enum RunKind {
    User,
    Generated,
    /// A generated read whose values the core parses itself (counts, statistics): its result
    /// is not masked, and the caller applies [`Session::masking`] to what it shows (ADR-051).
    Parsed,
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

/// Whether a session of this engine is always in some database that its edits depend on: SQL
/// Server and DuckDB write `dbo.t` or `main.t`, which is another table in another database.
/// There a database the driver cannot name is unknown (after an ambiguous `USE`), and rows
/// read in it are never editable. PostgreSQL and SQLite sessions stay in the database they
/// opened; a MySQL session may have no database, and its edits name the table's database.
fn database_can_change(engine: &str) -> bool {
    matches!(engine, "sqlserver" | "duckdb")
}

/// The database a session is in, when it is known: `Some(None)` only for an engine whose
/// sessions cannot change database (SDB-137).
fn known_database(
    snapshot: Result<Option<String>, spelaion_driver_api::DriverError>,
    can_change: bool,
) -> Option<Option<String>> {
    match snapshot {
        Ok(Some(database)) => Some(Some(database)),
        Ok(None) if !can_change => Some(None),
        Ok(None) | Err(_) => None,
    }
}

/// The next [`Session`]'s id: sessions are never told apart by anything the UI sends.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

impl Session {
    pub fn new(
        connection: Box<dyn Connection>,
        dialect: Arc<dyn Dialect>,
        results: Arc<ResultStore>,
    ) -> Self {
        let tracked = Arc::new(Tracked::new(Arc::from(connection)));
        let health = tracked.health().clone();
        Self {
            id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            connection: tracked.clone(),
            tracked,
            health,
            keep_alive: std::sync::Mutex::new(None),
            idle_close: std::sync::Mutex::new(None),
            dialect: Arc::new(crate::sql_mode::SessionDialect::new(dialect)),
            results,
            options: ExecOptions::default(),
            control: Arc::new(tokio::sync::Mutex::new(())),
            user_runs: Arc::new(AtomicUsize::new(0)),
            transaction: Arc::new(tokio::sync::watch::Sender::new(TransactionStatus::default())),
            pause: Arc::new(Pause::default()),
            cancels: Arc::new(AtomicU64::new(0)),
            cancel_gate: CancelGate::default(),
            masking: Arc::new(MaskPolicy::none()),
            read_only: false,
        }
    }

    /// Makes the session refuse every script a read-only connection does not run (ADR-053).
    /// [`crate::open_connection`] sets it from the connection.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// The session belongs to a read-only connection (ADR-053).
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Masks these columns in every result the session stores (ADR-051).
    pub fn with_masking(mut self, masking: MaskPolicy) -> Self {
        self.masking = Arc::new(masking);
        self
    }

    /// The connection's column masking (ADR-051).
    pub fn masking(&self) -> &MaskPolicy {
        &self.masking
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

    /// The dialect the session splits with: the driver's, as the session's `sql_mode` reads
    /// text (MySQL, MariaDB; SDB-161).
    pub fn dialect(&self) -> Arc<dyn Dialect> {
        self.dialect.current()
    }

    /// Reads the session's `sql_mode` again, so the next run splits as the session reads text.
    /// [`crate::open_connection`] calls it once the session setup ran; a run calls it after a
    /// statement that names `sql_mode`.
    pub async fn refresh_sql_mode(&self) {
        let _run = self.control.lock().await;
        self.cancel_gate.settled().await;
        self.dialect.refresh(self.connection.as_ref()).await;
    }

    pub fn results(&self) -> &Arc<ResultStore> {
        &self.results
    }

    /// What edits are written through now (SDB-137): this session, in the database it is in
    /// at this moment. [`crate::edit_statements`] refuses a result read elsewhere. Where the
    /// engine can change database and the driver cannot name the current one, no stored
    /// result matches it: nothing is written until the query runs again.
    pub async fn edit_target(&self) -> Result<crate::EditTarget, crate::EditError> {
        let database = self
            .connection
            .database_context()
            .await
            .map_err(|e| crate::EditError::Source(e.to_string()))?;
        Ok(crate::EditTarget::new(
            ResultSource {
                session: self.id,
                database,
            },
            self.dialect(),
        ))
    }

    /// Cancels the running statement, and the run that sent it: a cancelled script always
    /// stops, whatever its [`OnError`]. A run waiting for an answer after an error stops at
    /// once, without asking the engine anything.
    ///
    /// The engine's cancel may still be on its way when the run it was meant for has already
    /// stopped (MySQL's `KILL QUERY` and PostgreSQL's cancel request travel over a connection
    /// of their own, and stop whatever runs when they arrive). It is counted in flight before
    /// the run can see it, and a new run, a commit or a rollback waits until it has returned,
    /// so it never stops them (SDB-266). A cancel never waits for anything.
    pub async fn cancel(&self) -> Result<(), DriverError> {
        let _in_flight = self.cancel_gate.enter();
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
        self.cancel_gate.settled().await;
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
        let _running = self.transaction_control().await?;
        self.cancel_gate.settled().await;
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

    /// The session for Commit, Rollback or the auto-commit switch. Refused while a run the
    /// user sees is going or waiting: what it does is part of the transaction. The core's own
    /// short reads that hold the session meanwhile (the editor reads the session's schema after
    /// every run) are waited for, at most [`CORE_READ_WAIT`], instead of refusing a click that
    /// came right after a run (SDB-277).
    async fn transaction_control(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, ()>, TransactionError> {
        if let Ok(guard) = self.control.try_lock() {
            return Ok(guard);
        }
        if self.user_runs.load(Ordering::SeqCst) > 0 {
            return Err(TransactionError::Busy);
        }
        tokio::time::timeout(CORE_READ_WAIT, self.control.lock())
            .await
            .map_err(|_| TransactionError::Busy)
    }

    async fn end_transaction(&self, commit: bool) -> Result<TransactionOutcome, TransactionError> {
        let _running = self.transaction_control().await?;
        // A late cancel must not stop the COMMIT or ROLLBACK (SDB-266).
        self.cancel_gate.settled().await;
        if !self.transaction_status().supported {
            return Err(TransactionError::Unsupported);
        }
        let failed = |e: DriverError| TransactionError::Failed(e.to_string());
        let before = self.connection.transaction_state().await.map_err(failed)?;
        if before == TransactionState::Idle {
            self.transaction.send_modify(|s| s.observe(before));
            return Err(TransactionError::NotOpen);
        }
        let engine = self.dialect().name();
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

    /// Runs a read the core wrote and parses itself (counts, statistics), without masking its
    /// result (ADR-051): the caller must apply [`Session::masking`] to anything it shows.
    pub(crate) fn run_parsed(&self, script: &str) -> BoxStream<'static, CoreEvent> {
        self.run(script, RunKind::Parsed, ScriptOptions::default())
    }

    fn run(
        &self,
        script: &str,
        kind: RunKind,
        run_options: ScriptOptions,
    ) -> BoxStream<'static, CoreEvent> {
        self.run_with_intent(script, kind, run_options, None, None)
    }

    /// Only the core's opaque, parsed plan request can pair EXPLAIN SQL with its intent.
    pub(crate) fn run_plan(
        &self,
        plan: &crate::plan::PreparedPlan,
        options: ScriptOptions,
    ) -> BoxStream<'static, CoreEvent> {
        self.run_with_intent(
            plan.sql(),
            RunKind::User,
            options,
            Some(plan.intent().to_owned()),
            plan.driver_mode(),
        )
    }

    fn run_with_intent(
        &self,
        script: &str,
        kind: RunKind,
        run_options: ScriptOptions,
        intent: Option<String>,
        plan_mode: Option<spelaion_driver_api::PlanMode>,
    ) -> BoxStream<'static, CoreEvent> {
        // This run is requested now, even if history or another run delays its first poll.
        // A cancel during those waits stops it; a later, newly requested run gets a new epoch.
        let cancels_at_start = self.cancels.load(Ordering::SeqCst);
        let (managed, count_statement) = match kind {
            RunKind::User => (true, true),
            RunKind::Generated | RunKind::Parsed => (false, true),
            RunKind::Metadata => (false, false),
        };
        // Every result that shows rows is masked (ADR-051); execution plans show the plan,
        // not rows, and the core's own metadata and parsed reads apply it themselves.
        let masking = match kind {
            RunKind::User | RunKind::Generated if plan_mode.is_none() && intent.is_none() => {
                Some(self.masking.clone())
            }
            RunKind::User | RunKind::Generated | RunKind::Parsed | RunKind::Metadata => None,
        };
        let script = script.to_string();
        let dialect = self.dialect();
        // A read-only connection refuses the whole run before anything is sent (ADR-053),
        // whatever the caller checked: the user's scripts, query pages and plans all come here.
        let refusal = match kind {
            RunKind::User if self.read_only => {
                crate::guard::check_read_only(dialect.as_ref(), &script).err()
            }
            RunKind::User | RunKind::Generated | RunKind::Parsed | RunKind::Metadata => None,
        };
        let spans = dialect.split_statements(&script);
        let engine = dialect.name();
        let session_dialect = self.dialect.clone();
        let connection = self.connection.clone();
        let results = self.results.clone();
        // Results that show rows remember where they were read (SDB-137): edits are written
        // only through this session, still in the same database.
        let track_source = match kind {
            RunKind::User | RunKind::Generated => plan_mode.is_none(),
            RunKind::Parsed | RunKind::Metadata => false,
        };
        let session_id = self.id;
        let can_change = database_can_change(engine);
        let mut options = self.options.clone();
        // Masking matches each column's origin too (ADR-052): ask the driver for it.
        options.origins |= masking.is_some() && !self.masking.is_empty();
        let control = self.control.clone();
        let user_runs = match kind {
            RunKind::User | RunKind::Generated => Some(self.user_runs.clone()),
            RunKind::Parsed | RunKind::Metadata => None,
        };
        let transaction = self.transaction.clone();
        let pause = self.pause.clone();
        let cancels = self.cancels.clone();
        let cancel_gate = self.cancel_gate.clone();
        let mut health = self.health.subscribe();

        Box::pin(async_stream::stream! {
            if let Some(refusal) = refusal {
                let index = refusal.index;
                let total = spans.len();
                yield CoreEvent::StatementStarted { index, span: refusal.span };
                yield CoreEvent::StatementFailed {
                    index,
                    error: StatementError {
                        message: refusal.to_string(),
                        code: None,
                        detail: None,
                        hint: None,
                        script_offset: Some(refusal.span.start),
                        cancelled: false,
                        timed_out: false,
                    },
                };
                yield CoreEvent::ScriptFinished {
                    executed: 0,
                    failed: true,
                    summary: ScriptSummary {
                        statements: total,
                        failed: 1,
                        stop: (index + 1 < total).then_some(ScriptStop::Failed { index }),
                        ..Default::default()
                    },
                };
                return;
            }
            // A run the user sees makes Commit, Rollback and the toggle busy from now on, while
            // it waits for the session too (SDB-277).
            let seen = user_runs.map(UserRun::new);
            // One script at a time; Commit, Rollback and the toggle wait for none. Held until
            // just before the last event (below), not until the stream is polled once more.
            let running = control.lock_owned().await;
            // A cancel still on its way to the engine would stop this run's first statement:
            // nothing is sent before it has returned (SDB-266).
            cancel_gate.settled().await;
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
                let policy_text = intent.as_deref().unwrap_or(text);
                let kind = transaction::statement_kind(engine, policy_text);

                // Before: smart commit, then the manual-commit BEGIN.
                let mut before = None;
                if follow {
                    let mut status = *transaction.borrow();
                    if managed
                        && status.auto_commit
                        && status.smart_commit
                        && kind == StatementKind::Other
                        && transaction::is_write(engine, policy_text)
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
                // The database this statement starts in (a local snapshot, no SQL), and the
                // results it creates: they keep it only if it is still the same afterwards.
                let source_before = if track_source {
                    known_database(connection.database_context().await, can_change)
                } else {
                    None
                };
                let mut created: Vec<ResultId> = Vec::new();
                let execute = async {
                    match plan_mode {
                        Some(mode) => connection.execute_plan(text, mode).await,
                        None => connection.execute(text, &options).await,
                    }
                };
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
                                    let (result, schema) = match &masking {
                                        Some(policy) => results.create_masked(schema, policy),
                                        None => (results.create(schema.clone()), schema),
                                    };
                                    if let Some(database) = &source_before {
                                        results.set_source(result, Some(ResultSource {
                                            session: session_id,
                                            database: database.clone(),
                                        }));
                                    }
                                    created.push(result);
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
                // The statement changed the database (a `USE` in a batch): its rows cannot be
                // told to belong to one, so none of its results is editable.
                if let Some(before) = &source_before
                    && !created.is_empty()
                    && known_database(connection.database_context().await, can_change).as_ref()
                        != Some(before)
                {
                    for id in &created {
                        results.set_source(*id, None);
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
                // A statement that may change the session's sql_mode: later runs split as the
                // session now reads text (SDB-161).
                if session_dialect.follows_sql_mode() && crate::sql_mode::may_change_sql_mode(text) {
                    session_dialect.refresh(connection.as_ref()).await;
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
            // The session takes the next command before anyone hears the run is over: the app
            // hands this event to the window before it asks the stream for more, and the switch,
            // Commit and Rollback it enables again would find the session still busy (SDB-98).
            drop(running);
            drop(seen);
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

/// How long a run waits for cancels still on their way before it goes on anyway. Each one is
/// bounded by its engine's own connect timeout; this only keeps a cancel that never returns
/// from holding the session.
const CANCEL_SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(15);

/// The cancels of a session still on their way to the engine (SDB-266).
#[derive(Clone)]
struct CancelGate(Arc<tokio::sync::watch::Sender<usize>>);

impl Default for CancelGate {
    fn default() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(0)))
    }
}

impl CancelGate {
    /// Counts a cancel in flight until the guard is dropped (returned, failed or abandoned).
    fn enter(&self) -> CancelInFlight {
        self.0.send_modify(|n| *n += 1);
        CancelInFlight(self.0.clone())
    }

    /// Once no cancel is in flight (or [`CANCEL_SETTLE_LIMIT`] has passed).
    async fn settled(&self) {
        let mut idle = self.0.subscribe();
        let _ = tokio::time::timeout(CANCEL_SETTLE_LIMIT, idle.wait_for(|n| *n == 0)).await;
    }
}

struct CancelInFlight(Arc<tokio::sync::watch::Sender<usize>>);

impl Drop for CancelInFlight {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

    async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
        let session = Session::new(
            driver.connect(&params).await.expect("connecting"),
            driver.dialect(),
            Arc::new(ResultStore::new()),
        );
        // As an editor tab's session does (ADR-031).
        assert!(
            session
                .follow_transactions(&Safeguards::default())
                .await
                .supported
        );
        session
    }

    /// SQLite and DuckDB always; the Docker servers when their variables are set.
    async fn sessions() -> Vec<(&'static str, Session)> {
        let memory = || ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        };
        let mut out = vec![
            (
                "sqlite",
                session(
                    crate::open::test_driver(spelaion_config::Engine::Sqlite).as_ref(),
                    memory(),
                )
                .await,
            ),
            (
                "duckdb",
                session(
                    crate::open::test_driver(spelaion_config::Engine::Duckdb).as_ref(),
                    memory(),
                )
                .await,
            ),
        ];
        let server = |var: &str, user: &str, password: &str, database: &str| {
            std::env::var(var).ok().map(|address| {
                let (host, port) = address.rsplit_once(':').expect("host:port");
                ConnectParams {
                    host: host.into(),
                    port: port.parse().expect("port"),
                    user: user.into(),
                    password: Some(Secret::new(password)),
                    database: database.into(),
                    ..Default::default()
                }
            })
        };
        if let Some(params) = server("SPELAION_TEST_PG", "spelaion", "spelaion", "spelaion") {
            out.push((
                "postgres",
                session(
                    crate::open::test_driver(spelaion_config::Engine::Postgres).as_ref(),
                    params,
                )
                .await,
            ));
        }
        for (name, var, driver) in [
            (
                "mysql",
                "SPELAION_TEST_MYSQL",
                crate::open::test_driver(spelaion_config::Engine::Mysql),
            ),
            (
                "mariadb",
                "SPELAION_TEST_MARIADB",
                crate::open::test_driver(spelaion_config::Engine::Mariadb),
            ),
        ] {
            if let Some(params) = server(var, "spelaion", "spelaion", "spelaion") {
                out.push((name, session(driver.as_ref(), params).await));
            }
        }
        if let Some(mut params) = server("SPELAION_TEST_MSSQL", "sa", "Spelaion-test-1", "master") {
            params.tls = TlsMode::Require;
            out.push((
                "sqlserver",
                session(
                    crate::open::test_driver(spelaion_config::Engine::Sqlserver).as_ref(),
                    params,
                )
                .await,
            ));
        }
        out
    }

    /// The first event of a run: from then on it holds the session, until it is read to its end.
    async fn holding(
        name: &str,
        mut run: BoxStream<'static, CoreEvent>,
    ) -> BoxStream<'static, CoreEvent> {
        assert!(run.next().await.is_some(), "{name}: the run sent nothing");
        run
    }

    async fn drain(mut run: BoxStream<'static, CoreEvent>) {
        while run.next().await.is_some() {}
    }

    /// One of the core's own reads holding the session (the editor reads its schema after every
    /// run) does not make Commit, Rollback or the auto-commit switch fail: they wait for it. A run
    /// the user sees still makes them busy (SDB-277).
    #[tokio::test]
    async fn transaction_controls_wait_for_the_cores_reads_but_not_for_a_users_run() {
        for (name, session) in sessions().await {
            // The switch, while a metadata read holds the session.
            let read = holding(name, session.run_metadata("select 1")).await;
            let (off, ()) = tokio::join!(session.set_auto_commit(false), drain(read));
            assert!(
                matches!(&off, Ok(status) if !status.auto_commit),
                "{name}: {off:?}"
            );

            // In manual commit a run opens a transaction; Commit, while a parsed read holds the
            // session, is taken too.
            drain(session.run_script("select 1")).await;
            if session.transaction_status().is_open() {
                let read = holding(name, session.run_parsed("select 1")).await;
                let (committed, ()) = tokio::join!(session.commit(), drain(read));
                assert!(committed.is_ok(), "{name}: {:?}", committed.err());
            }

            // A run of the user's: busy, at once, whatever it is doing.
            let run = holding(name, session.run_script("select 1")).await;
            let on = session.set_auto_commit(true).await;
            assert!(matches!(on, Err(TransactionError::Busy)), "{name}: {on:?}");
            assert!(
                matches!(session.rollback().await, Err(TransactionError::Busy)),
                "{name}"
            );
            drain(run).await;
            assert_eq!(session.user_runs.load(Ordering::SeqCst), 0, "{name}");

            // And once it has ended, Rollback and the switch are taken (in manual commit that run
            // opened a transaction: auto-commit only comes back once it is ended).
            if session.transaction_status().is_open() {
                let rolled_back = session.rollback().await;
                assert!(rolled_back.is_ok(), "{name}: {:?}", rolled_back.err());
            }
            let on = session.set_auto_commit(true).await;
            assert!(
                matches!(&on, Ok(status) if status.auto_commit),
                "{name}: {on:?}"
            );
        }
    }
}
