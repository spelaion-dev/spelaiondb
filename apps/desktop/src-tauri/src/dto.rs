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

/// A saved query; `connection` is empty when it was saved without one.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct SnippetItem {
    #[specta(type = specta_typescript::Number)]
    pub id: i64,
    pub name: String,
    pub statement: String,
    pub connection: String,
    #[specta(type = specta_typescript::Number)]
    pub updated_ms: i64,
    /// Saved outside any repository: visible from every workspace.
    pub personal: bool,
}

impl From<spelunk_core::Snippet> for SnippetItem {
    fn from(s: spelunk_core::Snippet) -> Self {
        Self {
            id: s.id,
            personal: s.workspace.is_empty(),
            name: s.name,
            statement: s.statement,
            connection: s.connection,
            updated_ms: s.updated_ms,
        }
    }
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
    /// Through `kubectl port-forward` instead of a tunnel (ADR-015); `port` is the resource's.
    pub kubernetes: Option<KubernetesDraftItem>,
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
            kubernetes: item.kubernetes.map(Into::into),
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
            kubernetes: draft.kubernetes.map(Into::into),
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

/// The Kubernetes tab of the connection form; empty fields mean the defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KubernetesDraftItem {
    pub context: String,
    pub namespace: String,
    pub resource: String,
    pub kubeconfig: String,
    pub kubectl: String,
}

impl From<KubernetesDraftItem> for spelunk_config::KubernetesDraft {
    fn from(k: KubernetesDraftItem) -> Self {
        Self {
            context: k.context,
            namespace: k.namespace,
            resource: k.resource,
            kubeconfig: k.kubeconfig,
            kubectl: k.kubectl,
        }
    }
}

/// A port of a Kubernetes resource, for the port picker.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct KubePortItem {
    pub port: u16,
    pub name: Option<String>,
    pub container: Option<String>,
}

impl From<spelunk_core::kube::ResourcePort> for KubePortItem {
    fn from(p: spelunk_core::kube::ResourcePort) -> Self {
        Self {
            port: p.port,
            name: p.name,
            container: p.container,
        }
    }
}

impl From<spelunk_config::KubernetesDraft> for KubernetesDraftItem {
    fn from(k: spelunk_config::KubernetesDraft) -> Self {
        Self {
            context: k.context,
            namespace: k.namespace,
            resource: k.resource,
            kubeconfig: k.kubeconfig,
            kubectl: k.kubectl,
        }
    }
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

/// A DBeaver connection found by `scan_dbeaver`. No secret travels: only whether DBeaver saved
/// a password.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DbeaverItem {
    pub id: String,
    pub name: String,
    pub folder: String,
    /// `postgres`, `mysql`, `mariadb`, `sqlite`; `None` when SpelunkDB cannot open it.
    pub engine: Option<String>,
    /// Why it cannot be imported, if it cannot.
    pub unsupported: Option<String>,
    /// `host:port/database`, or the file for SQLite.
    pub target: String,
    pub user: String,
    pub env: String,
    pub has_password: bool,
    /// `user@host:port` of its SSH tunnel.
    pub ssh: Option<String>,
}

impl From<&spelunk_config::DbeaverConnection> for DbeaverItem {
    fn from(c: &spelunk_config::DbeaverConnection) -> Self {
        let sqlite = c.engine == Ok(spelunk_config::Engine::Sqlite);
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            folder: c.folder.clone(),
            engine: c.engine.as_ref().ok().map(|e| e.as_str().to_string()),
            unsupported: c.engine.as_ref().err().cloned(),
            target: if sqlite {
                c.database.clone()
            } else {
                let port = c.port.map(|p| format!(":{p}")).unwrap_or_default();
                let database = if c.database.is_empty() {
                    String::new()
                } else {
                    format!("/{}", c.database)
                };
                format!("{}{port}{database}", c.host)
            },
            user: c.user.clone(),
            env: c.env.clone(),
            has_password: c.password.is_some(),
            ssh: c.ssh.as_ref().map(|s| {
                let port = s.port.map(|p| format!(":{p}")).unwrap_or_default();
                format!("{}@{}{port}", s.user, s.host)
            }),
        }
    }
}

