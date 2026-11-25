//! A made-up engine, served as a plugin: what the tests of the plugin protocol talk to.
//!
//! Its "SQL" is a handful of words, each of which makes the driver do one thing a real one
//! does (several result sets, a statement that waits to be cancelled, an error with every
//! field) or one thing a plugin must survive or must never get away with (dying mid-query,
//! writing to standard output, repeating the password it was given).
//!
//! The program's file name chooses what it is: with `other-version` in it, a plugin that
//! speaks another major version of the protocol; with `minor-zero`, a plugin that speaks only
//! version 1.0 (before requests, ADR-056); with `not-a-plugin`, a program that prints text;
//! anything else, the plugin.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use async_trait::async_trait;
use spelaion_driver_api::{
    CancelHandle, Capabilities, ColumnInfo, ColumnMeta, ConnectParams, Connection, Dependencies,
    DependencyInfo, DependencyKind, Driver, DriverError, EventStream, ExecEvent, ExecOptions,
    InsertTarget, KeepAlive, LogicalType, Notice, ObjectInfo, ObjectKind, ObjectProperty,
    PartitionInfo, Partitioning, PlanMode, Result, ResultSchema, SchemaInfo, SchemaObject,
    SchemaObjectKind, ServerObject, ServerObjectKind, TableInfo, TextRows, TransactionState,
};
use spelaion_plugin_protocol::{Frame, Message, RpcError, code, read_frame, write_frame};
use tokio::sync::Notify;

/// Batches the row generator has handed over, whoever asked for them.
static PRODUCED: AtomicU64 = AtomicU64::new(0);
/// Connections that are open now.
static OPEN: AtomicU64 = AtomicU64::new(0);
/// Connections being made now.
static CONNECTING: AtomicU64 = AtomicU64::new(0);
/// The databases `create_database` made.
static CREATED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Every row `insert_rows` was given, with its target.
static INSERTED: Mutex<Vec<(String, Vec<Option<String>>)>> = Mutex::new(Vec::new());
/// How many `file:` databases a closed connection is still writing, and a wake-up each time
/// one is done: what a driver that keeps a file open for several connections does after the
/// last one goes (DuckDB writes what it holds into the file as it closes it).
static CLOSING: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

#[tokio::main]
async fn main() {
    let name = std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if name.contains("not-a-plugin") {
        println!("Hello! I am a program, but not a plugin.");
        return;
    }
    if name.contains("other-version") {
        other_version(name.contains("claims"));
        return;
    }
    if name.contains("minor-zero") {
        minor_zero();
        return;
    }
    let drivers: Vec<Arc<dyn Driver>> = vec![Arc::new(Made("test")), Arc::new(Made("test2"))];
    spelaion_plugin_sdk::run(spelaion_plugin_sdk::Plugin::new(
        "spelaion-test-plugin",
        env!("CARGO_PKG_VERSION"),
        drivers,
    ))
    .await
}

/// A plugin of protocol 2.x. `claims`: instead of refusing, it answers that it chose 2.0,
/// which nobody offered.
fn other_version(claims: bool) {
    use std::io::Write;
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    while let Ok(Some(frame)) = read_frame(&mut stdin) {
        let Frame::Message(bytes) = frame else {
            continue;
        };
        let Ok(message) = Message::from_bytes(&bytes) else {
            continue;
        };
        let Some(id) = message.id else { continue };
        let answer = if claims {
            Message::result(
                id,
                serde_json::json!({
                    "protocol": {"major": 2, "minor": 0},
                    "plugin": {"name": "from-the-future", "version": "9"},
                    "engines": ["test"],
                    "credit": 8,
                }),
            )
        } else {
            Message::error(
                id,
                RpcError {
                    code: code::UNSUPPORTED_PROTOCOL,
                    message: "this plugin speaks version 2.0 of the protocol".into(),
                    data: Some(serde_json::json!({"protocols": [{"major": 2, "minor": 0}]})),
                },
            )
        };
        let frame = Frame::Message(answer.to_bytes());
        if write_frame(&mut stdout, &frame).is_err() || stdout.flush().is_err() {
            return;
        }
    }
}

