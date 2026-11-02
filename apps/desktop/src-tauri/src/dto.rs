//! Types that cross to the UI. Exported to TypeScript with `tauri-specta`; no `u64`
//! (it does not fit in JS): counters and ids as `u32`, large counts as `f64`.

use serde::{Deserialize, Serialize};
use specta::Type;
use spelunk_core::CoreEvent;
use spelunk_driver_api::{ObjectInfo, ObjectKind, TableInfo};

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub workspace: Option<String>,
    pub trusted: bool,
    pub has_workspace_connections: bool,
    pub user_dir: String,
    /// Configuration files with errors, with file:line:column.
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionItem {
    /// `prod/orders`, or `~/local` if it is personal.
    pub id: String,
    pub name: String,
    pub engine: String,
    pub env: Option<String>,
    pub folders: Vec<String>,
    pub personal: bool,
    pub tunnel: Option<String>,
    pub mcp_exposed: bool,
    pub history_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    // Local row IDs, epoch milliseconds and counts are transported as JS numbers.
    #[specta(type = specta_typescript::Number)]
    pub id: i64,
    #[specta(type = specta_typescript::Number)]
    pub at_ms: i64,
    pub connection: String,
    pub statement: String,
    #[specta(type = specta_typescript::Number)]
    pub duration_ms: i64,
    #[specta(type = specta_typescript::Number)]
    pub rows: i64,
    pub error: Option<String>,
    pub cancelled: bool,
}

