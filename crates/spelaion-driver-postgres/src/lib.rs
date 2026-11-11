//! PostgreSQL driver for SpelaionDB (ADR-005, ADR-006).
//!
//! Each statement is prepared (extended protocol) to learn the exact type of every column
//! and is then executed in streaming mode. Values arrive in binary format and [`decode`]
//! decodes them without losing information for any type.

mod ddl;
mod decode;
mod dialect;
mod introspect;
mod objects;
mod tls;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use arrow::array::RecordBatch;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use spelaion_driver_api::{
    CancelHandle, Capabilities, ColumnMeta, ColumnOrigin, ConnectParams, Connection, Dialect,
    Driver, DriverError, EventStream, ExecEvent, ExecOptions, InsertTarget, KeepAlive, LikeFilter,
    NameFilter, Notice, ObjectInfo, ReferencingKey, Result, ResultSchema, SchemaColumn, SchemaInfo,
    SchemaObject, ServerObject, TableInfo, TransactionState,
};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{FromSql, Type};
use tokio_postgres::{AsyncMessage, CancelToken, Client, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

pub use dialect::PostgresDialect;

pub struct PostgresDriver;

const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const TCP_KEEPALIVE_RETRIES: u32 = 6;

#[async_trait]
impl Driver for PostgresDriver {
    fn engine(&self) -> &'static str {
        "postgres"
    }

    fn dialect(&self) -> Arc<dyn Dialect> {
        Arc::new(PostgresDialect)
    }

    async fn connect(&self, params: &ConnectParams) -> Result<Box<dyn Connection>> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&params.host)
            // Through a tunnel, the port is that of its local end. `port()` appends to a list:
            // calling it twice would fail with "invalid number of ports".
            .port(params.connect_addr.map_or(params.port, |addr| addr.port()))
            .user(&params.user)
            .dbname(&params.database)
            .application_name(params.application_name.as_deref().unwrap_or("SpelaionDB"))
            .connect_timeout(params.connect_timeout())
            // TCP keepalive after a minute idle, so a NAT in front of the server does not drop
            // a direct connection (ADR-028); tokio-postgres waits two hours by default.
            .keepalives(true)
            .keepalives_idle(TCP_KEEPALIVE_IDLE)
            .keepalives_interval(TCP_KEEPALIVE_INTERVAL)
            .keepalives_retries(TCP_KEEPALIVE_RETRIES);
        if let Some(password) = &params.password {
            config.password(password.expose());
        }
        // Through a tunnel: connect to the local end, but TLS verifies the real name.
        if let Some(addr) = params.connect_addr {
            config.hostaddr(addr.ip());
        }

        let identity = match (&params.client_certificate, &params.client_key) {
            (Some(cert), Some(key)) => Some(tls::ClientIdentity { cert, key }),
            _ => None,
        };
        let (ssl_mode, tls) =
            tls::configure(params.tls, params.ca_certificate.as_deref(), identity)?;
        config.ssl_mode(ssl_mode);

        // The connection is driven in its own task; server notices (NOTICE, WARNING…) are
        // kept and delivered at the end of each statement.
        let notices = Arc::new(Mutex::new(Vec::new()));
        let client = match &tls {
            Some(tls) => {
                let (client, connection) = config.connect(tls.clone()).await.map_err(map_error)?;
                tokio::spawn(drive(connection, notices.clone()));
                client
            }
            None => {
                let (client, connection) = config.connect(NoTls).await.map_err(map_error)?;
                tokio::spawn(drive(connection, notices.clone()));
                client
            }
        };

        let cancel = Arc::new(PgCancel {
            token: client.cancel_token(),
            tls,
        });
        Ok(Box::new(PgConnection {
            client: Arc::new(client),
            cancel,
            notices,
            capabilities: Capabilities {
                transactions: true,
                cancel: true,
                explain: true,
                editable_results: false, // comes in v0.2
                schemas: true,
                insert_rows: true,
                bound_params: true,
            },
        }))
    }
}

struct PgConnection {
    client: Arc<Client>,
    cancel: Arc<PgCancel>,
    notices: Arc<Mutex<Vec<Notice>>>,
    capabilities: Capabilities,
}

