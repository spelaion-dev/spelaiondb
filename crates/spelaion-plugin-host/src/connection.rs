//! A session of a plugin, as a [`Connection`].

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_util::Stream;
use serde::de::DeserializeOwned;
use serde_json::Value;
use spelaion_driver_api::{
    CancelHandle, Capabilities, ConnectParams, Connection, DriverError, EventStream, ExecEvent,
    ExecOptions, InsertTarget, KeepAlive, NameFilter, ObjectInfo, PlanMode, ReferencingKey, Result,
    SchemaColumn, SchemaInfo, SchemaObject, ServerObject, TableInfo, TransactionState,
};
use spelaion_plugin_protocol::ipc::{BatchEncoder, rows_to_batch};
use spelaion_plugin_protocol::{
    CatalogRequest, Connect, ConnectResult, Execute, InsertRows, Run, SessionRequest, StreamId,
    method,
};
use tokio::sync::mpsc;

use crate::process::Process;

pub(crate) struct PluginConnection {
    process: Arc<Process>,
    session: u64,
    capabilities: Capabilities,
    /// The plugin said the connection is known to be closed, or the plugin stopped.
    closed: Arc<AtomicBool>,
}

impl PluginConnection {
    pub(crate) async fn open(
        process: Arc<Process>,
        session: u64,
        engine: &str,
        params: &ConnectParams,
    ) -> Result<Self> {
        let shared = process.shared();
        // Before the plugin has it: nothing it says about it is ever shown.
        if let Some(password) = &params.password {
            shared.add_secret(password.expose());
        }
        let closed = shared.add_session(session);
        let request = Connect {
            session,
            engine: engine.to_string(),
            host: params.host.clone(),
            port: params.port,
            user: params.user.clone(),
            password: params.password.as_ref().map(|p| p.expose().to_string()),
            database: params.database.clone(),
            application_name: params.application_name.clone(),
            tls: params.tls,
            ca_certificate: params.ca_certificate.clone(),
            client_certificate: params.client_certificate.clone(),
            client_key: params.client_key.clone(),
            read_only: params.read_only,
            connect_addr: params.connect_addr,
            connect_timeout_ms: params
                .connect_timeout
                .map(|t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX)),
        };
        // Whoever stops waiting for the connection (a timeout, a Stop) must not leave it open
        // in the plugin.
        let mut opening = Opening {
            process: &process,
            session,
            armed: true,
        };
        let answer = shared.request(method::CONNECT, &request).await;
        let capabilities = answer.and_then(|value| {
            serde_json::from_value::<ConnectResult>(value)
                .map(|result| result.capabilities)
                .map_err(|_| unexpected(method::CONNECT))
        });
        match capabilities {
            Ok(capabilities) => {
                opening.armed = false;
                Ok(Self {
                    process: process.clone(),
                    session,
                    capabilities,
                    closed,
                })
            }
            // `opening` closes it as it is dropped.
            Err(error) => Err(error),
        }
    }

    async fn ask<T: DeserializeOwned>(
        &self,
        name: &str,
        request: CatalogRequest,
        key: &str,
    ) -> Result<T> {
        let value = self.process.shared().request(name, request).await?;
        field(value, key).ok_or_else(|| unexpected(name))
    }

    fn catalog(&self) -> CatalogRequest {
        CatalogRequest {
            session: self.session,
            ..Default::default()
        }
    }

    async fn run(&self, sql: &str, options: &ExecOptions, run: Run) -> Result<EventStream> {
        let shared = self.process.shared();
        let stream = shared.next_stream();
        // Registered first: its events may arrive before the answer is read.
        let events = shared.receive(stream);
        let mut events = Events {
            events,
            process: self.process.clone(),
            stream,
            ended: false,
        };
        let request = Execute {
            session: self.session,
            stream,
            sql: sql.to_string(),
            batch_size: options.batch_size,
            origins: options.origins,
            run,
        };
        match shared.request(method::EXECUTE, request).await {
            Ok(_) => Ok(Box::pin(events)),
            Err(error) => {
                // It never started: there is nothing for the plugin to stop.
                events.ended = true;
                shared.forget_stream(stream, false);
                Err(error)
            }
        }
    }
}

/// A `connect` in progress.
struct Opening<'a> {
    process: &'a Process,
    session: u64,
    armed: bool,
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        if self.armed {
            let shared = self.process.shared();
            shared.forget_session(self.session);
            shared.tell(
                method::CLOSE,
                SessionRequest {
                    session: self.session,
                },
            );
        }
    }
}

impl Drop for PluginConnection {
    fn drop(&mut self) {
        let shared = self.process.shared();
        shared.forget_session(self.session);
        shared.tell(
            method::CLOSE,
            SessionRequest {
                session: self.session,
            },
        );
    }
}

fn unexpected(name: &str) -> DriverError {
    DriverError::Other(format!(
        "the plugin answered `{name}` with something the protocol does not say"
    ))
}

fn field<T: DeserializeOwned>(value: Value, key: &str) -> Option<T> {
    let Value::Object(mut object) = value else {
        return None;
    };
    serde_json::from_value(object.remove(key)?).ok()
}

/// The events of a statement as they arrive from the plugin. Each batch read gives the plugin
/// credit for one more; dropping it before its end tells the plugin to stop sending.
struct Events {
    events: mpsc::UnboundedReceiver<Result<ExecEvent>>,
    process: Arc<Process>,
    stream: StreamId,
    ended: bool,
}

