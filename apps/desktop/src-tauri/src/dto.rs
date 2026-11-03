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

/// One engine in the "add connection" picker. `available`: it has a driver and can be used.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct EngineItem {
    pub id: String,
    pub name: String,
    /// `relational`, `analytical` or `nosql`.
    pub category: String,
    /// `network` or `file`.
    pub access: String,
    pub default_port: Option<u16>,
    pub available: bool,
}

impl From<spelunk_core::EngineInfo> for EngineItem {
    fn from(info: spelunk_core::EngineInfo) -> Self {
        Self {
            id: info.id.into(),
            name: info.name.into(),
            category: info.category.as_str().into(),
            access: match info.access {
                spelunk_core::EngineAccess::Network => "network",
                spelunk_core::EngineAccess::File => "file",
            }
            .into(),
            default_port: info.default_port,
            available: info.engine.is_some(),
        }
    }
}

/// A tunnel that a new connection can use.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TunnelItem {
    pub name: String,
    pub personal: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum PasswordMode {
    None,
    Keychain,
    Prompt,
    Env,
    /// A reference the form cannot build (`op://…`, `cmd:…`, `keychain:<name>`), kept as is.
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum TlsModeItem {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl From<spelunk_config::TlsModeConfig> for TlsModeItem {
    fn from(mode: spelunk_config::TlsModeConfig) -> Self {
        use spelunk_config::TlsModeConfig;
        match mode {
            TlsModeConfig::Disable => Self::Disable,
            TlsModeConfig::Prefer => Self::Prefer,
            TlsModeConfig::Require => Self::Require,
            TlsModeConfig::VerifyCa => Self::VerifyCa,
            TlsModeConfig::VerifyFull => Self::VerifyFull,
        }
    }
}

/// What a pasted connection URL fills in the form. The password goes only to the form's
/// password field, like a typed one.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct UrlFields {
    pub engine: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    pub password: Option<String>,
    pub tls_mode: Option<TlsModeItem>,
}

impl From<spelunk_config::UrlConnection> for UrlFields {
    fn from(c: spelunk_config::UrlConnection) -> Self {
        Self {
            engine: c.engine.as_str().into(),
            host: c.host,
            port: c.port,
            database: c.database,
            user: c.user,
            password: c.password.map(|p| p.expose().to_string()),
            tls_mode: c.tls_mode.map(TlsModeItem::from),
        }
    }
}

/// What the "add connection" form sends. The password travels apart and is never stored in
/// the connection file.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionDraftItem {
    pub personal: bool,
    pub folder: String,
    pub name: String,
    /// An available engine id from the catalog (`postgres`, `sqlite`).
    pub engine: String,
    pub env: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    pub password_mode: PasswordMode,
    pub password_env: String,
    pub password_other: String,
    pub tls_mode: TlsModeItem,
    pub tls_ca: String,
    pub tunnel: String,
    pub read_only: bool,
    pub mcp_exposed: bool,
}

impl TryFrom<ConnectionDraftItem> for spelunk_config::ConnectionDraft {
    type Error = String;

    fn try_from(item: ConnectionDraftItem) -> Result<Self, String> {
        use spelunk_config::{PasswordChoice, Source, TlsModeConfig};
        let engine = engine_named(&item.engine)?;
        Ok(Self {
            source: if item.personal {
                Source::User
            } else {
                Source::Workspace
            },
            folder: item.folder,
            name: item.name,
            engine,
            env: item.env,
            host: item.host,
            port: item.port,
            database: item.database,
            user: item.user,
            password: match item.password_mode {
                PasswordMode::None => PasswordChoice::None,
                PasswordMode::Keychain => PasswordChoice::Keychain,
                PasswordMode::Prompt => PasswordChoice::Prompt,
                PasswordMode::Env => PasswordChoice::Env(item.password_env),
                PasswordMode::Other => PasswordChoice::Other(item.password_other),
            },
            tls_mode: match item.tls_mode {
                TlsModeItem::Disable => TlsModeConfig::Disable,
                TlsModeItem::Prefer => TlsModeConfig::Prefer,
                TlsModeItem::Require => TlsModeConfig::Require,
                TlsModeItem::VerifyCa => TlsModeConfig::VerifyCa,
                TlsModeItem::VerifyFull => TlsModeConfig::VerifyFull,
            },
            tls_ca: item.tls_ca,
            tunnel: item.tunnel,
            read_only: item.read_only,
            mcp_exposed: item.mcp_exposed,
        })
    }
}

