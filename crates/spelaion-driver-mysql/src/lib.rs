//! MySQL and MariaDB driver for SpelaionDB (ADR-005, ADR-006).
//!
//! The connection lives in its own task (an actor) and the rest of the program talks to it
//! through channels, so a long query never blocks anything else. Statements use the text
//! protocol: values arrive exactly as the server writes them and [`decode`] turns them into
//! Arrow columns. Cancelling sends `KILL QUERY` over a second connection, which also goes
//! through the SSH tunnel when there is one.

mod decode;
use spelaion_dialect::mysql as dialect;
mod introspect;
mod objects;

use std::sync::Arc;
use std::time::Duration;

use arrow::array::RecordBatch;
use async_trait::async_trait;
use futures_util::StreamExt;
use mysql_async::prelude::Queryable;
use mysql_async::{ClientIdentity, Conn, Opts, OptsBuilder, SslOpts};
use spelaion_driver_api::{
    CancelHandle, Capabilities, ColumnMeta, ConnectParams, Connection, ContextDialect,
    DatabaseContext, Dialect, Driver, DriverError, EventStream, ExecEvent, ExecOptions,
    InsertTarget, KeepAlive, LikeFilter, NameFilter, Notice, ObjectInfo, ReferencingKey, Result,
    ResultSchema, SchemaColumn, SchemaInfo, SchemaObject, ServerObject, TableInfo, TlsMode,
    TransactionState,
};
use tokio::sync::{mpsc, oneshot};

pub use dialect::MysqlDialect;

/// MySQL and MariaDB speak the same protocol; the flavor only changes the engine name and a
/// few catalog details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Mysql,
    Mariadb,
}

pub struct MysqlDriver {
    pub flavor: Flavor,
}

impl MysqlDriver {
    pub fn mysql() -> Self {
        Self {
            flavor: Flavor::Mysql,
        }
    }

    pub fn mariadb() -> Self {
        Self {
            flavor: Flavor::Mariadb,
        }
    }
}

/// Server error "Query execution was interrupted" (after `KILL QUERY`).
const ER_QUERY_INTERRUPTED: u16 = 1317;

fn opts(params: &ConnectParams, tls: Option<SslOpts>) -> Opts {
    // Through a tunnel: connect to its local end, but TLS verifies the real name.
    let (address, port) = match params.connect_addr {
        Some(addr) => (addr.ip().to_string(), addr.port()),
        None => (params.host.clone(), params.port),
    };
    let tls = tls.map(|tls| match params.connect_addr {
        Some(_) => tls.with_danger_tls_hostname_override(Some(params.host.clone())),
        None => tls,
    });
    OptsBuilder::default()
        .ip_or_hostname(address)
        .tcp_port(port)
        .user(Some(params.user.clone()))
        .pass(params.password.as_ref().map(|p| p.expose().to_string()))
        .db_name((!params.database.is_empty()).then(|| params.database.clone()))
        // Always TCP: a local socket would bypass the host the user configured.
        .prefer_socket(false)
        // Affected rows count the rows matched, as other engines do: an UPDATE that leaves a
        // value as it was still reports its row (ADR-014 checks one row per statement).
        .client_found_rows(true)
        // TCP keepalive after a minute idle, so a NAT in front of the server does not drop a
        // direct connection (ADR-028).
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .ssl_opts(tls)
        .into()
}

