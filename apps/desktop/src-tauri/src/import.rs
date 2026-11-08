//! Importing data into a table (ADR-038): a thin adapter. The core reads the source, detects
//! the types, writes every statement, checks read-only and the connection type's safeguards,
//! and loads the rows; these commands only carry the user's choices to it and its answers back.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::{
    ColumnKind, ColumnMapping, IfExists, ImportCancel, ImportError, ImportFormat, ImportRequest,
    ImportSource, ImportTarget, OpenConnection, OpenOptions, ParseOptions, SourceEncoding,
    open_connection,
};
use spelaion_ssh::KnownHosts;
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

use crate::dto::RiskKind;
use crate::interaction::AppInteraction;
use crate::{AppState, CmdResult};

/// Where the rows come from.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ImportSourceItem {
    File {
        path: String,
    },
    /// Pasted or clipboard text.
    Text {
        text: String,
    },
}

impl From<ImportSourceItem> for ImportSource {
    fn from(item: ImportSourceItem) -> Self {
        match item {
            ImportSourceItem::File { path } => Self::File(PathBuf::from(path)),
            ImportSourceItem::Text { text } => Self::Text(text),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ImportFormatItem {
    Csv,
    Tsv,
    Json,
    Markdown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum SourceEncodingItem {
    Auto,
    Utf8,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

/// How the source is read. Characters travel as strings: the delimiter is one character, the
/// quote and the escape one or none.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportOptionsItem {
    pub format: ImportFormatItem,
    pub encoding: SourceEncodingItem,
    pub delimiter: String,
    /// Empty: values are never quoted.
    pub quote: String,
    /// Empty: a quote inside a quoted value is written twice.
    pub escape: String,
    pub header: bool,
    /// Empty: no text is NULL.
    pub null_text: String,
    pub empty_as_null: bool,
    pub trim: bool,
    /// `strftime` patterns; empty: ISO.
    pub date_format: String,
    pub timestamp_format: String,
    /// `UTC` or an offset (`+02:00`); empty: none.
    pub time_zone: String,
}

fn one_char(what: &str, text: &str, optional: bool) -> CmdResult<Option<char>> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (None, _) if optional => Ok(None),
        (Some(c), None) => Ok(Some(c)),
        _ => Err(format!(
            "the {what} must be {} character",
            if optional {
                "at most one"
            } else {
                "exactly one"
            }
        )),
    }
}

impl TryFrom<ImportOptionsItem> for ParseOptions {
    type Error = String;

    fn try_from(item: ImportOptionsItem) -> CmdResult<Self> {
        let text = |s: String| (!s.is_empty()).then_some(s);
        Ok(Self {
            format: match item.format {
                ImportFormatItem::Csv => ImportFormat::Csv,
                ImportFormatItem::Tsv => ImportFormat::Tsv,
                ImportFormatItem::Json => ImportFormat::Json,
                ImportFormatItem::Markdown => ImportFormat::Markdown,
            },
            encoding: match item.encoding {
                SourceEncodingItem::Auto => SourceEncoding::Auto,
                SourceEncodingItem::Utf8 => SourceEncoding::Utf8,
                SourceEncodingItem::Utf16Le => SourceEncoding::Utf16Le,
                SourceEncodingItem::Utf16Be => SourceEncoding::Utf16Be,
                SourceEncodingItem::Windows1252 => SourceEncoding::Windows1252,
            },
            delimiter: one_char("separator", &item.delimiter, false)?.unwrap_or(','),
            quote: one_char("quote", &item.quote, true)?,
            escape: one_char("escape character", &item.escape, true)?,
            header: item.header,
            null_text: text(item.null_text),
            empty_as_null: item.empty_as_null,
            trim: item.trim,
            date_format: text(item.date_format),
            timestamp_format: text(item.timestamp_format),
            time_zone: text(item.time_zone),
        })
    }
}

fn encoding_item(encoding: SourceEncoding) -> SourceEncodingItem {
    match encoding {
        SourceEncoding::Auto => SourceEncodingItem::Auto,
        SourceEncoding::Utf8 => SourceEncodingItem::Utf8,
        SourceEncoding::Utf16Le => SourceEncodingItem::Utf16Le,
        SourceEncoding::Utf16Be => SourceEncodingItem::Utf16Be,
        SourceEncoding::Windows1252 => SourceEncodingItem::Windows1252,
    }
}

impl From<ParseOptions> for ImportOptionsItem {
    fn from(o: ParseOptions) -> Self {
        let text = |c: Option<char>| c.map(String::from).unwrap_or_default();
        Self {
            format: match o.format {
                ImportFormat::Csv => ImportFormatItem::Csv,
                ImportFormat::Tsv => ImportFormatItem::Tsv,
                ImportFormat::Json => ImportFormatItem::Json,
                ImportFormat::Markdown => ImportFormatItem::Markdown,
            },
            encoding: encoding_item(o.encoding),
            delimiter: o.delimiter.to_string(),
            quote: text(o.quote),
            escape: text(o.escape),
            header: o.header,
            null_text: o.null_text.unwrap_or_default(),
            empty_as_null: o.empty_as_null,
            trim: o.trim,
            date_format: o.date_format.unwrap_or_default(),
            timestamp_format: o.timestamp_format.unwrap_or_default(),
            time_zone: o.time_zone.unwrap_or_default(),
        }
    }
}

/// What a column's sampled values look like.
#[derive(Debug, Clone, Copy, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum ColumnKindItem {
    Integer,
    Decimal,
    Float,
    Boolean,
    Date,
    Timestamp,
    TimestampTz,
    Text,
}

impl From<ColumnKind> for ColumnKindItem {
    fn from(kind: ColumnKind) -> Self {
        match kind {
            ColumnKind::Integer => Self::Integer,
            ColumnKind::Decimal => Self::Decimal,
            ColumnKind::Float => Self::Float,
            ColumnKind::Boolean => Self::Boolean,
            ColumnKind::Date => Self::Date,
            ColumnKind::Timestamp => Self::Timestamp,
            ColumnKind::TimestampTz => Self::TimestampTz,
            ColumnKind::Text => Self::Text,
        }
    }
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct PreviewColumnItem {
    pub name: String,
    pub kind: ColumnKindItem,
    /// The type a new column gets on this connection's engine.
    pub sql_type: String,
    pub samples: Vec<String>,
    pub nulls: u32,
}

/// The start of the source: its columns, their types and the first rows (values cut short).
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreviewItem {
    pub options: ImportOptionsItem,
    /// The encoding the source was read in.
    pub encoding: SourceEncodingItem,
    pub columns: Vec<PreviewColumnItem>,
    pub rows: Vec<Vec<Option<String>>>,
    pub sampled: u32,
    pub more: bool,
    pub suggested_table: String,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[derive(Debug, Clone, Copy, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum IfExistsItem {
    Fail,
    Append,
    Replace,
}

#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ImportTargetItem {
    Existing {
        schema: String,
        table: String,
    },
    New {
        schema: String,
        table: String,
        if_exists: IfExistsItem,
    },
}

/// Where one column of the source goes.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ColumnMappingItem {
    Skip,
    Existing { column: String },
    New { name: String, sql_type: String },
}

/// The user's choices; the core writes every statement from them.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportRequestItem {
    pub source: ImportSourceItem,
    pub options: ImportOptionsItem,
    pub target: ImportTargetItem,
    /// One per column of the source, in its order.
    pub columns: Vec<ColumnMappingItem>,
    /// Rows per transaction; 0: one transaction.
    pub commit_every: u32,
}

impl TryFrom<ImportRequestItem> for ImportRequest {
    type Error = String;

