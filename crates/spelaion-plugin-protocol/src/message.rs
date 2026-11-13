//! The JSON-RPC 2.0 messages: their envelope, the method names and what each carries.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spelaion_driver_api::{
    Capabilities, DriverError, InsertTarget, NameFilter, PlanMode, SchemaObject, TlsMode,
};

/// The id of a stream of data frames, chosen by the core and unique in the plugin's process.
pub type StreamId = u64;

/// A version of the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Method names.
pub mod method {
    pub const INITIALIZE: &str = "initialize";
    pub const CONNECT: &str = "connect";
    pub const CLOSE: &str = "close";
    pub const EXECUTE: &str = "execute";
    pub const CANCEL: &str = "cancel";
    pub const INSERT_ROWS: &str = "insert_rows";

    pub const SERVER_VERSION: &str = "server_version";
    pub const KEEP_ALIVE: &str = "keep_alive";
    pub const TRANSACTION_STATE: &str = "transaction_state";
    pub const CURRENT_DATABASE: &str = "current_database";
    pub const DATABASE_CONTEXT: &str = "database_context";
    pub const SELECT_DATABASE: &str = "select_database";
    pub const LIST_DATABASES: &str = "list_databases";
    pub const LIST_SCHEMAS: &str = "list_schemas";
    pub const LIST_OBJECTS: &str = "list_objects";
    pub const DESCRIBE_TABLE: &str = "describe_table";
    pub const LIST_REFERENCING_KEYS: &str = "list_referencing_keys";
    pub const LIST_COLUMNS: &str = "list_columns";
    pub const LIST_SCHEMA_OBJECTS: &str = "list_schema_objects";
    pub const LIST_SERVER_OBJECTS: &str = "list_server_objects";
    pub const OBJECT_DEFINITION: &str = "object_definition";
    pub const RELATION_DEFINITION: &str = "relation_definition";
    pub const PROCEDURE_CALL: &str = "procedure_call";

    // Notifications from the plugin.
    pub const SESSION_CLOSED: &str = "session.closed";
    pub const STREAM_NOTICE: &str = "stream.notice";
    pub const STREAM_DONE: &str = "stream.done";
    pub const STREAM_ERROR: &str = "stream.error";
    pub const STREAM_END: &str = "stream.end";
    // Notifications from the core.
    pub const STREAM_CREDIT: &str = "stream.credit";
    pub const STREAM_DROP: &str = "stream.drop";
}

/// Error codes.
pub mod code {
    /// JSON-RPC's own.
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// The request failed in the driver: `data` is an [`super::ErrorData`].
    pub const DRIVER: i64 = -32000;
    /// The plugin speaks none of the versions the core offered: `data.protocols` are its own.
    pub const UNSUPPORTED_PROTOCOL: i64 = -32001;
}

/// One JSON-RPC 2.0 object: a request (`id` and `method`), a notification (`method` only) or a
/// response (`id` with `result` or `error`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Message {
    fn empty() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: None,
            params: None,
            result: None,
            error: None,
        }
    }

    pub fn request(id: u64, method: &str, params: impl Serialize) -> Self {
        Self {
            id: Some(id),
            ..Self::notification(method, params)
        }
    }

    pub fn notification(method: &str, params: impl Serialize) -> Self {
        Self {
            method: Some(method.into()),
            // What this crate serializes is its own types: they always become JSON.
            params: Some(serde_json::to_value(params).unwrap_or(Value::Null)),
            ..Self::empty()
        }
    }

    pub fn result(id: u64, result: impl Serialize) -> Self {
        Self {
            id: Some(id),
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            ..Self::empty()
        }
    }

    pub fn error(id: u64, error: RpcError) -> Self {
        Self {
            id: Some(id),
            error: Some(error),
            ..Self::empty()
        }
    }

    /// The bytes of a message frame.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    /// The parameters as `T`.
    pub fn params<T: for<'de> Deserialize<'de>>(&self) -> Result<T, RpcError> {
        let params = self.params.clone().unwrap_or(Value::Null);
        serde_json::from_value(params).map_err(|e| RpcError {
            code: code::INVALID_PARAMS,
            message: e.to_string(),
            data: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: code::METHOD_NOT_FOUND,
            message: format!("unknown method: {method}"),
            data: None,
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: code::INVALID_REQUEST,
            message: message.into(),
            data: None,
        }
    }

    /// The driver's error behind this one. An error that is not a driver's (an unknown method,
    /// parameters that do not parse) becomes `Other` with its message.
    pub fn into_driver_error(self) -> DriverError {
        if self.code == code::DRIVER
            && let Some(data) = self.data
            && let Ok(data) = serde_json::from_value::<ErrorData>(data)
        {
            return data.into();
        }
        DriverError::Other(self.message)
    }
}

