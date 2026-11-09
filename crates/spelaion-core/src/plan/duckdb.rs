//! DuckDB's `EXPLAIN (FORMAT JSON)` and `EXPLAIN (ANALYZE, FORMAT JSON)` into the shared tree.
//! The estimated plan is an array of operators (`name`, `children`, `extra_info`); the analyzed
//! one is the profiler's output (`operator` with `type`, `timing`, `intermediate_rows`, or, from
//! older DuckDB versions, root `children` with `operator_type`, `operator_timing`,
//! `operator_cardinality` and `latency`). Only what DuckDB reports is filled in.
use super::*;
use serde_json::{Map, Value};

const NOTE: &str = "DuckDB's JSON plans are operator trees with each operator's estimated cardinality and its own details (projections, filters, join conditions, aggregates). DuckDB reports no costs. ANALYZE executes the statement and adds each operator's output rows and its own time; operators run in parallel pipelines, so an operator's time is CPU time summed over threads, and the cumulative time of a step is the sum of its own time and its children's. Execution is DuckDB's total query time. Operators run once per query, so loops are 1 and rows per loop are the operator's rows. Rows filtered are the rows a scan read minus the rows it kept. Tables are named as DuckDB writes them (catalog.schema.table). Missing metrics stay unavailable. Findings are heuristics, not automatic tuning advice.";

/// The operators DuckDB adds around the explained query when it analyzes it.
const WRAPPERS: [&str; 3] = ["RESULT_COLLECTOR", "EXECUTE", "EXPLAIN_ANALYZE"];

