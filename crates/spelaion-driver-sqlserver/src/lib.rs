//! SQL Server driver for SpelaionDB (ADR-005, ADR-017).
//!
//! The connection lives in its own task (an actor) and the rest of the program talks to it
//! through channels. Statements go as T-SQL batches: a batch may return several result sets
//! (procedures, `sp_help`), and each one becomes a result. Cancelling sends a TDS Attention,
//! which stops the running batch and keeps the session (`KILL` would end it).

mod ddl;
mod decode;
mod dialect;
mod introspect;
mod objects;
mod plan;

use std::sync::Arc;
use std::time::Duration;

use arrow::array::RecordBatch;
use async_trait::async_trait;
use futures_util::TryStreamExt;
use spelaion_driver_api::{
    CancelHandle, Capabilities, ConnectParams, Connection, ContextDialect, DatabaseContext,
    Dialect, Driver, DriverError, EventStream, ExecEvent, ExecOptions, InsertTarget, KeepAlive,
    LikeFilter, NameFilter, Notice, ObjectInfo, PlanMode, ReferencingKey, Result, ResultSchema,
    SchemaColumn, SchemaInfo, SchemaObject, ServerObject, TableInfo, TlsMode, TransactionState,
};
use tiberius::{AuthMethod, Client, ColumnType, Config, EncryptionLevel, QueryItem};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

pub use dialect::SqlServerDialect;

/// SQL Server's "chosen as deadlock victim" error.
const DEADLOCK_VICTIM: &str = "1205";

type SqlClient = Client<Compat<TcpStream>>;

#[derive(Debug, Default, Clone)]
pub struct SqlServerDriver;

fn config(params: &ConnectParams) -> Result<Config> {
    let mut config = Config::new();
    // The real name: TLS checks it even when the TCP connection goes to a tunnel's local end.
    config.host(&params.host);
    config.port(params.port);
    if !params.database.is_empty() {
        config.database(&params.database);
    }
    config.application_name(params.application_name.as_deref().unwrap_or("SpelaionDB"));
    if params.user.is_empty() {
        return Err(DriverError::Connection(
            "SQL Server needs a user: Windows authentication is not supported yet".into(),
        ));
    }
    let password = params
        .password
        .as_ref()
        .map(|p| p.expose().to_string())
        .unwrap_or_default();
    config.authentication(AuthMethod::sql_server(&params.user, password));
    // libpq-style modes. `disable` still encrypts the login (without checking the
    // certificate), as every SQL Server client does, so the password never travels in the
    // clear. `verify-ca` also checks the name: the TLS library cannot check only the authority.
    match params.tls {
        TlsMode::Disable => {
            config.encryption(EncryptionLevel::Off);
            config.trust_cert();
        }
        TlsMode::Prefer => {
            config.encryption(EncryptionLevel::On);
            config.trust_cert();
        }
        TlsMode::Require => {
            config.encryption(EncryptionLevel::Required);
            config.trust_cert();
        }
        TlsMode::VerifyCa | TlsMode::VerifyFull => {
            config.encryption(EncryptionLevel::Required);
            if let Some(ca) = &params.ca_certificate {
                config.trust_cert_ca(ca.display().to_string());
            }
        }
    }
    Ok(config)
}

async fn open(params: &ConnectParams) -> Result<SqlClient> {
    // rustls needs one crypto provider for the process: ring, like the other drivers.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = config(params)?;
    let address = match params.connect_addr {
        Some(addr) => addr.to_string(),
        None => format!("{}:{}", params.host, params.port),
    };
    let connect = async {
        let tcp = TcpStream::connect(&address)
            .await
            .map_err(|e| DriverError::Connection(format!("could not reach {address}: {e}")))?;
        let _ = tcp.set_nodelay(true);
        // TCP keepalive after a minute idle, so a NAT in front of the server does not drop a
        // direct connection (ADR-028).
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(60))
            .with_interval(Duration::from_secs(10));
        let _ = socket2::SockRef::from(&tcp).set_tcp_keepalive(&keepalive);
        Client::connect(config, tcp.compat_write())
            .await
            .map_err(|e| match e {
                // Azure SQL may send the login elsewhere; not through a tunnel.
                tiberius::error::Error::Routing { host, port } => DriverError::Connection(format!(
                    "the server redirects to {host}:{port}; connect to that address instead"
                )),
                other => map_error(other, None),
            })
    };
    let timeout = params.connect_timeout();
    tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| DriverError::Connection(format!("no response in {} s", timeout.as_secs())))?
}

