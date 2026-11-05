//! Whether a session's connection still works, and keeping an idle one alive (ADR-028).
//!
//! Every call a session makes goes through [`Tracked`], which notes when the connection was
//! last used and turns a connection error into [`SessionHealth::Lost`]. The keep-alive loop
//! sends the driver's cheapest round trip only when the session has been idle for the whole
//! interval, never while something runs, and stops while a transaction is open. Nothing is
//! ever reconnected or re-run here: a lost session stays lost until the user reconnects.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_driver_api::{
    CancelHandle, Capabilities, Connection, DriverError, EventStream, ExecOptions, KeepAlive,
    NameFilter, ObjectInfo, Result, SchemaInfo, SchemaObject, ServerObject, TableInfo,
};
use spelaion_ssh::TunnelState;
use tokio::sync::watch;
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
}

/// When the connection was last used, and whether something is using it now.
pub(crate) struct Activity {
    busy: AtomicUsize,
    /// Bumped by every use: a keep-alive paused by an open transaction resumes after one.
    uses: AtomicU64,
    last: Mutex<Instant>,
}

impl Activity {
    fn new() -> Self {
        Self {
            busy: AtomicUsize::new(0),
            uses: AtomicU64::new(0),
            last: Mutex::new(Instant::now()),
        }
    }

    fn begin(self: &Arc<Self>) -> Busy {
        self.busy.fetch_add(1, Ordering::SeqCst);
        self.uses.fetch_add(1, Ordering::SeqCst);
        Busy(self.clone())
    }

    fn last(&self) -> Instant {
        *self.last.lock().expect("lock poisoned")
    }
}

/// Marks the connection busy until dropped (for a statement, until its rows are read).
struct Busy(Arc<Activity>);

impl Drop for Busy {
    fn drop(&mut self) {
        *self.0.last.lock().expect("lock poisoned") = Instant::now();
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
            SessionHealth::Lost { .. } => false,
        });
    }

    fn is_lost(&self) -> bool {
        matches!(*self.0.borrow(), SessionHealth::Lost { .. })
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
        if paused_at == Some(uses) {
            // Still the same open transaction: wait a whole interval again.
            *activity.last.lock().expect("lock poisoned") = Instant::now();
            continue;
        }
        let Some(connection) = connection.upgrade() else {
            return;
        };
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

#[async_trait]
impl Connection for Tracked {
    fn capabilities(&self) -> &Capabilities {
        self.inner.capabilities()
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        let busy = self.activity.begin();
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
        let _busy = self.activity.begin();
        self.observe(self.inner.server_version().await)
    }

    async fn keep_alive(&self) -> Result<KeepAlive> {
        let _busy = self.activity.begin();
        self.observe(self.inner.keep_alive().await)
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    async fn current_database(&self) -> Result<Option<String>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.current_database().await)
    }

    async fn select_database(&self, database: &str) -> Result<()> {
        let _busy = self.activity.begin();
        self.observe(self.inner.select_database(database).await)
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.list_databases().await)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.list_schemas().await)
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.list_objects(schema).await)
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        let _busy = self.activity.begin();
        self.observe(self.inner.describe_table(schema, name).await)
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.list_schema_objects(schema).await)
    }

    async fn list_server_objects(&self) -> Result<Vec<ServerObject>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.list_server_objects().await)
    }

    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.list_schemas_matching(filter).await)
    }

    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let _busy = self.activity.begin();
        self.observe(self.inner.list_objects_matching(schema, filter).await)
    }

    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let _busy = self.activity.begin();
        self.observe(
            self.inner
                .list_schema_objects_matching(schema, filter)
                .await,
        )
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        let _busy = self.activity.begin();
        self.observe(self.inner.object_definition(object).await)
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        let _busy = self.activity.begin();
        self.observe(self.inner.relation_definition(schema, name).await)
    }

    async fn procedure_call(&self, object: &SchemaObject) -> Result<String> {
        let _busy = self.activity.begin();
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
            Ok(KeepAlive::Alive)
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
        })
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
