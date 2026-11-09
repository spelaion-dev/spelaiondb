//! SQL Server Showplan XML, normalized into the common plan tree.
//!
//! XML is untrusted: reject DTDs, never resolve entities, and bound all tree construction.
use std::collections::HashMap;

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

use super::*;

const NAMESPACE: &str = "http://schemas.microsoft.com/sqlserver/2004/07/showplan";
const MAX_XML_DEPTH: usize = 1_024;
const MAX_ATTRIBUTES: usize = 256;

fn invalid(error: impl std::fmt::Display) -> PlanError {
    PlanError::Invalid(format!("invalid SQL Server Showplan XML: {error}"))
}

fn attributes(tag: &BytesStart<'_>) -> Result<HashMap<String, String>, PlanError> {
    let mut out = HashMap::new();
    for (index, attr) in tag.attributes().enumerate() {
        if index >= MAX_ATTRIBUTES {
            return Err(PlanError::Limit("too many XML attributes".into()));
        }
        let attr = attr.map_err(invalid)?;
        let key = attr.key.as_ref().to_owned();
        let value = attr
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(invalid)?
            .into_owned();
        out.insert(key, value);
    }
    Ok(out)
}

fn number(attrs: &HashMap<String, String>, key: &str) -> Result<Option<f64>, PlanError> {
    attrs
        .get(key)
        .map(|value| {
            value
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| invalid(format!("{key} must be finite and non-negative")))
        })
        .transpose()
}

#[derive(Default)]
struct Runtime {
    threads: usize,
    rows: Option<f64>,
    read: Option<f64>,
    executions: Option<f64>,
    elapsed: Option<f64>,
    physical_reads: Option<f64>,
}

fn add(slot: &mut Option<f64>, next: Option<f64>, first: bool) {
    *slot = if first {
        next
    } else {
        slot.zip(next).and_then(|(a, b)| finite(a + b))
    };
}

impl Runtime {
    fn observe(&mut self, attrs: &HashMap<String, String>) -> Result<(), PlanError> {
        let first = self.threads == 0;
        add(&mut self.rows, number(attrs, "ActualRows")?, first);
        add(&mut self.read, number(attrs, "ActualRowsRead")?, first);
        add(
            &mut self.executions,
            number(attrs, "ActualExecutions")?,
            first,
        );
        add(&mut self.elapsed, number(attrs, "ActualElapsedms")?, first);
        add(
            &mut self.physical_reads,
            number(attrs, "ActualPhysicalReads")?,
            first,
        );
        self.threads += 1;
        Ok(())
    }
}

struct Frame {
    name: String,
    node: Option<u32>,
}

struct Builder {
    nodes: Vec<PlanNode>,
    runtime: Vec<Runtime>,
    stack: Vec<Frame>,
    statement_type: Option<String>,
    statement_rows: Option<f64>,
    statement_cost: Option<f64>,
    planning_ms: Option<f64>,
    execution_ms: Option<f64>,
    saw_root: bool,
    root_closed: bool,
}

impl Builder {
    fn new() -> Self {
        Self {
            nodes: vec![],
            runtime: vec![],
            stack: vec![],
            statement_type: None,
            statement_rows: None,
            statement_cost: None,
            planning_ms: None,
            execution_ms: None,
            saw_root: false,
            root_closed: false,
        }
    }

