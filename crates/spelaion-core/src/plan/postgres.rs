use super::*;
use serde_json::Value;

fn number(value: &Value, key: &str) -> Result<Option<f64>, PlanError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(n) => n
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(Some)
            .ok_or_else(|| {
                PlanError::Invalid(format!("{key} must be a finite, non-negative number"))
            }),
    }
}
fn text(value: &Value, key: &str) -> Result<Option<String>, PlanError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(PlanError::Invalid(format!("{key} must be text"))),
    }
}

fn blocks(value: &Value, shared: &str, local: &str) -> Result<Option<f64>, PlanError> {
    let a = number(value, shared)?;
    let b = number(value, local)?;
    Ok(match (a, b) {
        (None, None) => None,
        (a, b) => finite(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    })
}

fn identifier(name: &str) -> String {
    let mut chars = name.chars();
    let simple = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$');
    if simple {
        name.into()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

pub(super) fn parse(raw: &str, json: &Value, name: &str) -> Result<PlanDocument, PlanError> {
    let root = match json {
        Value::Array(a) if a.len() == 1 => &a[0],
        Value::Object(_) => json,
        _ => {
            return Err(PlanError::Invalid(
                "expected one PostgreSQL JSON plan".into(),
            ));
        }
    };
    let source = root
        .get("Plan")
        .ok_or_else(|| PlanError::Invalid("missing PostgreSQL Plan object".into()))?;
    let mut nodes = Vec::new();
    visit(source, None, 0, &mut nodes, None)?;
    let analyzed = nodes.iter().any(|n: &PlanNode| n.metrics.loops.is_some());
    let buffers = nodes
        .iter()
        .any(|n| n.metrics.buffers_hit.is_some() || n.metrics.buffers_read.is_some());
    let mut doc=PlanDocument {
        raw:raw.to_owned(),
        summary:PlanSummary{
            id:0,engine:"postgres".into(),name:name.into(),raw_format:PlanRawFormat::Json,analyzed,buffers,
            total_cost:nodes.first().and_then(|n|n.metrics.cost),
            planning_ms:number(root,"Planning Time")?,execution_ms:number(root,"Execution Time")?,
            highest_cost:None,slowest:None,estimate_gap:None,sequential_scans:0,sorts:0,temporary_operations:0,
            node_count:0,depth:0,finding_count:0,operation_types:0,relation_count:0,index_count:0,
            note:"PostgreSQL JSON plans provide estimates; actual rows and timing require ANALYZE, block counters require BUFFERS. Parent cost, time and buffers include their children: they are not summed as query totals. Rows and timing per loop are multiplied by loops for totals. Self metrics subtract comparable serial children only; they are unavailable for parallel, partial or repeated inputs. Parallel time totals represent cumulative worker work, not wall time. Buffer hits/reads include shared and local blocks; temporary I/O stays separate. Names are schema-qualified when VERBOSE supplies their namespace; saved non-verbose plans can merge homonyms in name counts. Missing metrics stay unavailable. Findings are heuristics, not automatic tuning advice.".into(),
        },nodes,
    };
    doc.analyze();
    Ok(doc)
}

fn visit(
    value: &Value,
    parent: Option<u32>,
    depth: u32,
    nodes: &mut Vec<PlanNode>,
    bitmap_schema: Option<String>,
) -> Result<u32, PlanError> {
    if depth > MAX_PLAN_DEPTH {
        return Err(PlanError::Limit("tree is deeper than 60 levels".into()));
    }
    if nodes.len() >= MAX_PLAN_NODES {
        return Err(PlanError::Limit("tree has more than 10,000 nodes".into()));
    }
    if !value.is_object() {
        return Err(PlanError::Invalid(
            "each plan node must be an object".into(),
        ));
    }
    let id = nodes.len() as u32;
    let node_type = text(value, "Node Type")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PlanError::Invalid("missing Node Type".into()))?;
    let children = match value.get("Plans") {
        None => &[][..],
        Some(Value::Array(a)) => a.as_slice(),
        Some(_) => return Err(PlanError::Invalid("Plans must be an array".into())),
    };
    let loops = number(value, "Actual Loops")?;
    let per_rows = number(value, "Actual Rows")?;
    let per_time = number(value, "Actual Total Time")?;
    let cost = number(value, "Total Cost")?;
    let operation = if node_type.contains("Scan") {
        PlanOperation::Scan
    } else if node_type.contains("Join") || node_type == "Nested Loop" {
        PlanOperation::Join
    } else if node_type.contains("Sort") {
        PlanOperation::Sort
    } else if node_type.contains("Aggregate") || node_type == "Group" {
        PlanOperation::Aggregate
    } else if node_type.contains("Materialize") || node_type == "Memoize" {
        PlanOperation::Temporary
    } else {
        PlanOperation::Other
    };
    let sort_spilled =
        text(value, "Sort Space Type")?.is_some_and(|s| s.eq_ignore_ascii_case("disk"));
    let schema = text(value, "Schema")?;
    let qualify = |name: String, namespace: Option<&str>| {
        namespace.map_or_else(
            || identifier(&name),
            |s| format!("{}.{}", identifier(s), identifier(&name)),
        )
    };
    let relation = text(value, "Relation Name")?.map(|name| qualify(name, schema.as_deref()));
    let index_schema = schema.as_deref().or(bitmap_schema.as_deref());
    let index = text(value, "Index Name")?.map(|name| qualify(name, index_schema));
    // Bitmap index nodes can omit Schema even in VERBOSE output. Their namespace belongs
    // to the containing heap access, through only bitmap conjunction/disjunction nodes.
    let child_bitmap_schema = match node_type.as_str() {
        "Bitmap Heap Scan" => schema,
        "BitmapAnd" | "BitmapOr" => bitmap_schema,
        _ => None,
    };
    let sequential_scan = node_type.contains("Seq Scan");
    nodes.push(PlanNode {
        id,
        parent,
        depth,
        children: vec![],
        node_type,
        operation,
        relation,
        index,
        join_type: text(value, "Join Type")?,
        filter: text(value, "Filter")?,
        index_condition: text(value, "Index Cond")?,
        hash_condition: text(value, "Hash Cond")?,
        sort_method: text(value, "Sort Method")?,
        detail: None,
        sort_spilled,
        sequential_scan,
        parallel: value
            .get("Parallel Aware")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || number(value, "Workers Planned")?.is_some()
            || parent.is_some_and(|p| nodes[p as usize].parallel),
        metrics: PlanMetrics {
            cost,
            self_cost: children.is_empty().then_some(cost).flatten(),
            estimated_rows: number(value, "Plan Rows")?,
            actual_rows_per_loop: per_rows,
            actual_rows: per_rows.zip(loops).and_then(|(a, b)| finite(a * b)),
            time_ms: per_time.zip(loops).and_then(|(a, b)| finite(a * b)),
            self_time_ms: if children.is_empty() {
                per_time.zip(loops).and_then(|(a, b)| finite(a * b))
            } else {
                None
            },
            time_per_loop_ms: per_time,
            loops,
            buffers_hit: blocks(value, "Shared Hit Blocks", "Local Hit Blocks")?,
            buffers_read: blocks(value, "Shared Read Blocks", "Local Read Blocks")?,
            rows_removed: number(value, "Rows Removed by Filter")?,
            heap_fetches: number(value, "Heap Fetches")?,
            workers_planned: number(value, "Workers Planned")?,
            workers_launched: number(value, "Workers Launched")?,
            temporary_read: number(value, "Temp Read Blocks")?,
            temporary_written: number(value, "Temp Written Blocks")?,
            sort_space_kb: number(value, "Sort Space Used")?,
        },
        findings: vec![],
    });
    for child in children {
        let child = visit(
            child,
            Some(id),
            depth + 1,
            nodes,
            child_bitmap_schema.clone(),
        )?;
        nodes[id as usize].children.push(child);
    }
    // Only subtract disjoint serial inputs whose loop basis is the same. LIMIT,
    // nested-loop scaling, init/subplans and parallel/async work are not comparable.
    let can_derive = !nodes[id as usize].parallel
        && !matches!(
            nodes[id as usize].node_type.as_str(),
            "Limit" | "Nested Loop"
        )
        && !value
            .get("Async Capable")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && !children.is_empty()
        && (loops.is_none()
            || nodes[id as usize]
                .children
                .iter()
                .all(|child| nodes[*child as usize].metrics.loops == loops))
        && !nodes[id as usize + 1..].iter().any(|n| n.parallel)
        && children.iter().all(|v| {
            !matches!(
                v.get("Parent Relationship").and_then(Value::as_str),
                Some("InitPlan" | "SubPlan")
            )
        });
    if can_derive {
        let child_ids = nodes[id as usize].children.clone();
        let costs: Option<Vec<f64>> = child_ids
            .iter()
            .map(|i| nodes[*i as usize].metrics.cost)
            .collect();
        if let Some((total, costs)) = cost.zip(costs) {
            nodes[id as usize].metrics.self_cost =
                finite(total - costs.iter().sum::<f64>()).filter(|n| *n >= 0.0);
        }
        if loops.is_some_and(|l| l > 0.0)
            && child_ids
                .iter()
                .all(|i| nodes[*i as usize].metrics.loops == loops)
        {
            let times: Option<Vec<f64>> = child_ids
                .iter()
                .map(|i| nodes[*i as usize].metrics.time_ms)
                .collect();
            if let Some((total, times)) = nodes[id as usize].metrics.time_ms.zip(times) {
                nodes[id as usize].metrics.self_time_ms =
                    finite(total - times.iter().sum::<f64>()).filter(|n| *n >= 0.0);
            }
        }
    }
    Ok(id)
}