impl Stream for Events {
    type Item = Result<ExecEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.events.poll_recv(cx) {
            Poll::Ready(Some(event)) => {
                if matches!(event, Ok(ExecEvent::Batch(_))) {
                    this.process.shared().credit(this.stream, 1);
                }
                Poll::Ready(Some(event))
            }
            Poll::Ready(None) => {
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        if !self.ended {
            self.process.shared().forget_stream(self.stream, true);
        }
    }
}

struct Cancel {
    process: Arc<Process>,
    session: u64,
}

#[async_trait]
impl CancelHandle for Cancel {
    async fn cancel(&self) -> Result<()> {
        let request = SessionRequest {
            session: self.session,
        };
        self.process
            .shared()
            .request(method::CANCEL, request)
            .await
            .map(|_| ())
    }
}

#[async_trait]
impl Connection for PluginConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        self.run(sql, opts, Run::Statement).await
    }

    async fn execute_plan(&self, sql: &str, mode: PlanMode) -> Result<EventStream> {
        self.run(sql, &ExecOptions::default(), Run::Plan(mode.into()))
            .await
    }

    async fn execute_bound(
        &self,
        sql: &str,
        params: Vec<String>,
        opts: &ExecOptions,
    ) -> Result<EventStream> {
        self.run(sql, opts, Run::Bound { params }).await
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        Arc::new(Cancel {
            process: self.process.clone(),
            session: self.session,
        })
    }

    async fn server_version(&self) -> Result<String> {
        self.ask(method::SERVER_VERSION, self.catalog(), "version")
            .await
    }

    async fn keep_alive(&self) -> Result<KeepAlive> {
        self.ask(method::KEEP_ALIVE, self.catalog(), "state").await
    }

    async fn transaction_state(&self) -> Result<TransactionState> {
        self.ask(method::TRANSACTION_STATE, self.catalog(), "state")
            .await
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.process.is_dead()
    }

    async fn current_database(&self) -> Result<Option<String>> {
        self.ask(method::CURRENT_DATABASE, self.catalog(), "database")
            .await
    }

    async fn database_context(&self) -> Result<Option<String>> {
        self.ask(method::DATABASE_CONTEXT, self.catalog(), "database")
            .await
    }

    async fn select_database(&self, database: &str) -> Result<()> {
        let request = CatalogRequest {
            database: Some(database.to_string()),
            ..self.catalog()
        };
        self.process
            .shared()
            .request(method::SELECT_DATABASE, request)
            .await
            .map(|_| ())
    }

    async fn list_databases(&self) -> Result<Vec<String>> {
        self.ask(method::LIST_DATABASES, self.catalog(), "databases")
            .await
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        self.ask(method::LIST_SCHEMAS, self.catalog(), "schemas")
            .await
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            ..self.catalog()
        };
        self.ask(method::LIST_OBJECTS, request, "objects").await
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            name: Some(name.to_string()),
            ..self.catalog()
        };
        self.ask(method::DESCRIBE_TABLE, request, "table").await
    }

    async fn list_referencing_keys(&self, schema: &str, name: &str) -> Result<Vec<ReferencingKey>> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            name: Some(name.to_string()),
            ..self.catalog()
        };
        self.ask(method::LIST_REFERENCING_KEYS, request, "keys")
            .await
    }

    async fn list_columns(&self, schema: &str) -> Result<Vec<SchemaColumn>> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            ..self.catalog()
        };
        self.ask(method::LIST_COLUMNS, request, "columns").await
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            ..self.catalog()
        };
        self.ask(method::LIST_SCHEMA_OBJECTS, request, "objects")
            .await
    }

    async fn list_server_objects(&self) -> Result<Vec<ServerObject>> {
        self.ask(method::LIST_SERVER_OBJECTS, self.catalog(), "objects")
            .await
    }

    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let request = CatalogRequest {
            filter: Some(filter.clone()),
            ..self.catalog()
        };
        self.ask(method::LIST_SCHEMAS, request, "schemas").await
    }

    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            filter: Some(filter.clone()),
            ..self.catalog()
        };
        self.ask(method::LIST_OBJECTS, request, "objects").await
    }

    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            filter: Some(filter.clone()),
            ..self.catalog()
        };
        self.ask(method::LIST_SCHEMA_OBJECTS, request, "objects")
            .await
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        let request = CatalogRequest {
            object: Some(object.clone()),
            ..self.catalog()
        };
        self.ask(method::OBJECT_DEFINITION, request, "sql").await
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        let request = CatalogRequest {
            schema: Some(schema.to_string()),
            name: Some(name.to_string()),
            ..self.catalog()
        };
        self.ask(method::RELATION_DEFINITION, request, "sql").await
    }

    async fn procedure_call(&self, object: &SchemaObject) -> Result<String> {
        let request = CatalogRequest {
            object: Some(object.clone()),
            ..self.catalog()
        };
        self.ask(method::PROCEDURE_CALL, request, "sql").await
    }

    async fn insert_rows(
        &self,
        target: &InsertTarget,
        rows: Vec<Vec<Option<String>>>,
    ) -> Result<u64> {
        target.check_rows(&rows)?;
        let shared = self.process.shared();
        let unreadable = |e: arrow::error::ArrowError| DriverError::Other(e.to_string());
        let batch = rows_to_batch(target.columns.len(), &rows).map_err(unreadable)?;
        drop(rows);
        let (mut encoder, start) = BatchEncoder::new(&batch.schema()).map_err(unreadable)?;
        let bytes = encoder.encode(&batch).map_err(unreadable)?;
        drop(batch);
        // The rows first, then the request that names their stream.
        let stream = shared.next_stream();
        shared.send_rows(stream, start, bytes);
        let request = InsertRows {
            session: self.session,
            stream,
            target: target.clone(),
        };
        let value = shared.request(method::INSERT_ROWS, request).await?;
        field(value, "rows").ok_or_else(|| unexpected(method::INSERT_ROWS))
    }
}