/// A plugin of protocol 1.0: it starts and connects, and anything else it is asked fails. The
/// core must refuse a request or connection settings before they reach it: a plugin of 1.0
/// would not know what they are.
fn minor_zero() {
    use std::io::Write;
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    while let Ok(Some(frame)) = read_frame(&mut stdin) {
        let Frame::Message(bytes) = frame else {
            continue;
        };
        let Ok(message) = Message::from_bytes(&bytes) else {
            continue;
        };
        let Some(id) = message.id else { continue };
        let answer = match message.method.as_deref() {
            Some("initialize") => Message::result(
                id,
                serde_json::json!({
                    "protocol": {"major": 1, "minor": 0},
                    "plugin": {"name": "minor-zero", "version": "1"},
                    "engines": ["test"],
                    "credit": 8,
                }),
            ),
            Some("connect") => {
                let options = message
                    .params
                    .as_ref()
                    .is_some_and(|params| params.get("options").is_some());
                if options {
                    // It would ignore them: the core must never get here.
                    std::process::exit(3);
                }
                Message::result(
                    id,
                    serde_json::json!({"capabilities": {"transactions": false, "cancel": false,
                        "explain": false, "editable_results": false, "schemas": false,
                        "insert_rows": false, "bound_params": false}}),
                )
            }
            // It would read an unknown kind of run as an error, or worse: the core must never
            // send one.
            Some("execute") => std::process::exit(4),
            // 1.2's requests (SDB-393): a plugin of 1.0 is never asked them.
            // 1.3's request (ADR-082): never asked of it either.
            Some("server_object_definition") => std::process::exit(6),
            Some("object_properties" | "table_partitions" | "object_dependencies") => {
                std::process::exit(5)
            }
            _ => Message::error(
                id,
                RpcError {
                    code: code::METHOD_NOT_FOUND,
                    message: "not here".into(),
                    data: None,
                },
            ),
        };
        let frame = Frame::Message(answer.to_bytes());
        if write_frame(&mut stdout, &frame).is_err() || stdout.flush().is_err() {
            return;
        }
    }
}