/// Accepts any type and hands over the raw binary bytes.
struct Raw<'a>(Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> std::result::Result<Self, decode::DecodeError> {
        Ok(Raw(Some(raw)))
    }

    fn from_sql_null(_: &Type) -> std::result::Result<Self, decode::DecodeError> {
        Ok(Raw(None))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

impl PgConnection {
    /// The table column each result column reads (ADR-052): the row description gives its
    /// table's OID and column number, resolved to names in the catalog as it is now (never
    /// cached, so a rename is seen at once). An expression has none. Read before the
    /// statement runs; if the catalog cannot be read, no column has an origin.
    async fn origins(&self, columns: &[tokio_postgres::Column]) -> Vec<Option<ColumnOrigin>> {
        let keys: Vec<(u32, i16)> = columns
            .iter()
            .filter_map(|c| Some((c.table_oid()?, c.column_id()?)))
            .filter(|(oid, attnum)| *oid != 0 && *attnum > 0)
            .collect();
        if keys.is_empty() {
            return vec![None; columns.len()];
        }
        let mut oids: Vec<u32> = keys.iter().map(|(oid, _)| *oid).collect();
        oids.sort_unstable();
        oids.dedup();
        // Only numbers are written into the statement.
        let list = oids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT a.attrelid::int8, a.attnum::int4, c.relname::text, a.attname::text \
             FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
             WHERE a.attrelid IN ({list}) AND a.attnum > 0"
        );
        let mut names: std::collections::HashMap<(u32, i16), ColumnOrigin> =
            std::collections::HashMap::new();
        if let Ok(rows) = self.client.query(sql.as_str(), &[]).await {
            for row in rows {
                let (Ok(oid), Ok(attnum), Ok(table), Ok(column)) = (
                    row.try_get::<_, i64>(0),
                    row.try_get::<_, i32>(1),
                    row.try_get::<_, String>(2),
                    row.try_get::<_, String>(3),
                ) else {
                    continue;
                };
                if let (Ok(oid), Ok(attnum)) = (u32::try_from(oid), i16::try_from(attnum)) {
                    names.insert((oid, attnum), ColumnOrigin { table, column });
                }
            }
        }
        columns
            .iter()
            .map(|c| names.get(&(c.table_oid()?, c.column_id()?)).cloned())
            .collect()
    }

    /// Streams the rows of a prepared statement, with `params` bound to it.
    async fn stream(
        &self,
        sql: &str,
        statement: tokio_postgres::Statement,
        params: Vec<String>,
        opts: &ExecOptions,
    ) -> Result<EventStream> {
        let types: Vec<Type> = statement
            .columns()
            .iter()
            .map(|c| c.type_().clone())
            .collect();
        let origins = if opts.origins {
            self.origins(statement.columns()).await
        } else {
            vec![None; statement.columns().len()]
        };
        let schema = Arc::new(ResultSchema::new(
            statement
                .columns()
                .iter()
                .zip(origins)
                .map(|(c, origin)| ColumnMeta {
                    name: c.name().to_string(),
                    logical: decode::logical_type(c.type_()),
                    native_type: c.type_().name().to_string(),
                    native_id: Some(c.type_().oid()),
                    origin,
                })
                .collect(),
        ));
        let rows = self
            .client
            .query_raw(&statement, params.iter().map(String::as_str))
            .await
            .map_err(map_error)?;
        let batch_size = opts.batch_size.max(1);
        let notices = self.notices.clone();
        let counts_rows = reports_row_count(sql);

        let stream = async_stream::try_stream! {
            let has_columns = !types.is_empty();
            if has_columns {
                yield ExecEvent::Columns(schema.clone());
            }
            let mut rows = std::pin::pin!(rows);
            let mut builders: Vec<decode::ColumnBuilder> =
                types.iter().map(|t| decode::ColumnBuilder::new(t, batch_size)).collect();
            let mut pending = 0usize;
            while let Some(row) = rows.try_next().await.map_err(map_error)? {
                for (i, builder) in builders.iter_mut().enumerate() {
                    let raw: Raw<'_> = row.try_get(i).map_err(map_error)?;
                    builder.append(raw.0).map_err(|e| decode_error(&types[i], e))?;
                }
                pending += 1;
                if pending == batch_size {
                    yield ExecEvent::Batch(finish_batch(&schema, &mut builders)?);
                    pending = 0;
                }
            }
            if pending > 0 {
                yield ExecEvent::Batch(finish_batch(&schema, &mut builders)?);
            }
            let drained: Vec<Notice> = std::mem::take(&mut *notices.lock().expect("notices lock poisoned"));
            for notice in drained {
                yield ExecEvent::Notice(notice);
            }
            yield ExecEvent::Done { rows_affected: rows.rows_affected().filter(|_| counts_rows) };
        };
        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl Connection for PgConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        let statement = self.client.prepare(sql).await.map_err(map_error)?;
        self.stream(sql, statement, Vec::new(), opts).await
    }

    /// Every parameter is declared `text`: the SQL casts it where it needs another type.
    async fn execute_bound(
        &self,
        sql: &str,
        params: Vec<String>,
        opts: &ExecOptions,
    ) -> Result<EventStream> {
        let types = vec![Type::TEXT; params.len()];
        let statement = self
            .client
            .prepare_typed(sql, &types)
            .await
            .map_err(map_error)?;
        self.stream(sql, statement, params, opts).await
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    async fn keep_alive(&self) -> Result<KeepAlive> {
        match self.transaction_state().await? {
            TransactionState::Idle => {}
            TransactionState::Open | TransactionState::Failed => {
                return Ok(KeepAlive::InTransaction);
            }
        }
        self.client
            .simple_query("select 1")
            .await
            .map_err(map_error)?;
        Ok(KeepAlive::Alive)
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        // ReadyForQuery exposes the server's actual transaction state without a query.
        // A dropped prepared statement may still be awaiting its close response: wait for
        // the protocol to settle rather than treating that transient state as a transaction.
        loop {
            if self.client.is_closed() {
                return Err(DriverError::Connection("the connection is closed".into()));
            }
            match self.client.transaction_status() {
                Some(b'I') => return Ok(TransactionState::Idle),
                Some(b'E') => return Ok(TransactionState::Failed),
                Some(_) => return Ok(TransactionState::Open),
                None => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    async fn server_version(&self) -> Result<String> {
        let row = self
            .client
            .query_one("select current_setting('server_version')", &[])
            .await
            .map_err(map_error)?;
        Ok(row.get(0))
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        introspect::list_schemas(&self.client, &LikeFilter::default()).await
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        introspect::list_objects(&self.client, schema, &LikeFilter::default()).await
    }

    // Narrowed in the catalog query, then matched exactly (ADR-025).
    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let mut schemas = introspect::list_schemas(&self.client, &filter.server_side()).await?;
        schemas.retain(|s| filter.matches(&s.name));
        Ok(schemas)
    }

    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let mut objects =
            introspect::list_objects(&self.client, schema, &filter.server_side()).await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let mut objects =
            objects::list_schema_objects(&self.client, schema, &filter.server_side()).await?;
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        introspect::describe_table(&self.client, schema, name).await
    }

    async fn list_columns(&self, schema: &str) -> Result<Vec<SchemaColumn>> {
        introspect::list_columns(&self.client, schema).await
    }

    async fn list_referencing_keys(&self, schema: &str, name: &str) -> Result<Vec<ReferencingKey>> {
        introspect::list_referencing_keys(&self.client, schema, name).await
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        objects::list_schema_objects(&self.client, schema, &LikeFilter::default()).await
    }

    async fn list_server_objects(&self) -> Result<Vec<ServerObject>> {
        objects::list_server_objects(&self.client).await
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        objects::object_definition(&self.client, object).await
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        ddl::relation_definition(&self.client, schema, name).await
    }

    async fn procedure_call(&self, object: &SchemaObject) -> Result<String> {
        objects::procedure_call(&self.client, object).await
    }

    /// `COPY … FROM STDIN` in text format: the values are the copy's data, never SQL.
    async fn insert_rows(
        &self,
        target: &InsertTarget,
        rows: Vec<Vec<Option<String>>>,
    ) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let sql = copy_sql(target, &PostgresDialect);
        let sink = self
            .client
            .copy_in::<_, bytes::Bytes>(&sql)
            .await
            .map_err(map_error)?;
        let mut sink = std::pin::pin!(sink);
        let mut chunk = String::with_capacity(COPY_CHUNK + 4096);
        for row in &rows {
            if row.len() != target.columns.len() {
                return Err(DriverError::Other(format!(
                    "a row has {} values for {} columns",
                    row.len(),
                    target.columns.len()
                )));
            }
            copy_text_row(&mut chunk, row);
            if chunk.len() >= COPY_CHUNK {
                let data = bytes::Bytes::from(std::mem::take(&mut chunk));
                sink.send(data).await.map_err(map_error)?;
            }
        }
        if !chunk.is_empty() {
            sink.send(bytes::Bytes::from(chunk))
                .await
                .map_err(map_error)?;
        }
        sink.as_mut().finish().await.map_err(map_error)
    }
}

/// Bytes of COPY data sent at a time.
const COPY_CHUNK: usize = 256 * 1024;

/// `COPY "schema"."table" ("a", "b") FROM STDIN`: only quoted names; the data follows apart.
pub fn copy_sql(target: &InsertTarget, dialect: &dyn Dialect) -> String {
    format!(
        "COPY {} {} FROM STDIN",
        target.qualified(dialect),
        target.column_list(dialect)
    )
}

/// One row of COPY's text format: values separated by tabs, `\N` for NULL, and backslashes,
/// tabs, newlines and carriage returns escaped, so a value can never end its field or its row.
fn copy_text_row(out: &mut String, row: &[Option<String>]) {
    for (i, value) in row.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        match value {
            None => out.push_str(r"\N"),
            Some(text) => {
                for c in text.chars() {
                    match c {
                        '\\' => out.push_str(r"\\"),
                        '\t' => out.push_str(r"\t"),
                        '\n' => out.push_str(r"\n"),
                        '\r' => out.push_str(r"\r"),
                        other => out.push(other),
                    }
                }
            }
        }
    }
    out.push('\n');
}

