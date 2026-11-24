//! ClickHouse's `EXPLAIN json = 1, indexes = 1, description = 1` into the shared tree (SDB-324).
//! The plan is an array with one object whose `Plan` is the root step: each step has a `Node
//! Type` (`Expression`, `Aggregating`, `Sorting`, `Join`, `ReadFromMergeTree`…), a
//! `Description` and its `Plans`. A read lists the indexes it used (`Indexes`: the primary key,
//! partitions, min-max, skip indexes), each with the parts and granules it kept out of those
//! there were. ClickHouse writes no costs, no row estimates and no timings, and has no ANALYZE
//! that runs the query through this view: only what it reports is filled in.
use super::*;
use serde_json::Value;

const NOTE: &str = "ClickHouse's plans list the query's steps in the order they feed each other, with each step's description and, for each read, the indexes it used: the primary key, the partitions, min-max and skip indexes, each with the parts and granules it kept out of those there were. A read that keeps every granule reads the whole table. ClickHouse reports no costs, row estimates or timings, and these plans are estimated only: ClickHouse has no ANALYZE that runs the query here. Missing metrics stay unavailable. Findings are heuristics, not automatic tuning advice.";

/// A ClickHouse plan: `[{"Plan": {...}}]` whose steps carry ClickHouse's own fields (a `Node
/// Id`, or a read from a table engine), which PostgreSQL's JSON plans never have.
pub(super) fn is_json(json: &Value) -> bool {
    let Some(root) = json
        .as_array()
        .and_then(|roots| roots.first())
        .and_then(|root| root.get("Plan"))
    else {
        return false;
    };
    root.get("Node Id").is_some_and(Value::is_string) || reads_a_table(root, 0)
}

/// Whether a step at most a few levels down reads from a table engine (`ReadFromMergeTree`,
/// `ReadFromStorage`…): ClickHouse before 24 wrote no `Node Id`.
fn reads_a_table(step: &Value, depth: u32) -> bool {
    if depth > 32 {
        return false;
    }
    let kind = step.get("Node Type").and_then(Value::as_str).unwrap_or("");
    kind.starts_with("ReadFrom")
        || step
            .get("Plans")
            .and_then(Value::as_array)
            .is_some_and(|plans| plans.iter().any(|p| reads_a_table(p, depth + 1)))
}

fn operation_of(node_type: &str) -> PlanOperation {
    if node_type.starts_with("ReadFrom") {
        PlanOperation::Scan
    } else if node_type.contains("Join") {
        PlanOperation::Join
    } else if node_type.contains("Sorting") {
        PlanOperation::Sort
    } else if node_type.contains("Aggregat") || node_type == "Totals" || node_type == "Rollup" {
        PlanOperation::Aggregate
    } else if node_type.starts_with("CreatingSet") || node_type.contains("Buffer") {
        PlanOperation::Temporary
    } else {
        PlanOperation::Other
    }
}

fn count(index: &Value, key: &str) -> Option<u64> {
    index.get(key).and_then(Value::as_u64)
}