/// libpq-style modes mapped to MySQL TLS options. `Prefer` is handled by the caller (it
/// retries without TLS only if the server does not support it).
fn tls_opts(params: &ConnectParams) -> Option<SslOpts> {
    let roots = |opts: SslOpts| match params.ca_certificate.as_deref() {
        Some(ca) => opts
            .with_root_certs(vec![ca.to_path_buf().into()])
            .with_disable_built_in_roots(true),
        None => opts,
    };
    // The certificate a client presents to log in without a password (ADR-022).
    let identity = match (&params.client_certificate, &params.client_key) {
        (Some(cert), Some(key)) => {
            Some(ClientIdentity::new(cert.clone().into(), key.clone().into()))
        }
        _ => None,
    };
    let base = SslOpts::default().with_client_identity(identity);
    match params.tls {
        TlsMode::Disable => None,
        TlsMode::Prefer | TlsMode::Require => Some(
            base.with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
        TlsMode::VerifyCa => Some(roots(base).with_danger_skip_domain_validation(true)),
        TlsMode::VerifyFull => Some(roots(base)),
    }
}

async fn open(opts: Opts, timeout: Duration) -> std::result::Result<Conn, mysql_async::Error> {
    match tokio::time::timeout(timeout, Conn::new(opts)).await {
        Ok(result) => result,
        Err(_) => Err(mysql_async::Error::Io(mysql_async::IoError::Io(
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no response in {} s", timeout.as_secs()),
            ),
        ))),
    }
}

#[async_trait]
impl Driver for MysqlDriver {
    fn engine(&self) -> &'static str {
        match self.flavor {
            Flavor::Mysql => "mysql",
            Flavor::Mariadb => "mariadb",
        }
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(MysqlDialect)
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        // mysql_async builds its TLS with the process's rustls provider: ring, like the other
        // drivers (with more than one compiled in, rustls would not choose).
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut opts = opts(params, tls_opts(params));
        let timeout = params.connect_timeout();
        let conn = match open(opts.clone(), timeout).await {
            // `prefer`: without TLS only if the server does not offer it.
            Err(mysql_async::Error::Driver(
                mysql_async::DriverError::NoClientSslFlagFromServer,
            )) if params.tls == TlsMode::Prefer => {
                opts = self::opts(params, None);
                open(opts.clone(), timeout).await
            }
            other => other,
        }
        .map_err(map_error)?;
        let cancel = Arc::new(MysqlCancel {
            opts,
            timeout,
            id: conn.id(),
        });
        let (requests, receiver) = mpsc::channel(8);
        tokio::spawn(actor(conn, receiver, Some(params.database.clone())));
        Ok(Box::new(MysqlConnection {
            requests,
            cancel,
            flavor: self.flavor,
            capabilities: Capabilities {
                transactions: true,
                cancel: true,
                explain: true,
                editable_results: false,
                schemas: true,
                insert_rows: true,
                bound_params: true,
            },
        }))
    }
}

type Rows = Vec<Vec<Option<String>>>;

