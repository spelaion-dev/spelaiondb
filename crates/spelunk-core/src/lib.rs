//! SpelunkDB core (ADR-002).
//!
//! It does not depend on Tauri: the desktop app, the CLI and the MCP server are thin
//! adapters over this crate. Everything that has to be the same for all of them lives here.

mod catalog;
mod classify;
mod cursor;
mod export;
mod history;
mod open;
mod readonly;
mod results;
mod session;
mod sort;

pub use catalog::{EngineAccess, EngineCategory, EngineInfo, engine_catalog};
pub use classify::{ReadKind, classify_read_only};
pub use cursor::statement_at;
pub use export::{ExportFormat, ExportOptions, GenericDialect, export_to_file, write_result};
pub use history::{HistoryError, QueryHistory};
pub use open::{
    Engines, OpenConnection, OpenError, OpenOptions, TunnelPool, open_connection, test_tunnel,
};
pub use readonly::{ReadError, ReadLimits, ReadResult};
pub use results::{ResultId, ResultStore, StoredResult};
pub use session::{CoreEvent, Session, StatementError};
pub use sort::{SortError, SortKey};
pub use spelunk_store::HistoryEntry;