struct Made(&'static str);

#[async_trait]
impl Driver for Made {
    fn engine(&self) -> &str {
        self.0
    }

    async fn create_database(&self, database: &str) -> Result<()> {
        // Only the first engine's databases are files.
        if self.0 != "test" {
            return Err(DriverError::Other(format!("{} is not a file", self.0)));
        }
        let mut created = CREATED.lock().expect("lock poisoned");
        if created.iter().any(|name| name == database) {
            return Err(DriverError::Other(format!(
                "{database} already exists: a new database never replaces a file"
            )));
        }
        created.push(database.to_string());
        Ok(())
    }

    /// Waits for every `file:` database to be written whole, as DuckDB's driver waits for its
    /// files to be closed.
    async fn shutdown(&self) {
        let _ = tokio::task::spawn_blocking(|| {
            let mut closing = CLOSING.0.lock().expect("lock poisoned");
            while *closing > 0 {
                closing = CLOSING.1.wait(closing).expect("lock poisoned");
            }
        })
        .await;
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let password = params
            .password
            .as_ref()
            .map(|p| p.expose().to_string())
            .unwrap_or_default();
        match params.database.as_str() {
            "refuse" => {
                return Err(DriverError::Connection(format!(
                    "no database for {}",
                    params.user
                )));
            }
            // What a careless driver could do: the password in its error.
            "leak" => {
                return Err(DriverError::Connection(format!(
                    "login failed for {} with password {password}",
                    params.user
                )));
            }
            "slow" => {
                // Counted for as long as the attempt lives, however it ends.
                struct Attempt;
                impl Drop for Attempt {
                    fn drop(&mut self) {
                        CONNECTING.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                CONNECTING.fetch_add(1, Ordering::SeqCst);
                let _attempt = Attempt;
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
            _ => {}
        }
        // `file:<path>`: the lines it is told to keep go into that file only as the connection
        // closes, slowly; `file-hang:<path>`: that closing never ends.
        let kept = [("file:", false), ("file-hang:", true)]
            .into_iter()
            .find_map(|(prefix, hangs)| {
                params.database.strip_prefix(prefix).map(|path| Kept {
                    path: PathBuf::from(path),
                    hangs,
                    lines: Mutex::new(Vec::new()),
                })
            });
        OPEN.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MadeConnection {
            kept,
            engine: self.0,
            password,
            params: format!(
                "{}:{} user={} tls={:?} addr={:?} read_only={} app={:?} timeout={:?} ca={:?} \
                 options={:?}",
                params.host,
                params.port,
                params.user,
                params.tls,
                params.connect_addr,
                params.read_only,
                params.application_name,
                params.connect_timeout,
                params.ca_certificate,
                params.options,
            ),
            capabilities: Capabilities {
                transactions: true,
                cancel: true,
                explain: true,
                editable_results: false,
                schemas: true,
                insert_rows: true,
                bound_params: true,
            },
            cancelled: Arc::new(Notify::new()),
            in_flight: Arc::new(Mutex::new(None)),
            closed: Arc::new(AtomicBool::new(false)),
        }))
    }
}

/// The lines a `file:` database holds until its connection closes.
struct Kept {
    path: PathBuf,
    hangs: bool,
    lines: Mutex<Vec<String>>,
}

impl Kept {
    /// Writes the lines, then `closed`, taking its time over it as a database writing what it
    /// holds into its file does: a program ended meanwhile leaves the file without them.
    fn write(self) {
        std::thread::sleep(Duration::from_millis(200));
        if self.hangs {
            std::thread::sleep(Duration::from_secs(600));
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .expect("the file opens");
        for line in self.lines.into_inner().expect("lock poisoned") {
            writeln!(file, "{line}").expect("a line is written");
            std::thread::sleep(Duration::from_millis(10));
        }
        writeln!(file, "closed").expect("a line is written");
    }
}

struct MadeConnection {
    kept: Option<Kept>,
    engine: &'static str,
    password: String,
    params: String,
    capabilities: Capabilities,
    cancelled: Arc<Notify>,
    /// The `numbered-sleep` statement in flight, as a driver whose cancel targets the
    /// statement it numbered (the MySQL and PostgreSQL plugins' rule, SDB-221, SDB-222).
    in_flight: InFlight,
    closed: Arc<AtomicBool>,
}

/// The statement a cancel stops: numbered when its `execute` is first polled, let go of when
/// it ends. A cancel while none is in flight does nothing, and is not remembered for the next.
type InFlight = Arc<Mutex<Option<(u64, Arc<Notify>)>>>;

/// The number of the next `numbered-sleep`.
static NUMBERED: AtomicU64 = AtomicU64::new(1);

impl Drop for MadeConnection {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::SeqCst);
        // Written on a thread of its own after the connection is gone, as DuckDB closes a file
        // on its connection's thread: only the driver's `shutdown` knows when it is done.
        if let Some(kept) = self.kept.take() {
            *CLOSING.0.lock().expect("lock poisoned") += 1;
            std::thread::spawn(move || {
                kept.write();
                *CLOSING.0.lock().expect("lock poisoned") -= 1;
                CLOSING.1.notify_all();
            });
        }
    }
}

struct Cancel {
    cancelled: Arc<Notify>,
    in_flight: InFlight,
}

#[async_trait]
impl CancelHandle for Cancel {
    async fn cancel(&self) -> Result<()> {
        self.cancelled.notify_one();
        let numbered = self.in_flight.lock().expect("lock poisoned").clone();
        if let Some((_, stop)) = numbered {
            stop.notify_one();
        }
        Ok(())
    }
}

fn column(name: &str, logical: LogicalType, native: &str) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        logical,
        native_type: native.into(),
        native_id: Some(7),
        origin: None,
    }
}

fn numbered() -> Arc<ResultSchema> {
    Arc::new(ResultSchema::new(vec![
        column("id", LogicalType::Int, "int8"),
        column("name", LogicalType::Text, "text"),
    ]))
}