fn operator_type(value: &Value) -> Option<&str> {
    ["type", "operator_type", "name", "operator_name"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// An estimated plan (an array of operators) or the profiler's analyzed output.
pub(super) fn is_json(json: &Value) -> bool {
    match json {
        Value::Array(roots) => roots.first().is_some_and(|root| {
            root.get("name").is_some_and(Value::is_string)
                && (root.get("children").is_some() || root.get("extra_info").is_some())
                && root.get("Plan").is_none()
        }),
        Value::Object(map) => {
            map.get("operator").is_some_and(Value::is_array)
                || (map.get("children").is_some_and(Value::is_array)
                    && (map.contains_key("latency") || map.contains_key("query_name")))
        }
        _ => false,
    }
}

fn number(value: Option<&Value>, key: &str) -> Result<Option<f64>, PlanError> {
    let parsed = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(_) => None,
    };
    parsed
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(Some)
        .ok_or_else(|| PlanError::Invalid(format!("{key} must be a finite, non-negative number")))
}

fn first_number(value: &Value, keys: &[&str]) -> Result<Option<f64>, PlanError> {
    for key in keys {
        if let Some(n) = number(value.get(*key), key)? {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

/// A detail value as text: `"a"`, `["a", "b"]` → `a, b`, numbers as written.
fn text(value: &Value, separator: &str) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(|v| text(v, separator)).collect();
            (!parts.is_empty()).then(|| parts.join(separator))
        }
        Value::Null | Value::Object(_) => None,
    }
}

fn operation_of(node_type: &str) -> PlanOperation {
    let upper = node_type.to_ascii_uppercase();
    if upper.contains("JOIN") || upper.contains("CROSS_PRODUCT") {
        PlanOperation::Join
    } else if upper.contains("SCAN") {
        PlanOperation::Scan
    } else if upper.contains("ORDER") || upper.contains("TOP_N") || upper.contains("SORT") {
        PlanOperation::Sort
    } else if upper.contains("GROUP") || upper.contains("AGGREGATE") {
        PlanOperation::Aggregate
    } else if upper.contains("CTE") || upper.contains("MATERIALIZ") {
        PlanOperation::Temporary
    } else {
        PlanOperation::Other
    }
}

pub(super) fn parse(raw: &str, json: &Value, name: &str) -> Result<PlanDocument, PlanError> {
    let (roots, execution_ms, analyzed) = match json {
        Value::Array(roots) => (roots.as_slice(), None, false),
        Value::Object(map) => {
            let roots = map
                .get("operator")
                .or_else(|| map.get("children"))
                .and_then(Value::as_array)
                .ok_or_else(|| PlanError::Invalid("missing DuckDB operators".into()))?;
            let seconds = match map.get("query") {
                Some(query) => number(query.get("total_time"), "total_time")?,
                None => number(map.get("latency"), "latency")?,
            };
            (
                roots.as_slice(),
                seconds.and_then(|s| finite(s * 1000.0)),
                true,
            )
        }
        _ => return Err(PlanError::Invalid("expected a DuckDB JSON plan".into())),
    };
    let roots = unwrap_explain(roots);
    if roots.is_empty() {
        return Err(PlanError::Invalid("the plan has no steps".into()));
    }
    let mut nodes = Vec::new();
    if let [root] = roots {
        visit(root, None, 0, analyzed, &mut nodes)?;
    } else {
        nodes.push(PlanNode::blank(
            0,
            None,
            0,
            "Query plan".into(),
            PlanOperation::Other,
        ));
        for root in roots {
            let child = visit(root, Some(0), 1, analyzed, &mut nodes)?;
            nodes[0].children.push(child);
        }
    }
    // Cumulative time: the operator's own time plus its children's, when all are known.
    for id in (0..nodes.len()).rev() {
        let own = nodes[id].metrics.self_time_ms;
        let children: Option<Vec<f64>> = nodes[id]
            .children
            .iter()
            .map(|c| nodes[*c as usize].metrics.time_ms)
            .collect();
        nodes[id].metrics.time_ms = own
            .zip(children)
            .and_then(|(own, children)| finite(own + children.iter().sum::<f64>()));
    }
    Ok(document(
        raw,
        name,
        "duckdb",
        PlanRawFormat::Json,
        nodes,
        execution_ms,
        NOTE,
    ))
}

/// Below the wrappers DuckDB adds to an analyzed query (result collector, `EXPLAIN_ANALYZE`),
/// when the chain of single operators reaches `EXPLAIN_ANALYZE`; otherwise the roots as given.
fn unwrap_explain(roots: &[Value]) -> &[Value] {
    let mut current = roots;
    for _ in 0..WRAPPERS.len() {
        let [only] = current else {
            return roots;
        };
        let Some(kind) = operator_type(only) else {
            return roots;
        };
        if !WRAPPERS.contains(&kind) {
            return roots;
        }
        let children = only
            .get("children")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        if kind == "EXPLAIN_ANALYZE" {
            return children;
        }
        current = children;
    }
    roots
}

fn visit(
    value: &Value,
    parent: Option<u32>,
    depth: u32,
    analyzed: bool,
    nodes: &mut Vec<PlanNode>,
) -> Result<u32, PlanError> {
    if depth > MAX_PLAN_DEPTH {
        return Err(PlanError::Limit("tree is deeper than 60 levels".into()));
    }
    if nodes.len() >= MAX_PLAN_NODES {
        return Err(PlanError::Limit("tree has more than 10,000 nodes".into()));
    }
    if !value.is_object() {
        return Err(PlanError::Invalid(
            "each plan operator must be an object".into(),
        ));
    }
    let node_type = operator_type(value)
        .ok_or_else(|| PlanError::Invalid("missing DuckDB operator name".into()))?
        .to_owned();
    let children = match value.get("children") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(a)) => a.as_slice(),
        Some(_) => return Err(PlanError::Invalid("children must be an array".into())),
    };
    let empty = Map::new();
    let (info, info_text) = match value.get("extra_info") {
        None | Some(Value::Null) => (&empty, None),
        Some(Value::Object(map)) => (map, None),
        // DuckDB before 1.1 wrote its details as one text.
        Some(Value::String(s)) => (&empty, Some(s.trim().to_owned()).filter(|s| !s.is_empty())),
        Some(_) => return Err(PlanError::Invalid("extra_info must be an object".into())),
    };
    let id = nodes.len() as u32;
    let mut node = PlanNode::blank(id, parent, depth, node_type, PlanOperation::Other);
    node.operation = operation_of(&node.node_type);
    let get = |key: &str| info.get(key).and_then(|v| text(v, ", "));
    node.relation = get("Table");
    node.index = get("Index");
    node.join_type = get("Join Type");
    node.filter = info.get("Filters").and_then(|v| text(v, " AND "));
    let hash = node.node_type.to_ascii_uppercase().contains("HASH_JOIN");
    if hash {
        node.hash_condition = info.get("Conditions").and_then(|v| text(v, " AND "));
    }
    node.sequential_scan = get("Type").as_deref() == Some("Sequential Scan")
        || node.node_type.eq_ignore_ascii_case("SEQ_SCAN");
    let shown = [
        "Table",
        "Index",
        "Join Type",
        "Filters",
        "Type",
        "Estimated Cardinality",
    ];
    let mut details: Vec<String> = info
        .iter()
        .filter(|(key, _)| !shown.contains(&key.as_str()) && !(hash && *key == "Conditions"))
        .filter_map(|(key, v)| text(v, ", ").map(|v| format!("{key}: {v}")))
        .collect();
    details.extend(info_text);
    node.detail = (!details.is_empty()).then(|| details.join("; "));
    node.metrics.estimated_rows =
        number(info.get("Estimated Cardinality"), "Estimated Cardinality")?;
    if analyzed {
        let rows = first_number(
            value,
            &["intermediate_rows", "operator_cardinality", "cardinality"],
        )?;
        node.metrics.actual_rows = rows;
        node.metrics.actual_rows_per_loop = rows;
        // An operator runs once per query: its pipeline pushes every row through it.
        node.metrics.loops = rows.map(|_| 1.0);
        node.metrics.self_time_ms =
            first_number(value, &["timing", "operator_timing"])?.and_then(|s| finite(s * 1000.0));
        let scanned = first_number(value, &["rows_scanned", "operator_rows_scanned"])?;
        node.metrics.rows_removed = scanned
            .zip(rows)
            .filter(|(scanned, rows)| scanned >= rows)
            .map(|(scanned, rows)| scanned - rows);
    }
    nodes.push(node);
    for child in children {
        let child = visit(child, Some(id), depth + 1, analyzed, nodes)?;
        nodes[id as usize].children.push(child);
    }
    Ok(id)
}
