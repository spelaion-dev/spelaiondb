//! SpelaionDB core (ADR-002).
//!
//! It does not depend on Tauri: the desktop app, the CLI and the MCP server are thin
//! adapters over this crate. Everything that has to be the same for all of them lives here.

mod app_log;
pub mod audit;
pub mod backup;
mod catalog;
mod chart;
mod classify;
mod column_stats;
mod conn_test;
mod cursor;
mod data_search;
mod display;
mod edit;
mod editor_snippets;
mod er;
mod export;
mod export_stream;
mod file_folder;
mod filter;
mod find;
mod generate;
mod geometry;
mod group;
mod guard;
mod health;
mod history;
mod import;
pub mod kube;
mod load_rows;
mod locks;
pub mod masking;
mod mongodb_srv;
mod objects;
mod observe;
mod open;
mod open_tabs;
mod panics;
mod params;
mod pending;
pub mod plan;
pub mod plugins;
mod processes;
mod progress;
mod query_page;
mod readonly;
mod references;
mod result_tabs;
mod results;
mod save_name;
mod script;
mod selection_stats;
mod server_family;
mod server_settings;
mod session;
mod session_setup;
mod sort;
mod sql_file;
mod sql_mode;
mod startup;
mod table_data;
mod table_filters;
mod table_stats;
mod temporal;
#[cfg(all(test, unix))]
mod test_files;
mod transaction;
mod transpose;
mod value_pick;
mod value_view;
mod variables;
mod write;

