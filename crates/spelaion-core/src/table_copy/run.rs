//! The copy itself (ADR-069): the source table read inside its engine's read barrier, a batch at
//! a time, and loaded by the import into the target. Each runs on a session of its own, opened
//! and closed by the caller (never a tab's). One cancel stops both: the source's statement through
//! the engine's cancel, and the load as the import's rule says (SDB-448).

use std::time::Instant;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_driver_api::{DriverError, ExecEvent, ExecOptions, LogicalType, TableInfo, Value};

use super::{ColumnPlan, SourceColumn, plan_column, text_for};
use crate::Session;
use crate::export_stream::{ExportError, read_only_transaction};
use crate::import::{
    Batch, ColumnMapping, IfExists, ImportError, ImportPlan, ImportProgress, ImportSummary,
    ImportTarget, canonical_normalizer, resolve_columns, run_resolved,
};
use crate::job::JobCancel;
use crate::table_data::table_export_sql;

/// Rows read from the source and loaded at a time.
pub const COPY_BATCH_ROWS: usize = 5_000;

/// What a copy does with the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyHow {
    /// A new table from the source's columns, with the source's rows (`rows`) or without.
    NewTable { if_exists: IfExists, rows: bool },
    /// The rows into a table that is there, columns matched by name.
    IntoExisting,
}

/// One table to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyRequest {
    pub source_schema: String,
    pub source_table: String,
    pub target_schema: String,
    pub target_table: String,
    pub how: CopyHow,
    /// Source columns left out, by name.
    pub skip: Vec<String>,
    /// Target types chosen instead of the mapped ones, by source column name (a new table).
    pub types: Vec<(String, String)>,
    /// Rows per transaction on the target; 0: one transaction.
    pub commit_every: u64,
}

/// Why a column cannot be copied as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// Binary into a target that would store its hex text (ADR-069's point 8).
    Binary,
    /// Masked on the source connection (ADR-051): its values never leave it.
    Masked,
}

/// What a copy will do, to show before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPlan {
    /// Every source column: what it becomes, or why it is left out.
    pub columns: Vec<ColumnPlan>,
    /// Columns left out, by the user or because they cannot be copied.
    pub skipped: Vec<String>,
    /// Columns that cannot be copied and were not left out: the copy is refused until they are.
    pub blocked: Vec<(String, Blocked)>,
    /// The new table's primary key (empty when it keeps none).
    pub primary_key: Vec<String>,
    /// What the source table has that the copy does not bring (ADR-069's point 2).
    pub not_copied: NotCopied,
    /// What runs on the target, as an import would show it.
    pub target: ImportPlan,
}

/// What of the source table a copy leaves behind, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotCopied {
    pub indexes: usize,
    pub foreign_keys: usize,
    /// Unique and check constraints (the primary key is copied).
    pub constraints: usize,
    pub defaults: usize,
    /// The primary key, where the target keeps none (ClickHouse) or a key column is left out.
    pub primary_key: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("{0}")]
    Source(String),
    #[error("these columns cannot be copied as they are: {0}; leave them out to copy the rest")]
    Blocked(String),
    #[error(transparent)]
    Target(#[from] ImportError),
}

impl From<ExportError> for CopyError {
    fn from(e: ExportError) -> Self {
        CopyError::Source(e.to_string())
    }
}

impl From<DriverError> for CopyError {
    fn from(e: DriverError) -> Self {
        CopyError::Source(e.to_string())
    }
}

/// The source table as the copy reads it: its columns with their native types (with their sizes,
/// from the catalog) and logical types (from a read of no rows, inside the read barrier).
async fn source_table(
    source: &Session,
    schema: &str,
    table: &str,
) -> Result<(TableInfo, Vec<SourceColumn>, String), CopyError> {
    let info = source.connection().describe_table(schema, table).await?;
    let dialect = source.dialect().clone();
    let select = table_export_sql(dialect.as_ref(), schema, &info.object.name, "", "")
        .map_err(|e| CopyError::Source(e.to_string()))?;
    let probe = table_export_sql(dialect.as_ref(), schema, &info.object.name, "1 = 0", "")
        .map_err(|e| CopyError::Source(e.to_string()))?;
    let metas = read_only_transaction(source, result_columns(source, &probe), |_| false).await?;
    let columns = info
        .columns
        .iter()
        .map(|column| {
            let logical = metas
                .iter()
                .find(|(name, _)| name == &column.name)
                .or_else(|| {
                    metas
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(&column.name))
                })
                .map_or(LogicalType::Text, |(_, logical)| *logical);
            SourceColumn {
                name: column.name.clone(),
                native_type: column.native_type.clone(),
                logical,
                nullable: column.nullable,
            }
        })
        .collect();
    Ok((info, columns, select))
}