    fn start(&mut self, tag: &BytesStart<'_>, empty: bool) -> Result<(), PlanError> {
        let local = tag.local_name();
        let name = local.as_ref().to_owned();
        let attrs = attributes(tag)?;
        if self.stack.is_empty() {
            if self.saw_root || self.root_closed || name != "ShowPlanXML" {
                return Err(invalid("expected a single ShowPlanXML root"));
            }
            let qualified = tag.name().as_ref().to_owned();
            let namespace_key = qualified
                .split_once(':')
                .map_or_else(|| "xmlns".to_owned(), |(p, _)| format!("xmlns:{p}"));
            if attrs.get(&namespace_key).map(String::as_str) != Some(NAMESPACE) {
                return Err(invalid("unexpected Showplan namespace"));
            }
            self.saw_root = true;
        }
        if self.stack.len() >= MAX_XML_DEPTH {
            return Err(PlanError::Limit("XML nesting is too deep".into()));
        }
        let parent = self.stack.last().and_then(|frame| frame.node);
        let mut current = parent;
        match name.as_str() {
            "StmtSimple" | "StmtCond" | "StmtCursor" if parent.is_none() => {
                if self.statement_type.is_some() {
                    return Err(invalid("multiple statements in one plan are not supported"));
                }
                self.statement_type = Some(
                    attrs
                        .get("StatementType")
                        .cloned()
                        .unwrap_or_else(|| "Statement".into()),
                );
                self.statement_rows = number(&attrs, "StatementEstRows")?;
                self.statement_cost = number(&attrs, "StatementSubTreeCost")?;
            }
            "QueryPlan" if parent.is_none() => {
                self.planning_ms = number(&attrs, "CompileTime")?;
            }
            "QueryTimeStats" if parent.is_none() => {
                self.execution_ms = number(&attrs, "ElapsedTime")?;
            }
            "RelOp" => {
                if self.nodes.len() >= MAX_PLAN_NODES {
                    return Err(PlanError::Limit("tree has more than 10,000 nodes".into()));
                }
                if parent.is_none() && !self.nodes.is_empty() {
                    return Err(invalid(
                        "multiple root operations in one plan are not supported",
                    ));
                }
                let depth = parent.map_or(0, |id| self.nodes[id as usize].depth + 1);
                if depth > MAX_PLAN_DEPTH {
                    return Err(PlanError::Limit("tree is deeper than 60 levels".into()));
                }
                let physical = attrs
                    .get("PhysicalOp")
                    .cloned()
                    .ok_or_else(|| invalid("RelOp has no PhysicalOp"))?;
                let logical = attrs.get("LogicalOp").cloned();
                let logical_name = logical.as_deref().unwrap_or("");
                let operation = if physical.contains("Scan") || physical.contains("Seek") {
                    PlanOperation::Scan
                } else if logical_name.contains("Join")
                    || physical == "Nested Loops"
                    || physical == "Merge Join"
                {
                    PlanOperation::Join
                } else if physical.contains("Sort") {
                    PlanOperation::Sort
                } else if logical_name.contains("Aggregate") || physical.contains("Aggregate") {
                    PlanOperation::Aggregate
                } else if physical.contains("Spool") {
                    PlanOperation::Temporary
                } else {
                    PlanOperation::Other
                };
                let io = number(&attrs, "EstimateIO")?;
                let cpu = number(&attrs, "EstimateCPU")?;
                let id = self.nodes.len() as u32;
                let parallel = attrs
                    .get("Parallel")
                    .is_some_and(|s| s == "1" || s.eq_ignore_ascii_case("true"));
                let sequential_scan = matches!(
                    physical.as_str(),
                    "Table Scan" | "Clustered Index Scan" | "Index Scan"
                );
                self.nodes.push(PlanNode {
                    id,
                    parent,
                    depth,
                    children: vec![],
                    node_type: physical,
                    operation,
                    relation: None,
                    index: None,
                    join_type: (operation == PlanOperation::Join)
                        .then_some(logical)
                        .flatten(),
                    filter: None,
                    index_condition: None,
                    hash_condition: None,
                    sort_method: None,
                    detail: None,
                    sort_spilled: false,
                    sequential_scan,
                    parallel,
                    metrics: PlanMetrics {
                        cost: number(&attrs, "EstimatedTotalSubtreeCost")?,
                        self_cost: io.zip(cpu).and_then(|(a, b)| finite(a + b)),
                        estimated_rows: number(&attrs, "EstimateRows")?,
                        ..Default::default()
                    },
                    findings: vec![],
                });
                self.runtime.push(Runtime::default());
                if let Some(parent) = parent {
                    self.nodes[parent as usize].children.push(id);
                }
                current = Some(id);
            }
            "Object" if parent.is_some() => {
                let node = &mut self.nodes[parent.unwrap() as usize];
                let relation = ["Database", "Schema", "Table"]
                    .into_iter()
                    .filter_map(|key| attrs.get(key).cloned())
                    .collect::<Vec<_>>()
                    .join(".");
                if !relation.is_empty() && node.relation.is_none() {
                    node.relation = Some(relation.clone());
                }
                if let Some(index) = attrs.get("Index") {
                    node.index = Some(if relation.is_empty() {
                        index.clone()
                    } else {
                        format!("{relation}.{index}")
                    });
                }
            }
            "RunTimeCountersPerThread" if parent.is_some() => {
                self.runtime[parent.unwrap() as usize].observe(&attrs)?;
            }
            "SpillToTempDb" | "SortSpillDetails" if parent.is_some() => {
                let node = &mut self.nodes[parent.unwrap() as usize];
                if node.operation == PlanOperation::Sort {
                    node.sort_spilled = true;
                }
            }
            "ScalarOperator" if parent.is_some() => {
                if let Some(text) = attrs.get("ScalarString") {
                    let node = &mut self.nodes[parent.unwrap() as usize];
                    if self.stack.iter().rev().any(|f| f.name == "SeekPredicates") {
                        if node.index_condition.is_none() {
                            node.index_condition = Some(text.clone());
                        }
                    } else if self.stack.iter().rev().any(|f| f.name == "ProbeResidual") {
                        if node.hash_condition.is_none() {
                            node.hash_condition = Some(text.clone());
                        }
                    } else if self.stack.iter().rev().any(|f| f.name == "Predicate")
                        && node.filter.is_none()
                    {
                        node.filter = Some(text.clone());
                    }
                }
            }
            _ => {}
        }
        if !empty {
            self.stack.push(Frame {
                name,
                node: current,
            });
        } else if self.stack.is_empty() {
            self.root_closed = true;
        }
        Ok(())
    }

