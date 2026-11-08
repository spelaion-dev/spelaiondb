//! Whether a session's connection still works, and keeping an idle one alive (ADR-028).
//!
//! Every call a session makes goes through [`Tracked`], which notes when the connection was
//! last used and turns a connection error into [`SessionHealth::Lost`]. The keep-alive loop
//! sends the driver's cheapest round trip only when the session has been idle for the whole
//! interval, never while something runs, and stops while a transaction is open. A connection
//! type may close sessions left idle (ADR-029): never while a transaction is open, and never
//! where the engine cannot tell. Nothing is ever reconnected or re-run here: a lost or closed
//! session stays so until the user reconnects.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_driver_api::{
    CancelHandle, Capabilities, Connection, DriverError, EventStream, ExecOptions, KeepAlive,
    NameFilter, ObjectInfo, ReferencingKey, Result, SchemaInfo, SchemaObject, ServerObject,
    TableInfo, TransactionState,
};
use spelaion_ssh::TunnelState;
use tokio::sync::{OwnedRwLockReadGuard, RwLock, watch};
use tokio::time::Instant;

/// How long a keep-alive may wait for its answer before the session counts as lost: packets
/// dropped by a firewall get no answer at all.
pub const KEEPALIVE_ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionHealth {
    Alive,
    /// The connection is gone. It is never reopened on its own; nothing is re-run.
    Lost {
        reason: String,
    },
    /// Closed after this long without use, as its connection type asks (ADR-029). Like a lost
    /// session, only the user reopens it.
    IdleClosed {
        after: Duration,
    },
}

/// When the connection was last used, and whether something is using it now.
pub(crate) struct Activity {
    busy: AtomicUsize,
    gate: Arc<RwLock<()>>,
    /// Bumped by every use: a keep-alive paused by an open transaction resumes after one.
    uses: AtomicU64,
    /// The end of the last use or keep-alive.
    last: Mutex<Instant>,
    /// The end of the last use: keep-alives do not count, for closing idle sessions.
    last_use: Mutex<Instant>,
}

impl Activity {
    fn new() -> Self {
        Self {
            busy: AtomicUsize::new(0),
            gate: Arc::new(RwLock::new(())),
            uses: AtomicU64::new(0),
            last: Mutex::new(Instant::now()),
            last_use: Mutex::new(Instant::now()),
        }
    }

    async fn begin(self: &Arc<Self>) -> Busy {
        let guard = self.gate.clone().read_owned().await;
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.uses.fetch_add(1, Ordering::SeqCst);
        Busy(self.clone(), guard)
    }

    /// Holds the connection like a use, without counting as one: reading the transaction
    /// state after a statement must not keep an idle session from being closed.
    async fn peek(&self) -> OwnedRwLockReadGuard<()> {
        self.gate.clone().read_owned().await
    }

    fn last(&self) -> Instant {
        *self.last.lock().expect("lock poisoned")
    }

    fn last_use(&self) -> Instant {
        *self.last_use.lock().expect("lock poisoned")
    }
}