/// The result columns of `sql`, by name and logical type.
async fn result_columns(
    session: &Session,
    sql: &str,
) -> Result<Vec<(String, LogicalType)>, CopyError> {
    let mut events = session
        .connection()
        .execute(sql, &ExecOptions::default())
        .await?;
    let mut columns = Vec::new();
    while let Some(event) = events.next().await {
        if let ExecEvent::Columns(schema) = event? {
            columns = schema
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.logical))
                .collect();
        }
    }
    Ok(columns)
}

/// Everything the copy decides before anything runs.
struct Prepared {
    plan: CopyPlan,
    resolved: crate::import::Resolved,
    select: String,
    /// Which source columns (by position in the `SELECT`) are loaded, in the target's order.
    loaded: Vec<usize>,
    logicals: Vec<LogicalType>,
}

async fn prepare(
    source: &Session,
    target: &Session,
    request: &CopyRequest,
    safeguards: &Safeguards,
    target_read_only: bool,
) -> Result<Prepared, CopyError> {
    let (info, columns, select) =
        source_table(source, &request.source_schema, &request.source_table).await?;
    let target_engine = target.dialect().name();
    let source_engine = source.dialect().name();
    let new_table = matches!(request.how, CopyHow::NewTable { .. });
    let mut plans = Vec::new();
    let mut skipped = Vec::new();
    let mut blocked = Vec::new();
    let mut mappings = Vec::new();
    let mut loaded = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        let mut plan = plan_column(source_engine, column, target_engine);
        if let Some((_, chosen)) = request.types.iter().find(|(name, _)| name == &column.name) {
            plan.target.sql_type = chosen.trim().to_string();
        }
        let left_out = request.skip.iter().any(|name| name == &column.name);
        let masked = source
            .masking()
            .masks_table_column(&info.object.name, &column.name);
        if left_out {
            skipped.push(column.name.clone());
            mappings.push(ColumnMapping::Skip);
        } else if masked {
            blocked.push((column.name.clone(), Blocked::Masked));
            mappings.push(ColumnMapping::Skip);
        } else if plan.blocked() && !request.types.iter().any(|(name, _)| name == &column.name) {
            blocked.push((column.name.clone(), Blocked::Binary));
            mappings.push(ColumnMapping::Skip);
        } else {
            mappings.push(if new_table {
                // Nullability as the source has it; ClickHouse says it in the type itself.
                let not_null = !column.nullable && target_engine != "clickhouse";
                ColumnMapping::New {
                    name: column.name.clone(),
                    sql_type: if not_null {
                        format!("{} NOT NULL", plan.target.sql_type)
                    } else {
                        plan.target.sql_type.clone()
                    },
                }
            } else {
                ColumnMapping::Existing(column.name.clone())
            });
            loaded.push(index);
        }
        plans.push(plan);
    }
    // The primary key, when every one of its columns is copied, on a target that keeps one.
    let key_copied = info
        .primary_key
        .iter()
        .all(|key| loaded.iter().any(|&i| columns[i].name == *key));
    let keeps_key = new_table && target_engine != "clickhouse" && key_copied;
    let primary_key = if keeps_key {
        info.primary_key.clone()
    } else {
        Vec::new()
    };
    let not_copied = NotCopied {
        indexes: info.indexes.iter().filter(|i| !i.primary).count(),
        foreign_keys: info.foreign_keys.len(),
        constraints: info
            .constraints
            .iter()
            .filter(|c| c.kind != spelaion_driver_api::ConstraintKind::PrimaryKey)
            .count(),
        defaults: info.columns.iter().filter(|c| c.default.is_some()).count(),
        primary_key: new_table && !info.primary_key.is_empty() && !keeps_key,
    };
    let import_target = match request.how {
        CopyHow::NewTable { if_exists, .. } => ImportTarget::New {
            schema: request.target_schema.clone(),
            table: request.target_table.clone(),
            if_exists,
        },
        CopyHow::IntoExisting => ImportTarget::Existing {
            schema: request.target_schema.clone(),
            table: request.target_table.clone(),
        },
    };
    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let resolved = resolve_columns(
        target,
        &import_target,
        &mappings,
        &names,
        &primary_key,
        safeguards,
        target_read_only,
    )
    .await?;
    let logicals = columns.iter().map(|c| c.logical).collect();
    Ok(Prepared {
        plan: CopyPlan {
            columns: plans,
            skipped,
            blocked,
            primary_key,
            not_copied,
            target: resolved.plan.clone(),
        },
        resolved,
        select,
        loaded,
        logicals,
    })
}

/// What copying `request` from `source` to `target` will do (catalog reads and a read of no
/// rows on the source; nothing runs on the target but its catalog's reads).
pub async fn plan_copy(
    source: &Session,
    target: &Session,
    request: &CopyRequest,
    safeguards: &Safeguards,
    target_read_only: bool,
) -> Result<CopyPlan, CopyError> {
    Ok(
        prepare(source, target, request, safeguards, target_read_only)
            .await?
            .plan,
    )
}