    fn finish(mut self, raw: &str, name: &str) -> Result<PlanDocument, PlanError> {
        if !self.saw_root
            || !self.root_closed
            || !self.stack.is_empty()
            || self.statement_type.is_none()
        {
            return Err(invalid("incomplete plan document"));
        }
        // Constant statements may have a StmtSimple but no relational QueryPlan at all.
        if self.nodes.is_empty() {
            self.nodes.push(PlanNode {
                id: 0,
                parent: None,
                depth: 0,
                children: vec![],
                node_type: self.statement_type.unwrap_or_else(|| "Statement".into()),
                operation: PlanOperation::Other,
                relation: None,
                index: None,
                join_type: None,
                filter: None,
                index_condition: None,
                hash_condition: None,
                sort_method: None,
                detail: None,
                sort_spilled: false,
                sequential_scan: false,
                parallel: false,
                metrics: PlanMetrics {
                    cost: self.statement_cost,
                    estimated_rows: self.statement_rows,
                    ..Default::default()
                },
                findings: vec![],
            });
            self.runtime.push(Runtime::default());
        }
        let analyzed = self.runtime.iter().any(|r| r.threads > 0) || self.execution_ms.is_some();
        for (node, stats) in self.nodes.iter_mut().zip(&self.runtime) {
            if stats.threads == 0 {
                continue;
            }
            node.metrics.actual_rows = stats.rows;
            node.metrics.loops = stats.executions;
            node.metrics.actual_rows_per_loop = stats
                .rows
                .zip(stats.executions)
                .and_then(|(rows, loops)| (loops > 0.0).then(|| rows / loops))
                .and_then(finite);
            node.metrics.rows_removed = stats
                .read
                .zip(stats.rows)
                .zip(stats.executions)
                .and_then(|((read, rows), loops)| {
                    (loops > 0.0 && read >= rows).then(|| (read - rows) / loops)
                })
                .and_then(finite);
            node.metrics.buffers_read = stats.physical_reads;
            // Elapsed thread times overlap in parallel and operator times include descendants.
            if !node.parallel && stats.threads == 1 {
                node.metrics.time_ms = stats.elapsed;
                node.metrics.time_per_loop_ms = stats
                    .elapsed
                    .zip(stats.executions)
                    .and_then(|(time, loops)| (loops > 0.0).then(|| time / loops))
                    .and_then(finite);
                if node.children.is_empty() {
                    node.metrics.self_time_ms = stats.elapsed;
                }
            }
        }
        let buffers = self
            .nodes
            .iter()
            .any(|node| node.metrics.buffers_read.is_some());
        let mut doc = PlanDocument { raw: raw.to_owned(), raw_utf16: None, nodes: self.nodes,
            summary: PlanSummary { id: 0, engine: "sqlserver".into(), name: name.into(),
                raw_format: PlanRawFormat::Xml, analyzed, buffers,
                total_cost: self.statement_cost, planning_ms: self.planning_ms, execution_ms: self.execution_ms,
                highest_cost: None, slowest: None, estimate_gap: None, sequential_scans: 0,
                sorts: 0, temporary_operations: 0, node_count: 0, depth: 0, finding_count: 0,
                operation_types: 0, relation_count: 0, index_count: 0,
                note: "SQL Server Showplan XML includes optimizer costs and estimates. STATISTICS XML executes the statement and may add runtime counters. Rows and physical reads are summed across reported threads; elapsed time is shown only for a single serial thread and includes children. Missing metrics stay unavailable. A sort spill is a server warning; findings are heuristics.".into() } };
        doc.analyze();
        Ok(doc)
    }
}