#[cfg(test)]
mod copy_tests {
    use super::copy_text_row;

    #[test]
    fn copy_rows_escape_everything_that_could_end_a_field() {
        let mut out = String::new();
        copy_text_row(
            &mut out,
            &[
                Some("a\tb\nc\\d\re".into()),
                None,
                Some(r"\N".into()),
                Some(String::new()),
            ],
        );
        assert_eq!(out, "a\\tb\\nc\\\\d\\re\t\\N\t\\\\N\t\n");
    }
}

/// tokio-postgres also returns `Some(0)` for statements without a row count (`CREATE TABLE`),
/// indistinguishable from an `UPDATE` that touched no rows. Only these statements have one.
fn reports_row_count(sql: &str) -> bool {
    const COUNTED: &[&str] = &[
        "select", "insert", "update", "delete", "merge", "with", "values", "table", "copy",
        "fetch", "move",
    ];
    let mut rest = sql.trim_start();
    // Skip leading comments.
    loop {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r
                .split_once('\n')
                .map_or("", |(_, after)| after)
                .trim_start();
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r
                .split_once("*/")
                .map_or("", |(_, after)| after)
                .trim_start();
        } else {
            break;
        }
    }
    let first = rest
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    COUNTED.iter().any(|k| first.eq_ignore_ascii_case(k)) || rest.starts_with('(')
}

