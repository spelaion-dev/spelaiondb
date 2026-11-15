//! A SQLite value with its storage class, owned.

/// One value as SQLite stores it: its storage class and its content.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqlValue {
    /// A TEXT value from its bytes. SQLite does not guarantee valid UTF-8 in TEXT: bytes that
    /// are not are kept as a BLOB, so nothing is lost.
    pub fn text_from_bytes(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(s) => Self::Text(s.to_string()),
            Err(_) => Self::Blob(bytes.to_vec()),
        }
    }
}