#[async_trait]
impl Driver for SqlServerDriver {
    fn engine(&self) -> &'static str {
        "sqlserver"
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(SqlServerDialect)
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let client = open(params).await?;
        let (cancels, cancelled) = watch::channel(0u64);
        let (requests, receiver) = mpsc::channel(8);
        tokio::spawn(actor(
            client,
            receiver,
            cancelled,
            Some(params.database.clone()),
        ));
        Ok(Box::new(SqlServerConnection {
            requests,
            cancel: Arc::new(SqlServerCancel(cancels)),
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

pub(crate) type Rows = Vec<Vec<Option<String>>>;

enum Request {
    DatabaseContext {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    CurrentDatabase {
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    Execute {
        sql: String,
        /// Bound as `@P1`, `@P2`… through `sp_executesql` (ADR-043); none for a plain batch.
        params: Vec<String>,
        batch_size: usize,
        events: mpsc::Sender<Result<ExecEvent>>,
    },
    /// The server settings, query and restoration belong to one actor request.
    Plan {
        sql: String,
        mode: PlanMode,
        cancel_epoch: u64,
        events: mpsc::Sender<Result<ExecEvent>>,
    },
    /// Small catalog queries: every value as text, parameters as `@P1`, `@P2`…
    KeepAlive {
        reply: oneshot::Sender<Result<KeepAlive>>,
    },
    Rows {
        sql: String,
        params: Vec<String>,
        reply: oneshot::Sender<Result<Rows>>,
    },
    /// The TDS transaction descriptor, or `XACT_STATE()` when it may be stale (ADR-031).
    TransactionState {
        reply: oneshot::Sender<Result<TransactionState>>,
    },
    /// Rows of an import (ADR-038): a TDS bulk load into a staging table, then one
    /// `INSERT … SELECT`.
    Insert {
        target: InsertTarget,
        rows: Vec<Vec<Option<String>>>,
        reply: oneshot::Sender<Result<u64>>,
    },
}

/// Owns the connection until every handle is dropped.
async fn actor(
    mut client: SqlClient,
    mut requests: mpsc::Receiver<Request>,
    mut cancels: watch::Receiver<u64>,
    database: Option<String>,
) {
    let mut context = DatabaseContext::new(ContextDialect::SqlServer, database);
    // An interrupted or failed response may have hidden transaction ENVCHANGE tokens.
    // Stay conservative until a later user operation completes cleanly.
    let mut transaction_state_unknown = false;
    while let Some(request) = requests.recv().await {
        match request {
            Request::Plan {
                sql,
                mode,
                cancel_epoch,
                events,
            } => {
                let (healthy, attempted) =
                    plan::execute(&mut client, &sql, mode, &events, &mut cancels, cancel_epoch)
                        .await;
                // A failed or interrupted batch can have hidden transaction ENVCHANGE tokens.
                transaction_state_unknown |= attempted;
                if !healthy {
                    // Do not let a queued request run on a session whose SET state is unknown.
                    requests.close();
                    return;
                }
            }
            Request::Execute {
                sql,
                params,
                batch_size,
                events,
            } => {
                // Cancels asked for before this statement do not count.
                let asked = *cancels.borrow_and_update();
                let cancelled = async {
                    while cancels.changed().await.is_ok() {
                        if *cancels.borrow() > asked {
                            return;
                        }
                    }
                    std::future::pending::<()>().await;
                };
                let finished = tokio::select! {
                    outcome = execute(&mut client, &sql, &params, batch_size, &events) => outcome,
                    () = cancelled => ExecutionOutcome::Cancelled,
                };
                transaction_state_unknown = !matches!(finished, ExecutionOutcome::Complete);
                context.observe(&sql, matches!(finished, ExecutionOutcome::Complete));
                if matches!(
                    finished,
                    ExecutionOutcome::Cancelled
                        | ExecutionOutcome::Abandoned
                        | ExecutionOutcome::Failed { definitive: false }
                ) {
                    context.refresh(None);
                }
                if matches!(
                    finished,
                    ExecutionOutcome::Cancelled | ExecutionOutcome::Abandoned
                ) {
                    // The batch's stream is dropped: the Attention stops it on the server and
                    // drains what was left, so the session goes on.
                    let result = client.cancel_query().await;
                    let _ = events
                        .send(Err(match result {
                            Ok(()) => DriverError::Cancelled,
                            Err(e) => map_error(e, None),
                        }))
                        .await;
                }
            }
            Request::Rows { sql, params, reply } => {
                let result = rows(&mut client, &sql, &params).await;
                context.observe(&sql, result.is_ok());
                if result.is_err() && !matches!(&result, Err(DriverError::Database { .. })) {
                    context.refresh(None);
                }
                transaction_state_unknown = result.is_err();
                let _ = reply.send(result);
            }
            Request::DatabaseContext { reply } => {
                let _ = reply.send(Ok(context.snapshot()));
            }
            Request::Insert {
                target,
                rows,
                reply,
            } => {
                let result = bulk_insert(&mut client, &target, rows).await;
                transaction_state_unknown = result.is_err();
                let _ = reply.send(result);
            }
            Request::CurrentDatabase { reply } => {
                // SHOWPLAN can return text/XML instead of executing a SELECT. Only the
                // actual two-column row with this literal is an authoritative observation.
                let result = rows(
                    &mut client,
                    "select db_name(), N'spelaion-context-executed'",
                    &[],
                )
                .await
                .map(|rows| {
                    if rows.len() == 1
                        && rows[0].len() == 2
                        && rows[0][1].as_deref() == Some("spelaion-context-executed")
                    {
                        rows[0][0].clone()
                    } else {
                        None
                    }
                });
                context.refresh(result.as_ref().ok().cloned().flatten());
                transaction_state_unknown = result.is_err();
                let _ = reply.send(result);
            }
            Request::KeepAlive { reply } => {
                let result = match if transaction_state_unknown {
                    Ok(true)
                } else {
                    client.in_transaction().await
                } {
                    Ok(true) => Ok(KeepAlive::InTransaction),
                    Ok(false) => rows(&mut client, "select 1", &[])
                        .await
                        .map(|_| KeepAlive::Alive),
                    Err(error) => Err(map_error(error, None)),
                };
                let _ = reply.send(result);
            }
            Request::TransactionState { reply } => {
                let result = if transaction_state_unknown {
                    // After an error or a cancel: ask the server. -1 is a transaction that
                    // can only be rolled back (`XACT_ABORT`, some errors).
                    let answer = rows(&mut client, "select xact_state()", &[]).await;
                    transaction_state_unknown = answer.is_err();
                    answer.map(|rows| {
                        match rows
                            .first()
                            .and_then(|row| row.first())
                            .and_then(|value| value.as_deref())
                        {
                            Some("0") => TransactionState::Idle,
                            Some("-1") => TransactionState::Failed,
                            _ => TransactionState::Open,
                        }
                    })
                } else {
                    client
                        .in_transaction()
                        .await
                        .map(|open| {
                            if open {
                                TransactionState::Open
                            } else {
                                TransactionState::Idle
                            }
                        })
                        .map_err(|error| map_error(error, None))
                };
                let _ = reply.send(result);
            }
        }
    }
    let _ = client.close().await;
}

/// An import's rows (ADR-038): a TDS bulk load (the protocol's own bulk path) into a temporary
/// table of `nvarchar(max)` columns, whose names are ours, then one `INSERT … SELECT` that lets
/// the server convert each value to its column's type, in the order the rows came. The values
/// are the bulk load's data: they are never SQL text. The staging table lives in the session (and
/// in its transaction: a rollback drops it, and the next batch creates it again).
async fn bulk_insert(
    client: &mut SqlClient,
    target: &InsertTarget,
    rows: Vec<Vec<Option<String>>>,
) -> Result<u64> {
    let width = target.columns.len();
    let staging = format!("#spelaion_import_{width}");
    let names: Vec<String> = (1..=width).map(|i| format!("c{i}")).collect();
    let create = format!(
        "if object_id('tempdb..{staging}') is null create table {staging}          (spelaion_row bigint identity(1, 1), {})",
        names
            .iter()
            .map(|n| format!("{n} nvarchar(max) null"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    run_batch(client, &create).await?;
    {
        let columns: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut load = client
            .bulk_insert_columns(&staging, &columns)
            .await
            .map_err(|e| map_error(e, None))?;
        for row in rows {
            let mut token = tiberius::TokenRow::new();
            for value in row {
                token.push(tiberius::ColumnData::String(
                    value.map(std::borrow::Cow::Owned),
                ));
            }
            load.send(token).await.map_err(|e| map_error(e, None))?;
        }
        load.finalize().await.map_err(|e| map_error(e, None))?;
    }
    let insert = format!(
        "INSERT INTO {} {} SELECT {} FROM {staging} ORDER BY spelaion_row",
        target.qualified(&SqlServerDialect),
        target.column_list(&SqlServerDialect),
        names.join(", ")
    );
    let inserted = client
        .execute(insert.as_str(), &[])
        .await
        .map_err(|e| map_error(e, None))?
        .total();
    run_batch(client, &format!("truncate table {staging}")).await?;
    Ok(inserted)
}

async fn run_batch(client: &mut SqlClient, sql: &str) -> Result<()> {
    client
        .simple_query(sql)
        .await
        .map_err(|e| map_error(e, Some(sql)))?
        .into_results()
        .await
        .map_err(|e| map_error(e, Some(sql)))?;
    Ok(())
}

async fn rows(client: &mut SqlClient, sql: &str, params: &[String]) -> Result<Rows> {
    // Without parameters, a plain batch: through `sp_executesql` a `USE` would only last for
    // that call.
    let stream = if params.is_empty() {
        client.simple_query(sql).await
    } else {
        let mut query = tiberius::Query::new(sql);
        for param in params {
            query.bind(param.as_str());
        }
        query.query(client).await
    }
    .map_err(|e| map_error(e, Some(sql)))?;
    let rows = stream
        .into_first_result()
        .await
        .map_err(|e| map_error(e, Some(sql)))?;
    Ok(rows
        .into_iter()
        .map(|row| row.into_iter().map(|data| decode::text(&data)).collect())
        .collect())
}

/// One result set being received.
struct Current {
    schema: Arc<ResultSchema>,
    types: Vec<ColumnType>,
    builders: Vec<decode::ColumnBuilder>,
    pending: usize,
}

impl Current {
    fn finish(&mut self) -> Result<RecordBatch> {
        self.pending = 0;
        let columns = self
            .builders
            .iter_mut()
            .map(decode::ColumnBuilder::finish)
            .collect();
        RecordBatch::try_new(self.schema.arrow.clone(), columns)
            .map_err(|e| DriverError::Other(e.to_string()))
    }
}

enum ExecutionOutcome {
    Complete,
    Failed { definitive: bool },
    Abandoned,
    Cancelled,
}

/// Runs one batch and streams every result set, then the server's messages (`PRINT`,
/// `RAISERROR` with severity 10 or less) and its end. A dropped receiver asks the actor to
/// cancel the remaining server work; transaction state then stays unknown until another user
/// request.
async fn execute(
    client: &mut SqlClient,
    sql: &str,
    params: &[String],
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> ExecutionOutcome {
    // Messages of the driver's own queries and of the login are not this batch's.
    let _ = client.take_info_messages();
    let streamed = stream_batch(client, sql, params, batch_size, events).await;
    if matches!(streamed, Streamed::Abandoned) {
        return ExecutionOutcome::Abandoned;
    }
    for notice in info_notices(client) {
        if events.send(Ok(ExecEvent::Notice(notice))).await.is_err() {
            return ExecutionOutcome::Complete;
        }
    }
    match streamed {
        Streamed::Done(rows_affected) => {
            let _ = events.send(Ok(ExecEvent::Done { rows_affected })).await;
            ExecutionOutcome::Complete
        }
        Streamed::Failed(error) => {
            let definitive = matches!(&error, DriverError::Database { .. });
            let _ = events.send(Err(error)).await;
            ExecutionOutcome::Failed { definitive }
        }
        Streamed::Gone => ExecutionOutcome::Complete,
        Streamed::Abandoned => ExecutionOutcome::Abandoned,
    }
}

/// How [`stream_batch`] ended.
enum Streamed {
    /// Every row was sent; the batch's row count, if it has one.
    Done(Option<u64>),
    Failed(DriverError),
    /// The receiver went away mid-batch: the server's work must be cancelled.
    Abandoned,
    /// The receiver went away after the last row.
    Gone,
}

/// The server's informational messages since the batch started, as notices.
fn info_notices(client: &mut SqlClient) -> Vec<Notice> {
    let (messages, dropped) = client.take_info_messages();
    let mut notices: Vec<Notice> = messages
        .into_iter()
        .map(|m| Notice {
            severity: "INFO".into(),
            message: m.message,
        })
        .collect();
    if dropped > 0 {
        notices.push(Notice {
            severity: "WARNING".into(),
            message: format!(
                "{dropped} more messages from the server were not kept (the first {} are above)",
                tiberius::MAX_INFO_MESSAGES
            ),
        });
    }
    notices
}

/// Streams one batch's result sets; the caller sends its messages and its end.
async fn stream_batch(
    client: &mut SqlClient,
    sql: &str,
    params: &[String],
    batch_size: usize,
    events: &mpsc::Sender<Result<ExecEvent>>,
) -> Streamed {
    let send = |event| async move { events.send(event).await.is_ok() };
    let started = async {
        if params.is_empty() {
            return client.simple_query(sql).await;
        }
        // Owned text, so the stream borrows only the client.
        let mut query = tiberius::Query::new(sql.to_string());
        for param in params {
            query.bind(param.clone());
        }
        query.query(client).await
    };
    let mut stream = match started.await {
        Ok(stream) => stream,
        Err(e) => return Streamed::Failed(map_error(e, Some(sql))),
    };
    let mut current: Option<Current> = None;
    let mut total_rows: u64 = 0;
    let mut any_result = false;
    loop {
        let next = tokio::select! {
            result = stream.try_next() => result,
            () = events.closed() => return Streamed::Abandoned,
        };
        let item = match next {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(e) => return Streamed::Failed(map_error(e, Some(sql))),
        };
        match item {
            QueryItem::Metadata(meta) => {
                if let Some(mut previous) = current.take()
                    && previous.pending > 0
                    && !send(previous.finish().map(ExecEvent::Batch)).await
                {
                    return Streamed::Abandoned;
                }
                any_result = true;
                let metas: Vec<_> = meta
                    .columns()
                    .iter()
                    .map(|c| decode::column_meta(c.name(), c.column_type()))
                    .collect();
                let types: Vec<ColumnType> =
                    meta.columns().iter().map(|c| c.column_type()).collect();
                let schema = Arc::new(ResultSchema::new(metas));
                let builders = schema
                    .columns
                    .iter()
                    .zip(&types)
                    .map(|(meta, ty)| decode::ColumnBuilder::new(meta, *ty, batch_size))
                    .collect();
                if !send(Ok(ExecEvent::Columns(schema.clone()))).await {
                    return Streamed::Abandoned;
                }
                current = Some(Current {
                    schema,
                    types,
                    builders,
                    pending: 0,
                });
            }
            QueryItem::Row(row) => {
                let Some(result) = current.as_mut() else {
                    continue;
                };
                for (i, data) in row.into_iter().enumerate() {
                    if let Some(builder) = result.builders.get_mut(i)
                        && let Err(value) = builder.append(&data)
                    {
                        let column = &result.schema.columns[i];
                        send(Err(DriverError::Other(format!(
                            "column {} ({:?}) received a value of another type: {value}",
                            column.name, result.types[i]
                        ))))
                        .await;
                        return Streamed::Abandoned;
                    }
                }
                result.pending += 1;
                total_rows += 1;
                if result.pending == batch_size
                    && !send(result.finish().map(ExecEvent::Batch)).await
                {
                    return Streamed::Abandoned;
                }
            }
        }
    }
    drop(stream);
    if let Some(mut last) = current.take()
        && last.pending > 0
        && !send(last.finish().map(ExecEvent::Batch)).await
    {
        return Streamed::Gone;
    }
    let rows_affected = if any_result {
        Some(total_rows)
    } else if dialect::counts_rows(sql) {
        // The batch's count, asked right after it in the same session.
        match rows(client, "select rowcount_big()", &[]).await {
            Ok(rows) => rows
                .first()
                .and_then(|r| r.first().cloned().flatten())
                .and_then(|n| n.parse().ok()),
            Err(e) => return Streamed::Failed(e),
        }
    } else {
        None
    };
    Streamed::Done(rows_affected)
}

struct SqlServerConnection {
    requests: mpsc::Sender<Request>,
    cancel: Arc<SqlServerCancel>,
    capabilities: Capabilities,
}

fn closed() -> DriverError {
    DriverError::Connection("the connection is closed".into())
}

impl SqlServerConnection {
    /// A catalog query of the driver's own. They only read, so when SQL Server picks one as the
    /// victim of a deadlock with other sessions' DDL (error 1205) it is simply run again.
    pub(crate) async fn rows(&self, sql: &str, params: Vec<String>) -> Result<Rows> {
        const ATTEMPTS: usize = 3;
        let mut attempt = 1;
        loop {
            let (reply, response) = oneshot::channel();
            self.requests
                .send(Request::Rows {
                    sql: sql.to_string(),
                    params: params.clone(),
                    reply,
                })
                .await
                .map_err(|_| closed())?;
            match response.await.map_err(|_| closed())? {
                Err(DriverError::Database { code, .. })
                    if code.as_deref() == Some(DEADLOCK_VICTIM) && attempt < ATTEMPTS =>
                {
                    attempt += 1;
                }
                result => return result,
            }
        }
    }
}

#[async_trait]
impl Connection for SqlServerConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        let (events, mut receiver) = mpsc::channel(4);
        self.requests
            .send(Request::Execute {
                sql: sql.to_string(),
                params: Vec::new(),
                batch_size: opts.batch_size.max(1),
                events,
            })
            .await
            .map_err(|_| closed())?;
        Ok(Box::pin(futures_util::stream::poll_fn(move |cx| {
            receiver.poll_recv(cx)
        })))
    }

    async fn execute_bound(
        &self,
        sql: &str,
        params: Vec<String>,
        opts: &ExecOptions,
    ) -> Result<EventStream> {
        let (events, mut receiver) = mpsc::channel(4);
        self.requests
            .send(Request::Execute {
                sql: sql.to_string(),
                params,
                batch_size: opts.batch_size.max(1),
                events,
            })
            .await
            .map_err(|_| closed())?;
        Ok(Box::pin(futures_util::stream::poll_fn(move |cx| {
            receiver.poll_recv(cx)
        })))
    }

    async fn execute_plan(&self, sql: &str, mode: PlanMode) -> Result<EventStream> {
        let (events, mut receiver) = mpsc::channel(4);
        let cancel_epoch = *self.cancel.0.borrow();
        self.requests
            .send(Request::Plan {
                sql: sql.to_owned(),
                mode,
                cancel_epoch,
                events,
            })
            .await
            .map_err(|_| closed())?;
        Ok(Box::pin(futures_util::stream::poll_fn(move |cx| {
            receiver.poll_recv(cx)
        })))
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    async fn keep_alive(&self) -> Result<KeepAlive> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::KeepAlive { reply })
            .await
            .map_err(|_| closed())?;
        response.await.map_err(|_| closed())?
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::TransactionState { reply })
            .await
            .map_err(|_| closed())?;
        response.await.map_err(|_| closed())?
    }

    fn is_closed(&self) -> bool {
        self.requests.is_closed()
    }

    async fn server_version(&self) -> Result<String> {
        let rows = self
            .rows(
                "select cast(serverproperty('ProductVersion') as nvarchar(128)) + ' ' \
                 + cast(serverproperty('Edition') as nvarchar(128))",
                vec![],
            )
            .await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|row| row.into_iter().next().flatten())
            .unwrap_or_default())
    }

    /// A TDS bulk load into a staging table, then `INSERT … SELECT` (see `bulk_insert`).
    async fn insert_rows(
        &self,
        target: &InsertTarget,
        rows: Vec<Vec<Option<String>>>,
    ) -> Result<u64> {
        target.check_rows(&rows)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request::Insert {
                target: target.clone(),
                rows,
                reply,
            })
            .await
            .map_err(|_| closed())?;
        response.await.map_err(|_| closed())?
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

    /// The online databases this login can enter; the system ones last.
    async fn list_databases(&self) -> Result<Vec<String>> {
        let rows = self
            .rows(
                "select name from sys.databases where state = 0 and has_dbaccess(name) = 1 \
                 order by case when database_id <= 4 then 1 else 0 end, name",
                vec![],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .collect())
    }

    async fn select_database(&self, database: &str) -> Result<()> {
        // Always bracketed: the name is data, never SQL.
        let sql = format!("USE [{}]", database.replace(']', "]]"));
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
        introspect::describe_table(self, schema, name).await
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
        objects::list_server_objects(self).await
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        objects::object_definition(self, object).await
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        ddl::relation_definition(self, schema, name).await
    }

    async fn procedure_call(&self, object: &SchemaObject) -> Result<String> {
        objects::procedure_call(self, object).await
    }
}

/// Asks the actor to send an Attention for the statement it is running.
struct SqlServerCancel(watch::Sender<u64>);

#[async_trait]
impl CancelHandle for SqlServerCancel {
    async fn cancel(&self) -> Result<()> {
        self.0.send_modify(|asked| *asked += 1);
        Ok(())
    }
}

/// Server errors keep their number as `code` (`18456` = login failed) and the line in
/// `detail`. With the statement, the error points at the start of that line.
pub(crate) fn map_error(e: tiberius::error::Error, sql: Option<&str>) -> DriverError {
    match e {
        tiberius::error::Error::Server(token) => {
            let line = token.line();
            let position = sql.filter(|_| line > 0).and_then(|sql| {
                let before: usize = sql
                    .split_inclusive('\n')
                    .take(line as usize - 1)
                    .map(|l| l.chars().count())
                    .sum();
                u32::try_from(before + 1)
                    .ok()
                    .filter(|_| sql.lines().count() >= line as usize)
            });
            DriverError::Database {
                code: Some(token.code().to_string()),
                message: token.message().to_string(),
                detail: Some(format!("SQL Server error {}, line {line}", token.code())),
                hint: None,
                position,
            }
        }
        other => DriverError::Connection(other.to_string()),
    }
}