impl From<spelunk_core::HistoryEntry> for HistoryItem {
    fn from(entry: spelunk_core::HistoryEntry) -> Self {
        Self {
            id: entry.id,
            at_ms: entry.at_ms,
            connection: entry.connection,
            statement: entry.statement,
            duration_ms: entry.duration_ms,
            rows: entry.rows,
            error: entry.error,
            cancelled: entry.cancelled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ConnectOutcome {
    Connected {
        server_version: String,
    },
    /// The password must be asked for. `can_remember`: it can be stored in the keychain.
    PasswordRequired {
        can_remember: bool,
    },
    /// The server rejected the password (or the user): it must be asked for again.
    AuthFailed {
        message: String,
        can_remember: bool,
    },
    /// The repository is not trusted: ask before resolving anything.
    Untrusted,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnItem {
    pub name: String,
    /// Common logical type (`int`, `text`, `timestamptz`…).
    pub logical: String,
    pub native_type: String,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ScriptEvent {
    /// Byte range of the statement within the script.
    StatementStarted {
        index: u32,
        start: u32,
        end: u32,
    },
    ResultStarted {
        index: u32,
        result: u32,
        columns: Vec<ColumnItem>,
    },
    Rows {
        index: u32,
        result: u32,
        total_rows: u32,
    },
    Notice {
        index: u32,
        severity: String,
        message: String,
    },
    StatementFinished {
        index: u32,
        rows_affected: Option<f64>,
        elapsed_ms: f64,
    },
    StatementFailed {
        index: u32,
        message: String,
        code: Option<String>,
        detail: Option<String>,
        hint: Option<String>,
        /// Error position in bytes within the script, to underline it.
        script_offset: Option<u32>,
        cancelled: bool,
    },
    ScriptFinished {
        executed: u32,
        failed: bool,
    },
    HistoryWarning {
        message: String,
    },
}

fn u32_of(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl From<CoreEvent> for ScriptEvent {
    fn from(event: CoreEvent) -> Self {
        match event {
            CoreEvent::StatementStarted { index, span } => ScriptEvent::StatementStarted {
                index: u32_of(index),
                start: u32_of(span.start),
                end: u32_of(span.end),
            },
            CoreEvent::ResultStarted {
                index,
                result,
                schema,
            } => ScriptEvent::ResultStarted {
                index: u32_of(index),
                result: u32::try_from(result.0).unwrap_or(u32::MAX),
                columns: schema
                    .columns
                    .iter()
                    .map(|c| ColumnItem {
                        name: c.name.clone(),
                        logical: c.logical.as_str().into(),
                        native_type: c.native_type.clone(),
                    })
                    .collect(),
            },
            CoreEvent::Rows {
                index,
                result,
                total_rows,
            } => ScriptEvent::Rows {
                index: u32_of(index),
                result: u32::try_from(result.0).unwrap_or(u32::MAX),
                total_rows: u32_of(total_rows),
            },
            CoreEvent::Notice { index, notice } => ScriptEvent::Notice {
                index: u32_of(index),
                severity: notice.severity,
                message: notice.message,
            },
            CoreEvent::StatementFinished {
                index,
                rows_affected,
                elapsed,
            } => ScriptEvent::StatementFinished {
                index: u32_of(index),
                rows_affected: rows_affected.map(|n| n as f64),
                elapsed_ms: elapsed.as_secs_f64() * 1000.0,
            },
            CoreEvent::StatementFailed { index, error } => ScriptEvent::StatementFailed {
                index: u32_of(index),
                message: error.message,
                code: error.code,
                detail: error.detail,
                hint: error.hint,
                script_offset: error.script_offset.map(u32_of),
                cancelled: error.cancelled,
            },
            CoreEvent::ScriptFinished { executed, failed } => ScriptEvent::ScriptFinished {
                executed: u32_of(executed),
                failed,
            },
            CoreEvent::HistoryWarning { message } => ScriptEvent::HistoryWarning { message },
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SchemaItem {
    pub name: String,
    pub system: bool,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ObjectItem {
    pub schema: String,
    pub name: String,
    /// `table`, `partitioned_table`, `view`, `materialized_view`, `foreign_table`.
    pub kind: String,
    pub estimated_rows: Option<f64>,
    pub comment: Option<String>,
}

impl From<ObjectInfo> for ObjectItem {
    fn from(o: ObjectInfo) -> Self {
        let kind = match o.kind {
            ObjectKind::Table => "table",
            ObjectKind::PartitionedTable => "partitioned_table",
            ObjectKind::View => "view",
            ObjectKind::MaterializedView => "materialized_view",
            ObjectKind::ForeignTable => "foreign_table",
        };
        Self {
            schema: o.schema,
            name: o.name,
            kind: kind.into(),
            estimated_rows: o.estimated_rows.map(|n| n as f64),
            comment: o.comment,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ColumnDetails {
    pub name: String,
    pub native_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableDetails {
    pub columns: Vec<ColumnDetails>,
    pub primary_key: Vec<String>,
    pub indexes: Vec<String>,
    pub foreign_keys: Vec<String>,
}

impl From<TableInfo> for TableDetails {
    fn from(t: TableInfo) -> Self {
        Self {
            columns: t
                .columns
                .into_iter()
                .map(|c| ColumnDetails {
                    name: c.name,
                    native_type: c.native_type,
                    nullable: c.nullable,
                    default: c.default,
                    comment: c.comment,
                })
                .collect(),
            primary_key: t.primary_key,
            indexes: t.indexes.into_iter().map(|i| i.definition).collect(),
            foreign_keys: t
                .foreign_keys
                .into_iter()
                .map(|fk| {
                    format!(
                        "({}) → {}.{} ({})",
                        fk.columns.join(", "),
                        fk.referenced_schema,
                        fk.referenced_table,
                        fk.referenced_columns.join(", ")
                    )
                })
                .collect(),
        }
    }
}

/// Dialog the tunnel needs to show. The UI answers with `answer_interaction`.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRequest {
    pub id: u32,
    pub request: InteractionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InteractionKind {
    UnknownHost {
        hop: u32,
        host: String,
        port: u32,
        algorithm: String,
        fingerprint: String,
    },
    Secret {
        title: String,
        prompt: String,
        echo: bool,
        label: SecretLabel,
    },
}

/// Structured context for our own prompts; remote SSH challenges stay verbatim.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SecretLabel {
    KeyPassphrase { path: String, retry: bool },
    Password { user: String, host: String },
    Challenge { host: String, instructions: String },
}