    fn try_from(item: ImportRequestItem) -> CmdResult<Self> {
        Ok(Self {
            source: item.source.into(),
            options: item.options.try_into()?,
            target: match item.target {
                ImportTargetItem::Existing { schema, table } => {
                    ImportTarget::Existing { schema, table }
                }
                ImportTargetItem::New {
                    schema,
                    table,
                    if_exists,
                } => ImportTarget::New {
                    schema,
                    table,
                    if_exists: match if_exists {
                        IfExistsItem::Fail => IfExists::Fail,
                        IfExistsItem::Append => IfExists::Append,
                        IfExistsItem::Replace => IfExists::Replace,
                    },
                },
            },
            columns: item
                .columns
                .into_iter()
                .map(|c| match c {
                    ColumnMappingItem::Skip => ColumnMapping::Skip,
                    ColumnMappingItem::Existing { column } => ColumnMapping::Existing(column),
                    ColumnMappingItem::New { name, sql_type } => {
                        ColumnMapping::New { name, sql_type }
                    }
                })
                .collect(),
            commit_every: u64::from(item.commit_every),
        })
    }
}

/// A statement of the plan the guard asks to confirm (`statement` = the number of statements is
/// the rows' statement).
#[derive(Debug, Clone, Copy, Serialize, Type)]
pub struct ImportRiskItem {
    pub statement: u32,
    pub risk: RiskKind,
}

/// What will run, written by the core, to show before it does.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportPlanItem {
    pub statements: Vec<String>,
    pub insert: String,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub creates_table: bool,
    pub risks: Vec<ImportRiskItem>,
}

#[derive(Debug, Clone, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgressItem {
    pub read: f64,
    pub committed: f64,
}