fn numbered_batch(schema: &ResultSchema, from: i64, rows: i64) -> RecordBatch {
    let ids: Vec<i64> = (from..from + rows).collect();
    let names: Vec<String> = ids.iter().map(|i| format!("row {i}")).collect();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(ids)),
        Arc::new(StringArray::from(names)),
    ];
    RecordBatch::try_new(schema.arrow.clone(), columns).expect("a batch of the schema")
}

/// One text column and these rows.
fn texts(name: &str, rows: Vec<Option<String>>) -> Vec<Result<ExecEvent>> {
    let schema = Arc::new(ResultSchema::new(vec![column(
        name,
        LogicalType::Text,
        "text",
    )]));
    let columns: Vec<ArrayRef> = vec![Arc::new(StringArray::from(rows))];
    let batch = RecordBatch::try_new(schema.arrow.clone(), columns).expect("a batch of the schema");
    vec![
        Ok(ExecEvent::Columns(schema)),
        Ok(ExecEvent::Batch(batch)),
        Ok(ExecEvent::Done {
            rows_affected: None,
        }),
    ]
}

fn events(events: Vec<Result<ExecEvent>>) -> Result<EventStream> {
    Ok(Box::pin(async_stream::stream! {
        for event in events {
            yield event;
        }
    }))
}