impl From<&DriverError> for RpcError {
    fn from(error: &DriverError) -> Self {
        Self {
            code: code::DRIVER,
            message: error.to_string(),
            data: serde_json::to_value(ErrorData::from(error)).ok(),
        }
    }
}

/// A driver's error on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ErrorData {
    Connection {
        message: String,
    },
    Database {
        message: String,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        hint: Option<String>,
        #[serde(default)]
        position: Option<u32>,
    },
    Cancelled,
    Decode {
        type_name: String,
        message: String,
    },
    Other {
        message: String,
    },
}

impl From<&DriverError> for ErrorData {
    fn from(error: &DriverError) -> Self {
        match error {
            DriverError::Connection(message) => Self::Connection {
                message: message.clone(),
            },
            DriverError::Database {
                code,
                message,
                detail,
                hint,
                position,
            } => Self::Database {
                message: message.clone(),
                code: code.clone(),
                detail: detail.clone(),
                hint: hint.clone(),
                position: *position,
            },
            DriverError::Cancelled => Self::Cancelled,
            DriverError::Decode { type_name, message } => Self::Decode {
                type_name: type_name.clone(),
                message: message.clone(),
            },
            DriverError::Other(message) => Self::Other {
                message: message.clone(),
            },
        }
    }
}

impl From<ErrorData> for DriverError {
    fn from(data: ErrorData) -> Self {
        match data {
            ErrorData::Connection { message } => Self::Connection(message),
            ErrorData::Database {
                message,
                code,
                detail,
                hint,
                position,
            } => Self::Database {
                code,
                message,
                detail,
                hint,
                position,
            },
            ErrorData::Cancelled => Self::Cancelled,
            ErrorData::Decode { type_name, message } => Self::Decode { type_name, message },
            ErrorData::Other { message } => Self::Other(message),
        }
    }
}

impl ErrorData {
    /// Every text of the error, to take secrets out of.
    pub fn texts_mut(&mut self) -> Vec<&mut String> {
        match self {
            Self::Connection { message } | Self::Other { message } => vec![message],
            Self::Database {
                message,
                code,
                detail,
                hint,
                position: _,
            } => {
                let mut texts = vec![message];
                texts.extend(code.as_mut());
                texts.extend(detail.as_mut());
                texts.extend(hint.as_mut());
                texts
            }
            Self::Cancelled => Vec::new(),
            Self::Decode { type_name, message } => vec![type_name, message],
        }
    }
}

/// A program's name and version: the core's in `initialize`, the plugin's in its answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Initialize {
    /// Every version the core speaks.
    pub protocols: Vec<Version>,
    pub host: Program,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializeResult {
    /// The version the plugin chose among the core's.
    pub protocol: Version,
    pub plugin: Program,
    /// The engines it serves, by the names the configuration uses.
    pub engines: Vec<String>,
    /// How many record batches it may have sent on a stream without credit given back.
    pub credit: u32,
}

/// `connect`. No `Debug`: it carries the password.
#[derive(Clone, Serialize, Deserialize)]
pub struct Connect {
    pub session: u64,
    pub engine: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub password: Option<String>,
    pub database: String,
    #[serde(default)]
    pub application_name: Option<String>,
    pub tls: TlsMode,
    #[serde(default)]
    pub ca_certificate: Option<PathBuf>,
    #[serde(default)]
    pub client_certificate: Option<PathBuf>,
    #[serde(default)]
    pub client_key: Option<PathBuf>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub connect_addr: Option<SocketAddr>,
    #[serde(default)]
    pub connect_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectResult {
    pub capabilities: Capabilities,
}

/// A request that names only its session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    pub session: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execute {
    pub session: u64,
    pub stream: StreamId,
    pub sql: String,
    pub batch_size: usize,
    #[serde(default)]
    pub origins: bool,
    pub run: Run,
}

/// What `execute` runs the statement as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Run {
    Statement,
    /// One read with these text values bound to its placeholders.
    Bound {
        params: Vec<String>,
    },
    /// The engine's own plan request.
    Plan(PlanRun),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PlanRun {
    Estimated,
    Analyze {
        #[serde(default)]
        query_result: bool,
    },
}

impl From<PlanMode> for PlanRun {
    fn from(mode: PlanMode) -> Self {
        match mode {
            PlanMode::Estimated => Self::Estimated,
            PlanMode::Analyze { query_result } => Self::Analyze { query_result },
        }
    }
}

