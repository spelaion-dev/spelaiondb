//! A session is a connection with its own state (transaction, `SET`, temporary tables).
//! In the app there is one per editor tab (ADR-004).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use spelaion_config::Safeguards;
use spelaion_driver_api::{
    Connection, Dialect, DriverError, ExecEvent, ExecOptions, Notice, ResultSchema, StatementSpan,
};

use crate::health::{Health, SessionHealth, Tracked};
use crate::masking::MaskPolicy;
use crate::results::{PartialReason, ResultEnd, ResultId, ResultSource, ResultStore};
pub use crate::run_state::RunState;
use crate::run_state::Runs;
pub use crate::script::{
    AnswerError, ErrorAnswer, ErrorTicket, OnError, ScriptOptions, ScriptStop, ScriptSummary,
};
use crate::script::{Pause, Resume};
use crate::transaction::{
    self, StatementKind, TransactionError, TransactionLifecycle, TransactionOutcome,
    TransactionState, TransactionStatus,
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
    /// No more rows arrive for this result (SDB-367): it is whole, or it was cut short and
    /// says why. Sent before its statement's [`CoreEvent::StatementFinished`] or
    /// [`CoreEvent::StatementFailed`]; `end` is never [`ResultEnd::Loading`].
    ResultEnded {
        index: usize,
        result: ResultId,
        end: ResultEnd,
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
    /// The run ended; its summary says how. Built only by [`CoreEvent::finished`]: `executed`
    /// and `failed` are the summary's, for readers that only ask that.
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

impl CoreEvent {
    /// The run's last event, from its summary.
    pub(crate) fn finished(summary: ScriptSummary) -> Self {
        CoreEvent::ScriptFinished {
            executed: summary.executed,
            failed: summary.failed > 0,
            summary,
        }
    }
}

/// How a statement that did not run to its end ended: the words of [`crate::JobEnd`] where
/// they mean the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementEnd {
    /// The engine, the connection or the core refused or stopped it with an error.
    Failed,
    /// The user cancelled it.
    Cancelled,
    /// It ran longer than the run's statement timeout and was cancelled (ADR-035).
    TimedOut,
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
    /// How the statement ended.
    pub end: StatementEnd,
    /// `end` is [`StatementEnd::Cancelled`]. Set only with `end` ([`StatementError::ended`]).
    pub cancelled: bool,
    /// `end` is [`StatementEnd::TimedOut`]. Set only with `end` ([`StatementError::ended`]).
    pub timed_out: bool,
}

impl StatementError {
    /// A statement that failed with `message` and nothing more to show.
    pub(crate) fn failed(message: String, script_offset: Option<usize>) -> Self {
        Self {
            message,
            code: None,
            detail: None,
            hint: None,
            script_offset,
            end: StatementEnd::Failed,
            cancelled: false,
            timed_out: false,
        }
    }

    /// Says how the statement ended.
    pub(crate) fn ended(&mut self, end: StatementEnd) {
        self.end = end;
        match end {
            StatementEnd::Failed => (self.cancelled, self.timed_out) = (false, false),
            StatementEnd::Cancelled => (self.cancelled, self.timed_out) = (true, false),
            StatementEnd::TimedOut => (self.cancelled, self.timed_out) = (false, true),
        }
    }

    fn from_driver(error: DriverError, script: &str, span: StatementSpan) -> Self {
        let mut out = Self::failed(error.to_string(), None);
        if matches!(error, DriverError::Cancelled) {
            out.ended(StatementEnd::Cancelled);
        }
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

/// The request a statement is, for an engine whose language is not SQL (ADR-056), run in the
/// session's current database: `None` for an engine queried with SQL. A statement the core
/// cannot read is an error with its position, and nothing is sent.
async fn request_of(
    language: &dyn Dialect,
    connection: &dyn Connection,
    statement: &str,
) -> Option<Result<serde_json::Value, DriverError>> {
    if !language.reads_requests() {
        return None;
    }
    let database = connection
        .current_database()
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    Some(
        language
            .to_request(statement, &database)?
            .map_err(|error| request_error(statement, error)),
    )
}

/// A statement the core could not read, as the error the editor shows at its place.
fn request_error(statement: &str, error: spelaion_driver_api::RequestError) -> DriverError {
    let at = error.position.min(statement.len());
    // The engine-like position: characters, 1-based, within the statement.
    let position = statement
        .get(..at)
        .map(|before| before.chars().count() as u32 + 1);
    DriverError::Database {
        code: None,
        message: format!(
            "SpelaionDB could not read this statement: {}",
            error.message
        ),
        detail: None,
        hint: None,
        position,
    }
}

pub struct Session {
    /// Unique in this process: a result read through this session remembers it (SDB-137).
    id: u64,
    /// Every use goes through [`Tracked`] (ADR-028).
    connection: Arc<dyn Connection>,
    tracked: Arc<Tracked>,
    /// Behind a pooler in transaction mode (SDB-396).
    transaction_pooled: bool,
    health: Health,
    keep_alive: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    idle_close: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The connection type's idle transaction rollback (ADR-081), when started.
    idle_rollback: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The driver's dialect, as the session's `sql_mode` reads text (SDB-161).
    dialect: Arc<crate::sql_mode::SessionDialect>,
    results: Arc<ResultStore>,
    options: ExecOptions,
    /// Held while a script runs: one at a time, and no Commit or Rollback meanwhile.
    control: Arc<tokio::sync::Mutex<()>>,
    /// The runs the user sees (their scripts, the core's generated statements such as a
    /// table's page) that hold `control` or wait for it, the cancels, and the [`RunState`]
    /// they mean. Commit, Rollback and the auto-commit switch are refused as busy while it is
    /// not idle; the core's own short reads (the editor's schema, a catalog) only make them
    /// wait (SDB-277).
    runs: Arc<Runs>,
    transaction: Arc<tokio::sync::watch::Sender<TransactionLifecycle>>,
    /// The run that waits for an answer after an error ([`OnError::Ask`]), if any.
    pause: Arc<Pause>,
    /// Cancels still on their way to the engine: a new run, or a commit, waits for them before
    /// it sends anything (SDB-266).
    cancel_gate: CancelGate,
    /// The connection's column masking (ADR-051), applied to every result this session stores
    /// from a run, and by the core's own reads that do not go through the result store.
    masking: Arc<MaskPolicy>,
    /// The connection is read-only (ADR-053): a run refuses, before anything is sent, a script
    /// with a statement that is not a read, a session setting or transaction control.
    read_only: bool,
    /// The user made this session read-only from its tab (SDB-395): the same barrier as a
    /// read-only connection's, added to the connection's and never taking it away.
    session_read_only: AtomicBool,
    /// The server a PostgreSQL session reached (SDB-169), read by [`crate::open_connection`]
    /// when it opens; `None` for the other engines and for sessions built without it.
    server_family: Option<crate::ServerFamily>,
    /// The file of an embedded database (SQLite, DuckDB), as [`crate::open_connection`] names it:
    /// two sessions on one file have the same text (ADR-069). `None` for servers, in-memory
    /// databases and sessions built without it.
    database_file: Option<String>,
    /// The table data page statements the core wrote on this session, with their tables: a
    /// run that sends one binds its results to that table (SDB-440).
    table_pages: Arc<crate::table_pages::TablePages>,
}

/// A session's cancel, owned ([`Session::canceller`]). It is the session's own: the run sees
/// it, so a cancelled script stops whatever its [`OnError`], which a cancel sent straight to
/// the connection does not do (SDB-365).
#[derive(Clone)]
pub(crate) struct SessionCancel {
    connection: Arc<dyn Connection>,
    pause: Arc<Pause>,
    runs: Arc<Runs>,
    cancel_gate: CancelGate,
}

impl SessionCancel {
    /// Counts a cancel in flight, so the session's next statement waits for it, until the
    /// returned guard is dropped ([`crate::JobCancel::cancel`] holds it across its cancel).
    pub(crate) fn in_flight(&self) -> CancelInFlight {
        self.cancel_gate.enter()
    }

    pub(crate) async fn cancel(&self) -> Result<(), DriverError> {
        let _in_flight = self.cancel_gate.enter();
        self.runs.cancel();
        if self.pause.resume(Resume::Cancelled) {
            return Ok(());
        }
        self.connection.cancel_handle().cancel().await
    }
}

/// How long Commit, Rollback or the auto-commit switch wait for one of the core's own reads
/// that holds the session (SDB-277) before they say it is busy.
const CORE_READ_WAIT: Duration = Duration::from_secs(10);

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
        if let Some(task) = self.idle_rollback.lock().expect("lock poisoned").take() {
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
            idle_rollback: std::sync::Mutex::new(None),
            dialect: Arc::new(crate::sql_mode::SessionDialect::new(dialect)),
            results,
            options: ExecOptions::default(),
            control: Arc::new(tokio::sync::Mutex::new(())),
            runs: Arc::new(Runs::default()),
            transaction: Arc::new(tokio::sync::watch::Sender::new(
                TransactionLifecycle::NotFollowed,
            )),
            pause: Arc::new(Pause::default()),
            cancel_gate: CancelGate::default(),
            masking: Arc::new(MaskPolicy::none()),
            read_only: false,
            session_read_only: AtomicBool::new(false),
            server_family: None,
            database_file: None,
            transaction_pooled: false,
            table_pages: Arc::default(),
        }
    }

    /// Records a table data page statement the core wrote, with the table it reads (SDB-440).
    pub(crate) fn record_table_page(&self, sql: &str, table: crate::results::ResultTable) {
        self.table_pages.record(sql, table);
    }

    /// The session reaches PostgreSQL through a pooler in transaction mode (SDB-396): each
    /// transaction may get another server connection, so nothing set for the session holds.
    pub fn with_transaction_pooler(mut self, pooled: bool) -> Self {
        self.transaction_pooled = pooled;
        self
    }

    /// Whether a setting made for the session would hold no longer than one transaction.
    pub fn is_transaction_pooled(&self) -> bool {
        self.transaction_pooled
    }

    /// The databases its connection shows (SDB-396): every list of databases the session
    /// gives keeps only these and `own`, the connection's database. What is shown, never a
    /// permission.
    pub fn with_shown_databases(
        self,
        names: &[String],
        own: &str,
        schemas_are_databases: bool,
    ) -> Self {
        self.tracked.show_only(crate::shown::ShownDatabases::new(
            names,
            own,
            schemas_are_databases,
        ));
        self
    }

    /// The server this PostgreSQL session reached (SDB-169): which barriers it enforces.
    pub fn with_server_family(mut self, family: crate::ServerFamily) -> Self {
        self.server_family = Some(family);
        self
    }

    pub fn server_family(&self) -> Option<&crate::ServerFamily> {
        self.server_family.as_ref()
    }

    /// The embedded database file this session works on (ADR-069), named so that two sessions
    /// on one file have the same text.
    pub fn with_database_file(mut self, file: impl Into<String>) -> Self {
        self.database_file = Some(file.into());
        self
    }

    pub fn database_file(&self) -> Option<&str> {
        self.database_file.as_deref()
    }

    /// Why a read that relies on the engine's read-only transaction (agents' reads, streaming
    /// exports, column statistics, the data search) cannot run on this session, if it cannot:
    /// its server accepts `BEGIN READ ONLY` without enforcing it (SDB-169).
    pub fn read_only_transaction_refusal(&self) -> Option<String> {
        self.server_family
            .as_ref()
            .and_then(crate::ServerFamily::read_only_transactions_refusal)
    }

    /// Why writes that rely on one transaction (several table edits at once, an import) cannot
    /// be made atomic on this session, if they cannot: the engine has no transactions, or its
    /// server accepts `BEGIN` and ignores it (SDB-354). Rolled back, they would leave the
    /// statements before the failure applied.
    pub fn atomic_writes_refusal(&self) -> Option<String> {
        if !self.connection().capabilities().transactions {
            return Some(format!("{} has no transactions", self.dialect().name()));
        }
        match &self.server_family {
            Some(family) if !family.enforces_transactions() => Some(format!(
                "{} accepts transactions but does not enforce them",
                family.name()
            )),
            Some(_) | None => None,
        }
    }

    /// Makes the session refuse every script a read-only connection does not run (ADR-053).
    /// [`crate::open_connection`] sets it from the connection.
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// The session refuses every script a read-only connection does not run (ADR-053): its
    /// connection is read-only, or the user made the session read-only (SDB-395). Every check
    /// of the barrier reads this.
    pub fn is_read_only(&self) -> bool {
        self.read_only || self.is_session_read_only()
    }

    /// The session's connection is read-only (ADR-053): nothing done to the session loosens it.
    pub fn is_connection_read_only(&self) -> bool {
        self.read_only
    }

    /// The user made this session read-only from its tab (SDB-395).
    pub fn is_session_read_only(&self) -> bool {
        self.session_read_only.load(Ordering::SeqCst)
    }

    /// Sets or clears the session's own read-only mark. Only `session_read_only.rs` calls it,
    /// after its checks; it never changes what the connection says.
    pub(crate) fn mark_session_read_only(&self, read_only: bool) {
        self.session_read_only.store(read_only, Ordering::SeqCst);
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

    /// Rolls back this editor session's transaction once it has stayed open `after` without
    /// use, as its connection type asks (ADR-081): first the transaction's phase is marked
    /// warned ([`TransactionLifecycle::idle_rollback_at`]), then the engine's `ROLLBACK` runs
    /// and `on_rollback` is told once. A session in use is never touched, nothing is committed,
    /// and a rollback that cannot run marks the session lost. Only for a session that follows
    /// its transaction ([`Session::follow_transactions`]). Replaces a previous one.
    pub fn roll_back_idle_transactions(
        &self,
        after: Duration,
        on_rollback: impl Fn(crate::IdleRolledBack) + Send + Sync + 'static,
    ) {
        let parts = crate::idle_transaction::Parts {
            tracked: Arc::downgrade(&self.tracked),
            health: self.health.clone(),
            control: self.control.clone(),
            runs: self.runs.clone(),
            transaction: self.transaction.clone(),
            dialect: self.dialect.clone(),
            options: self.options.clone(),
        };
        let task = tokio::spawn(crate::idle_transaction::run(
            parts,
            after,
            Box::new(on_rollback),
        ));
        if let Some(previous) = self
            .idle_rollback
            .lock()
            .expect("lock poisoned")
            .replace(task)
        {
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
        self.canceller().cancel().await
    }

    /// [`Session::cancel`] for whoever must stop this session's run without holding the
    /// session: the Stop of a job that runs in a session of its own.
    pub(crate) fn canceller(&self) -> SessionCancel {
        SessionCancel {
            connection: self.connection.clone(),
            pause: self.pause.clone(),
            runs: self.runs.clone(),
            cancel_gate: self.cancel_gate.clone(),
        }
    }

    /// What the session runs, as the user sees it (SDB-370): idle, running a statement,
    /// paused after an error, or cancelling. Sent to every receiver when it changes, and only
    /// then.
    pub fn run_state(&self) -> tokio::sync::watch::Receiver<RunState> {
        self.runs.subscribe()
    }

    /// The [`RunState`] now.
    pub fn current_run_state(&self) -> RunState {
        self.runs.state()
    }

    /// Why a transaction action was refused, saying what runs when it was refused as busy
    /// ("statement 3 is running on this tab: …").
    pub fn transaction_refusal(&self, error: &TransactionError) -> String {
        match (error, self.current_run_state()) {
            (TransactionError::Busy, RunState::Running { statement }) => format!(
                "statement {} is running on this tab: wait for it to finish or cancel it",
                statement + 1
            ),
            (TransactionError::Busy, RunState::Paused { statement, .. }) => format!(
                "statement {} failed and the run on this tab waits for your answer: answer it or cancel the run",
                statement + 1
            ),
            (TransactionError::Busy, RunState::Cancelling { statement }) => format!(
                "the run on this tab is being cancelled at statement {}: wait for it to end",
                statement + 1
            ),
            // One of the core's own reads held the session too long.
            (TransactionError::Busy, RunState::Idle)
            | (TransactionError::Unsupported, _)
            | (TransactionError::Open, _)
            | (TransactionError::NotOpen, _)
            | (TransactionError::Failed(_), _) => error.to_string(),
        }
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

    /// The session's transaction (ADR-031, ADR-060). Changes are sent to every receiver.
    pub fn transaction(&self) -> tokio::sync::watch::Receiver<TransactionLifecycle> {
        self.transaction.subscribe()
    }

    /// The last [`TransactionLifecycle`] read; also after the session was lost, to say whether
    /// it had a transaction open.
    pub fn transaction_lifecycle(&self) -> TransactionLifecycle {
        *self.transaction.borrow()
    }

    /// The last state read, as a [`TransactionStatus`].
    pub fn transaction_status(&self) -> TransactionStatus {
        self.transaction_lifecycle().into()
    }

    /// Makes this an editor session that follows its transaction, under its connection type's
    /// safeguards: auto-commit unless the type asks for manual commit, and smart commit when it
    /// asks for it (ADR-031). An engine that cannot tell whether a transaction is open stays in
    /// auto-commit and says so ([`TransactionStatus::supported`]). So does a server that
    /// accepts `BEGIN` and writes at once anyway ([`crate::ServerFamily::opens_transactions`]:
    /// CrateDB, RisingWave…), where manual commit would only seem to hold changes back and
    /// Rollback would undo nothing (SDB-310, SDB-304).
    pub async fn follow_transactions(&self, safeguards: &Safeguards) -> TransactionStatus {
        let _running = self.control.lock().await;
        self.cancel_gate.settled().await;
        // A driver without transactions, or a server whose `START TRANSACTION` opens nothing
        // (SDB-310), has no transaction to follow: no manual commit, Commit or Rollback.
        let opens = self.connection.capabilities().transactions
            && self
                .server_family
                .as_ref()
                .is_none_or(crate::ServerFamily::opens_transactions);
        let lifecycle = match self.connection.transaction_state().await {
            Ok(state) if opens => TransactionLifecycle::follow(safeguards, state),
            Ok(_) | Err(_) => TransactionLifecycle::NotFollowed,
        };
        self.transaction.send_replace(lifecycle);
        lifecycle.into()
    }

    /// Whether closing the session (closing its tab, switching its connection, quitting) must
    /// ask first, by [`TransactionLifecycle::closing_asks`]. Asks the driver once more unless
    /// the session is busy ([`RunState::is_busy`], or one of the core's reads holds it), when
    /// the last state read decides.
    pub async fn close_needs_confirmation(&self) -> bool {
        if !self.transaction_lifecycle().is_followed() {
            return false;
        }
        if self.current_run_state().is_busy() {
            return self.closing_asks_from_last_state();
        }
        let Ok(_running) = self.control.try_lock() else {
            return self.closing_asks_from_last_state();
        };
        match self.connection.transaction_state().await {
            Ok(state) => {
                self.transaction.send_modify(|s| s.observe(state));
                self.transaction_lifecycle().closing_asks()
            }
            // A session that cannot answer any more has nothing left to commit.
            Err(_) => false,
        }
    }

    /// [`Session::close_needs_confirmation`] from the last state read, with no round trip (the
    /// quit question, which a server that does not answer must not hold). A lost session has
    /// nothing left to commit: its server rolled the transaction back (ADR-028).
    pub fn closing_asks_from_last_state(&self) -> bool {
        let alive = match *self.health().borrow() {
            SessionHealth::Alive => true,
            SessionHealth::Lost { .. } | SessionHealth::IdleClosed { .. } => false,
        };
        alive && self.transaction_lifecycle().closing_asks()
    }

    /// Turns auto-commit on or off. Turning it on is refused while a transaction is open: the
    /// user commits or rolls back first, nothing is committed by a toggle. Turning it off sends
    /// nothing until the next statement.
    pub async fn set_auto_commit(&self, on: bool) -> Result<TransactionStatus, TransactionError> {
        let _running = self.transaction_control().await?;
        self.cancel_gate.settled().await;
        if !self.transaction_lifecycle().is_followed() {
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
            s.set_auto_commit(on);
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
        let busy = || self.current_run_state().is_busy();
        if busy() {
            return Err(TransactionError::Busy);
        }
        if let Ok(guard) = self.control.try_lock() {
            return Ok(guard);
        }
        // A run may have asked for the session since.
        if busy() {
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
        if !self.transaction_lifecycle().is_followed() {
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
        let cancels_at_start = self.runs.cancels();
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
            RunKind::User if self.is_read_only() => {
                crate::guard::check_read_only(dialect.as_ref(), &script)
                    .err()
                    .map(|refusal| refusal.for_session(self))
            }
            RunKind::User | RunKind::Generated | RunKind::Parsed | RunKind::Metadata => None,
        };
        let spans = dialect.split_statements(&script);
        let engine = dialect.name();
        // An engine whose language is not SQL is sent the request the core reads from each
        // statement, never its text (ADR-056).
        let language = dialect.clone();
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
        let table_pages = self.table_pages.clone();
        let can_change = database_can_change(engine);
        let mut options = self.options.clone();
        // Masking matches each column's origin too (ADR-052): ask the driver for it.
        options.origins |= masking.is_some() && !self.masking.is_empty();
        let control = self.control.clone();
        // A run the user sees is part of the session's run state; the core's own reads are not.
        let seen_by_user = match kind {
            RunKind::User | RunKind::Generated => true,
            RunKind::Parsed | RunKind::Metadata => false,
        };
        let runs = self.runs.clone();
        let transaction = self.transaction.clone();
        let pause = self.pause.clone();
        let cancel_gate = self.cancel_gate.clone();
        let mut health = self.health.subscribe();
        let losing = self.health.clone();
        let losing_in_run = self.health.clone();
        let total_statements = spans.len();
        // A run that rolls back only its failed statements (ADR-086): a user's run, asked for,
        // on an engine whose errors abort the whole transaction and that keeps savepoints in
        // it, reached on a server known to keep them too.
        let savepoint = match kind {
            RunKind::User
                if run_options.statement_savepoints
                    && keeps_savepoints(self.server_family.as_ref()) =>
            {
                transaction::statement_savepoint(engine)
            }
            RunKind::User | RunKind::Generated | RunKind::Parsed | RunKind::Metadata => None,
        };

        let run = Box::pin(async_stream::stream! {
            if let Some(refusal) = refusal {
                let index = refusal.index;
                let total = spans.len();
                yield CoreEvent::StatementStarted { index, span: refusal.span };
                yield CoreEvent::StatementFailed {
                    index,
                    error: StatementError::failed(refusal.to_string(), Some(refusal.span.start)),
                };
                yield CoreEvent::finished(ScriptSummary {
                    statements: total,
                    failed: 1,
                    stop: Some(ScriptStop::Failed { index }),
                    ..Default::default()
                });
                return;
            }
            // A run the user sees makes Commit, Rollback and the toggle busy from now on, while
            // it waits for the session too (SDB-277): it is running its first statement.
            let seen = seen_by_user.then(|| runs.begin(cancels_at_start));
            // One script at a time; Commit, Rollback and the toggle wait for none. Held until
            // just before the last event (below), not until the stream is polled once more.
            let running = control.lock_owned().await;
            // A cancel still on its way to the engine would stop this run's first statement:
            // nothing is sent before it has returned (SDB-266).
            cancel_gate.settled().await;
            let cancelled = || runs.cancelled_since(cancels_at_start);
            let follow = transaction.borrow().is_followed();
            let mut on_error = run_options.on_error;
            let mut summary = ScriptSummary { statements: spans.len(), ..Default::default() };
            let total = spans.len();
            for (index, span) in spans.into_iter().enumerate() {
                if cancelled() {
                    summary.stop = Some(ScriptStop::Cancelled { index });
                    break;
                }
                if let Some(seen) = &seen {
                    seen.running(index);
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
                        && status.auto_commit()
                        && status.smart_commit()
                        && kind == StatementKind::Other
                        && transaction::is_write(engine, policy_text)
                        && status.switch_by_smart_commit()
                    {
                        transaction.send_replace(status);
                    }
                    if managed
                        && !status.auto_commit()
                        && kind == StatementKind::Other
                        && status.state() == Some(TransactionState::Idle)
                    {
                        if let Err(e) = run_control(&*connection, transaction::begin_statement(engine), &options).await {
                            let mut error = StatementError::from_driver(e, &script, span);
                            error.message = format!("could not open a transaction: {}", error.message);
                            error.script_offset = None;
                            summary.failed += 1;
                            // The run stops here whatever its mode, also at its last statement.
                            summary.stop = Some(if cancelled() {
                                error.ended(StatementEnd::Cancelled);
                                ScriptStop::Cancelled { index }
                            } else {
                                ScriptStop::Failed { index }
                            });
                            yield CoreEvent::StatementFailed { index, error };
                            break;
                        }
                        status.opened_by_core();
                        transaction.send_replace(status);
                    }
                    before = Some(status);
                }
                // The core's savepoint before a statement of the user's in a transaction
                // (ADR-086). A user's own BEGIN, COMMIT, ROLLBACK [TO], SAVEPOINT or RELEASE runs
                // as written, never wrapped: rolling back to a savepoint of theirs would take
                // the core's with it. If the core's cannot be set, the statement runs as before.
                let wrapped = match savepoint {
                    Some(points)
                        if kind == StatementKind::Other
                            && before.is_some_and(|b| b.in_transaction()) =>
                    {
                        run_control(&*connection, points.set, &options).await.is_ok().then_some(points)
                    }
                    Some(_) | None => None,
                };

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
                // The result the statement stopped in the middle of (SDB-367), and whether the
                // connection was lost under it; its reason is known once the cancel and the
                // timeout are read below.
                let mut cut: Option<ResultId> = None;
                let mut lost = false;
                // The rows after a `COPY … FROM STDIN` (SDB-162): loaded through the import's
                // path, never read as SQL. Only the statement went through the checks above.
                let copy_data = match plan_mode {
                    None => language.copy_data(&script, &span),
                    Some(_) => None,
                };
                let execute = async {
                    if let Some(data) = copy_data.clone() {
                        return crate::copy_in::load(&*connection, text, &script[data], &cancelled, &options)
                            .await
                            .map(|rows| -> spelaion_driver_api::EventStream {
                                Box::pin(futures_util::stream::iter([Ok(ExecEvent::Done {
                                    rows_affected: Some(rows),
                                })]))
                            });
                    }
                    match plan_mode {
                        Some(mode) => connection.execute_plan(text, mode).await,
                        None => match request_of(&*language, &*connection, text).await {
                            Some(Ok(request)) => connection.execute_request(request, &options).await,
                            Some(Err(error)) => Err(error),
                            // A language sent as text may still refuse a statement (CQL's
                            // `USE`): nothing is sent.
                            None => match language.refuse_statement(text) {
                                Some(error) => Err(request_error(text, error)),
                                None => connection.execute(text, &options).await,
                            },
                        },
                    }
                };
                tokio::pin!(execute);
                // Polled once: the statement is sent (a plugin's `execute` is on its pipe once
                // polled, and the plugin numbers it before it delivers a cancel read after it).
                let early = first_poll(&mut execute).await;
                // A cancel that came after the last check above (while the database context was
                // read, say) may have reached the engine before this statement did: an engine
                // that numbers its statements (MySQL's and PostgreSQL's plugins) then found
                // nothing to stop. It is sent again, once (SDB-428), never on a timer, and only
                // while this statement is known not to have ended: it was just sent, and the
                // engine has not answered it, or answered with a stream none of whose events
                // has been read (an error there means it ended: nothing is sent again). It is
                // started before anything more is read, and counted in flight like any cancel:
                // no later statement, run or commit is sent before it has returned (SDB-266).
                if cancelled() && !matches!(early, Some(Err(_))) {
                    let in_flight = cancel_gate.enter();
                    let again = connection.cancel_handle();
                    let mut sending = Box::pin(async move {
                        let _ = again.cancel().await;
                    });
                    if first_poll(&mut sending).await.is_none() {
                        // On its way: it ends on its own, still counted in flight until then.
                        tokio::spawn(async move {
                            let _in_flight = in_flight;
                            sending.await;
                        });
                    }
                }
                let opened = match early {
                    Some(opened) => opened,
                    None => loop {
                        match until(&mut execute, deadline, timed_out).await {
                            Waited::Ready(opened) => break opened,
                            Waited::TimedOut => {
                                timed_out = true;
                                let _ = connection.cancel_handle().cancel().await;
                            }
                        }
                    },
                };
                match opened {
                    Err(e) => {
                        answered = Some(Instant::now());
                        failure = Some(StatementError::from_driver(e, &script, span));
                    }
                    Ok(mut events) => {
                        let mut current: Option<ResultId> = None;
                        // Every statement the engine finishes ends with `Done` or an error. A
                        // stream that ends without either was cut off: the driver's actor
                        // stopped halfway (a panic in its thread, SDB-163). Its rows are not
                        // the whole result, and the connection cannot be trusted any more.
                        let mut finished = false;
                        loop {
                            let event = match until(events.next(), deadline, timed_out).await {
                                Waited::Ready(event) => event,
                                Waited::TimedOut => {
                                    timed_out = true;
                                    let _ = connection.cancel_handle().cancel().await;
                                    continue;
                                }
                            };
                            let Some(event) = event else {
                                if !finished {
                                    cut = current.take();
                                    if cancelled() || timed_out {
                                        failure = Some(StatementError::from_driver(
                                            DriverError::Cancelled,
                                            &script,
                                            span,
                                        ));
                                    } else {
                                        lost = true;
                                        losing_in_run.lose(format!(
                                            "the connection stopped in the middle of a statement: {}",
                                            crate::panics::INTERNAL_ERROR
                                        ));
                                        failure = Some(internal_statement_error(
                                            "the connection stopped in the middle of the statement",
                                        ));
                                    }
                                }
                                break;
                            };
                            answered.get_or_insert_with(Instant::now);
                            match event {
                                Ok(ExecEvent::Columns(schema)) => {
                                    // A batch may return several result sets (SQL Server).
                                    if let Some(previous) = current {
                                        results.end(previous, ResultEnd::Whole);
                                        yield CoreEvent::ResultEnded {
                                            index,
                                            result: previous,
                                            end: ResultEnd::Whole,
                                        };
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
                                        // A table data page the core wrote reads its table
                                        // (SDB-440): its rows are bound to it, and only then
                                        // are they editable.
                                        if let Some(table) = table_pages.table_of(text) {
                                            results.bind_table_of(result, table);
                                        }
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
                                        results.end(result, ResultEnd::Whole);
                                        yield CoreEvent::ResultEnded {
                                            index,
                                            result,
                                            end: ResultEnd::Whole,
                                        };
                                    }
                                    rows_affected = rows;
                                    finished = true;
                                }
                                Err(e) => {
                                    cut = current.take();
                                    lost = matches!(e, DriverError::Connection(_));
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
                        error.ended(StatementEnd::Cancelled);
                    } else if timed_out {
                        error.ended(StatementEnd::TimedOut);
                        let limit = run_options.statement_timeout.unwrap_or_default();
                        error.message = format!(
                            "the statement ran longer than its timeout ({}) and was cancelled: {}",
                            seconds(limit),
                            error.message
                        );
                    }
                }
                // The result cut short says why (SDB-367): its rows stay kept and shown.
                if let Some(result) = cut {
                    let reason = if user_cancelled {
                        PartialReason::Cancelled
                    } else if timed_out {
                        PartialReason::TimedOut
                    } else if lost {
                        PartialReason::ConnectionLost
                    } else {
                        PartialReason::Failed
                    };
                    let end = ResultEnd::Partial(reason);
                    results.end(result, end);
                    yield CoreEvent::ResultEnded { index, result, end };
                }
                for notice in notices {
                    yield CoreEvent::Notice { index, notice };
                }
                // The core's savepoint (ADR-086): a failed statement is rolled back to it, which
                // leaves the transaction open without that statement; a statement that ran is
                // kept by releasing it. These are the core's statements: never in the history,
                // never counted. Each waits for a cancel still on its way, which must not reach
                // it (SDB-266). A savepoint the statement itself ended (a function that commits)
                // is not there any more: then the state read below says what happened.
                if let Some(points) = wrapped {
                    cancel_gate.settled().await;
                    if failure.is_some() {
                        if run_control(&*connection, points.rollback_to, &options).await.is_ok() {
                            let _ = run_control(&*connection, points.release, &options).await;
                            yield CoreEvent::Notice {
                                index,
                                notice: transaction::rolled_back_to_savepoint_notice(),
                            };
                        }
                    } else {
                        let _ = run_control(&*connection, points.release, &options).await;
                    }
                }

                // After: what the driver says now.
                let mut after = None;
                if let Some(before) = before
                    && let Ok(state) = connection.transaction_state().await
                {
                    after = Some(state);
                    let had_work = before.has_work();
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
                                status.restart_count();
                            }
                            if count_statement && !kind.is_control() {
                                status.count_statement();
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
                // A cancel always ends the run, and says so even at the last statement.
                if user_cancelled {
                    summary.stop = Some(ScriptStop::Cancelled { index });
                    break;
                }
                let remaining = total - index - 1;
                if remaining == 0 {
                    // Nothing is left either way, but only a run that goes on past errors went
                    // through every statement as its mode says: the others stopped here.
                    match on_error {
                        OnError::Continue => {}
                        OnError::Stop | OnError::Ask => {
                            summary.stop = Some(ScriptStop::Failed { index });
                        }
                    }
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
                    if let Some(seen) = &seen {
                        seen.paused(index, ticket, remaining);
                    }
                    yield CoreEvent::ErrorPaused { index, ticket, remaining };
                    let ended = health.wait_for(|h| *h != SessionHealth::Alive);
                    let resume = tokio::select! {
                        answer = answer => answer.unwrap_or(Resume::Closed),
                        health = ended => match health.as_deref() {
                            Ok(SessionHealth::Lost { .. }) => Resume::Lost,
                            // Closed for being idle (ADR-029), or the session was dropped.
                            Ok(SessionHealth::IdleClosed { .. } | SessionHealth::Alive) | Err(_) => {
                                Resume::Closed
                            }
                        },
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
                        Resume::Lost => {
                            summary.stop = Some(ScriptStop::SessionLost { index });
                            break;
                        }
                    }
                }
            }
            // The session takes the next command before anyone hears the run is over: the app
            // hands this event to the window before it asks the stream for more, and the switch,
            // Commit and Rollback it enables again would find the session still busy (SDB-98).
            // The run state is idle by then too.
            drop(running);
            drop(seen);
            yield CoreEvent::finished(summary);
        });
        end_on_panic(run, losing, total_statements, self.results.clone())
    }
}

/// Whether the server a session reached keeps savepoints inside a transaction (ADR-086).
/// Unknown (a session opened without reading its server) is the engine itself; of
/// PostgreSQL's protocol only the servers that keep them, the others' errors would abort the
/// transaction on the core's own `SAVEPOINT`. The MySQL family and Babelfish never get here:
/// their engines undo only the failed statement.
fn keeps_savepoints(family: Option<&crate::ServerFamily>) -> bool {
    use crate::ServerFamily as F;
    match family {
        None => true,
        Some(F::PostgreSql | F::CockroachDb | F::YugabyteDb) => true,
        Some(
            F::RisingWave
            | F::Materialize
            | F::CrateDb
            | F::Unchecked(_)
            | F::MySql
            | F::MariaDb
            | F::OceanBase
            | F::TiDb
            | F::SingleStore
            | F::Doris
            | F::GreptimeDb
            | F::Databend
            | F::StarRocks
            | F::Babelfish,
        ) => false,
    }
}

/// The error of a statement that an internal error stopped (SDB-163).
fn internal_statement_error(what: &str) -> StatementError {
    StatementError::failed(
        format!(
            "{what}: {}. Reconnect to go on: nothing is re-run on its own.",
            crate::panics::INTERNAL_ERROR
        ),
        None,
    )
}

/// `run`, ending as a failed run if it panics (SDB-163): the statement that was running fails
/// with an internal error, the run finishes, and the session is lost (ADR-028: what state the
/// connection was left in is unknown, so it is never used again; nothing is re-run). Without
/// this the app would wait for the end of the run forever. A result still loading then ends
/// cut short, as its connection stopped (SDB-367).
fn end_on_panic(
    run: BoxStream<'static, CoreEvent>,
    health: Health,
    statements: usize,
    results: Arc<ResultStore>,
) -> BoxStream<'static, CoreEvent> {
    Box::pin(async_stream::stream! {
        let mut run = std::panic::AssertUnwindSafe(run).catch_unwind();
        let mut summary = ScriptSummary { statements, ..Default::default() };
        let mut running = None;
        // The last statement that started: a panic between statements stops the run there.
        let mut last = None;
        let mut loading: Option<(usize, ResultId)> = None;
        loop {
            let Some(event) = run.next().await else { break };
            match event {
                Ok(event) => {
                    match &event {
                        CoreEvent::StatementStarted { index, .. } => {
                            summary.executed += 1;
                            running = Some(*index);
                            last = Some(*index);
                        }
                        CoreEvent::StatementFinished { .. } => {
                            summary.succeeded += 1;
                            running = None;
                        }
                        CoreEvent::StatementFailed { .. } => {
                            summary.failed += 1;
                            running = None;
                        }
                        CoreEvent::ScriptFinished { .. } => running = None,
                        CoreEvent::ResultStarted { index, result, .. } => {
                            loading = Some((*index, *result));
                        }
                        CoreEvent::ResultEnded { .. } => loading = None,
                        CoreEvent::Rows { .. }
                        | CoreEvent::Notice { .. }
                        | CoreEvent::ErrorPaused { .. }
                        | CoreEvent::HistoryWarning { .. } => {}
                    }
                    yield event;
                }
                Err(_) => {
                    // What the run held goes now, before anyone hears it ended: the session
                    // and its run state (idle), as when a run ends (SDB-98).
                    drop(run);
                    health.lose(format!("a run was stopped by {}", crate::panics::INTERNAL_ERROR));
                    if let Some((index, result)) = loading {
                        let end = ResultEnd::Partial(PartialReason::ConnectionLost);
                        results.end(result, end);
                        yield CoreEvent::ResultEnded { index, result, end };
                    }
                    // The run stops in every mode, as any failure that stops it does.
                    if let Some(index) = running {
                        summary.failed += 1;
                        yield CoreEvent::StatementFailed {
                            index,
                            error: internal_statement_error("the statement did not finish"),
                        };
                    }
                    summary.stop = Some(ScriptStop::Failed { index: last.unwrap_or(0) });
                    yield CoreEvent::finished(summary);
                    break;
                }
            }
        }
    })
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
pub(crate) async fn run_control(
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

pub(crate) struct CancelInFlight(Arc<tokio::sync::watch::Sender<usize>>);

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
            spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
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

    /// SDB-310: a server whose `START TRANSACTION` opens nothing is not followed: no manual
    /// commit, even where the connection type asks for it, and no switch to it; a server whose
    /// transactions hold is followed as before.
    #[tokio::test]
    async fn a_server_whose_transactions_open_nothing_is_not_followed() {
        let driver = crate::open::test_driver(spelaion_config::Engine::Sqlite);
        let open = |family: crate::ServerFamily| {
            let driver = driver.clone();
            async move {
                Session::new(
                    driver
                        .connect(&ConnectParams {
                            database: ":memory:".into(),
                            ..Default::default()
                        })
                        .await
                        .expect("connecting"),
                    spelaion_dialect::for_engine("sqlite").expect("a dialect"),
                    Arc::new(ResultStore::new()),
                )
                .with_server_family(family)
            }
        };
        let manual = Safeguards {
            manual_commit: true,
            ..Safeguards::default()
        };
        for family in [
            crate::ServerFamily::StarRocks,
            crate::ServerFamily::CrateDb,
            crate::ServerFamily::GreptimeDb,
        ] {
            let session = open(family.clone()).await;
            let status = session.follow_transactions(&manual).await;
            assert!(!status.supported, "{family:?}");
            assert!(status.auto_commit, "{family:?}");
            assert!(
                matches!(
                    session.set_auto_commit(false).await,
                    Err(TransactionError::Unsupported)
                ),
                "{family:?}"
            );
        }
        let held = open(crate::ServerFamily::TiDb).await;
        let status = held.follow_transactions(&manual).await;
        assert!(status.supported);
        assert!(!status.auto_commit);
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
            assert_eq!(session.current_run_state(), RunState::Idle, "{name}");

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

    /// The quit question reads the last state, without a round trip: it asks about an open
    /// transaction while the session is alive, and not once it is lost, when its server has
    /// already rolled the transaction back (SDB-372). Before, a lost session stayed listed.
    #[tokio::test]
    async fn the_quit_question_leaves_out_a_lost_sessions_transaction() {
        let driver = crate::open::test_driver(spelaion_config::Engine::Sqlite);
        let session = session(
            driver.as_ref(),
            ConnectParams {
                database: ":memory:".into(),
                ..Default::default()
            },
        )
        .await;
        assert!(!session.closing_asks_from_last_state());
        drain(session.run_script("begin; create table t (a int)")).await;
        assert!(session.transaction_lifecycle().is_open());
        assert!(session.closing_asks_from_last_state());
        assert!(session.close_needs_confirmation().await);
        session.mark_lost("the tunnel went down");
        assert!(
            session.transaction_lifecycle().is_open(),
            "the last state read stays, to say a transaction was lost with it"
        );
        assert!(!session.closing_asks_from_last_state());
    }
}
