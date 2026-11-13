//! A made-up engine, served as a plugin: what the tests of the plugin protocol talk to.
//!
//! Its "SQL" is a handful of words, each of which makes the driver do one thing a real one
//! does (several result sets, a statement that waits to be cancelled, an error with every
//! field) or one thing a plugin must survive or must never get away with (dying mid-query,
//! writing to standard output, repeating the password it was given).
//!
//! The program's file name chooses what it is: with `other-version` in it, a plugin that
//! speaks another major version of the protocol; with `not-a-plugin`, a program that prints
//! text; anything else, the plugin.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use async_trait::async_trait;
use spelaion_driver_api::{
    CancelHandle, Capabilities, ColumnInfo, ColumnMeta, ConnectParams, Connection, Driver,
    DriverError, EventStream, ExecEvent, ExecOptions, InsertTarget, KeepAlive, LogicalType, Notice,
    ObjectInfo, ObjectKind, PlanMode, Result, ResultSchema, SchemaInfo, TableInfo, TextRows,
    TransactionState,
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
        OPEN.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(MadeConnection {
            engine: self.0,
            password,
            params: format!(
                "{}:{} user={} tls={:?} addr={:?} read_only={} app={:?} timeout={:?} ca={:?}",
                params.host,
                params.port,
                params.user,
                params.tls,
                params.connect_addr,
                params.read_only,
                params.application_name,
                params.connect_timeout,
                params.ca_certificate,
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
            closed: Arc::new(AtomicBool::new(false)),
        }))
    }
}

struct MadeConnection {
    engine: &'static str,
    password: String,
    params: String,
    capabilities: Capabilities,
    cancelled: Arc<Notify>,
    closed: Arc<AtomicBool>,
}

impl Drop for MadeConnection {
    fn drop(&mut self) {
        OPEN.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Cancel(Arc<Notify>);

#[async_trait]
impl CancelHandle for Cancel {
    async fn cancel(&self) -> Result<()> {
        self.0.notify_one();
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

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(Cancel(self.cancelled.clone()))
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
