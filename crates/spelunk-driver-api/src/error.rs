/// Driver error. Messages never contain secrets.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    /// The connection could not be established or was lost.
    #[error("connection: {0}")]
    Connection(String),

    /// The server rejected the statement. `code` is the engine's code (SQLSTATE in PostgreSQL).
    #[error("{message}")]
    Database {
        code: Option<String>,
        message: String,
        detail: Option<String>,
        hint: Option<String>,
        /// Position (in characters, 1-based) within the statement, if the engine reports it.
        position: Option<u32>,
    },

    /// The statement was cancelled.
    #[error("query cancelled")]
    Cancelled,

    /// A value from the server could not be converted.
    #[error("type {type_name}: {message}")]
    Decode { type_name: String, message: String },

    #[error("{0}")]
    Other(String),
}