enum Request {
    DatabaseContext {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    CurrentDatabase {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    Execute {
        sql: String,
        /// Bound to `@spelaion_p1`, `@spelaion_p2`… before the statement runs (ADR-043).
        params: Vec<String>,
        batch_size: usize,
        events: mpsc::Sender<Result<ExecEvent>>,
    },
    /// Small catalog queries: every value as text (parameters go through a prepared statement).
    Rows {
        sql: String,
        params: Vec<String>,
        reply: oneshot::Sender<Result<Rows>>,
    },
    /// COM_PING unless a transaction is open (ADR-028).
    KeepAlive {
        reply: oneshot::Sender<Result<KeepAlive>>,
    },
    /// The transaction flag of the last OK packet: nothing is sent (ADR-031).
    TransactionState {
        reply: oneshot::Sender<Result<TransactionState>>,
    },
    /// A multi-row INSERT of an import (ADR-038): `?` placeholders, values bound.
    Insert {
        sql: String,
        values: Vec<Option<String>>,
        reply: oneshot::Sender<Result<u64>>,
    },
}

/// Owns the connection until every handle is dropped.
async fn actor(mut conn: Conn, mut requests: mpsc::Receiver<Request>, database: Option<String>) {
    let mut context = DatabaseContext::new(ContextDialect::Mysql, database);
    // An error is an ERR packet, which carries no status: the last OK packet may be older than
    // a deadlock that rolled the transaction back.
    let mut status_stale = false;
    while let Some(request) = requests.recv().await {
        match request {
            Request::Execute {
                sql,
                params,
                batch_size,
                events,
            } => {
                let outcome = if params.is_empty() {
                    execute(&mut conn, &sql, batch_size, &events).await
                } else {
                    execute_bound(&mut conn, &sql, params, batch_size, &events).await
                };
                let complete = matches!(outcome, ExecutionOutcome::Complete);
                context.observe(&sql, complete);
                if matches!(outcome, ExecutionOutcome::Uncertain) {
                    context.refresh(None);
                }
                status_stale = !complete;
            }
            Request::Rows { sql, params, reply } => {
                let result = rows(&mut conn, &sql, params).await;
                context.observe(&sql, result.is_ok());
                if result.is_err() && !matches!(&result, Err(DriverError::Database { .. })) {
                    context.refresh(None);
                }
                status_stale = result.is_err();
                let _ = reply.send(result);
            }
            Request::DatabaseContext { reply } => {
                let _ = reply.send(Ok(context.snapshot()));
            }
            Request::CurrentDatabase { reply } => {
                let result = rows(&mut conn, "SELECT DATABASE()", vec![])
                    .await
                    .map(|rows| {
                        rows.into_iter()
                            .next()
                            .and_then(|row| row.into_iter().next().flatten())
                    });
                context.refresh(result.as_ref().ok().cloned().flatten());
                status_stale = result.is_err();
                let _ = reply.send(result);
            }
            Request::KeepAlive { reply } => {
                let _ = reply.send(keep_alive(&mut conn).await);
            }
            Request::Insert { sql, values, reply } => {
                let params: Vec<mysql_async::Value> = values
                    .into_iter()
                    .map(|v| v.map_or(mysql_async::Value::NULL, |t| t.into_bytes().into()))
                    .collect();
                let result = conn
                    .exec_drop(sql.as_str(), mysql_async::Params::Positional(params))
                    .await
                    .map(|()| conn.affected_rows())
                    .map_err(map_error);
                status_stale = result.is_err();
                let _ = reply.send(result);
            }
            Request::TransactionState { reply } => {
                // `DO 0` touches no table, so it opens nothing, and its OK packet is fresh.
                let refreshed = if status_stale {
                    conn.query_drop("DO 0").await.map_err(map_error)
                } else {
                    Ok(())
                };
                status_stale = refreshed.is_err();
                let _ = reply.send(refreshed.map(|()| {
                    if in_transaction(&conn) {
                        TransactionState::Open
                    } else {
                        TransactionState::Idle
                    }
                }));
            }
        }
    }
    let _ = conn.disconnect().await;
}

/// The server reports an open transaction in the status of every OK packet: with one open,
/// nothing is sent at all.
async fn keep_alive(conn: &mut Conn) -> Result<KeepAlive> {
    if in_transaction(conn) {
        return Ok(KeepAlive::InTransaction);
    }
    conn.ping().await.map_err(map_error)?;
    Ok(KeepAlive::Alive)
}

/// The server's transaction flag in the status of the last OK packet (every statement ends
/// with one). MySQL has no failed state: after an error the transaction stays open, or the
/// server rolled it back (a deadlock) and says so in the flag.
fn in_transaction(conn: &Conn) -> bool {
    conn.last_ok_packet().is_some_and(|ok| {
        ok.status_flags()
            .contains(mysql_async::consts::StatusFlags::SERVER_STATUS_IN_TRANS)
    })
}

async fn rows(conn: &mut Conn, sql: &str, params: Vec<String>) -> Result<Rows> {
    let text = |value: mysql_async::Value| match value {
        mysql_async::Value::NULL => None,
        mysql_async::Value::Bytes(bytes) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        other => Some(other.as_sql(true).trim_matches('\'').to_string()),
    };
    let result: Vec<mysql_async::Row> = if params.is_empty() {
        conn.query(sql).await
    } else {
        conn.exec(sql, params).await
    }
    .map_err(|e| map_error_in(e, sql))?;
    Ok(result
        .into_iter()
        .map(|row| row.unwrap().into_iter().map(text).collect())
        .collect())
}

/// Runs one statement and streams its first result set. If the receiver goes away (tab
/// closed), reading stops; the connection discards the rest before the next statement.
/// An interrupted response may have changed context before the client lost its result.
enum ExecutionOutcome {
    Complete,
    Failed,
    Uncertain,
}

async fn execute(
    conn: &mut Conn,
    sql: &str,
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> ExecutionOutcome {
    let send = |event| async move { events.send(event).await.is_ok() };
    let mut result = match conn.query_iter(sql).await {
        Ok(result) => result,
        Err(e) => {
            let definite = matches!(&e,mysql_async::Error::Server(server) if server.code!=ER_QUERY_INTERRUPTED);
            send(Err(map_error_in(e, sql))).await;
            return if definite {
                ExecutionOutcome::Failed
            } else {
                ExecutionOutcome::Uncertain
            };
        }
    };
    let columns = result.columns().unwrap_or_else(|| Arc::from(Vec::new()));
    if !columns.is_empty() {
        // Every column's type comes from its metadata: no value decides it, so a value far down
        // the result can never fail it (temporal columns are text, see `decode::logical_type`).
        let metas: Vec<ColumnMeta> = columns
            .iter()
            .map(|column| decode::column_meta(column, decode::logical_type(column)))
            .collect();
        let schema = Arc::new(ResultSchema::new(metas));
        if !send(Ok(ExecEvent::Columns(schema.clone()))).await {
            return ExecutionOutcome::Uncertain;
        }
        let new_builders = || {
            schema
                .columns
                .iter()
                .map(|c| decode::ColumnBuilder::new(c.logical, batch_size))
                .collect::<Vec<_>>()
        };
        let mut builders = new_builders();
        let mut row_number: u64 = 0;
        let mut pending = 0usize;
        loop {
            let row = match result.next().await {
                Ok(Some(row)) => raw_values(row),
                Ok(None) => break,
                Err(e) => {
                    send(Err(map_error_in(e, sql))).await;
                    return ExecutionOutcome::Uncertain;
                }
            };
            row_number += 1;
            for (i, builder) in builders.iter_mut().enumerate() {
                if let Err(mismatch) = builder.append(row[i].as_deref()) {
                    send(Err(decode::mismatch_error(
                        &schema.columns[i],
                        row_number,
                        mismatch,
                    )))
                    .await;
                    return ExecutionOutcome::Uncertain;
                }
            }
            pending += 1;
            if pending == batch_size {
                let batch = finish(&schema, &mut builders);
                if !send(batch.map(ExecEvent::Batch)).await {
                    return ExecutionOutcome::Uncertain;
                }
                pending = 0;
            }
        }
        if pending > 0 && !send(finish(&schema, &mut builders).map(ExecEvent::Batch)).await {
            return ExecutionOutcome::Uncertain;
        }
    }
    let affected = result.affected_rows();
    // Only the first result set is shown; the rest (from procedures) is discarded.
    if let Err(e) = result.drop_result().await {
        send(Err(map_error_in(e, sql))).await;
        return ExecutionOutcome::Uncertain;
    }
    let rows_affected = (columns.is_empty() && reports_row_count(sql)).then_some(affected);
    // The server counts the statement's warnings and notes in its last OK packet: only when
    // there are some does it cost a round trip to read them (ADR-035).
    if conn.get_warnings() > 0 && dialect::first_word(sql) != "show" {
        for notice in warnings(conn).await {
            if !send(Ok(ExecEvent::Notice(notice))).await {
                return ExecutionOutcome::Complete;
            }
        }
    }
    send(Ok(ExecEvent::Done { rows_affected })).await;
    ExecutionOutcome::Complete
}

/// Binds `params` to the user variables `@spelaion_p1`, `@spelaion_p2`… with a prepared `SET`
/// (the values travel as parameters, never as SQL), runs `sql` with the text protocol so its
/// values decode as every other result's, and clears the variables again.
async fn execute_bound(
    conn: &mut Conn,
    sql: &str,
    params: Vec<String>,
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> ExecutionOutcome {
    let names: Vec<String> = (1..=params.len())
        .map(|n| spelaion_driver_api::bound_placeholder("mysql", n))
        .collect();
    let set = names
        .iter()
        .map(|name| format!("{name} = ?"))
        .collect::<Vec<_>>()
        .join(", ");
    let values: Vec<mysql_async::Value> = params
        .into_iter()
        .map(|v| mysql_async::Value::Bytes(v.into_bytes()))
        .collect();
    if let Err(e) = conn
        .exec_drop(
            format!("SET {set}"),
            mysql_async::Params::Positional(values),
        )
        .await
    {
        let _ = events.send(Err(map_error(e))).await;
        return ExecutionOutcome::Failed;
    }
    let outcome = execute(conn, sql, batch_size, events).await;
    let clear = names
        .iter()
        .map(|name| format!("{name} = NULL"))
        .collect::<Vec<_>>()
        .join(", ");
    if conn.query_drop(format!("SET {clear}")).await.is_err() {
        return ExecutionOutcome::Uncertain;
    }
    outcome
}

/// Most warnings read after one statement.
const MAX_WARNINGS: usize = 100;

/// The last statement's warnings and notes (`SHOW WARNINGS`), as notices: "Warning: Data
/// truncated for column 'v' at row 1 (1265)". A failure to read them is not the statement's.
async fn warnings(conn: &mut Conn) -> Vec<Notice> {
    let rows: Vec<(String, u32, String)> = match conn
        .query(format!("SHOW WARNINGS LIMIT {MAX_WARNINGS}"))
        .await
    {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    rows.into_iter()
        .map(|(level, code, message)| Notice {
            severity: level.to_uppercase(),
            message: format!("{message} ({code})"),
        })
        .collect()
}

fn raw_values(row: mysql_async::Row) -> Vec<Option<Vec<u8>>> {
    row.unwrap()
        .into_iter()
        .map(|value| match value {
            mysql_async::Value::NULL => None,
            mysql_async::Value::Bytes(bytes) => Some(bytes),
            // The text protocol only sends bytes; any other variant is written as SQL text.
            other => Some(other.as_sql(true).into_bytes()),
        })
        .collect()
}

fn finish(schema: &ResultSchema, builders: &mut [decode::ColumnBuilder]) -> Result<RecordBatch> {
    let columns = builders
        .iter_mut()
        .map(decode::ColumnBuilder::finish)
        .collect();
    RecordBatch::try_new(schema.arrow.clone(), columns)
        .map_err(|e| DriverError::Other(e.to_string()))
}

/// MySQL reports 0 affected rows for DDL too; only these statements have a real count.
fn reports_row_count(sql: &str) -> bool {
    const COUNTED: &[&str] = &["insert", "update", "delete", "replace", "load", "call"];
    let first = dialect::first_word(sql);
    COUNTED.iter().any(|k| first.eq_ignore_ascii_case(k))
}

/// A prepared statement takes at most 65,535 placeholders.
const MAX_PLACEHOLDERS: usize = 65_535;
/// Rows per INSERT of an import.
const MAX_INSERT_ROWS: usize = 1_000;
/// About this much data per INSERT, well under the default `max_allowed_packet` (64 MiB).
const MAX_INSERT_BYTES: usize = 4 * 1024 * 1024;

/// An error before any result (syntax, permissions…) is returned directly.
async fn first_event(
    mut stream: futures_util::stream::BoxStream<'static, Result<ExecEvent>>,
) -> Result<EventStream> {
    match stream.next().await {
        Some(Err(e)) => Err(e),
        Some(Ok(first)) => Ok(Box::pin(
            futures_util::stream::once(async { Ok(first) }).chain(stream),
        )),
        None => Err(closed()),
    }
}

struct MysqlConnection {
    requests: mpsc::Sender<Request>,
    cancel: Arc<MysqlCancel>,
    flavor: Flavor,
    capabilities: Capabilities,
}

fn closed() -> DriverError {
    DriverError::Connection("the connection is closed".into())
}

impl MysqlConnection {
    pub(crate) async fn rows(&self, sql: &str, params: Vec<String>) -> Result<Rows> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Rows {
                sql: sql.to_string(),
                params,
                reply,
            })
            .await
            .map_err(|_| closed())?;
        response.await.map_err(|_| closed())?
    }
}

#[async_trait]
impl Connection for MysqlConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        let (events, receiver) = mpsc::channel(4);
        self.requests
            .send(Request::Execute {
                sql: sql.to_string(),
                params: Vec::new(),
                batch_size: opts.batch_size.max(1),
                events,
            })
            .await
            .map_err(|_| closed())?;
        first_event(tokio_stream_from(receiver)).await
    }

    async fn execute_bound(
        &self,
        sql: &str,
        params: Vec<String>,
        opts: &ExecOptions,
    ) -> Result<EventStream> {
        let (events, receiver) = mpsc::channel(4);
        self.requests
            .send(Request::Execute {
                sql: sql.to_string(),
                params,
                batch_size: opts.batch_size.max(1),
                events,
            })
            .await
            .map_err(|_| closed())?;
        first_event(tokio_stream_from(receiver)).await
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    async fn keep_alive(&self) -> Result<KeepAlive> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::KeepAlive { reply })
            .await
            .map_err(|_| DriverError::Connection("the connection is closed".into()))?;
        response
            .await
            .map_err(|_| DriverError::Connection("the connection is closed".into()))?
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::TransactionState { reply })
            .await
            .map_err(|_| DriverError::Connection("the connection is closed".into()))?;
        response
            .await
            .map_err(|_| DriverError::Connection("the connection is closed".into()))?
    }

    fn is_closed(&self) -> bool {
        self.requests.is_closed()
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self.rows("select version()", vec![]).await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next().flatten())
            .unwrap_or_default())
    }

    async fn current_database(&self) -> Result<Option<String>> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::CurrentDatabase { reply })
            .await
            .map_err(|_| closed())?;
        response.await.map_err(|_| closed())?
    }

    async fn database_context(&self) -> Result<Option<String>> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::DatabaseContext { reply })
            .await
            .map_err(|_| closed())?;
        response.await.map_err(|_| closed())?
    }

    /// MySQL calls its databases schemas: the same list as the explorer.
    /// Prepared multi-row INSERTs with `?` placeholders. `LOAD DATA LOCAL INFILE` is not used:
    /// it needs `local_infile` on the server and the client, off by default (ADR-038).
    async fn insert_rows(
        &self,
        target: &InsertTarget,
        rows: Vec<Vec<Option<String>>>,
    ) -> Result<u64> {
        target.check_rows(&rows)?;
        let mut inserted = 0;
        for chunk in
            target.statement_chunks(rows, MAX_PLACEHOLDERS, MAX_INSERT_ROWS, MAX_INSERT_BYTES)
        {
            let sql = target.insert_sql(&dialect::MysqlDialect, chunk.len(), |_| "?".into());
            let (reply, response) = oneshot::channel();
            self.requests
                .send(Request::Insert {
                    sql,
                    values: chunk.into_iter().flatten().collect(),
                    reply,
                })
                .await
                .map_err(|_| closed())?;
            inserted += response.await.map_err(|_| closed())??;
        }
        Ok(inserted)
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        Ok(introspect::list_schemas(self, &LikeFilter::default())
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect())
    }

    async fn select_database(&self, database: &str) -> Result<()> {
        let sql = format!("USE {}", MysqlDialect.quote_identifier(database));
        self.rows(&sql, vec![]).await?;
        Ok(())
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        introspect::list_schemas(self, &LikeFilter::default()).await
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        introspect::list_objects(self, schema, &LikeFilter::default()).await
    }

    // Narrowed in the catalog query, then matched exactly (ADR-025).
    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let mut schemas = introspect::list_schemas(self, &filter.server_side()).await?;
        schemas.retain(|s| filter.matches(&s.name));
        Ok(schemas)
    }

    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let mut objects = introspect::list_objects(self, schema, &filter.server_side()).await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let mut objects = objects::list_schema_objects(self, schema, &filter.server_side()).await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        introspect::describe_table(self, self.flavor, schema, name).await
    }

    async fn list_columns(&self, schema: &str) -> Result<Vec<SchemaColumn>> {
        introspect::list_columns(self, schema).await
    }

    async fn list_referencing_keys(&self, schema: &str, name: &str) -> Result<Vec<ReferencingKey>> {
        introspect::list_referencing_keys(self, schema, name).await
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        objects::list_schema_objects(self, schema, &LikeFilter::default()).await
    }

    async fn list_server_objects(&self) -> Result<Vec<ServerObject>> {
        objects::list_server_objects(self, self.flavor).await
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        objects::object_definition(self, object).await
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        objects::relation_definition(self, schema, name).await
    }

    async fn procedure_call(&self, object: &SchemaObject) -> Result<String> {
        objects::procedure_call(self, object).await
    }
}

