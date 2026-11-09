//! Execution plans: bounded source documents and engine-independent analysis.
//! Raw database rows stay in the core. Adapters receive a summary or a bounded projection.

mod mysql;
#[cfg(test)]
mod mysql_tests;
mod postgres;
mod run;
mod sqlserver;
#[cfg(test)]
mod sqlserver_tests;
mod store;
#[cfg(test)]
mod tests;

pub(crate) use mysql::Flavor;
pub(crate) use run::PreparedPlan;
pub use run::{
    ExplainOptions, ExplainPolicy, PlanCapabilities, PlanPreflight, explain, explain_preflight,
    explain_read_only, plan_capabilities,
};
pub use store::{PlanStore, RawWindow};

use serde::{Deserialize, Serialize};

pub const MAX_PLAN_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PLAN_NODES: usize = 10_000;
pub const MAX_PLAN_DEPTH: u32 = 60;
pub const MAX_PLAN_PAGE: usize = 100;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("execution plans are not available for {0} yet")]
    Unsupported(String),
    #[error("invalid execution plan: {0}")]
    Invalid(String),
    #[error("execution plan exceeds the limit: {0}")]
    Limit(String),
    #[error("this plan is no longer available; open the file or explain the query again")]
    Missing,
    #[error("{0}")]
    Execution(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum PlanOperation {
    Scan,
    Join,
    Sort,
    Aggregate,
    Temporary,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum FindingKind {
    Hotspot,
    UnderestimatedRows,
    OverestimatedRows,
    SortSpill,
    FilteredRows,
    LargeSequentialScan,
    HeapFetches,
    MissingWorkers,
    HighLoops,
    DiskReads,
    NeverExecuted,
}

/// Findings use the normalized metrics only. Engines do not supply their own heuristics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanFinding {
    pub kind: FindingKind,
    pub explanation: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanMetrics {
    pub cost: Option<f64>,
    pub self_cost: Option<f64>,
    pub estimated_rows: Option<f64>,
    pub actual_rows_per_loop: Option<f64>,
    pub actual_rows: Option<f64>,
    pub time_ms: Option<f64>,
    pub self_time_ms: Option<f64>,
    pub time_per_loop_ms: Option<f64>,
    pub loops: Option<f64>,
    pub buffers_hit: Option<f64>,
    pub buffers_read: Option<f64>,
    pub rows_removed: Option<f64>,
    pub heap_fetches: Option<f64>,
    pub workers_planned: Option<f64>,
    pub workers_launched: Option<f64>,
    pub temporary_read: Option<f64>,
    pub temporary_written: Option<f64>,
    pub sort_space_kb: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    pub id: u32,
    pub parent: Option<u32>,
    pub depth: u32,
    pub children: Vec<u32>,
    pub node_type: String,
    pub operation: PlanOperation,
    pub relation: Option<String>,
    pub index: Option<String>,
    pub join_type: Option<String>,
    pub filter: Option<String>,
    pub index_condition: Option<String>,
    pub hash_condition: Option<String>,
    pub sort_method: Option<String>,
    /// The engine's own text for this step, when it says more than the node type
    /// (MySQL's `Index lookup on c using PRIMARY (id=o.customer_id)`).
    pub detail: Option<String>,
    pub sort_spilled: bool,
    pub sequential_scan: bool,
    pub parallel: bool,
    pub metrics: PlanMetrics,
    pub findings: Vec<PlanFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub node: u32,
    pub label: String,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct OperationTime {
    pub operation: String,
    pub self_time_ms: Option<f64>,
    pub nodes: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum PlanStatKind {
    Operations,
    Relations,
    Indexes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanStat {
    pub name: String,
    pub nodes: u32,
    pub cumulative_time_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanStatsPage {
    pub items: Vec<PlanStat>,
    pub offset: u32,
    pub total: u32,
    pub next_offset: Option<u32>,
}

/// How the engine wrote the original plan, so a saved copy keeps a fitting extension.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum PlanRawFormat {
    Json,
    /// MySQL's EXPLAIN ANALYZE tree.
    Text,
    Xml,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanSummary {
    pub id: u32,
    pub engine: String,
    pub name: String,
    pub raw_format: PlanRawFormat,
    pub analyzed: bool,
    pub buffers: bool,
    pub total_cost: Option<f64>,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    pub highest_cost: Option<PlanStep>,
    pub slowest: Option<PlanStep>,
    pub estimate_gap: Option<PlanStep>,
    pub sequential_scans: u32,
    pub sorts: u32,
    pub temporary_operations: u32,
    pub node_count: u32,
    pub depth: u32,
    pub finding_count: u32,
    pub operation_types: u32,
    pub relation_count: u32,
    pub index_count: u32,
    /// A capability note provided by the core, not guessed from the engine in React.
    pub note: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum PlanSort {
    #[default]
    Tree,
    NodeType,
    Relation,
    Index,
    Cost,
    SelfCost,
    EstimatedRows,
    ActualRowsPerLoop,
    ActualRows,
    Time,
    SelfTime,
    TimePerLoop,
    Loops,
    BuffersHit,
    BuffersRead,
    RowsRemoved,
    HeapFetches,
    WorkersPlanned,
    WorkersLaunched,
    TemporaryRead,
    TemporaryWritten,
    SortSpace,
    Findings,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanView {
    pub sort: PlanSort,
    pub descending: bool,
    pub collapsed: Vec<u32>,
    pub findings_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanPage {
    pub nodes: Vec<PlanNode>,
    pub offset: u32,
    pub total: u32,
    pub next_offset: Option<u32>,
}

/// The only complete document is held in the core. Its node vector is preorder, IDs stable.
#[derive(Debug, Clone)]
pub struct PlanDocument {
    pub(crate) raw: String,
    pub(crate) nodes: Vec<PlanNode>,
    pub(crate) summary: PlanSummary,
    pub(crate) raw_utf16: Option<bool>,
}

/// Reads a PostgreSQL, MySQL, MariaDB or SQL Server plan, recognized by its shape.
pub fn parse_plan(raw: &str, name: &str) -> Result<PlanDocument, PlanError> {
    parse_plan_from(raw, name, None)
}

/// `hint` names the flavor of the session that produced the plan; the document's own keys
/// win when they tell MySQL and MariaDB apart.
pub(crate) fn parse_plan_from(
    raw: &str,
    name: &str,
    hint: Option<Flavor>,
) -> Result<PlanDocument, PlanError> {
    if raw.len() > MAX_PLAN_BYTES {
        return Err(PlanError::Limit("raw output is larger than 8 MiB".into()));
    }
    let text = raw.trim_start_matches('\u{feff}');
    if text.trim_start().starts_with('<') {
        return sqlserver::parse(raw, name);
    }
    if mysql::is_tree(text) {
        return mysql::parse_tree(raw, name);
    }
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| PlanError::Invalid(e.to_string()))?;
    if mysql::is_json(&json) {
        mysql::parse_json(raw, &json, name, hint)
    } else {
        postgres::parse(raw, &json, name)
    }
}

pub fn read_plan(path: &std::path::Path) -> Result<PlanDocument, PlanError> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((MAX_PLAN_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_PLAN_BYTES {
        return Err(PlanError::Limit("file is larger than 8 MiB".into()));
    }
    let (raw, utf16) = if let Some(rest) = bytes.strip_prefix(b"\xFF\xFE") {
        (
            crate::sql_file::decode_utf16(rest, u16::from_le_bytes)
                .map_err(|_| PlanError::Invalid("invalid UTF-16 plan file".into()))?,
            Some(true),
        )
    } else if let Some(rest) = bytes.strip_prefix(b"\xFE\xFF") {
        (
            crate::sql_file::decode_utf16(rest, u16::from_be_bytes)
                .map_err(|_| PlanError::Invalid("invalid UTF-16 plan file".into()))?,
            Some(false),
        )
    } else {
        (
            std::str::from_utf8(&bytes)
                .map_err(|_| PlanError::Invalid("plan files must be UTF-8 or UTF-16 text".into()))?
                .to_owned(),
            None,
        )
    };
    let mut plan = parse_plan(
        &raw,
        &path.file_name().unwrap_or_default().to_string_lossy(),
    )?;
    plan.raw_utf16 = utf16;
    Ok(plan)
}

impl PlanDocument {
    pub fn summary(&self) -> &PlanSummary {
        &self.summary
    }
    pub fn nodes(&self) -> &[PlanNode] {
        &self.nodes
    }
    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub(crate) fn analyze(&mut self) {
        let total_time = self
            .summary
            .execution_ms
            .or_else(|| self.nodes.first().and_then(|n| n.metrics.time_ms));
        for node in &mut self.nodes {
            node.findings = findings(node, total_time);
        }
        let step = |metric: fn(&PlanNode) -> Option<f64>| {
            self.nodes
                .iter()
                .filter_map(|n| metric(n).map(|v| (n, v)))
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(n, value)| PlanStep {
                    node: n.id,
                    label: n.node_type.clone(),
                    value,
                })
        };
        self.summary.highest_cost = step(|n| n.metrics.self_cost);
        self.summary.slowest = step(|n| (!n.parallel).then_some(n.metrics.self_time_ms).flatten());
        self.summary.estimate_gap = step(estimate_gap);
        self.summary.node_count = self.nodes.len() as u32;
        self.summary.depth = self.nodes.iter().map(|n| n.depth + 1).max().unwrap_or(0);
        self.summary.finding_count = self.nodes.iter().map(|n| n.findings.len() as u32).sum();
        self.summary.sequential_scans =
            self.nodes.iter().filter(|n| n.sequential_scan).count() as u32;
        self.summary.sorts = self
            .nodes
            .iter()
            .filter(|n| n.operation == PlanOperation::Sort)
            .count() as u32;
        self.summary.temporary_operations = self
            .nodes
            .iter()
            .filter(|n| n.operation == PlanOperation::Temporary || n.sort_spilled)
            .count() as u32;
        self.summary.relation_count = self
            .nodes
            .iter()
            .filter_map(|n| n.relation.as_deref())
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u32;
        self.summary.index_count = self
            .nodes
            .iter()
            .filter_map(|n| n.index.as_deref())
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u32;
        self.summary.operation_types = self
            .nodes
            .iter()
            .map(|n| n.node_type.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u32;
    }
}

fn finite(n: f64) -> Option<f64> {
    n.is_finite().then_some(n)
}
fn estimate_gap(n: &PlanNode) -> Option<f64> {
    if n.metrics.loops == Some(0.0) {
        return None;
    }
    let (estimated, actual) = n
        .metrics
        .estimated_rows
        .zip(n.metrics.actual_rows_per_loop)?;
    if estimated == 0.0 && actual == 0.0 {
        Some(1.0)
    } else {
        finite(estimated.max(1.0).max(actual.max(1.0)) / estimated.max(1.0).min(actual.max(1.0)))
    }
}

fn findings(node: &PlanNode, total: Option<f64>) -> Vec<PlanFinding> {
    let m = &node.metrics;
    let mut out = Vec::new();
    let mut add = |kind, explanation: &str| {
        out.push(PlanFinding {
            kind,
            explanation: explanation.into(),
        })
    };
    if m.loops == Some(0.0) {
        add(
            FindingKind::NeverExecuted,
            "This step was never executed. Check the branch, parent limit or join before tuning it.",
        );
        return out;
    }
    if !node.parallel
        && m.self_time_ms
            .zip(total)
            .is_some_and(|(time, total)| total >= 5.0 && time >= total * 0.5)
    {
        add(
            FindingKind::Hotspot,
            "This step's comparable serial self work accounts for most execution time. Inspect its inputs and predicates first.",
        );
    }
    if let Some((estimated, actual)) = m.estimated_rows.zip(m.actual_rows_per_loop) {
        if actual >= 100.0 && actual >= estimated.max(1.0) * 10.0 {
            add(
                FindingKind::UnderestimatedRows,
                "Many more rows were returned than estimated. Refresh statistics and inspect correlated predicates.",
            );
        }
        if estimated >= 100.0 && estimated >= actual.max(1.0) * 10.0 {
            add(
                FindingKind::OverestimatedRows,
                "Many fewer rows were returned than estimated. Check statistics and whether predicates are correlated.",
            );
        }
    }
    if node.sort_spilled {
        add(
            FindingKind::SortSpill,
            "The sort used disk. Reduce the rows or width to sort; consider an index or a session memory adjustment.",
        );
    }
    if m.rows_removed
        .zip(m.actual_rows_per_loop)
        .is_some_and(|(removed, rows)| removed >= 100.0 && removed >= rows * 9.0)
    {
        add(
            FindingKind::FilteredRows,
            "The filter discarded most input rows. Check a more selective access path or move the predicate earlier.",
        );
    }
    if node.sequential_scan
        && m.actual_rows_per_loop
            .zip(m.rows_removed)
            .zip(m.loops)
            .and_then(|((rows, removed), loops)| finite((rows + removed) * loops))
            .or(m.actual_rows)
            .or(m.estimated_rows)
            .is_some_and(|r| r >= 10_000.0)
    {
        add(
            FindingKind::LargeSequentialScan,
            "A sequential scan visits many rows. Check predicate selectivity before considering an index; a full scan may be appropriate.",
        );
    }
    if m.heap_fetches.is_some_and(|n| n >= 1_000.0) {
        add(
            FindingKind::HeapFetches,
            "Many heap fetches accompany the index access. Check index coverage and table maintenance before tuning the lookup.",
        );
    }
    if m.workers_planned
        .zip(m.workers_launched)
        .is_some_and(|(planned, launched)| launched < planned)
    {
        add(
            FindingKind::MissingWorkers,
            "Fewer parallel workers launched than planned. Check worker availability and server parallel limits.",
        );
    }
    if m.loops.is_some_and(|n| n >= 1_000.0) {
        add(
            FindingKind::HighLoops,
            "This step ran many times. Inspect nested-loop inputs and whether repeated lookups can be reduced.",
        );
    }
    if m.buffers_read
        .zip(m.buffers_hit)
        .is_some_and(|(read, hit)| read >= 100.0 && read > hit)
    {
        add(
            FindingKind::DiskReads,
            "More blocks were read than found in cache. Compare a warm run and inspect the amount of data accessed.",
        );
    }
    out
}