pub(super) fn parse(raw: &str, name: &str) -> Result<PlanDocument, PlanError> {
    let mut reader = NsReader::from_str(raw.trim_start_matches('\u{feff}'));
    reader.config_mut().check_comments = true;
    let mut plan = Builder::new();
    let mut saw_declaration = false;
    let mut saw_content = false;
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(invalid)?;
        if matches!(event, Event::Decl(_)) {
            if saw_declaration || saw_content {
                return Err(invalid("XML declaration must be first and unique"));
            }
            saw_declaration = true;
        } else if !matches!(event, Event::Eof) {
            saw_content = true;
        }
        if matches!(event, Event::Start(_) | Event::Empty(_) | Event::End(_))
            && !matches!(namespace, ResolveResult::Bound(ns) if ns.0 == NAMESPACE)
        {
            return Err(invalid("element is outside the Showplan namespace"));
        }
        match event {
            Event::Start(tag) => plan.start(&tag, false)?,
            Event::Empty(tag) => plan.start(&tag, true)?,
            Event::End(_) => {
                if plan.stack.pop().is_none() {
                    return Err(invalid("unmatched closing tag"));
                }
                if plan.stack.is_empty() {
                    plan.root_closed = true;
                }
            }
            Event::DocType(_) => {
                return Err(invalid("DOCTYPE and external entities are not allowed"));
            }
            Event::GeneralRef(reference) => {
                let predefined =
                    quick_xml::escape::resolve_predefined_entity(reference.as_ref()).is_some();
                let character = reference.resolve_char_ref().map_err(invalid)?;
                let valid_character = character.is_some_and(|c| matches!(c, '\u{9}' | '\u{a}' | '\u{d}' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}'));
                if plan.stack.is_empty() || !(predefined || valid_character) {
                    return Err(invalid("unknown or invalid XML entity reference"));
                }
            }
            Event::CData(_) if plan.stack.is_empty() => {
                return Err(invalid("CDATA outside the root"));
            }
            Event::Text(text)
                if plan.stack.is_empty()
                    && !text.as_ref().bytes().all(|c| c.is_ascii_whitespace()) =>
            {
                return Err(invalid("text outside the root"));
            }
            Event::Eof => break,
            Event::Decl(_)
            | Event::PI(_)
            | Event::Comment(_)
            | Event::CData(_)
            | Event::Text(_) => {}
        }
    }
    plan.finish(raw, name)
}