/// `PrimaryKey (customer, id): (customer in [7, 7]); 1/2 parts, 1/3 granules`.
fn describe_index(index: &Value) -> Option<String> {
    let kind = index.get("Type").and_then(Value::as_str)?;
    let mut text = kind.to_owned();
    if let Some(name) = index.get("Name").and_then(Value::as_str) {
        text.push_str(&format!(" {name}"));
    }
    let keys: Vec<&str> = index
        .get("Keys")
        .and_then(Value::as_array)
        .map(|keys| keys.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !keys.is_empty() {
        text.push_str(&format!(" ({})", keys.join(", ")));
    }
    let mut kept = Vec::new();
    for (what, initial, selected) in [
        ("parts", "Initial Parts", "Selected Parts"),
        ("granules", "Initial Granules", "Selected Granules"),
    ] {
        if let (Some(initial), Some(selected)) = (count(index, initial), count(index, selected)) {
            kept.push(format!("{selected}/{initial} {what}"));
        }
    }
    if !kept.is_empty() {
        text.push_str(&format!(": {}", kept.join(", ")));
    }
    Some(text)
}

/// Whether a read's indexes kept every granule there was (or it used none): it reads the whole
/// table. A read with no granule counts at all is not judged.
fn reads_everything(indexes: &[Value]) -> Option<bool> {
    let counted: Vec<(u64, u64)> = indexes
        .iter()
        .filter_map(|i| {
            Some((
                count(i, "Initial Granules")?,
                count(i, "Selected Granules")?,
            ))
        })
        .collect();
    if indexes.is_empty() {
        return Some(true);
    }
    if counted.is_empty() {
        return None;
    }
    Some(
        counted
            .iter()
            .all(|(initial, selected)| selected >= initial),
    )
}

pub(super) fn parse(raw: &str, json: &Value, name: &str) -> Result<PlanDocument, PlanError> {
    let roots = json
        .as_array()
        .ok_or_else(|| PlanError::Invalid("expected a ClickHouse JSON plan".into()))?;
    let mut nodes = Vec::new();
    match roots.as_slice() {
        [] => return Err(PlanError::Invalid("the plan has no steps".into())),
        [root] => {
            let plan = root
                .get("Plan")
                .ok_or_else(|| PlanError::Invalid("missing ClickHouse Plan".into()))?;
            visit(plan, None, 0, &mut nodes)?;
        }
        // One plan per statement of a UNION or a multi-statement EXPLAIN: under one root.
        roots => {
            nodes.push(PlanNode::blank(
                0,
                None,
                0,
                "Query plan".into(),
                PlanOperation::Other,
            ));
            for root in roots {
                let plan = root
                    .get("Plan")
                    .ok_or_else(|| PlanError::Invalid("missing ClickHouse Plan".into()))?;
                let child = visit(plan, Some(0), 1, &mut nodes)?;
                nodes[0].children.push(child);
            }
        }
    }
    Ok(document(
        raw,
        name,
        "clickhouse",
        PlanRawFormat::Json,
        nodes,
        None,
        NOTE,
    ))
}

fn visit(
    value: &Value,
    parent: Option<u32>,
    depth: u32,
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
            "each plan step must be an object".into(),
        ));
    }
    let node_type = value
        .get("Node Type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PlanError::Invalid("missing ClickHouse Node Type".into()))?
        .to_owned();
    let children = match value.get("Plans") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(plans)) => plans.as_slice(),
        Some(_) => return Err(PlanError::Invalid("Plans must be an array".into())),
    };
    let indexes = match value.get("Indexes") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(indexes)) => indexes.as_slice(),
        Some(_) => return Err(PlanError::Invalid("Indexes must be an array".into())),
    };
    let id = nodes.len() as u32;
    let operation = operation_of(&node_type);
    let mut node = PlanNode::blank(id, parent, depth, node_type, operation);
    let description = value
        .get("Description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let mut details: Vec<String> = Vec::new();
    if operation == PlanOperation::Scan {
        // A read's description is the table it reads, `database.table`.
        node.relation = description;
        let used: Vec<String> = indexes.iter().filter_map(describe_index).collect();
        node.index = (!used.is_empty()).then(|| {
            indexes
                .iter()
                .filter_map(|i| i.get("Type").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(", ")
        });
        let conditions: Vec<&str> = indexes
            .iter()
            .filter_map(|i| i.get("Condition").and_then(Value::as_str))
            .filter(|c| !c.is_empty() && *c != "true")
            .collect();
        node.index_condition = (!conditions.is_empty()).then(|| conditions.join(" AND "));
        node.sequential_scan = reads_everything(indexes) == Some(true);
        details.extend(used);
    } else {
        details.extend(description);
    }
    if let Some(column) = value.get("Filter Column").and_then(Value::as_str) {
        node.filter = Some(column.to_owned());
    }
    if let Some(sort) = value.get("Sort Description").and_then(Value::as_array) {
        let columns: Vec<String> = sort
            .iter()
            .filter_map(|s| {
                let column = s.get("Column").and_then(Value::as_str)?;
                let descending = s.get("Ascending").and_then(Value::as_bool) == Some(false);
                Some(if descending {
                    format!("{column} DESC")
                } else {
                    column.to_owned()
                })
            })
            .collect();
        if !columns.is_empty() {
            details.push(format!("Sort: {}", columns.join(", ")));
        }
    }
    if let Some(keys) = value.get("Keys").and_then(Value::as_array) {
        let keys: Vec<&str> = keys.iter().filter_map(Value::as_str).collect();
        if !keys.is_empty() {
            details.push(format!("Keys: {}", keys.join(", ")));
        }
    }
    if let Some(limit) = value.get("Limit").and_then(Value::as_u64) {
        details.push(format!("Limit: {limit}"));
    }
    node.detail = (!details.is_empty()).then(|| details.join("; "));
    nodes.push(node);
    for child in children {
        let child = visit(child, Some(id), depth + 1, nodes)?;
        nodes[id as usize].children.push(child);
    }
    Ok(id)
}