/// What `scan_dbeaver` found, and where.
#[derive(Debug, Clone, Serialize, Type)]
pub struct DbeaverScan {
    pub dir: String,
    pub connections: Vec<DbeaverItem>,
}

/// One connection that could not be imported, and why.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ImportSkip {
    pub name: String,
    pub reason: String,
}

/// The outcome of an import: the names given to the new connections (`(DBeaver)` is added
/// when the name was taken) and those left out.
#[derive(Debug, Clone, Serialize, Type)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub tunnels: Vec<String>,
    pub skipped: Vec<ImportSkip>,
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

/// A column filter operator (see `spelunk_core::FilterOp`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum FilterOpItem {
    Eq,
    NotEq,
    Gt,
    Lt,
    Ge,
    Le,
    Like,
    NotLike,
    IsNull,
    IsNotNull,
}

/// One condition on a result column; all of them must hold.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ColumnFilterItem {
    pub column: u32,
    pub op: FilterOpItem,
    pub value: String,
}

impl From<ColumnFilterItem> for spelunk_core::ColumnFilter {
    fn from(item: ColumnFilterItem) -> Self {
        use spelunk_core::FilterOp;
        Self {
            column: item.column as usize,
            op: match item.op {
                FilterOpItem::Eq => FilterOp::Eq,
                FilterOpItem::NotEq => FilterOp::NotEq,
                FilterOpItem::Gt => FilterOp::Gt,
                FilterOpItem::Lt => FilterOp::Lt,
                FilterOpItem::Ge => FilterOp::Ge,
                FilterOpItem::Le => FilterOp::Le,
                FilterOpItem::Like => FilterOp::Like,
                FilterOpItem::NotLike => FilterOp::NotLike,
                FilterOpItem::IsNull => FilterOp::IsNull,
                FilterOpItem::IsNotNull => FilterOp::IsNotNull,
            },
            value: item.value,
        }
    }
}

/// A page of a table for the table data tab (see `spelunk_core::TablePage`).
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TablePageItem {
    pub schema: String,
    pub name: String,
    pub filter: String,
    pub order_by: String,
    pub limit: u32,
    pub offset: u32,
}

impl From<TablePageItem> for spelunk_core::TablePage {
    fn from(item: TablePageItem) -> Self {
        Self {
            schema: item.schema,
            name: item.name,
            filter: item.filter,
            order_by: item.order_by,
            limit: item.limit.into(),
            offset: item.offset.into(),
        }
    }
}

/// A value typed in the grid for a result column (`None` is NULL).
#[derive(Debug, Clone, Deserialize, Type)]
pub struct CellEditItem {
    pub column: u32,
    pub value: Option<String>,
}

/// A pending change of a table data tab (ADR-014), by position in the page shown.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RowChangeItem {
    Update { row: u32, values: Vec<CellEditItem> },
    Delete { row: u32 },
    Insert { values: Vec<CellEditItem> },
}

impl From<RowChangeItem> for spelunk_core::RowChange {
    fn from(item: RowChangeItem) -> Self {
        let values = |values: Vec<CellEditItem>| {
            values
                .into_iter()
                .map(|v| {
                    (
                        v.column as usize,
                        match v.value {
                            Some(text) => spelunk_core::CellInput::Text(text),
                            None => spelunk_core::CellInput::Null,
                        },
                    )
                })
                .collect()
        };
        match item {
            RowChangeItem::Update { row, values: v } => Self::Update {
                row: row as usize,
                values: values(v),
            },
            RowChangeItem::Delete { row } => Self::Delete { row: row as usize },
            RowChangeItem::Insert { values: v } => Self::Insert { values: values(v) },
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