/// Marks the connection busy until dropped (for a statement, until its rows are read).
struct Busy(Arc<Activity>, #[allow(dead_code)] OwnedRwLockReadGuard<()>);

impl Drop for Busy {
    fn drop(&mut self) {
        *self.0.last.lock().expect("lock poisoned") = Instant::now();
        *self.0.last_use.lock().expect("lock poisoned") = Instant::now();
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The session's view of its health; shared with the keep-alive loop and the tunnel watcher.
#[derive(Clone)]
pub(crate) struct Health(Arc<watch::Sender<SessionHealth>>);

impl Health {
    fn new() -> Self {
        Self(Arc::new(watch::Sender::new(SessionHealth::Alive)))
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<SessionHealth> {
        self.0.subscribe()
    }

    /// The first reason wins: later errors are consequences of it.
    pub(crate) fn lose(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.0.send_if_modified(|health| match health {
            SessionHealth::Alive => {
                *health = SessionHealth::Lost { reason };
                true
            }
            SessionHealth::Lost { .. } | SessionHealth::IdleClosed { .. } => false,
        });
    }

    /// The session was closed for being idle (unless it was already lost).
    fn close_idle(&self, after: Duration) {
        self.0.send_if_modified(|health| match health {
            SessionHealth::Alive => {
                *health = SessionHealth::IdleClosed { after };
                true
            }
            SessionHealth::Lost { .. } | SessionHealth::IdleClosed { .. } => false,
        });
    }

    /// Lost or closed: nothing more is sent on it.
    fn is_lost(&self) -> bool {
        match *self.0.borrow() {
            SessionHealth::Alive => false,
            SessionHealth::Lost { .. } | SessionHealth::IdleClosed { .. } => true,
        }
    }

    /// Marks the session lost when its tunnel goes down. The watch ends with the tunnel, and
    /// does nothing once the session is gone.
    pub(crate) fn watch_tunnel(&self, mut tunnel: watch::Receiver<TunnelState>) {
        let health = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            let reason = match tunnel
                .wait_for(|s| matches!(s, TunnelState::Down { .. }))
                .await
            {
                Ok(state) => match &*state {
                    TunnelState::Down { reason } => reason.clone(),
                    TunnelState::Up => return,
                },
                Err(_) => return,
            };
            if let Some(health) = health.upgrade() {
                Health(health).lose(format!("the tunnel went down: {reason}"));
            }
        });
    }
}

/// A connection whose every use is noted, and whose connection errors mark it lost.
pub(crate) struct Tracked {
    inner: Arc<dyn Connection>,
    activity: Arc<Activity>,
    health: Health,
}

impl Tracked {
    pub(crate) fn new(inner: Arc<dyn Connection>) -> Self {
        Self {
            inner,
            activity: Arc::new(Activity::new()),
            health: Health::new(),
        }
    }

    pub(crate) fn health(&self) -> &Health {
        &self.health
    }

    fn observe<T>(&self, result: Result<T>) -> Result<T> {
        observe(&*self.inner, &self.health, &result);
        result
    }

    /// Sends keep-alives every `interval` of idleness until the session is dropped or lost.
    pub(crate) fn keep_alive(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let connection = Arc::downgrade(&self.inner);
        let activity = self.activity.clone();
        let health = self.health.clone();
        tokio::spawn(keep_alive_loop(connection, activity, health, interval))
    }

    /// Closes the session (marks it [`SessionHealth::IdleClosed`]) once it has not been used
    /// for `after` (ADR-029).
    pub(crate) fn close_when_idle(&self, after: Duration) -> tokio::task::JoinHandle<()> {
        let connection = Arc::downgrade(&self.inner);
        let activity = self.activity.clone();
        let health = self.health.clone();
        tokio::spawn(idle_close_loop(connection, activity, health, after))
    }
}

/// SQLSTATEs that end the session: class 08 (connection exceptions, also MySQL's `08S01`) and
/// PostgreSQL's terminations (administrator, crash, `idle_session_timeout`). They arrive
/// as ordinary errors, before the driver sees the connection close.
fn ends_the_session(code: &str) -> bool {
    code.starts_with("08") || matches!(code, "57P01" | "57P02" | "57P05")
}

fn observe<T>(connection: &dyn Connection, health: &Health, result: &Result<T>) {
    match result {
        Err(DriverError::Connection(message)) => health.lose(message.clone()),
        Err(DriverError::Database {
            code: Some(code),
            message,
            ..
        }) if ends_the_session(code) => health.lose(message.clone()),
        // A server that ends the session sends an ordinary error first (PostgreSQL's
        // `terminating connection…`); the connection is closed after it.
        Err(_) if connection.is_closed() => health.lose("the server closed the connection"),
        Ok(_)
        | Err(
            DriverError::Database { .. }
            | DriverError::Cancelled
            | DriverError::Decode { .. }
            | DriverError::Other(_),
        ) => {}
    }
}

async fn keep_alive_loop(
    connection: Weak<dyn Connection>,
    activity: Arc<Activity>,
    health: Health,
    interval: Duration,
) {
    // Uses seen when a transaction was found open: no keep-alive until the session is used.
    let mut paused_at: Option<u64> = None;
    loop {
        let due = activity.last() + interval;
        tokio::time::sleep_until(due.max(Instant::now() + Duration::from_millis(10))).await;
        if health.is_lost() {
            return;
        }
        let idle =
            activity.busy.load(Ordering::SeqCst) == 0 && activity.last().elapsed() >= interval;
        let uses = activity.uses.load(Ordering::SeqCst);
        if !idle {
            continue;
        }
        let Some(connection) = connection.upgrade() else {
            return;
        };
        if connection.is_closed() {
            health.lose("the server closed the connection");
            return;
        }
        if paused_at == Some(uses) {
            // Still the same open transaction: wait a whole interval again.
            *activity.last.lock().expect("lock poisoned") = Instant::now();
            continue;
        }
        // Reserve the idle connection before starting the timeout. User operations acquire a
        // read permit, held until their result stream is dropped, so no query can race a ping.
        let Ok(_permit) = activity.gate.clone().try_write_owned() else {
            continue;
        };
        if activity.busy.load(Ordering::SeqCst) != 0 || activity.last().elapsed() < interval {
            continue;
        }
        let answer = tokio::time::timeout(KEEPALIVE_ANSWER_TIMEOUT, connection.keep_alive()).await;
        // A keep-alive is not a use: the next one is due a whole interval from now.
        *activity.last.lock().expect("lock poisoned") = Instant::now();
        match answer {
            Ok(Ok(KeepAlive::Alive)) => paused_at = None,
            Ok(Ok(KeepAlive::InTransaction)) => paused_at = Some(uses),
            // A connection error marks the session lost; anything else (an engine without
            // keep-alives) only stops them.
            Ok(Err(e)) => {
                observe(&*connection, &health, &Err::<(), _>(e));
                return;
            }
            Err(_) => {
                health.lose(format!(
                    "the server did not answer a keep-alive in {} s",
                    KEEPALIVE_ANSWER_TIMEOUT.as_secs()
                ));
                return;
            }
        }
    }
}

/// Waits until the session has not been used for `after`, then asks the driver whether a
/// transaction is open (its keep-alive check): closed only when it says none is. With an open
/// transaction it waits a whole period again; an engine that cannot tell is never closed, so
/// uncommitted work is never rolled back behind the user's back.
async fn idle_close_loop(
    connection: Weak<dyn Connection>,
    activity: Arc<Activity>,
    health: Health,
    after: Duration,
) {
    loop {
        let due = activity.last_use() + after;
        tokio::time::sleep_until(due.max(Instant::now() + Duration::from_millis(10))).await;
        if health.is_lost() {
            return;
        }
        let idle =
            || activity.busy.load(Ordering::SeqCst) == 0 && activity.last_use().elapsed() >= after;
        if !idle() {
            continue;
        }
        let Some(connection) = connection.upgrade() else {
            return;
        };
        if connection.is_closed() {
            health.lose("the server closed the connection");
            return;
        }
        // Like a keep-alive, it holds the connection so no user operation starts meanwhile.
        let Ok(_permit) = activity.gate.clone().try_write_owned() else {
            continue;
        };
        if !idle() {
            continue;
        }
        let answer = tokio::time::timeout(KEEPALIVE_ANSWER_TIMEOUT, connection.keep_alive()).await;
        *activity.last.lock().expect("lock poisoned") = Instant::now();
        match answer {
            Ok(Ok(KeepAlive::Alive)) => {
                health.close_idle(after);
                return;
            }
            Ok(Ok(KeepAlive::InTransaction)) => {
                *activity.last_use.lock().expect("lock poisoned") = Instant::now();
            }
            // An engine without the check is left open; a connection error marks it lost.
            Ok(Err(e)) => {
                observe(&*connection, &health, &Err::<(), _>(e));
                return;
            }
            Err(_) => {
                health.lose(format!(
                    "the server did not answer in {} s",
                    KEEPALIVE_ANSWER_TIMEOUT.as_secs()
                ));
                return;
            }
        }
    }
}

#[async_trait]
impl Connection for Tracked {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        let busy = self.activity.begin().await;
        let events = self.observe(self.inner.execute(sql, opts).await)?;
        let inner = self.inner.clone();
        let health = self.health.clone();
        // Busy until the rows are read (or the stream is dropped).
        Ok(Box::pin(events.map(move |event| {
            let _busy = &busy;
            observe(&*inner, &health, &event);
            event
        })))
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.inner.cancel_handle()
    }

    async fn server_version(&self) -> Result<String> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.server_version().await)
    }

    async fn keep_alive(&self) -> Result<KeepAlive> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.keep_alive().await)
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        let _held = self.activity.peek().await;
        self.observe(self.inner.transaction_state().await)
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    async fn current_database(&self) -> Result<Option<String>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.current_database().await)
    }
    async fn database_context(&self) -> Result<Option<String>> {
        // A local actor snapshot is not server activity and must not extend an idle session.
        self.inner.database_context().await
    }

    async fn select_database(&self, database: &str) -> Result<()> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.select_database(database).await)
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_databases().await)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_schemas().await)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_objects(schema).await)
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.describe_table(schema, name).await)
    }

    async fn list_referencing_keys(&self, schema: &str, name: &str) -> Result<Vec<ReferencingKey>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_referencing_keys(schema, name).await)
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_schema_objects(schema).await)
    }

    async fn list_server_objects(&self) -> Result<Vec<ServerObject>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_server_objects().await)
    }

    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_schemas_matching(filter).await)
    }

    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.list_objects_matching(schema, filter).await)
    }

    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let _busy = self.activity.begin().await;
        self.observe(
            self.inner
                .list_schema_objects_matching(schema, filter)
                .await,
        )
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.object_definition(object).await)
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.relation_definition(schema, name).await)
    }

    async fn procedure_call(&self, object: &SchemaObject) -> Result<String> {
        let _busy = self.activity.begin().await;
        self.observe(self.inner.procedure_call(object).await)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use spelaion_driver_api::ExecEvent;

    use super::*;

    /// Counts keep-alives; `execute` blocks until told to finish.
    struct Fake {
        pings: AtomicUsize,
        capabilities: Capabilities,
        release: tokio::sync::Notify,
        /// The keep-alive check answers that a transaction is open.
        in_transaction: std::sync::atomic::AtomicBool,
        /// The engine has no keep-alive check (SQLite, DuckDB).
        no_check: std::sync::atomic::AtomicBool,
    }

    struct NoCancel;

    #[async_trait]
    impl CancelHandle for NoCancel {
        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl Connection for Fake {
        fn capabilities(&self) -> &Capabilities {
            &self.capabilities
        }
        async fn execute(&self, _sql: &str, _opts: &ExecOptions) -> Result<EventStream> {
            self.release.notified().await;
            let done: Result<ExecEvent> = Ok(ExecEvent::Done {
                rows_affected: None,
            });
            Ok(Box::pin(futures_util::stream::iter([done])))
        }
        fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
            Arc::new(NoCancel)
        }
        async fn server_version(&self) -> Result<String> {
            Err(DriverError::Connection("gone".into()))
        }
        async fn keep_alive(&self) -> Result<KeepAlive> {
            self.pings.fetch_add(1, Ordering::SeqCst);
            if self.no_check.load(Ordering::SeqCst) {
                Err(DriverError::Other("this engine has no keep-alive".into()))
            } else if self.in_transaction.load(Ordering::SeqCst) {
                Ok(KeepAlive::InTransaction)
            } else {
                Ok(KeepAlive::Alive)
            }
        }
        async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
            Ok(Vec::new())
        }
        async fn list_objects(&self, _schema: &str) -> Result<Vec<ObjectInfo>> {
            Ok(Vec::new())
        }
        async fn describe_table(&self, _schema: &str, _name: &str) -> Result<TableInfo> {
            Err(DriverError::Other("none".into()))
        }
    }

    fn fake() -> Arc<Fake> {
        Arc::new(Fake {
            pings: AtomicUsize::new(0),
            capabilities: Capabilities {
                transactions: true,
                cancel: false,
                explain: false,
                editable_results: false,
                schemas: false,
            },
            release: tokio::sync::Notify::new(),
            in_transaction: Default::default(),
            no_check: Default::default(),
        })
    }

    fn closed(tracked: &Tracked) -> bool {
        matches!(
            *tracked.health().subscribe().borrow(),
            SessionHealth::IdleClosed { .. }
        )
    }

    #[tokio::test(start_paused = true)]
    async fn idle_sessions_close_after_their_time_and_uses_or_keep_alives_postpone_it() {
        let inner = fake();
        let tracked = Arc::new(Tracked::new(inner.clone()));
        let keep_alive = tracked.keep_alive(Duration::from_secs(10));
        let task = tracked.close_when_idle(Duration::from_secs(60));
        tokio::time::sleep(Duration::from_secs(40)).await;
        // A use halfway: the 60 s start again from it.
        let _ = tracked.list_schemas().await;
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert!(!closed(&tracked), "used 40 s ago");
        assert!(
            inner.pings.load(Ordering::SeqCst) >= 2,
            "keep-alives ran meanwhile"
        );
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert!(closed(&tracked), "keep-alives are not uses");
        assert_eq!(
            *tracked.health().subscribe().borrow(),
            SessionHealth::IdleClosed {
                after: Duration::from_secs(60)
            }
        );
        assert!(tracked.health().is_lost());
        task.await.unwrap();
        keep_alive.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn never_closed_while_running_in_a_transaction_or_where_the_engine_cannot_tell() {
        // Running for longer than the idle time.
        let inner = fake();
        let tracked = Arc::new(Tracked::new(inner.clone()));
        let task = tracked.close_when_idle(Duration::from_secs(10));
        let running = {
            let tracked = tracked.clone();
            tokio::spawn(async move {
                tracked
                    .execute("select pg_sleep(60)", &ExecOptions::default())
                    .await
                    .map(|_| ())
            })
        };
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert!(!closed(&tracked), "busy the whole time");
        inner.release.notify_one();
        running.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert!(closed(&tracked), "idle after it");
        task.abort();

        // An open transaction: it waits, and closes once the transaction is over.
        let inner = fake();
        inner.in_transaction.store(true, Ordering::SeqCst);
        let tracked = Arc::new(Tracked::new(inner.clone()));
        let task = tracked.close_when_idle(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(100)).await;
        assert!(!closed(&tracked), "a transaction is open");
        inner.in_transaction.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(11)).await;
        assert!(closed(&tracked), "the transaction ended");
        task.abort();

        // SQLite and DuckDB cannot say whether one is open: left alone.
        let inner = fake();
        inner.no_check.store(true, Ordering::SeqCst);
        let tracked = Arc::new(Tracked::new(inner.clone()));
        let task = tracked.close_when_idle(Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(100)).await;
        assert!(!tracked.health().is_lost());
        assert!(task.is_finished(), "it gave up");
    }

    #[tokio::test(start_paused = true)]
    async fn no_keep_alive_while_something_runs() {
        let inner = fake();
        let tracked = Arc::new(Tracked::new(inner.clone()));
        let task = tracked.keep_alive(Duration::from_secs(10));
        let running = {
            let tracked = tracked.clone();
            tokio::spawn(async move {
                tracked
                    .execute("select pg_sleep(60)", &ExecOptions::default())
                    .await
                    .map(|_| ())
            })
        };
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(inner.pings.load(Ordering::SeqCst), 0, "busy the whole time");
        inner.release.notify_one();
        running.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_secs(25)).await;
        assert!(inner.pings.load(Ordering::SeqCst) >= 1, "idle again");
        task.abort();
    }

    #[tokio::test]
    async fn keep_alive_reservation_and_user_operations_exclude_each_other() {
        let activity = Arc::new(Activity::new());
        let ping = activity.gate.clone().try_write_owned().unwrap();
        let operation = tokio::spawn({
            let activity = activity.clone();
            async move { activity.begin().await }
        });
        tokio::task::yield_now().await;
        assert!(
            !operation.is_finished(),
            "a user operation must wait for the ping"
        );
        drop(ping);
        let busy = operation.await.unwrap();
        assert!(activity.gate.clone().try_write_owned().is_err());
        drop(busy);
        assert!(activity.gate.clone().try_write_owned().is_ok());
    }

    #[tokio::test]
    async fn connection_errors_and_a_downed_tunnel_mark_the_session_lost() {
        let tracked = Tracked::new(fake());
        assert!(!tracked.health().is_lost());
        assert!(tracked.server_version().await.is_err());
        assert_eq!(
            *tracked.health().subscribe().borrow(),
            SessionHealth::Lost {
                reason: "gone".into()
            }
        );

        let tracked = Tracked::new(fake());
        let (tunnel, state) = watch::channel(TunnelState::Up);
        tracked.health().watch_tunnel(state);
        let mut health = tracked.health().subscribe();
        tunnel.send_replace(TunnelState::Down {
            reason: "hop 1 (bastion): keep-alive timeout".into(),
        });
        let lost = health
            .wait_for(|h| matches!(h, SessionHealth::Lost { .. }))
            .await
            .unwrap()
            .clone();
        assert_eq!(
            lost,
            SessionHealth::Lost {
                reason: "the tunnel went down: hop 1 (bastion): keep-alive timeout".into()
            }
        );
    }
}
