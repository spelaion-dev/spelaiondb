use super::*;
use spelaion_config::atomic_file::{AtomicWriter, NewFileMode};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// A bounded raw-text window. Offsets are UTF-8 bytes and always returned on boundaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct RawWindow {
    pub text: String,
    pub offset: u32,
    pub next_offset: Option<u32>,
    pub total: u32,
}

#[derive(Default)]
pub struct PlanStore {
    next: AtomicU32,
    plans: Mutex<HashMap<u32, Arc<PlanDocument>>>,
}

impl PlanStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&self, mut plan: PlanDocument) -> Result<PlanSummary, PlanError> {
        let mut plans = self.plans.lock().expect("plan lock poisoned");
        let weight = |p: &PlanDocument| {
            p.raw.len().saturating_mul(4) + p.nodes.len() * std::mem::size_of::<PlanNode>()
        };
        let held: usize = plans.values().map(|p| weight(p)).sum();
        if plans.len() >= 32 || held.saturating_add(weight(&plan)) > 64 * 1024 * 1024 {
            return Err(PlanError::Limit(
                "close an execution plan before opening another (64 MiB / 32 plans)".into(),
            ));
        }
        let id = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| PlanError::Limit("plan identifiers exhausted".into()))?;
        plan.summary.id = id;
        let summary = plan.summary.clone();
        plans.insert(id, Arc::new(plan));
        Ok(summary)
    }
    fn get(&self, id: u32) -> Result<Arc<PlanDocument>, PlanError> {
        self.plans
            .lock()
            .expect("plan lock poisoned")
            .get(&id)
            .cloned()
            .ok_or(PlanError::Missing)
    }
    pub fn release(&self, id: u32) {
        self.plans.lock().expect("plan lock poisoned").remove(&id);
    }
    pub fn summary(&self, id: u32) -> Result<PlanSummary, PlanError> {
        Ok(self.get(id)?.summary.clone())
    }
    pub fn tool_json(
        &self,
        id: u32,
        offset: usize,
        limit: usize,
        budget: usize,
    ) -> Result<String, PlanError> {
        self.get(id)?.tool_json(offset, limit, budget)
    }
    pub fn page(
        &self,
        id: u32,
        view: &PlanView,
        offset: usize,
        limit: usize,
    ) -> Result<PlanPage, PlanError> {
        self.get(id)?.page(view, offset, limit)
    }
    pub fn statistics(
        &self,
        id: u32,
        kind: PlanStatKind,
        offset: usize,
        limit: usize,
    ) -> Result<PlanStatsPage, PlanError> {
        Ok(self.get(id)?.statistics(kind, offset, limit))
    }
    pub fn raw_window(&self, id: u32, offset: usize) -> Result<RawWindow, PlanError> {
        let plan = self.get(id)?;
        if offset > plan.raw.len() || !plan.raw.is_char_boundary(offset) {
            return Err(PlanError::Invalid(
                "raw offset is not a UTF-8 boundary".into(),
            ));
        }
        let mut end = (offset + 32_768).min(plan.raw.len());
        while !plan.raw.is_char_boundary(end) {
            end -= 1;
        }
        Ok(RawWindow {
            text: plan.raw[offset..end].into(),
            offset: offset as u32,
            total: plan.raw.len() as u32,
            next_offset: (end < plan.raw.len()).then_some(end as u32),
        })
    }
    pub fn save_raw(
        &self,
        id: u32,
        path: &Path,
        cancel: &crate::ExportCancel,
    ) -> Result<u32, PlanError> {
        let plan = self.get(id)?;
        atomic(path, cancel, |out| {
            if let Some(little_endian) = plan.raw_utf16 {
                out.write_all(if little_endian {
                    b"\xFF\xFE"
                } else {
                    b"\xFE\xFF"
                })?;
                let mut bytes = Vec::with_capacity(32_768);
                for unit in plan.raw.encode_utf16() {
                    let word = if little_endian {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    };
                    bytes.extend_from_slice(&word);
                    if bytes.len() >= 32_768 {
                        check_cancel(cancel)?;
                        out.write_all(&bytes)?;
                        bytes.clear();
                    }
                }
                check_cancel(cancel)?;
                out.write_all(&bytes)?;
            } else {
                for chunk in plan.raw.as_bytes().chunks(32_768) {
                    check_cancel(cancel)?;
                    out.write_all(chunk)?;
                }
            }
            Ok(())
        })?;
        Ok(plan
            .raw_utf16
            .map_or(plan.raw.len(), |_| plan.raw.encode_utf16().count() * 2 + 2) as u32)
    }
    pub fn export_csv(
        &self,
        id: u32,
        view: &PlanView,
        path: &Path,
        cancel: &crate::ExportCancel,
    ) -> Result<u32, PlanError> {
        let plan = self.get(id)?;
        atomic(path, cancel, |out| plan.write_csv(view, out, cancel))?;
        Ok(plan.order(view)?.len() as u32)
    }
}

