//! SpelunkDB core (ADR-002).
//!
//! It does not depend on Tauri: the desktop app, the CLI and the MCP server are thin
//! adapters over this crate. Everything that has to be the same for all of them lives here.

mod catalog;
mod classify;
mod cursor;
mod edit;
mod export;
mod filter;
mod generate;
mod guard;
mod history;
pub mod kube;
mod open;
mod params;
mod processes;
mod query_page;
mod readonly;
mod results;
mod session;
mod sort;
mod table_data;
mod write;

pub use catalog::{EngineAccess, EngineCategory, EngineInfo, engine_catalog};
pub use classify::{ReadKind, classify_read_only};
pub use cursor::statement_at;
pub use edit::{
    CellInput, EditError, RowChange, apply_edits, check_edit_policy, edit_statements,
    editable_query_source,
};
pub use export::{ExportFormat, ExportOptions, GenericDialect, export_to_file, write_result};
pub use filter::{ColumnFilter, FilterOp};
pub use generate::{SqlTemplate, count_rows, table_sql};
pub use guard::{Risk, RiskyStatement, script_risks};
pub use history::{HistoryError, QueryHistory};
pub use open::{
    Engines, OpenConnection, OpenError, OpenOptions, TunnelPool, open_connection, test_tunnel,
};
pub use params::{ParamError, bind_params, script_params};
pub use processes::{Process, ProcessSnapshot, ProcessTarget, control_process, list_processes};
pub use query_page::{append_query_page, count_query_rows, query_count_sql, query_page_sql};
pub use readonly::{ReadError, ReadLimits, ReadResult};
pub use results::{ResultId, ResultStore, StoredResult};
pub use session::{CoreEvent, Session, StatementError};
pub use sort::{SortKey, ViewError};
pub use spelunk_store::{HistoryEntry, Snippet};
pub use table_data::{
    MAX_PAGE_ROWS, PageError, TablePage, count_table_rows, table_count_sql, table_page_sql,
};
pub use write::{WriteError, WriteLimits, WriteResult, check_write};

/// Creates an empty database file for an embedded engine (SQLite, DuckDB). An existing file
/// is never touched.
pub fn create_file_database(
    engine: spelunk_config::Engine,
    path: &std::path::Path,
) -> Result<(), spelunk_driver_api::DriverError> {
    use spelunk_config::Engine;
    match engine {
        Engine::Sqlite => spelunk_driver_sqlite::create_database(path),
        Engine::Duckdb => spelunk_driver_duckdb::create_database(path),
        Engine::Postgres | Engine::Mysql | Engine::Mariadb | Engine::Sqlserver => Err(
            spelunk_driver_api::DriverError::Other(format!("{} is not a file", engine.as_str())),
        ),
    }
}