fn finish_batch(
    schema: &ResultSchema,
    builders: &mut [decode::ColumnBuilder],
) -> Result<RecordBatch> {
    let columns = builders
        .iter_mut()
        .map(decode::ColumnBuilder::finish)
        .collect();
    RecordBatch::try_new(schema.arrow.clone(), columns)
        .map_err(|e| DriverError::Other(e.to_string()))
}

/// Cancellation opens a new connection, which must use the same TLS as the original.
struct PgCancel {
    token: CancelToken,
    tls: Option<MakeRustlsConnect>,
}

#[async_trait]
impl CancelHandle for PgCancel {
    async fn cancel(&self) -> Result<()> {
        match &self.tls {
            Some(tls) => self.token.cancel_query(tls.clone()).await,
            None => self.token.cancel_query(NoTls).await,
        }
        .map_err(map_error)
    }
}

/// Drives the connection until it closes, collecting server notices.
async fn drive<S, T>(
    mut connection: tokio_postgres::Connection<S, T>,
    notices: Arc<Mutex<Vec<Notice>>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut messages = futures_util::stream::poll_fn(move |cx| connection.poll_message(cx));
    while let Some(message) = messages.next().await {
        match message {
            Ok(AsyncMessage::Notice(n)) => {
                notices.lock().expect("notices lock poisoned").push(Notice {
                    severity: n.severity().to_string(),
                    message: n.message().to_string(),
                })
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

fn decode_error(ty: &Type, e: decode::DecodeError) -> DriverError {
    DriverError::Decode {
        type_name: ty.name().to_string(),
        message: e.to_string(),
    }
}

pub(crate) fn map_error(e: tokio_postgres::Error) -> DriverError {
    if let Some(db) = e.as_db_error() {
        if *db.code() == SqlState::QUERY_CANCELED {
            return DriverError::Cancelled;
        }
        return DriverError::Database {
            code: Some(db.code().code().to_string()),
            message: db.message().to_string(),
            detail: db.detail().map(str::to_string),
            hint: db.hint().map(str::to_string),
            position: match db.position() {
                Some(tokio_postgres::error::ErrorPosition::Original(p)) => Some(*p),
                _ => None,
            },
        };
    }
    // tokio-postgres connection errors hide the cause in `source()`.
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    DriverError::Connection(message)
}

#[cfg(test)]
mod tests {
    use super::reports_row_count;

    #[test]
    fn only_counted_statements_report_rows() {
        for sql in [
            "select 1",
            "INSERT into t values (1)",
            "-- x\n update t set a = 1",
            "/* y */ delete from t",
            "(select 1)",
            "with x as (select 1) select * from x",
        ] {
            assert!(reports_row_count(sql), "{sql}");
        }
        for sql in [
            "create table t (a int)",
            "drop table t",
            "set timezone = 'UTC'",
            "begin",
            "do $$ begin end $$",
        ] {
            assert!(!reports_row_count(sql), "{sql}");
        }
    }
}