impl PlanDocument {
    fn order(&self, view: &PlanView) -> Result<Vec<u32>, PlanError> {
        if view.collapsed.len() > MAX_PLAN_NODES {
            return Err(PlanError::Limit("too many collapsed nodes".into()));
        }
        let collapsed: HashSet<_> = view.collapsed.iter().copied().collect();
        let mut out = Vec::with_capacity(self.nodes.len());
        fn visit(
            doc: &PlanDocument,
            id: u32,
            view: &PlanView,
            collapsed: &HashSet<u32>,
            out: &mut Vec<u32>,
        ) {
            let node = &doc.nodes[id as usize];
            if !view.findings_only || !node.findings.is_empty() {
                out.push(id);
            }
            if collapsed.contains(&id) {
                return;
            }
            let mut children = node.children.clone();
            if view.sort != PlanSort::Tree {
                children.sort_by(|a, b| {
                    compare(&doc.nodes[*a as usize], &doc.nodes[*b as usize], view)
                });
            }
            for child in children {
                visit(doc, child, view, collapsed, out);
            }
        }
        if !self.nodes.is_empty() {
            visit(self, 0, view, &collapsed, &mut out);
        }
        Ok(out)
    }
    pub fn page(
        &self,
        view: &PlanView,
        offset: usize,
        limit: usize,
    ) -> Result<PlanPage, PlanError> {
        let order = self.order(view)?;
        let offset = offset.min(order.len());
        let end = (offset + limit.clamp(1, MAX_PLAN_PAGE)).min(order.len());
        Ok(PlanPage {
            nodes: order[offset..end]
                .iter()
                .map(|id| self.nodes[*id as usize].clone())
                .collect(),
            offset: offset as u32,
            total: order.len() as u32,
            next_offset: (end < order.len()).then_some(end as u32),
        })
    }
    pub fn statistics(&self, kind: PlanStatKind, offset: usize, limit: usize) -> PlanStatsPage {
        let mut stats = BTreeMap::<String, (u32, Option<f64>)>::new();
        for node in &self.nodes {
            let name = match kind {
                PlanStatKind::Operations => Some(&node.node_type),
                PlanStatKind::Relations => node.relation.as_ref(),
                PlanStatKind::Indexes => node.index.as_ref(),
            };
            if let Some(name) = name {
                let stat = stats.entry(name.clone()).or_insert((0, Some(0.0)));
                stat.0 += 1;
                stat.1 = stat
                    .1
                    .zip(node.metrics.time_ms)
                    .and_then(|(a, b)| finite(a + b));
            }
        }
        let total = stats.len();
        let offset = offset.min(total);
        let end = (offset + limit.clamp(1, MAX_PLAN_PAGE)).min(total);
        PlanStatsPage {
            items: stats
                .into_iter()
                .skip(offset)
                .take(end - offset)
                .map(|(name, (nodes, cumulative_time_ms))| PlanStat {
                    name,
                    nodes,
                    cumulative_time_ms,
                })
                .collect(),
            offset: offset as u32,
            total: total as u32,
            next_offset: (end < total).then_some(end as u32),
        }
    }
    /// Complete JSON, at most `budget` bytes. No text slicing or second tree parser.
    /// A page that does not fit is made smaller; a single oversized node gives a clear error.
    pub fn tool_json(
        &self,
        offset: usize,
        limit: usize,
        budget: usize,
    ) -> Result<String, PlanError> {
        let mut limit = limit.clamp(1, MAX_PLAN_PAGE);
        loop {
            let page = self.page(&PlanView::default(), offset, limit)?;
            let output = serde_json::json!({"summary":self.summary,"page":page});
            let json =
                serde_json::to_string(&output).map_err(|e| PlanError::Invalid(e.to_string()))?;
            if json.len() <= budget {
                return Ok(json);
            }
            if limit == 1 {
                return Err(PlanError::Limit("one plan node exceeds the MCP response budget; open the raw plan in the desktop app".into()));
            }
            limit = (limit / 2).max(1);
        }
    }
    fn write_csv(
        &self,
        view: &PlanView,
        out: &mut dyn Write,
        cancel: &crate::ExportCancel,
    ) -> Result<(), PlanError> {
        use spelaion_driver_api::{ColumnMeta, LogicalType, Value};
        const NAMES: [&str; 31] = [
            "Node",
            "Parent",
            "Depth",
            "Operation",
            "Relation",
            "Index",
            "Join type",
            "Cost",
            "Self cost",
            "Estimated rows",
            "Actual rows per loop",
            "Actual rows total",
            "Time ms cumulative",
            "Self time ms",
            "Time ms per loop",
            "Loops",
            "Buffers hit",
            "Buffers read",
            "Rows removed per loop",
            "Heap fetches",
            "Workers planned",
            "Workers launched",
            "Temporary blocks read",
            "Temporary blocks written",
            "Sort space (kB)",
            "Sort method",
            "Filter",
            "Index condition",
            "Hash condition",
            "Detail",
            "Findings",
        ];
        let numeric = |i: usize| i < 3 || (7..25).contains(&i);
        let columns = NAMES
            .iter()
            .enumerate()
            .map(|(i, name)| ColumnMeta {
                name: (*name).into(),
                logical: if numeric(i) {
                    LogicalType::Float
                } else {
                    LogicalType::Text
                },
                native_type: "plan".into(),
                native_id: None,
                origin: None,
            })
            .collect();
        let mut writer = crate::export::RowWriter::new(
            crate::ExportFormat::Csv,
            true,
            crate::CsvOptions {
                escape_formulas: true,
                ..Default::default()
            },
            &crate::GenericDialect,
            columns,
        );
        writer.begin(out)?;
        let text = |v: &Option<String>| v.clone().map_or(Value::Null, Value::Text);
        let number = |v: Option<f64>| v.map_or(Value::Null, Value::Float);
        for id in self.order(view)? {
            check_cancel(cancel)?;
            let n = &self.nodes[id as usize];
            let m = &n.metrics;
            let values = vec![
                Value::Float(n.id.into()),
                number(n.parent.map(f64::from)),
                Value::Float(n.depth.into()),
                Value::Text(n.node_type.clone()),
                text(&n.relation),
                text(&n.index),
                text(&n.join_type),
                number(m.cost),
                number(m.self_cost),
                number(m.estimated_rows),
                number(m.actual_rows_per_loop),
                number(m.actual_rows),
                number(m.time_ms),
                number(m.self_time_ms),
                number(m.time_per_loop_ms),
                number(m.loops),
                number(m.buffers_hit),
                number(m.buffers_read),
                number(m.rows_removed),
                number(m.heap_fetches),
                number(m.workers_planned),
                number(m.workers_launched),
                number(m.temporary_read),
                number(m.temporary_written),
                number(m.sort_space_kb),
                text(&n.sort_method),
                text(&n.filter),
                text(&n.index_condition),
                text(&n.hash_condition),
                text(&n.detail),
                Value::Text(
                    n.findings
                        .iter()
                        .map(|f| f.explanation.as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                ),
            ];
            writer.row(&values, out)?;
        }
        writer.finish(out)?;
        Ok(())
    }
}

fn compare(a: &PlanNode, b: &PlanNode, view: &PlanView) -> std::cmp::Ordering {
    use std::cmp::Ordering as Cmp;
    fn metric(n: &PlanNode, key: PlanSort) -> Option<f64> {
        let m = &n.metrics;
        match key {
            PlanSort::Tree => Some(n.id.into()),
            PlanSort::NodeType | PlanSort::Relation | PlanSort::Index => None,
            PlanSort::Cost => m.cost,
            PlanSort::SelfCost => m.self_cost,
            PlanSort::EstimatedRows => m.estimated_rows,
            PlanSort::ActualRowsPerLoop => m.actual_rows_per_loop,
            PlanSort::ActualRows => m.actual_rows,
            PlanSort::Time => m.time_ms,
            PlanSort::SelfTime => m.self_time_ms,
            PlanSort::TimePerLoop => m.time_per_loop_ms,
            PlanSort::Loops => m.loops,
            PlanSort::BuffersHit => m.buffers_hit,
            PlanSort::BuffersRead => m.buffers_read,
            PlanSort::RowsRemoved => m.rows_removed,
            PlanSort::HeapFetches => m.heap_fetches,
            PlanSort::WorkersPlanned => m.workers_planned,
            PlanSort::WorkersLaunched => m.workers_launched,
            PlanSort::TemporaryRead => m.temporary_read,
            PlanSort::TemporaryWritten => m.temporary_written,
            PlanSort::SortSpace => m.sort_space_kb,
            PlanSort::Findings => Some(n.findings.len() as f64),
        }
    }
    let ord = match view.sort {
        PlanSort::NodeType => Some(a.node_type.cmp(&b.node_type)),
        PlanSort::Relation | PlanSort::Index => {
            let names = if view.sort == PlanSort::Relation {
                (a.relation.as_deref(), b.relation.as_deref())
            } else {
                (a.index.as_deref(), b.index.as_deref())
            };
            match names {
                (Some(a), Some(b)) => Some(a.cmp(b)),
                (None, None) => Some(Cmp::Equal),
                (None, Some(_)) => return Cmp::Greater,
                (Some(_), None) => return Cmp::Less,
            }
        }
        PlanSort::Tree
        | PlanSort::Cost
        | PlanSort::SelfCost
        | PlanSort::EstimatedRows
        | PlanSort::ActualRowsPerLoop
        | PlanSort::ActualRows
        | PlanSort::Time
        | PlanSort::SelfTime
        | PlanSort::TimePerLoop
        | PlanSort::Loops
        | PlanSort::BuffersHit
        | PlanSort::BuffersRead
        | PlanSort::RowsRemoved
        | PlanSort::HeapFetches
        | PlanSort::WorkersPlanned
        | PlanSort::WorkersLaunched
        | PlanSort::TemporaryRead
        | PlanSort::TemporaryWritten
        | PlanSort::SortSpace
        | PlanSort::Findings => match (metric(a, view.sort), metric(b, view.sort)) {
            (Some(a), Some(b)) => Some(a.total_cmp(&b)),
            (None, None) => Some(Cmp::Equal),
            (None, Some(_)) => return Cmp::Greater,
            (Some(_), None) => return Cmp::Less,
        },
    }
    .unwrap_or(Cmp::Equal);
    (if view.descending { ord.reverse() } else { ord }).then(a.id.cmp(&b.id))
}

fn check_cancel(cancel: &crate::ExportCancel) -> Result<(), PlanError> {
    if cancel.is_cancelled() {
        Err(PlanError::Execution(
            "Plan export cancelled: no file was written".into(),
        ))
    } else {
        Ok(())
    }
}

fn atomic(
    path: &Path,
    cancel: &crate::ExportCancel,
    write: impl FnOnce(&mut dyn Write) -> Result<(), PlanError>,
) -> Result<(), PlanError> {
    check_cancel(cancel)?;
    // Dropped on any error or cancel, which deletes the temporary file.
    let mut writer = std::io::BufWriter::new(AtomicWriter::create(path, NewFileMode::Default)?);
    write(&mut writer)?;
    check_cancel(cancel)?;
    writer.into_inner().map_err(|e| e.into_error())?.commit()?;
    Ok(())
}