impl From<spelunk_config::ConnectionDraft> for ConnectionDraftItem {
    fn from(draft: spelunk_config::ConnectionDraft) -> Self {
        use spelunk_config::{PasswordChoice, Source};
        let (password_mode, password_env, password_other) = match draft.password {
            PasswordChoice::None => (PasswordMode::None, String::new(), String::new()),
            PasswordChoice::Keychain => (PasswordMode::Keychain, String::new(), String::new()),
            PasswordChoice::Prompt => (PasswordMode::Prompt, String::new(), String::new()),
            PasswordChoice::Env(var) => (PasswordMode::Env, var, String::new()),
            PasswordChoice::Other(reference) => (PasswordMode::Other, String::new(), reference),
        };
        Self {
            personal: match draft.source {
                Source::User => true,
                Source::Workspace => false,
            },
            folder: draft.folder,
            name: draft.name,
            engine: draft.engine.as_str().into(),
            env: draft.env,
            host: draft.host,
            port: draft.port,
            database: draft.database,
            user: draft.user,
            password_mode,
            password_env,
            password_other,
            tls_mode: draft.tls_mode.into(),
            tls_ca: draft.tls_ca,
            tunnel: draft.tunnel,
            read_only: draft.read_only,
            mcp_exposed: draft.mcp_exposed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuthItem {
    Agent,
    Key,
    Password,
    KeyboardInteractive,
}

/// One hop of the tunnel editor. The password travels apart and never reaches the file.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HopDraftItem {
    pub host: String,
    pub port: Option<u16>,
    pub user: String,
    /// Methods to try, in order.
    pub auth: Vec<SshAuthItem>,
    pub identity_file: String,
    pub password_mode: PasswordMode,
    pub password_env: String,
    pub password_other: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TunnelDraftItem {
    pub personal: bool,
    pub name: String,
    pub hops: Vec<HopDraftItem>,
}

/// Result of testing a tunnel from the editor.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum TunnelTestOutcome {
    Connected,
    /// The repository is not trusted: ask before resolving anything.
    Untrusted,
}

impl From<SshAuthItem> for spelunk_config::SshAuth {
    fn from(item: SshAuthItem) -> Self {
        match item {
            SshAuthItem::Agent => Self::Agent,
            SshAuthItem::Key => Self::Key,
            SshAuthItem::Password => Self::Password,
            SshAuthItem::KeyboardInteractive => Self::KeyboardInteractive,
        }
    }
}

impl From<spelunk_config::SshAuth> for SshAuthItem {
    fn from(auth: spelunk_config::SshAuth) -> Self {
        match auth {
            spelunk_config::SshAuth::Agent => Self::Agent,
            spelunk_config::SshAuth::Key => Self::Key,
            spelunk_config::SshAuth::Password => Self::Password,
            spelunk_config::SshAuth::KeyboardInteractive => Self::KeyboardInteractive,
        }
    }
}

fn password_choice(
    mode: PasswordMode,
    env: String,
    other: String,
) -> spelunk_config::PasswordChoice {
    use spelunk_config::PasswordChoice;
    match mode {
        PasswordMode::None => PasswordChoice::None,
        PasswordMode::Keychain => PasswordChoice::Keychain,
        PasswordMode::Prompt => PasswordChoice::Prompt,
        PasswordMode::Env => PasswordChoice::Env(env),
        PasswordMode::Other => PasswordChoice::Other(other),
    }
}

fn password_fields(choice: spelunk_config::PasswordChoice) -> (PasswordMode, String, String) {
    use spelunk_config::PasswordChoice;
    match choice {
        PasswordChoice::None => (PasswordMode::None, String::new(), String::new()),
        PasswordChoice::Keychain => (PasswordMode::Keychain, String::new(), String::new()),
        PasswordChoice::Prompt => (PasswordMode::Prompt, String::new(), String::new()),
        PasswordChoice::Env(var) => (PasswordMode::Env, var, String::new()),
        PasswordChoice::Other(reference) => (PasswordMode::Other, String::new(), reference),
    }
}

impl From<TunnelDraftItem> for spelunk_config::TunnelDraft {
    fn from(item: TunnelDraftItem) -> Self {
        use spelunk_config::{HopDraft, Source};
        Self {
            source: if item.personal {
                Source::User
            } else {
                Source::Workspace
            },
            name: item.name,
            hops: item
                .hops
                .into_iter()
                .map(|hop| HopDraft {
                    host: hop.host,
                    port: hop.port,
                    user: hop.user,
                    auth: hop.auth.into_iter().map(Into::into).collect(),
                    identity_file: hop.identity_file,
                    password: password_choice(
                        hop.password_mode,
                        hop.password_env,
                        hop.password_other,
                    ),
                })
                .collect(),
        }
    }
}

impl From<spelunk_config::TunnelDraft> for TunnelDraftItem {
    fn from(draft: spelunk_config::TunnelDraft) -> Self {
        Self {
            personal: match draft.source {
                spelunk_config::Source::User => true,
                spelunk_config::Source::Workspace => false,
            },
            name: draft.name,
            hops: draft
                .hops
                .into_iter()
                .map(|hop| {
                    let (password_mode, password_env, password_other) =
                        password_fields(hop.password);
                    HopDraftItem {
                        host: hop.host,
                        port: hop.port,
                        user: hop.user,
                        auth: hop.auth.into_iter().map(Into::into).collect(),
                        identity_file: hop.identity_file,
                        password_mode,
                        password_env,
                        password_other,
                    }
                })
                .collect(),
        }
    }
}

/// Text format to copy or export rows in.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TextFormat {
    Tsv,
    Csv,
    Json,
    SqlInsert,
    Markdown,
    InList,
}

/// The engine behind the id the UI uses (`postgres`, `mysql`…).
pub fn engine_named(name: &str) -> Result<spelunk_config::Engine, String> {
    use spelunk_config::Engine;
    Ok(match name {
        "postgres" => Engine::Postgres,
        "sqlite" => Engine::Sqlite,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        other => return Err(format!("unsupported engine: {other}")),
    })
}

/// Why a statement asks for confirmation before running (see `spelunk_core::Risk`).
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum RiskKind {
    DeleteAll,
    UpdateAll,
    Drop,
    Truncate,
    UncheckedWrite,
    ProductionWrite,
}

impl From<spelunk_core::Risk> for RiskKind {
    fn from(risk: spelunk_core::Risk) -> Self {
        use spelunk_core::Risk;
        match risk {
            Risk::DeleteAll => Self::DeleteAll,
            Risk::UpdateAll => Self::UpdateAll,
            Risk::Drop => Self::Drop,
            Risk::Truncate => Self::Truncate,
            Risk::UncheckedWrite => Self::UncheckedWrite,
            Risk::ProductionWrite => Self::ProductionWrite,
        }
    }
}

/// A statement of a script that needs confirmation; `start`/`end` in UTF-8 bytes.
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct RiskyItem {
    pub index: u32,
    pub start: u32,
    pub end: u32,
    pub risk: RiskKind,
}

impl From<spelunk_core::RiskyStatement> for RiskyItem {
    fn from(r: spelunk_core::RiskyStatement) -> Self {
        let n = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        Self {
            index: n(r.index),
            start: n(r.span.start),
            end: n(r.span.end),
            risk: r.risk.into(),
        }
    }
}

/// SQL the explorer can write for a table (see `spelunk_core::SqlTemplate`).
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum SqlTemplateItem {
    Preview,
    Select,
    Insert,
    Update,
    Delete,
}

impl From<SqlTemplateItem> for spelunk_core::SqlTemplate {
    fn from(item: SqlTemplateItem) -> Self {
        match item {
            SqlTemplateItem::Preview => Self::Preview,
            SqlTemplateItem::Select => Self::Select,
            SqlTemplateItem::Insert => Self::Insert,
            SqlTemplateItem::Update => Self::Update,
            SqlTemplateItem::Delete => Self::Delete,
        }
    }
}

/// Rows left after filtering a result, out of all it has.
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct FilterOutcome {
    pub rows: u32,
    pub total: u32,
}

/// A piece of a script, in UTF-8 bytes like every position the core gives.
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct TextSpan {
    pub start: u32,
    pub end: u32,
}

/// A rectangle of a result: rows and columns by position.
#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CellRange {
    pub first_row: u32,
    pub rows: u32,
    pub first_column: u32,
    pub columns: u32,
}