pub use app_log::{AppLog, MAX_LOG_BYTES};
pub use catalog::{EngineAccess, EngineCategory, EngineInfo, engine_catalog};
pub use chart::{
    ChartAggregate, ChartData, ChartError, ChartImageFormat, ChartKind, ChartLabel, ChartReduction,
    ChartRequest, ChartSeries, MAX_CHART_IMAGE_BYTES, MAX_CHART_POINTS, MAX_CHART_SERIES,
    MAX_LABEL_CHARS, MAX_PIE_GROUPS, MAX_PIE_SLICES, save_chart_image,
};
pub use classify::{
    ReadKind, classify_read_only, classify_read_only_connection, classify_session_setup,
};
pub use column_stats::{
    ColumnStats, ColumnStatsError, ColumnStatsRequest, ColumnStatsSql, PickableValue,
    STATS_SAMPLE_ROWS, STATS_TEXT_CHARS, Stat, TOP_VALUES, TopValue, TopValues, column_stats,
    column_stats_sql, column_top_values,
};
pub use conn_test::{
    FailureKind, REDACTED, Redactor, StepFailure, StepKind, StepObserver, StepStatus, TestOptions,
    TestReport, TestScope, TestStep, classify_driver, classify_ssh, test_connection,
    test_network_profile,
};
pub use cursor::{
    MAX_OUTLINE_ENTRIES, OUTLINE_SUMMARY_CHARS, Outline, OutlineEntry, PickedText, RunKind,
    RunPickError, RunTarget, from_cursor, neighbour_statement, outline, pick_statements,
    run_target, statement_at, statement_to_run,
};
pub use data_search::{
    ColumnMatch, DEFAULT_ROWS_PER_TABLE, DataSearchError, DataSearchEvent, DataSearchOptions,
    DataSearchRequest, DataSearchSummary, MAX_ROWS_PER_TABLE, MAX_SEARCH_HITS, MAX_SEARCH_TABLES,
    MAX_SEARCH_VALUE_CHARS, SNIPPET_CHARS, SearchColumn, SearchColumnKind, SearchHit, SearchTable,
    TableOutcome, TableSearchSql, TableStatus, column_kind, data_search, is_search_number,
    probe_sql, table_search_sql,
};
pub use display::{
    BinaryDisplay, DecimalMark, Display, DisplayError, DisplayFormat, Grouping, KIND_KEY,
    LOCAL_ZONE, MAX_PATTERN, ValueKind, ZONE_KEY, displayed_text, time_zones, window_ipc,
};
pub use edit::{
    CellInput, EditError, EditTarget, RowChange, UpdateCopyError, UpdateSource, UpdateTable,
    apply_edits, check_edit_policy, edit_statements, editable_query_source, editable_query_table,
    update_copy_table,
};
pub use editor_snippets::{
    MAX_SCRIPT_TEMPLATE, MAX_SNIPPET_BODY, MAX_SNIPPET_DESCRIPTION, MAX_SNIPPET_NAME, MAX_SNIPPETS,
    SnippetError, check_script_template, check_snippet,
};
pub use er::{
    Cardinality, DiagramError, DiagramScope, ErColumn, ErColumns, ErEdge, ErKeyMark, ErLayout,
    ErLayoutOptions, ErMetrics, ErModel, ErNode, ErPoint, ErRelation, ErRow, ErSide, ErTable,
    MAX_DIAGRAM_TABLES, MAX_DIAGRAM_TEXT_BYTES, ReadProgress, TableName, dbml, layout as er_layout,
    mermaid, read_model as read_er_model, save_diagram_text,
};
pub use export::{
    CopyError, CopySpecial, CsvOptions, ExportFormat, ExportOptions, FileSummary, GenericDialect,
    MAX_IN_LIST, MAX_NULL_TEXT, NameStyle, RowDelimiter, TextEncoding, column_in_list,
    column_names, copy_selected_rows, copy_special, copy_text, copy_updates, export_to_file,
    formula_escaped, write_result,
};
pub use export_stream::{
    DEFAULT_BATCH_ROWS, ExportCancel, ExportError, ExportProgress, ExportedFile, RerunCheck,
    StreamExport, TableSource, TablesError, export_query, export_table, export_tables, file_name,
    follow_session_context, rerun_check,
};
pub use file_folder::{FolderReport, FolderView, MAX_VIEWS as MAX_FOLDER_VIEWS, SkippedFile};
pub use filter::{ColumnFilter, FilterMode, FilterOp};
pub use find::{FindMatch, FindMatches, FindQuery, FindTicket, MAX_MATCHES};
pub use generate::{SqlTemplate, count_rows, join_sql, table_sql, table_templates};
pub use geometry::{
    Bounds, ColumnMap, Dimensions, GeoFormat, GeoValue, Geometry, MAP_MAX_POINTS, MAP_MAX_SHAPES,
    MapCrs, MapError, MapShape, Position, column_map, is_geometry_type, map_crs, read_geometry,
};
pub use group::{
    GROUP_KEY_CHARS, Group, GroupCounts, GroupError, GroupFiltered, GroupRequest,
    MAX_GROUP_COLUMNS, MAX_GROUPS_COUNTED, MAX_GROUPS_RETURNED,
};
pub use guard::{
    NeedsConfirmation, ReadOnlyRefusal, Risk, RiskyStatement, RunRefused, check_read_only,
    check_run, check_script, run_risks, script_risks,
};
pub use health::{KEEPALIVE_ANSWER_TIMEOUT, SessionHealth};
pub use history::{HistoryError, HistoryScope, QueryHistory};
pub use import::{
    ColumnKind, ColumnMapping, DEFAULT_COMMIT_EVERY, DEFAULT_SAMPLE_ROWS, IMPORT_BATCH_ROWS,
    IMPORT_PREVIEW_CHARS, IMPORT_PREVIEW_ROWS, IfExists, ImportCancel, ImportError, ImportFormat,
    ImportPlan, ImportPreview, ImportProgress, ImportRequest, ImportSource, ImportSummary,
    ImportTarget, MAX_COLUMNS, MAX_RECORD_BYTES, MAX_SAMPLE_ROWS, MAX_WHOLE_FILE, ParseOptions,
    PreviewColumn, SourceEncoding, check_sql_type, format_for_path, guess_options, plan_import,
    preview, run_import, suggested_table, text_format,
};
pub use load_rows::{
    LOAD_PAGE_ROWS, LoadCancel, LoadEstimate, LoadOutcome, LoadedRows, MAX_LOADED_ROWS,
    estimate_query_load, estimate_table_load, load_query_rows,
};
pub use locks::{BlockingNode, Lock, LockSnapshot, LockWait, blocking_tree, list_locks};
pub use masking::{MASK, MaskPolicy};
pub use objects::{
    AlterationPlan, ColumnEdit, CreateKind, DroppedConstraint, EditableColumn, FoundObject,
    GrantedPrivilege, NewColumn, NewForeignKey, NewIndex, NewIndexKind, NewPrincipal, NewTable,
    NewTableElement, ObjectActions, ObjectChange, ObjectError, ObjectTarget, PlanRisk,
    PrincipalActions, PrincipalChange, PrincipalKind, PrincipalTarget, PrivilegeChange,
    PrivilegeScope, PrivilegeScopeFields, PrivilegeTarget, SEARCH_LIMIT, SourceTarget,
    TableAlteration, TableStructure, active_schema, alteration_plan, apply_alteration,
    apply_change, apply_create_table, apply_principal_change, apply_privilege_change, can_cascade,
    change_script, change_statements, create_sql, create_table_plan, editable_source,
    new_index_sql, new_table_element_sql, object_actions, principal_actions, principal_locked,
    principal_plan, principal_privileges, privilege_options, privilege_plan,
    privilege_scope_fields, privilege_scopes, qualified_name, search_objects, set_active_schema,
    supports_added_checks, table_structure,
};
pub use open::{
    Engines, OpenConnection, OpenError, OpenOptions, Route, RouteOptions, TunnelPool,
    create_file_database, dialect_of, open_connection, open_route, test_tunnel,
};
pub use open_tabs::{RestoreNote, RestoredTab, TAB_SAVE_DELAY, TabKeeper, restore_tabs};
pub use panics::{INTERNAL_ERROR, Panicked, catch_panic, install_panic_log};
pub use params::{ParamError, bind_params, script_params};
pub use pending::{
    Block, BulkAction, CellEdit, EditOutcome, MAX_PASTE_BYTES, MAX_UNDO_STEPS, PendingEdit,
    PendingEdits, parse_block,
};
pub use processes::{Process, ProcessSnapshot, ProcessTarget, control_process, list_processes};
pub use progress::{ROWS_PROGRESS_INTERVAL, coalesce_rows};
pub use query_page::{
    append_query_page, count_query_rows, first_query_page_sql, query_count_sql, query_pages_exactly,
};
pub use readonly::{ReadError, ReadLimits, ReadResult};
pub use references::{
    FollowError, KeyDirection, KeyRef, KeyRows, KeyTarget, MAX_PREVIEW_ROWS, PREVIEW_CHARS,
    ReferenceOutcome, RowReference, TableKey, find_key, follow, key_rows, read_references,
    reference_targets, table_keys,
};
pub use result_tabs::{
    DEFAULT_RESULT_TABS, MAX_RESULT_NAME, MAX_RESULT_TABS, Placed, Placement, ResultTab,
    ResultTabs, ResultTabsError, RunResults, statement_title,
};
pub use results::{ResultId, ResultSource, ResultStore, RevealError, RevealedCell, StoredResult};
pub use save_name::{renamed_for_type, type_extension, type_pattern};
pub use selection_stats::{
    CellBlock, MAX_DISTINCT, MAX_MEDIAN_VALUES, STAT_TEXT_CHARS, SelectionStats, StatsTicket,
};
pub use server_family::ServerFamily;
pub use server_settings::{ServerSetting, SettingKind, server_settings};
pub use session::{
    AnswerError, CoreEvent, ErrorAnswer, ErrorTicket, OnError, ScriptOptions, ScriptStop,
    ScriptSummary, Session, StatementError,
};
pub use session_setup::{
    SetupError, SetupReport, SkippedSetup, check_file_setup, check_session_setup,
    isolation_statement, read_only_statement, setup_is_restricted,
};
pub use sort::{MAX_SORT_KEYS, SortKey, ViewError};
pub use spelaion_store::{
    AuditFilter, AuditRecord, AuditSession, Decision as AuditDecision, HistoryEntry, HistoryFilter,
    HistoryPage, HistoryStatus, Snippet, SnippetFilter, SnippetFolder, ToolOutput,
};
pub use spelaion_store::{
    BackupPreferences, Bookmark, DEFAULT_EDITOR_ZOOM, DEFAULT_INTERFACE_SIZE, EDITOR_ZOOMS,
    EditorPreferences, EditorSnippet, ExplorerFilter, ExplorerPreferences, FilterKind,
    FilteredTable, GridPreferences, INTERFACE_SIZES, MapPreferences, RecentFile, SavedFile,
    SavedTab, SavedTabs, ScriptErrorMode, ScriptPreferences, TILE_ATTRIBUTION_MAX, TILE_NAME_MAX,
    TILE_URL_MAX, TileServerError, check_tile_server,
};
pub use sql_file::{
    FileEncoding, FileError, FileFormat, LineEnding, MAX_FILE_BYTES, SaveCheck, SqlFile,
    content_hash, read_sql_file, write_sql_file,
};
pub use startup::{StartOverError, StartupAction, StartupDialog, StartupFailure, start_over};
pub use table_data::{
    ColumnSort, ConditionError, LastPage, MAX_PAGE_ROWS, OrderError, PageError, TablePage,
    add_condition, cell_filter_value, count_table_rows, key_filter, last_page_of, last_table_page,
    order_by_columns, order_with_key, table_count_sql, table_export_sql, table_order_by,
    table_page_sql, table_page_sql_by_key, table_rows_sql_by_key, typed_value, value_condition,
};
pub use table_filters::{RefusedFilter, TableFilterError, TableFilters};
pub use table_stats::{TableStats, table_stats};
pub use transaction::{TransactionError, TransactionOutcome, TransactionState, TransactionStatus};
pub use transpose::{MAX_TRANSPOSED_ROWS, TransposeError, Transposed, transpose};
pub use value_pick::{
    ChoicePicker, ChoiceSource, KEY_LABEL_CHARS, KEY_LABEL_COLUMNS, KEY_PAGE_ROWS, KeyPicker,
    MAX_OFFSET_MINUTES, PickError, PickedTime, ReferencedPageSql, ReferencedRows, TemporalKind,
    TemporalPicker, TemporalStep, ValuePicker, check_list, picked_choice_text, picked_time_text,
    referenced_page_sql, search_referenced, temporal_picker, value_picker,
};
pub use value_view::{
    CellContent, CellView, DISPLAY_STATEMENT_CHARS, ImageFormat, LOAD_FILE_MAX, VIEW_LIMIT,
    VIEW_MAX, ValueError, file_change, pretty_xml, read_value_file, save_cell,
    statement_for_display, view_cell,
};
pub use variables::{
    MAX_VARIABLE_NAME, MAX_VARIABLE_VALUE, VariableError, is_variable_name, is_variable_value,
    script_variables, substitute_variables,
};
pub use write::{
    AgentAccess, AgentView, ReadOnlyBecause, WriteError, WriteLimits, WriteResult, agent_access,
    agent_view, check_write, has_read_barrier, runs_agent_writes,
};