#[async_trait]
impl Connection for MadeConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        let words: Vec<&str> = sql.split_whitespace().collect();
        let batch_size = opts.batch_size as i64;
        match words.as_slice() {
            // `keep LINE`: a `file:` database holds the line until its connection closes.
            ["keep", line] => {
                let kept = self
                    .kept
                    .as_ref()
                    .ok_or_else(|| other("keep needs file:"))?;
                kept.lines
                    .lock()
                    .expect("lock poisoned")
                    .push(line.to_string());
                events(vec![Ok(ExecEvent::Done {
                    rows_affected: Some(1),
                })])
            }
            // `rows N`: N numbered rows, in batches of the size asked for.
            ["rows", total] => {
                let total: i64 = total.parse().map_err(|_| other("rows N"))?;
                let schema = numbered();
                Ok(Box::pin(async_stream::stream! {
                    yield Ok(ExecEvent::Columns(schema.clone()));
                    let mut from = 0;
                    while from < total {
                        let rows = batch_size.min(total - from);
                        PRODUCED.fetch_add(1, Ordering::SeqCst);
                        yield Ok(ExecEvent::Batch(numbered_batch(&schema, from, rows)));
                        from += rows;
                    }
                    yield Ok(ExecEvent::Done { rows_affected: None });
                }))
            }
            ["results"] => {
                let schema = numbered();
                let other = Arc::new(ResultSchema::new(vec![column(
                    "only",
                    LogicalType::Native,
                    "geometry",
                )]));
                let points: Vec<ArrayRef> =
                    vec![Arc::new(StringArray::from(vec![Some("POINT(1 2)"), None]))];
                events(vec![
                    Ok(ExecEvent::Columns(schema.clone())),
                    Ok(ExecEvent::Batch(numbered_batch(&schema, 0, 2))),
                    Ok(ExecEvent::Columns(other.clone())),
                    Ok(ExecEvent::Batch(
                        RecordBatch::try_new(other.arrow.clone(), points).expect("a batch"),
                    )),
                    Ok(ExecEvent::Notice(Notice {
                        severity: "NOTICE".into(),
                        message: "two results".into(),
                    })),
                    Ok(ExecEvent::Done {
                        rows_affected: Some(2),
                    }),
                ])
            }
            ["empty"] => events(vec![
                Ok(ExecEvent::Columns(numbered())),
                Ok(ExecEvent::Done {
                    rows_affected: None,
                }),
            ]),
            ["update"] => events(vec![Ok(ExecEvent::Done {
                rows_affected: Some(3),
            })]),
            // Refused before it starts.
            ["refused"] => Err(database_error()),
            // Fails after it has returned rows.
            ["fails"] => {
                let schema = numbered();
                events(vec![
                    Ok(ExecEvent::Columns(schema.clone())),
                    Ok(ExecEvent::Batch(numbered_batch(&schema, 0, 1))),
                    Err(database_error()),
                ])
            }
            // `numbered-sleep MS`: numbered now, as the first poll of a real driver's execute
            // numbers its statement; it ends after MS milliseconds, or as cancelled if a cancel
            // finds it in flight first.
            ["numbered-sleep", ms] => {
                let ms: u64 = ms.parse().map_err(|_| other("numbered-sleep MS"))?;
                let number = NUMBERED.fetch_add(1, Ordering::SeqCst);
                let stop = Arc::new(Notify::new());
                *self.in_flight.lock().expect("lock poisoned") = Some((number, stop.clone()));
                let in_flight = self.in_flight.clone();
                Ok(Box::pin(async_stream::stream! {
                    yield Ok(ExecEvent::Columns(numbered()));
                    let cancelled = tokio::select! {
                        _ = stop.notified() => true,
                        _ = tokio::time::sleep(Duration::from_millis(ms)) => false,
                    };
                    // Let go of: a cancel from now on finds nothing in flight.
                    {
                        let mut current = in_flight.lock().expect("lock poisoned");
                        if current.as_ref().is_some_and(|(n, _)| *n == number) {
                            *current = None;
                        }
                    }
                    if cancelled {
                        yield Err(DriverError::Cancelled);
                    } else {
                        yield Ok(ExecEvent::Done { rows_affected: None });
                    }
                }))
            }
            // Runs until it is cancelled.
            ["sleep"] => {
                let cancelled = self.cancelled.clone();
                Ok(Box::pin(async_stream::stream! {
                    yield Ok(ExecEvent::Columns(numbered()));
                    tokio::select! {
                        _ = cancelled.notified() => yield Err(DriverError::Cancelled),
                        _ = tokio::time::sleep(Duration::from_secs(600)) => {
                            yield Err(other("nobody cancelled"));
                        }
                    }
                }))
            }
            // The same, from an engine that answers only when its first response arrives: the
            // statement has not "started" until it ends.
            ["sleep-unanswered"] => {
                tokio::select! {
                    _ = self.cancelled.notified() => Err(DriverError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_secs(600)) => {
                        Err(other("nobody cancelled"))
                    }
                }
            }
            // The program dies in the middle of a result.
            ["crash"] => {
                let schema = numbered();
                Ok(Box::pin(async_stream::stream! {
                    yield Ok(ExecEvent::Columns(schema.clone()));
                    yield Ok(ExecEvent::Batch(numbered_batch(&schema, 0, 1)));
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    std::process::exit(3);
                }))
            }
            // What a plugin must never do: write to standard output.
            ["print"] => {
                println!("a line that is not the protocol, long enough to be a frame's length");
                events(vec![Ok(ExecEvent::Done {
                    rows_affected: None,
                })])
            }
            ["panic"] => panic!("the driver panicked"),
            // The password, wherever a careless driver could put it.
            ["leak"] => {
                eprintln!("debug: connected with password {}", self.password);
                events(vec![
                    Ok(ExecEvent::Notice(Notice {
                        severity: "WARNING".into(),
                        message: format!("your password is {}", self.password),
                    })),
                    Err(DriverError::Database {
                        code: Some(self.password.clone()),
                        message: format!("bad statement near {}", self.password),
                        detail: Some(format!("detail {}", self.password)),
                        hint: Some(format!("hint {}", self.password)),
                        position: Some(3),
                    }),
                ])
            }
            // How this program was started.
            ["started"] => {
                let mut rows: Vec<Option<String>> = std::env::args()
                    .skip(1)
                    .map(|arg| Some(format!("arg {arg}")))
                    .collect();
                rows.extend(
                    std::env::vars().map(|(key, value)| Some(format!("env {key}={value}"))),
                );
                events(texts("started", rows))
            }
            ["pid"] => events(texts("pid", vec![Some(std::process::id().to_string())])),
            ["engine"] => events(texts("engine", vec![Some(self.engine.to_string())])),
            ["params"] => events(texts("params", vec![Some(self.params.clone())])),
            ["produced"] => events(texts(
                "produced",
                vec![Some(PRODUCED.load(Ordering::SeqCst).to_string())],
            )),
            ["created"] => events(texts(
                "created",
                CREATED
                    .lock()
                    .expect("lock poisoned")
                    .iter()
                    .map(|name| Some(name.clone()))
                    .collect(),
            )),
            ["connecting"] => events(texts(
                "connecting",
                vec![Some(CONNECTING.load(Ordering::SeqCst).to_string())],
            )),
            ["open"] => events(texts(
                "open",
                vec![Some(OPEN.load(Ordering::SeqCst).to_string())],
            )),
            ["inserted"] => {
                let rows = INSERTED
                    .lock()
                    .expect("lock poisoned")
                    .iter()
                    .map(|(target, row)| Some(format!("{target} {row:?}")))
                    .collect();
                events(texts("inserted", rows))
            }
            // The server ends the connection.
            ["drop-connection"] => {
                self.closed.store(true, Ordering::SeqCst);
                Err(DriverError::Connection(
                    "the server closed the connection".into(),
                ))
            }
            // The connection is found closed while nothing is asked of it.
            ["close-later"] => {
                let closed = self.closed.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    closed.store(true, Ordering::SeqCst);
                });
                events(vec![Ok(ExecEvent::Done {
                    rows_affected: None,
                })])
            }
            _ => Err(other(&format!("the made-up engine does not know: {sql}"))),
        }
    }

    async fn execute_plan(&self, sql: &str, mode: PlanMode) -> Result<EventStream> {
        events(texts("plan", vec![Some(format!("{sql} as {mode:?}"))]))
    }

    async fn execute_bound(
        &self,
        sql: &str,
        params: Vec<String>,
        _opts: &ExecOptions,
    ) -> Result<EventStream> {
        let mut rows = vec![Some(sql.to_string())];
        rows.extend(params.into_iter().map(Some));
        events(texts("bound", rows))
    }

    async fn execute_request(
        &self,
        request: serde_json::Value,
        opts: &ExecOptions,
    ) -> Result<EventStream> {
        // The made-up engine's requests: `{"run": "<word>"}` does what that word does as a
        // statement (so the conformance set can check requests), and anything else says what
        // it was given.
        if let Some(word) = request.get("run").and_then(|run| run.as_str()) {
            return self.execute(word, opts).await;
        }
        events(texts("request", vec![Some(request.to_string())]))
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(Cancel {
            cancelled: self.cancelled.clone(),
            in_flight: self.in_flight.clone(),
        })
    }

    async fn server_version(&self) -> Result<String> {
        Ok("made-up 1.0".into())
    }

    async fn keep_alive(&self) -> Result<KeepAlive> {
        Ok(KeepAlive::InTransaction)
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        Ok(TransactionState::Failed)
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    async fn current_database(&self) -> Result<Option<String>> {
        Ok(Some("made".into()))
    }

    async fn select_database(&self, database: &str) -> Result<()> {
        match database {
            "made" => Ok(()),
            other_database => Err(other(&format!("no database {other_database}"))),
        }
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(vec!["made".into(), "up".into()])
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        Ok(vec![
            SchemaInfo {
                name: "main".into(),
                system: false,
            },
            SchemaInfo {
                name: "sys".into(),
                system: true,
            },
        ])
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        Ok(vec![table(schema), view(schema)])
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        if name != "people" {
            return Err(other(&format!("no table {schema}.{name}")));
        }
        Ok(TableInfo {
            object: table(schema),
            columns: vec![ColumnInfo {
                name: "id".into(),
                native_type: "int8".into(),
                nullable: false,
                default: Some("nextval('s')".into()),
                comment: None,
            }],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        })
    }

    // 1.3's kinds (ADR-082): a sink in `main`, a cluster with a replica.
    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        Ok(match schema {
            "main" => vec![SchemaObject {
                schema: "main".into(),
                name: "events_out".into(),
                kind: SchemaObjectKind::Sink,
                detail: None,
                comment: None,
            }],
            _ => Vec::new(),
        })
    }

    async fn list_server_objects(&self) -> Result<Vec<ServerObject>> {
        let object = |name: &str, kind, detail: Option<&str>| ServerObject {
            name: name.into(),
            kind,
            detail: detail.map(String::from),
            system: false,
        };
        Ok(vec![
            object("quickstart", ServerObjectKind::Cluster, Some("25cc")),
            object(
                "r1",
                ServerObjectKind::ClusterReplica,
                Some("cluster quickstart"),
            ),
        ])
    }

    async fn server_object_definition(&self, object: &ServerObject) -> Result<String> {
        match (object.kind, object.name.as_str()) {
            (ServerObjectKind::Cluster, "quickstart") => {
                Ok("CREATE CLUSTER quickstart (SIZE = '25cc');".into())
            }
            _ => Err(other(&format!("no definition for {}", object.name))),
        }
    }

    async fn object_properties(&self, schema: &str, name: &str) -> Result<Vec<ObjectProperty>> {
        if name != "people" {
            return Err(other(&format!("no table {schema}.{name}")));
        }
        Ok(vec![
            ObjectProperty {
                name: "owner".into(),
                value: "app".into(),
            },
            ObjectProperty {
                name: "made_up_flag".into(),
                value: "on".into(),
            },
        ])
    }

    async fn table_partitions(&self, schema: &str, name: &str) -> Result<Option<Partitioning>> {
        if name != "people" {
            return Ok(None);
        }
        Ok(Some(Partitioning {
            method: "range".into(),
            key: "id".into(),
            partitions: vec![
                PartitionInfo {
                    schema: schema.into(),
                    name: "people_low".into(),
                    bound: Some("FOR VALUES FROM (0) TO (100)".into()),
                    estimated_rows: Some(40),
                    subpartitioned: false,
                },
                PartitionInfo {
                    schema: schema.into(),
                    name: "people_rest".into(),
                    bound: None,
                    estimated_rows: None,
                    subpartitioned: true,
                },
            ],
        }))
    }

    async fn object_dependencies(&self, schema: &str, name: &str) -> Result<Option<Dependencies>> {
        if name != "people" {
            return Ok(None);
        }
        Ok(Some(Dependencies {
            uses: vec![DependencyInfo {
                schema: schema.into(),
                name: "s".into(),
                kind: DependencyKind::Sequence,
                detail: Some("id".into()),
            }],
            used_by: vec![DependencyInfo {
                schema: schema.into(),
                name: "adults".into(),
                kind: DependencyKind::View,
                detail: None,
            }],
            untracked: vec![DependencyKind::Trigger],
        }))
    }

    async fn insert_rows(&self, target: &InsertTarget, rows: TextRows) -> Result<u64> {
        let count = rows.len() as u64;
        let name = format!(
            "{}.{}({})",
            target.schema,
            target.table,
            target.columns.join(",")
        );
        let mut inserted = INSERTED.lock().expect("lock poisoned");
        inserted.extend(rows.to_vecs().into_iter().map(|row| (name.clone(), row)));
        Ok(count)
    }
}

fn table(schema: &str) -> ObjectInfo {
    ObjectInfo {
        schema: schema.into(),
        name: "people".into(),
        kind: ObjectKind::Table,
        estimated_rows: Some(42),
        comment: Some("who".into()),
    }
}

fn view(schema: &str) -> ObjectInfo {
    ObjectInfo {
        schema: schema.into(),
        name: "adults".into(),
        kind: ObjectKind::View,
        estimated_rows: None,
        comment: None,
    }
}

fn other(message: &str) -> DriverError {
    DriverError::Other(message.to_string())
}

fn database_error() -> DriverError {
    DriverError::Database {
        code: Some("42P01".into()),
        message: "relation \"t\" does not exist".into(),
        detail: Some("it never did".into()),
        hint: Some("create it".into()),
        position: Some(15),
    }
}