impl From<PlanRun> for PlanMode {
    fn from(run: PlanRun) -> Self {
        match run {
            PlanRun::Estimated => Self::Estimated,
            PlanRun::Analyze { query_result } => Self::Analyze { query_result },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamNotice {
    pub stream: StreamId,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDone {
    pub stream: StreamId,
    pub rows_affected: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamError {
    pub stream: StreamId,
    pub error: ErrorData,
}

/// `stream.credit`, and with `batches` left out `stream.end` and `stream.drop`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamCredit {
    pub stream: StreamId,
    #[serde(default)]
    pub batches: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertRows {
    pub session: u64,
    /// The stream the rows arrived on, before this request.
    pub stream: StreamId,
    pub target: InsertTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsertRowsResult {
    pub rows: u64,
}

/// The catalog requests: a session and whichever of these the method takes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogRequest {
    pub session: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<NameFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<SchemaObject>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drivers_error_round_trips() {
        for error in [
            DriverError::Connection("refused".into()),
            DriverError::Database {
                code: Some("42P01".into()),
                message: "relation \"t\" does not exist".into(),
                detail: None,
                hint: Some("create it".into()),
                position: Some(15),
            },
            DriverError::Cancelled,
            DriverError::Decode {
                type_name: "numeric".into(),
                message: "too long".into(),
            },
            DriverError::Other("no".into()),
        ] {
            let wire = serde_json::to_string(&RpcError::from(&error)).unwrap();
            let back: RpcError = serde_json::from_str(&wire).unwrap();
            assert_eq!(
                format!("{:?}", back.into_driver_error()),
                format!("{error:?}"),
                "{wire}"
            );
        }
    }

    #[test]
    fn an_error_that_is_not_a_drivers_keeps_its_message() {
        let error = RpcError::method_not_found("nope").into_driver_error();
        assert!(matches!(error, DriverError::Other(m) if m == "unknown method: nope"));
        // A driver code with data that is not an error's.
        let error = RpcError {
            code: code::DRIVER,
            message: "odd".into(),
            data: Some(serde_json::json!({"kind": "unheard of"})),
        };
        assert!(matches!(error.into_driver_error(), DriverError::Other(m) if m == "odd"));
    }

    #[test]
    fn messages_are_told_apart_by_their_fields() {
        let request = Message::request(7, method::CANCEL, SessionRequest { session: 1 });
        let text = String::from_utf8(request.to_bytes()).unwrap();
        assert_eq!(
            text,
            r#"{"jsonrpc":"2.0","id":7,"method":"cancel","params":{"session":1}}"#
        );
        let back = Message::from_bytes(text.as_bytes()).unwrap();
        assert_eq!(back.params::<SessionRequest>().unwrap().session, 1);
        // Fields this version does not know are ignored.
        let newer = br#"{"jsonrpc":"2.0","method":"stream.credit","params":{"stream":4,"batches":2,"later":true},"extra":1}"#;
        let message = Message::from_bytes(newer).unwrap();
        assert_eq!(message.params::<StreamCredit>().unwrap().batches, 2);
    }

    #[test]
    fn what_a_statement_runs_as_reads_as_the_specification_says() {
        for (run, json) in [
            (Run::Statement, r#"{"kind":"statement"}"#),
            (
                Run::Bound {
                    params: vec!["a".into()],
                },
                r#"{"kind":"bound","params":["a"]}"#,
            ),
            (
                Run::Plan(PlanRun::Estimated),
                r#"{"kind":"plan","mode":"estimated"}"#,
            ),
            (
                Run::Plan(PlanRun::Analyze { query_result: true }),
                r#"{"kind":"plan","mode":"analyze","query_result":true}"#,
            ),
        ] {
            assert_eq!(serde_json::to_string(&run).unwrap(), json);
            assert_eq!(serde_json::from_str::<Run>(json).unwrap(), run);
        }
    }

    #[test]
    fn a_connect_names_tls_as_libpq_does() {
        let connect = Connect {
            session: 1,
            engine: "postgres".into(),
            host: "db".into(),
            port: 5432,
            user: "app".into(),
            password: Some("hunter2".into()),
            database: "shop".into(),
            application_name: None,
            tls: TlsMode::VerifyFull,
            ca_certificate: None,
            client_certificate: None,
            client_key: None,
            read_only: false,
            connect_addr: Some("127.0.0.1:50211".parse().unwrap()),
            connect_timeout_ms: Some(15_000),
        };
        let json = serde_json::to_value(&connect).unwrap();
        assert_eq!(json["tls"], "verify-full");
        assert_eq!(json["connect_addr"], "127.0.0.1:50211");
        assert_eq!(json["password"], "hunter2");
    }
}