/// How an import ended.
#[derive(Debug, Clone, Serialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ImportOutcome {
    Done {
        schema: String,
        table: String,
        rows: f64,
        created_table: bool,
        millis: f64,
    },
    /// The plan needs a confirmation: ask, then run again with `confirmed`.
    NeedsConfirmation,
    Cancelled {
        committed: f64,
    },
    Failed {
        message: String,
        /// Rows committed before the failure (they stay).
        committed: f64,
    },
}

/// Reads the start of the source and detects its columns' types for the engine of `session`.
/// `options: None` guesses them from the source.
#[tauri::command]
#[specta::specta]
pub async fn import_preview(
    session: String,
    source: ImportSourceItem,
    options: Option<ImportOptionsItem>,
    state: State<'_, AppState>,
) -> CmdResult<ImportPreviewItem> {
    let engine = state.session(&session).await?.session.dialect().name();
    let options: Option<ParseOptions> = options.map(TryInto::try_into).transpose()?;
    let source: ImportSource = source.into();
    let preview = tokio::task::spawn_blocking(move || {
        spelaion_core::preview(
            &source,
            options.as_ref(),
            spelaion_core::DEFAULT_SAMPLE_ROWS,
            engine,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(ImportPreviewItem {
        options: preview.options.into(),
        encoding: encoding_item(preview.encoding),
        columns: preview
            .columns
            .into_iter()
            .map(|c| PreviewColumnItem {
                name: c.name,
                kind: c.kind.into(),
                sql_type: c.sql_type,
                samples: c.samples,
                nulls: count(c.nulls),
            })
            .collect(),
        rows: preview.rows,
        sampled: count(preview.sampled),
        more: preview.more,
        suggested_table: preview.suggested_table,
    })
}

/// The statements an import will run, written by the core and checked against the table (read
/// on `session`, the explorer's), and what the guard asks to confirm.
#[tauri::command]
#[specta::specta]
pub async fn import_plan(
    session: String,
    request: ImportRequestItem,
    state: State<'_, AppState>,
) -> CmdResult<ImportPlanItem> {
    let open = state.session(&session).await?;
    let request: ImportRequest = request.try_into()?;
    let plan = spelaion_core::plan_import(
        &open.session,
        &request,
        &open.resolved.safeguards(),
        open.resolved.params.read_only,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(ImportPlanItem {
        risks: plan
            .risks
            .iter()
            .map(|(statement, risk)| ImportRiskItem {
                statement: count(*statement),
                risk: (*risk).into(),
            })
            .collect(),
        statements: plan.statements,
        insert: plan.insert,
        schema: plan.schema,
        table: plan.table,
        columns: plan.columns,
        creates_table: plan.creates_table,
    })
}

/// A new session for an import, opened from the `source` session's resolved connection and
/// in-memory password, and registered under `import_id` so `cancel_import` can stop it. Never a
/// tab's own session (ADR-031).
async fn open_import_session(
    state: &AppState,
    app: &AppHandle,
    source: &str,
    import_id: u32,
) -> CmdResult<(Arc<OpenConnection>, Arc<OpenConnection>, Arc<ImportCancel>)> {
    let (source_open, password) = {
        let sessions = state.sessions.lock().await;
        let source = sessions
            .get(source)
            .ok_or("the connection of this import is closed")?;
        (source.open.clone(), source.password.clone())
    };
    let cancel = Arc::new(ImportCancel::new());
    {
        let mut imports = state.imports.lock().expect("lock poisoned");
        if imports.contains_key(&import_id) {
            return Err("an import with this id is already running".into());
        }
        imports.insert(import_id, cancel.clone());
    }
    let options = OpenOptions {
        tunnels: state.tunnels.clone(),
        known_hosts: KnownHosts::for_user(&state.user_dir),
        interaction: Arc::new(AppInteraction::new(app.clone(), state.pending.clone())),
        password,
        read_only_session: false,
    };
    match open_connection(
        source_open.resolved.clone(),
        &state.engines,
        options,
        state.results.clone(),
    )
    .await
    {
        Ok(open) => {
            cancel.attach(&open.session);
            Ok((source_open, Arc::new(open), cancel))
        }
        Err(e) => {
            state
                .imports
                .lock()
                .expect("lock poisoned")
                .remove(&import_id);
            Err(e.to_string())
        }
    }
}

/// Runs an import in a session of its own: the core writes the plan again, refuses a
/// read-only connection and an unconfirmed plan that needs a confirmation, and loads the rows
/// in transactions, reporting progress on `on_progress`.
#[tauri::command]
#[specta::specta]
pub async fn run_import(
    source: String,
    import_id: u32,
    request: ImportRequestItem,
    confirmed: bool,
    on_progress: Channel<ImportProgressItem>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> CmdResult<ImportOutcome> {
    let request: ImportRequest = request.try_into()?;
    let (source_open, open, cancel) = open_import_session(&state, &app, &source, import_id).await?;
    // The new session finds names where the explorer's does (its database or schema).
    let context = spelaion_core::follow_session_context(&source_open.session, &open.session).await;
    let safeguards = source_open.resolved.safeguards();
    let read_only = source_open.resolved.params.read_only;
    drop(source_open);
    let progress = move |p: spelaion_core::ImportProgress| {
        let _ = on_progress.send(ImportProgressItem {
            read: p.read as f64,
            committed: p.committed as f64,
        });
    };
    let outcome = match context {
        Err(message) => Err(ImportError::Invalid(message)),
        Ok(()) => {
            spelaion_core::run_import(
                &open.session,
                &request,
                &safeguards,
                read_only,
                confirmed,
                &cancel,
                &progress,
            )
            .await
        }
    };
    state
        .imports
        .lock()
        .expect("lock poisoned")
        .remove(&import_id);
    if let Ok(open) = Arc::try_unwrap(open) {
        open.close().await;
    }
    Ok(match outcome {
        Ok(summary) => ImportOutcome::Done {
            schema: summary.schema,
            table: summary.table,
            rows: summary.rows as f64,
            created_table: summary.created_table,
            millis: summary.elapsed.as_millis() as f64,
        },
        Err(ImportError::NeedsConfirmation) => ImportOutcome::NeedsConfirmation,
        Err(ImportError::Cancelled { committed }) => ImportOutcome::Cancelled {
            committed: committed as f64,
        },
        Err(e @ ImportError::Failed { committed, .. }) => ImportOutcome::Failed {
            message: e.to_string(),
            committed: committed as f64,
        },
        Err(
            e @ (ImportError::Io { .. }
            | ImportError::Source(_)
            | ImportError::Invalid(_)
            | ImportError::TableExists(_)
            | ImportError::ReadOnly
            | ImportError::Database(_)
            | ImportError::Statement { .. }),
        ) => ImportOutcome::Failed {
            message: e.to_string(),
            committed: 0.0,
        },
    })
}

/// Stops a running import: the batch it is in is rolled back; committed batches stay.
#[tauri::command]
#[specta::specta]
pub async fn cancel_import(import_id: u32, state: State<'_, AppState>) -> CmdResult<()> {
    let cancel = state
        .imports
        .lock()
        .expect("lock poisoned")
        .get(&import_id)
        .cloned();
    if let Some(cancel) = cancel {
        cancel.cancel().await;
    }
    Ok(())
}