fn tokio_stream_from<T: Send + 'static>(
    mut receiver: mpsc::Receiver<T>,
) -> futures_util::stream::BoxStream<'static, T> {
    Box::pin(futures_util::stream::poll_fn(move |cx| {
        receiver.poll_recv(cx)
    }))
}

/// `KILL QUERY` from a second connection with the same options (and the same tunnel).
struct MysqlCancel {
    opts: Opts,
    timeout: Duration,
    id: u32,
}

#[async_trait]
impl CancelHandle for MysqlCancel {
    async fn cancel(&self) -> Result<()> {
        let mut conn = open(self.opts.clone(), self.timeout)
            .await
            .map_err(map_error)?;
        let result = conn.query_drop(format!("KILL QUERY {}", self.id)).await;
        let _ = conn.disconnect().await;
        result.map_err(map_error)
    }
}

/// Server errors keep their SQLSTATE as `code` (`28000` = access denied, like PostgreSQL)
/// and the MySQL error number in `detail`.
pub(crate) fn map_error(e: mysql_async::Error) -> DriverError {
    match e {
        mysql_async::Error::Server(server) if server.code == ER_QUERY_INTERRUPTED => {
            DriverError::Cancelled
        }
        mysql_async::Error::Server(server) => DriverError::Database {
            code: Some(server.state.clone()),
            message: server.message.clone(),
            detail: Some(format!("MySQL error {}", server.code)),
            hint: None,
            position: None,
        },
        other => {
            let mut message = other.to_string();
            let mut source = std::error::Error::source(&other);
            while let Some(cause) = source {
                message = format!("{message}: {cause}");
                source = cause.source();
            }
            DriverError::Connection(message)
        }
    }
}

