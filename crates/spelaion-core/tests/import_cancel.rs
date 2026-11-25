//! A cancelled import never interrupts its own transaction's statements (SDB-448). A cancel
//! that reaches SQL Server while `BEGIN TRANSACTION`, `COMMIT` or `ROLLBACK` runs can leave the
//! server in a transaction the driver does not know of (its TDS descriptor was in the part of
//! the response the cancel threw away): every later request then fails with error 3989. Seen
//! under load on `import::sqlserver_imports`, where the cancel came right after a commit, as
//! the next batch's `BEGIN TRANSACTION` went out. A fake connection makes that moment certain:
//! its cancel stops whatever statement runs, as an Attention does.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spelaion_config::Safeguards;
use spelaion_core::{
    ColumnMapping, IfExists, ImportError, ImportProgress, ImportRequest, ImportSource,
    ImportTarget, JobCancel, ParseOptions, ResultStore, Session, run_import,
};
use spelaion_driver_api::{
    CancelHandle, Capabilities, Connection, DriverError, EventStream, ExecEvent, ExecOptions,
    InsertTarget, ObjectInfo, Result, SchemaInfo, TableInfo, TextRows, TransactionState,
};

/// What the fake engine and its cancel handle share.
#[derive(Default)]
struct Engine {
    /// Everything it was sent, in order (`insert` for each batch of rows).
    sent: Mutex<Vec<String>>,
    /// The statement running now, if any.
    running: Mutex<Option<String>>,
    /// The statements a cancel reached while they ran.
    interrupted: Mutex<Vec<String>>,
    /// How many `BEGIN TRANSACTION` it was sent.
    begins: AtomicUsize,
    /// The `BEGIN TRANSACTION` to hold until the test lets it go (1 = the first).
    hold_begin: usize,
    /// Hold the batches of rows instead, from this one on (1 = the first).
    hold_insert: usize,
    inserts: AtomicUsize,
    /// A held statement started.
    holding: tokio::sync::watch::Sender<bool>,
    /// The held statement may end.
    release: tokio::sync::watch::Sender<bool>,
}

impl Engine {
    fn record(&self, what: &str) {
        self.sent.lock().unwrap().push(what.to_string());
        *self.running.lock().unwrap() = Some(what.to_string());
    }

    /// Holds the running statement until the test lets it go; `Err(Cancelled)` if a cancel
    /// reached it meanwhile, as an Attention fails the statement it stops.
    async fn hold(&self, what: &str) -> Result<()> {
        self.holding.send_replace(true);
        let mut release = self.release.subscribe();
        let _ = release.wait_for(|go| *go).await;
        *self.running.lock().unwrap() = None;
        if self.interrupted.lock().unwrap().iter().any(|s| s == what) {
            Err(DriverError::Cancelled)
        } else {
            Ok(())
        }
    }
}

struct Fake {
    engine: Arc<Engine>,
    capabilities: Capabilities,
}

/// Like SQL Server's Attention: it stops whatever statement runs when it arrives.
struct Attention(Arc<Engine>);

#[async_trait]
impl CancelHandle for Attention {
    async fn cancel(&self) -> Result<()> {
        if let Some(running) = self.0.running.lock().unwrap().clone() {
            self.0.interrupted.lock().unwrap().push(running);
        }
        Ok(())
    }
}

fn done() -> EventStream {
    let done: Result<ExecEvent> = Ok(ExecEvent::Done {
        rows_affected: None,
    });
    Box::pin(futures_util::stream::iter([done]))
}

#[async_trait]
impl Connection for Fake {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, _opts: &ExecOptions) -> Result<EventStream> {
        let engine = &self.engine;
        engine.record(sql);
        if sql == "BEGIN TRANSACTION"
            && engine.begins.fetch_add(1, Ordering::SeqCst) + 1 == engine.hold_begin
        {
            engine.hold(sql).await?;
        }
        *engine.running.lock().unwrap() = None;
        Ok(done())
    }

    async fn insert_rows(&self, _target: &InsertTarget, rows: TextRows) -> Result<u64> {
        let engine = &self.engine;
        engine.record("insert");
        let nth = engine.inserts.fetch_add(1, Ordering::SeqCst) + 1;
        if engine.hold_insert > 0 && nth >= engine.hold_insert {
            engine.hold("insert").await?;
        }
        *engine.running.lock().unwrap() = None;
        Ok(rows.len() as u64)
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        Ok(TransactionState::Idle)
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(Attention(self.engine.clone()))
    }

    async fn server_version(&self) -> Result<String> {
        Ok("fake".into())
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

const NONE: Safeguards = Safeguards {
    confirm_execution: false,
    confirm_data_changes: false,
    close_idle_after: None,
    close_idle_transaction_after: None,
    manual_commit: false,
    smart_commit: false,
};

/// Thirty rows, committed every ten.
fn thirty_rows(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("spelaion-{name}-{}.csv", std::process::id()));
    let mut text = String::from("id\n");
    for id in 1..=30 {
        text.push_str(&format!("{id}\n"));
    }
    std::fs::write(&path, text).unwrap();
    path
}