/// What a finished copy did.
pub type CopySummary = ImportSummary;

/// Copies `request` from `source` to `target`, each a session of the copy's own. The plan is
/// written again from the request; a column that cannot be copied refuses the copy until it is
/// left out, and a target plan the guard asks to confirm runs only when `confirmed`.
#[allow(clippy::too_many_arguments)]
pub async fn run_copy(
    source: &Session,
    target: &Session,
    request: &CopyRequest,
    safeguards: &Safeguards,
    target_read_only: bool,
    confirmed: bool,
    cancel: &JobCancel,
    progress: &(dyn Fn(ImportProgress) + Send + Sync),
) -> Result<CopySummary, CopyError> {
    let started = Instant::now();
    let prepared = prepare(source, target, request, safeguards, target_read_only).await?;
    if !prepared.plan.blocked.is_empty() {
        let names: Vec<&str> = prepared
            .plan
            .blocked
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        return Err(CopyError::Blocked(names.join(", ")));
    }
    let Prepared {
        resolved,
        select,
        loaded,
        logicals,
        ..
    } = prepared;
    let normalizer = canonical_normalizer(target, &resolved);
    let target_engine = target.dialect().name();
    let rows = !matches!(request.how, CopyHow::NewTable { rows: false, .. });
    let (sender, batches) = tokio::sync::mpsc::channel::<Batch>(2);
    let (whole_sender, whole) = tokio::sync::oneshot::channel::<bool>();
    // The source's statement has a cancel of its own, passed on from the job's: the job's is bound
    // to the target while it loads (one session at a time).
    let source_cancel = JobCancel::new();
    let read = async {
        let whole = if rows && !cancel.is_cancelled() {
            match source_cancel.attach(source) {
                None => false,
                Some(_attached) => read_only_transaction(
                    source,
                    send_rows(source, &select, &loaded, &logicals, target_engine, &sender),
                    |result| matches!(result, Ok(false)),
                )
                .await
                .unwrap_or_else(|e: CopyError| {
                    // Said to the load as the source's failure, at the row it stopped at.
                    let _ = sender.try_send(Err(ImportError::Source(e.to_string())));
                    false
                }),
            }
        } else {
            true
        };
        drop(sender);
        let _ = whole_sender.send(whole);
    };
    let load = run_resolved(
        target,
        resolved,
        &normalizer,
        request.commit_every,
        confirmed,
        cancel,
        progress,
        batches,
        async move { whole.await.unwrap_or(false) },
        started,
    );
    let work = async { tokio::join!(read, load).1 };
    tokio::pin!(work);
    let passed_on = async {
        cancel.cancelled().await;
        source_cancel.cancel().await;
        std::future::pending::<()>().await
    };
    let result = tokio::select! {
        result = &mut work => result,
        () = passed_on => unreachable!("passing the cancel on never ends"),
    };
    Ok(result?)
}

/// Reads `select` on `source` and sends its rows, the `loaded` columns only, as the text the
/// target reads. `Ok(true)` once every row was sent, `Ok(false)` when the load stopped taking
/// them (it failed, or was cancelled).
async fn send_rows(
    source: &Session,
    select: &str,
    loaded: &[usize],
    logicals: &[LogicalType],
    target_engine: &str,
    sender: &tokio::sync::mpsc::Sender<Batch>,
) -> Result<bool, CopyError> {
    let mut events = source
        .connection()
        .execute(
            select,
            &ExecOptions {
                batch_size: COPY_BATCH_ROWS,
                ..Default::default()
            },
        )
        .await?;
    // The read's own types read its values: an engine that types a result by its values
    // (SQLite) says more here than the plan's read of no rows could.
    let mut logicals = logicals.to_vec();
    while let Some(event) = events.next().await {
        let batch = match event? {
            ExecEvent::Columns(schema) => {
                for (i, column) in schema.columns.iter().enumerate() {
                    if let Some(logical) = logicals.get_mut(i) {
                        *logical = column.logical;
                    }
                }
                continue;
            }
            ExecEvent::Batch(batch) => batch,
            ExecEvent::Notice(_) | ExecEvent::Done { .. } => continue,
        };
        let mut rows = Vec::with_capacity(batch.num_rows());
        for row in 0..batch.num_rows() {
            let mut values = Vec::with_capacity(loaded.len());
            for &column in loaded {
                // A value the core cannot read is never sent as NULL: the copy stops there.
                let value = Value::from_array(batch.column(column).as_ref(), row, logicals[column])
                    .ok_or_else(|| {
                        CopyError::Source(format!(
                            "a value of column {} could not be read as {}",
                            batch.schema().field(column).name(),
                            logicals[column].as_str()
                        ))
                    })?;
                values.push(text_for(target_engine, &value));
            }
            rows.push(values);
        }
        if sender.send(Ok(rows)).await.is_err() {
            return Ok(false);
        }
    }
    Ok(true)
}