/// Like [`map_error`], and syntax errors get a position: MySQL only says
/// "near '…' at line N", so the quoted text is looked up in the statement.
fn map_error_in(e: mysql_async::Error, sql: &str) -> DriverError {
    match map_error(e) {
        DriverError::Database {
            code,
            message,
            detail,
            hint,
            position: None,
        } => {
            let position = message
                .split_once("near '")
                .and_then(|(_, rest)| rest.rsplit_once("' at line"))
                .map(|(snippet, _)| snippet)
                .filter(|snippet| !snippet.is_empty())
                .and_then(|snippet| sql.find(snippet))
                .and_then(|byte| u32::try_from(sql[..byte].chars().count() + 1).ok());
            DriverError::Database {
                code,
                message,
                detail,
                hint,
                position,
            }
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_counted_statements_report_rows() {
        for sql in [
            "insert into t values (1)",
            "/* x */ UPDATE t set a = 1",
            "replace into t values (1)",
        ] {
            assert!(reports_row_count(sql), "{sql}");
        }
        for sql in [
            "create table t (a int)",
            "set names utf8mb4",
            "begin",
            "select 1",
        ] {
            assert!(!reports_row_count(sql), "{sql}");
        }
    }

    #[test]
    fn syntax_errors_point_at_the_quoted_text() {
        let sql = "select id\nfrom orders\nwher id = 1";
        let error = map_error_in(
            mysql_async::Error::Server(mysql_async::ServerError {
                code: 1064,
                message: "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'wher id = 1' at line 3".into(),
                state: "42000".into(),
            }),
            sql,
        );
        let DriverError::Database { position, code, .. } = error else {
            panic!("{error:?}");
        };
        assert_eq!(position, Some(23));
        assert_eq!(code.as_deref(), Some("42000"));
    }
}
