//! SpelaionDB core (ADR-002).
//!
//! It does not depend on Tauri: the desktop app, the CLI and the MCP server are thin
//! adapters over this crate. Everything that has to be the same for all of them lives here.

mod catalog;
mod classify;
mod conn_test;
mod cursor;
mod edit;
mod export;
mod filter;
mod find;
mod generate;
mod guard;
mod health;
mod history;
pub mod kube;
mod objects;
mod open;
mod params;
mod processes;
mod query_page;
mod readonly;
mod results;
mod session;
mod sort;
mod table_data;
mod transaction;
mod write;

pub use catalog::{EngineAccess, EngineCategory, EngineInfo, engine_catalog};
pub use classify::{ReadKind, classify_read_only};
pub use conn_test::{
    FailureKind, REDACTED, Redactor, StepFailure, StepKind, StepObserver, StepStatus, TestOptions,
    TestReport, TestScope, TestStep, classify_driver, classify_ssh, test_connection,
    test_network_profile,
};
pub use cursor::statement_at;
pub use edit::{
    CellInput, EditError, RowChange, apply_edits, check_edit_policy, edit_statements,
    editable_query_source, editable_query_table,
};
pub use export::{
    CopyError, ExportFormat, ExportOptions, GenericDialect, MAX_IN_LIST, NameStyle, column_in_list,
    column_names, export_to_file, write_result,
};
pub use filter::{ColumnFilter, FilterOp};
pub use find::{FindMatch, FindMatches, FindQuery, FindTicket, MAX_MATCHES};
pub use generate::{SqlTemplate, count_rows, join_sql, table_sql, table_templates};
pub use guard::{NeedsConfirmation, Risk, RiskyStatement, check_script, script_risks};
pub use health::{KEEPALIVE_ANSWER_TIMEOUT, SessionHealth};
pub use history::{HistoryError, QueryHistory};
pub use objects::{
    CreateKind, FoundObject, NewIndexKind, NewTableElement, ObjectActions, ObjectChange,
    ObjectError, ObjectTarget, SEARCH_LIMIT, active_schema, apply_change, can_cascade,
    change_script, change_statements, create_sql, new_index_sql, new_table_element_sql,
    object_actions, qualified_name, search_objects, set_active_schema, supports_added_checks,
};
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
pub use spelaion_store::{
    Bookmark, ExplorerFilter, ExplorerPreferences, FilterKind, GridPreferences,
};
pub use spelaion_store::{HistoryEntry, Snippet};
pub use table_data::{
    ConditionError, MAX_PAGE_ROWS, PageError, TablePage, add_condition, count_table_rows,
    table_count_sql, table_page_sql, typed_value, value_condition,
};
pub use transaction::{TransactionError, TransactionOutcome, TransactionState, TransactionStatus};
pub use write::{WriteError, WriteLimits, WriteResult, check_write};

/// Creates an empty database file for an embedded engine (SQLite, DuckDB). An existing file
/// is never touched.
pub fn create_file_database(
    engine: spelaion_config::Engine,
    path: &std::path::Path,
) -> Result<(), spelaion_driver_api::DriverError> {
    use spelaion_config::Engine;
    match engine {
        Engine::Sqlite => spelaion_driver_sqlite::create_database(path),
        Engine::Duckdb => spelaion_driver_duckdb::create_database(path),
        Engine::Postgres | Engine::Mysql | Engine::Mariadb | Engine::Sqlserver => Err(
            spelaion_driver_api::DriverError::Other(format!("{} is not a file", engine.as_str())),
        ),
    }
}
