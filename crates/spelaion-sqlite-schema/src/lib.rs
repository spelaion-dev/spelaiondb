//! What every program that talks to SQLite shares (ADR-058): the catalog queries and how
//! their rows become the driver contract's model (`catalog`, and `details` for a relation's
//! properties and dependencies), and SQLite's dynamic typing,
//! from rows of storage-class values and declared types to Arrow batches (`typing`, ADR-006).
//!
//! It knows no transport: the compiled-in SQLite driver (a local file through `rusqlite`, and a
//! file on a server through its `sqlite3`) and the libSQL plugin (Hrana over HTTP) hand it
//! values and get the same explorer and the same types back, so they cannot drift apart.
//! Plugins take it by the plugin SDK's tag (ADR-042).

pub mod catalog;
/// A relation's properties and dependencies (SDB-393).
pub mod details;
pub mod typing;
mod value;

pub use catalog::Row;
pub use typing::{Affinity, Builder, ResultBuilder, ResultColumn, Seen, affinity, decide};
pub use value::SqlValue;