fn session(engine: &Arc<Engine>) -> Arc<Session> {
    let fake = Fake {
        engine: engine.clone(),
        capabilities: Capabilities {
            transactions: true,
            cancel: true,
            explain: false,
            editable_results: false,
            schemas: true,
            insert_rows: true,
            bound_params: false,
            insert_bytes: false,
        },
    };
    Arc::new(Session::new(
        Box::new(fake),
        Arc::new(spelaion_dialect::SqlServerDialect),
        Arc::new(ResultStore::new()),
    ))
}

fn import(path: PathBuf) -> ImportRequest {
    ImportRequest {
        source: ImportSource::File(path),
        options: ParseOptions::default(),
        target: ImportTarget::New {
            schema: "dbo".into(),
            table: "import_cancel".into(),
            if_exists: IfExists::Fail,
        },
        columns: vec![ColumnMapping::New {
            name: "id".into(),
            sql_type: "int".into(),
        }],
        commit_every: 10,
    }
}

fn nothing(_: ImportProgress) {}

/// Runs the import until the held statement starts, cancels it there, lets the statement end,
/// and returns how the import ended.
async fn cancel_while_held(
    engine: &Arc<Engine>,
    path: PathBuf,
) -> std::result::Result<u64, ImportError> {
    let session = session(engine);
    let cancel = Arc::new(JobCancel::new());
    let running = tokio::spawn({
        let session = session.clone();
        let cancel = cancel.clone();
        async move {
            run_import(
                &session,
                &import(path),
                &NONE,
                false,
                false,
                &cancel,
                &nothing,
            )
            .await
            .map(|summary| summary.rows)
        }
    });
    let mut holding = engine.holding.subscribe();
    tokio::time::timeout(Duration::from_secs(10), holding.wait_for(|h| *h))
        .await
        .expect("the held statement started")
        .unwrap();
    cancel.cancel().await;
    engine.release.send_replace(true);
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the import ended")
        .unwrap()
}

#[tokio::test]
async fn a_cancel_while_the_import_begins_its_transaction_never_reaches_the_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    // The second batch's BEGIN TRANSACTION runs when the cancel comes, right after the first
    // batch was committed.
    let engine = Arc::new(Engine {
        hold_begin: 2,
        ..Engine::default()
    });
    let outcome = cancel_while_held(&engine, thirty_rows("import-cancel-begin")).await;
    assert!(
        matches!(outcome, Err(ImportError::Cancelled { committed: 10 })),
        "{outcome:?}"
    );
    // The engine never had its transaction's own statement stopped under it…
    assert_eq!(*engine.interrupted.lock().unwrap(), Vec::<String>::new());
    // …and the transaction that BEGIN opened was rolled back, with nothing loaded into it.
    let sent = engine.sent.lock().unwrap().clone();
    let tail: Vec<&str> = sent
        .iter()
        .rev()
        .take(2)
        .rev()
        .map(String::as_str)
        .collect();
    assert_eq!(tail, ["BEGIN TRANSACTION", "ROLLBACK"], "{sent:?}");
}

#[tokio::test]
async fn a_cancel_while_rows_load_stops_the_load_and_rolls_its_batch_back() {
    let _turn = crate::serial::file_turn(module_path!());
    // The second batch of rows is loading when the cancel comes: a long load must stop.
    let engine = Arc::new(Engine {
        hold_insert: 2,
        ..Engine::default()
    });
    let outcome = cancel_while_held(&engine, thirty_rows("import-cancel-insert")).await;
    assert!(
        matches!(outcome, Err(ImportError::Cancelled { committed: 10 })),
        "{outcome:?}"
    );
    assert_eq!(*engine.interrupted.lock().unwrap(), ["insert"]);
    let sent = engine.sent.lock().unwrap().clone();
    assert_eq!(
        sent.last().map(String::as_str),
        Some("ROLLBACK"),
        "{sent:?}"
    );
    // The ROLLBACK went out after the cancel had reached the engine: nothing stopped it.
    assert_eq!(
        sent.iter().filter(|s| *s == "ROLLBACK").count(),
        1,
        "{sent:?}"
    );
}
