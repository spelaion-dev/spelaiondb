//! MySQL and MariaDB plans into the shared tree: `EXPLAIN FORMAT=JSON` (MySQL's format
//! versions 1 and 2, and MariaDB's), MariaDB's `ANALYZE FORMAT=JSON` and MySQL's
//! `EXPLAIN ANALYZE` text tree. Only what the engine reports is filled in; findings, overview
//! and statistics stay in `mod.rs`.
use super::*;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Mysql,
    Mariadb,
}

impl Flavor {
    fn engine(self) -> &'static str {
        match self {
            Flavor::Mysql => "mysql",
            Flavor::Mariadb => "mariadb",
        }
    }
    fn note(self) -> &'static str {
        match self {
            Flavor::Mysql => MYSQL_NOTE,
            Flavor::Mariadb => MARIADB_NOTE,
        }
    }
}

const MYSQL_NOTE: &str = "MySQL's EXPLAIN FORMAT=JSON (format version 1) lists tables in join order with estimated rows (rows examined per scan × filtered %) and each table's own read and evaluation cost; the steps around them (nested loop, grouping, ordering, materialization, subqueries) are shown as the JSON names them, without timings. Format version 2 and the EXPLAIN ANALYZE tree are operator trees with inclusive costs: ANALYZE executes the statement and adds actual rows and time per loop, multiplied by loops for totals, and parents overlap their children. Self metrics subtract comparable children only. MySQL reports no buffer, worker or sort-memory counters, and names tables by their alias in the query (format version 2 adds schema and table). Missing metrics stay unavailable. Findings are heuristics, not automatic tuning advice.";
const MARIADB_NOTE: &str = "MariaDB's JSON plans list tables in join order with estimated rows (rows per scan × filtered %) and each table's own cost; nested loops, block joins, sorts, temporary tables, materializations and subqueries are shown as the JSON names them. ANALYZE FORMAT=JSON executes the statement and adds loops, rows read per loop (× r_filtered for the rows kept; the rest count as removed by the attached condition), each table's own time and InnoDB page accesses (buffer hits; pages read from disk as reads). Query block times are totals across loops and include their children; a sort's time is its own. Tables have no inclusive time, so their parents are not sums of them. Tables are named by their alias in the query. Missing metrics stay unavailable. Findings are heuristics, not automatic tuning advice.";

/// A JSON plan this module reads: MySQL/MariaDB format (`query_block`) or MySQL's format
/// version 2 (an operator tree with `operation`).
pub(super) fn is_json(json: &Value) -> bool {
    json.get("query_block").is_some_and(Value::is_object)
        || json
            .get("operation")
            .is_some_and(|o| o.is_string() && json.get("Plan").is_none())
}

/// MySQL's `EXPLAIN ANALYZE` / `FORMAT=TREE` output starts with its first `->` step.
pub(super) fn is_tree(text: &str) -> bool {
    text.trim_start().starts_with("->")
}

fn number(value: &Value, key: &str) -> Result<Option<f64>, PlanError> {
    // MySQL writes costs and percentages as strings ("16313.58"), MariaDB as numbers.
    let parsed = match value.get(key) {
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

fn text(value: &Value, key: &str) -> Result<Option<String>, PlanError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Number(n)) => Ok(Some(n.to_string())),
        Some(_) => Err(PlanError::Invalid(format!("{key} must be text"))),
    }
}

/// `["a", "b"]` (or one string) as `a, b`.
fn list(value: &Value, key: &str, separator: &str) -> Result<Option<String>, PlanError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| PlanError::Invalid(format!("{key} must list text")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|items| (!items.is_empty()).then(|| items.join(separator))),
        Some(_) => Err(PlanError::Invalid(format!("{key} must list text"))),
    }
}

fn flag(value: &Value, key: &str) -> bool {
    match value.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        _ => false,
    }
}

fn sum(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => finite(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    }
}

fn per_loop(total: Option<f64>, loops: Option<f64>) -> Option<f64> {
    total
        .zip(loops)
        .filter(|(_, loops)| *loops > 0.0)
        .and_then(|(total, loops)| finite(total / loops))
}

fn times(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    a.zip(b).and_then(|(a, b)| finite(a * b))
}

fn operation_of(node_type: &str) -> PlanOperation {
    let lower = node_type.to_ascii_lowercase();
    if lower.contains("join") || lower.starts_with("nested loop") {
        PlanOperation::Join
    } else if lower.contains("scan") || lower.contains("lookup") || lower.contains("search") {
        PlanOperation::Scan
    } else if lower.starts_with("sort") || lower == "ordering (filesort)" {
        PlanOperation::Sort
    } else if lower.contains("aggregate") || lower.starts_with("grouping") {
        PlanOperation::Aggregate
    } else if lower.starts_with("materializ")
        || lower.starts_with("temporary table")
        || lower.starts_with("union materialize")
    {
        PlanOperation::Temporary
    } else {
        PlanOperation::Other
    }
}

/// Builds the preorder node vector with the shared bounds.
struct Builder {
    nodes: Vec<PlanNode>,
}

impl Builder {
    fn push(
        &mut self,
        parent: Option<u32>,
        node_type: String,
        operation: PlanOperation,
    ) -> Result<u32, PlanError> {
        let depth = parent.map_or(0, |p| self.nodes[p as usize].depth + 1);
        if depth > MAX_PLAN_DEPTH {
            return Err(PlanError::Limit("tree is deeper than 60 levels".into()));
        }
        if self.nodes.len() >= MAX_PLAN_NODES {
            return Err(PlanError::Limit("tree has more than 10,000 nodes".into()));
        }
        let id = self.nodes.len() as u32;
        self.nodes.push(PlanNode {
            id,
            parent,
            depth,
            children: vec![],
            node_type,
            operation,
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
            metrics: PlanMetrics::default(),
            findings: vec![],
        });
        if let Some(parent) = parent {
            self.nodes[parent as usize].children.push(id);
        }
        Ok(id)
    }
    fn node(&mut self, id: u32) -> &mut PlanNode {
        &mut self.nodes[id as usize]
    }
}

fn document(
    raw: &str,
    name: &str,
    flavor: Flavor,
    raw_format: PlanRawFormat,
    nodes: Vec<PlanNode>,
    planning_ms: Option<f64>,
    execution_ms: Option<f64>,
) -> Result<PlanDocument, PlanError> {
    if nodes.is_empty() {
        return Err(PlanError::Invalid("the plan has no steps".into()));
    }
    // MariaDB's ANALYZE of a single-table UPDATE or DELETE reports rows and times without
    // loops: any measured figure means the statement ran.
    let analyzed = nodes.iter().any(|n| {
        let m = &n.metrics;
        m.loops.is_some()
            || m.actual_rows_per_loop.is_some()
            || m.time_ms.is_some()
            || m.self_time_ms.is_some()
    }) || execution_ms.is_some();
    let buffers = nodes
        .iter()
        .any(|n| n.metrics.buffers_hit.is_some() || n.metrics.buffers_read.is_some());
    let mut doc = PlanDocument {
        raw: raw.to_owned(),
        summary: PlanSummary {
            id: 0,
            engine: flavor.engine().into(),
            name: name.into(),
            raw_format,
            analyzed,
            buffers,
            total_cost: nodes.first().and_then(|n| n.metrics.cost),
            planning_ms,
            execution_ms,
            highest_cost: None,
            slowest: None,
            estimate_gap: None,
            sequential_scans: 0,
            sorts: 0,
            temporary_operations: 0,
            node_count: 0,
            depth: 0,
            finding_count: 0,
            operation_types: 0,
            relation_count: 0,
            index_count: 0,
            note: flavor.note().into(),
        },
        nodes,
    };
    doc.analyze();
    Ok(doc)
}

// ---------------------------------------------------------------------------------------
// MySQL format version 1 and MariaDB: tables in join order inside query blocks.
// ---------------------------------------------------------------------------------------

/// Which flavor wrote this JSON, from the keys only one of them uses.
fn detect(value: &Value, budget: &mut u32) -> Option<Flavor> {
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    match value {
        Value::Object(map) => {
            if map.contains_key("query_optimization")
                || map.contains_key("r_loops")
                || map.contains_key("block-nl-join")
                || (map.contains_key("select_id") && map.get("cost").is_some_and(Value::is_number))
            {
                return Some(Flavor::Mariadb);
            }
            if map.contains_key("cost_info") || map.contains_key("rows_examined_per_scan") {
                return Some(Flavor::Mysql);
            }
            // A table: MariaDB writes `rows` and a numeric `filtered`, MySQL
            // `rows_examined_per_scan` and `filtered` as text ("100.00").
            if map.contains_key("table_name") {
                match map.get("filtered") {
                    Some(Value::String(_)) => return Some(Flavor::Mysql),
                    Some(Value::Number(_)) => return Some(Flavor::Mariadb),
                    _ if map.contains_key("rows") => return Some(Flavor::Mariadb),
                    _ => {}
                }
            }
            map.values().find_map(|v| detect(v, budget))
        }
        Value::Array(items) => items.iter().find_map(|v| detect(v, budget)),
        _ => None,
    }
}

pub(super) fn parse_json(
    raw: &str,
    json: &Value,
    name: &str,
    hint: Option<Flavor>,
) -> Result<PlanDocument, PlanError> {
    let mut builder = Builder { nodes: vec![] };
    if let Some(block) = json.get("query_block") {
        let flavor = detect(json, &mut 100_000).or(hint).unwrap_or(Flavor::Mysql);
        visit_block(&mut builder, block, None, "Query block", &[])?;
        let planning = json
            .get("query_optimization")
            .map(|o| number(o, "r_total_time_ms"))
            .transpose()?
            .flatten();
        let execution = number(block, "r_total_time_ms")?;
        return document(
            raw,
            name,
            flavor,
            PlanRawFormat::Json,
            builder.nodes,
            planning,
            execution,
        );
    }
    visit_v2(&mut builder, json, None)?;
    derive_self(&mut builder.nodes);
    document(
        raw,
        name,
        Flavor::Mysql,
        PlanRawFormat::Json,
        builder.nodes,
        None,
        None,
    )
}

fn object(value: &Value) -> Result<&Map<String, Value>, PlanError> {
    value
        .as_object()
        .ok_or_else(|| PlanError::Invalid("each plan step must be an object".into()))
}

const SUBQUERY_KEYS: [(&str, &str); 8] = [
    ("attached_subqueries", "attached"),
    ("optimized_away_subqueries", "optimized away"),
    ("order_by_subqueries", "in ORDER BY"),
    ("group_by_subqueries", "in GROUP BY"),
    ("having_subqueries", "in HAVING"),
    ("select_list_subqueries", "in the select list"),
    ("update_value_subqueries", "in an updated value"),
    ("subqueries", ""),
];

/// Visits the plan-bearing keys of one object, main content first, in a fixed order.
fn visit_content(b: &mut Builder, value: &Value, parent: Option<u32>) -> Result<(), PlanError> {
    let map = object(value)?;
    let mut keys: Vec<(u8, &String)> = map
        .keys()
        .filter_map(|key| {
            let rank = match key.as_str() {
                "query_block" | "table" | "nested_loop" | "block-nl-join" => 0,
                "ordering_operation"
                | "grouping_operation"
                | "duplicates_removal"
                | "windowing"
                | "filesort"
                | "temporary_table"
                | "read_sorted_file"
                | "window_functions_computation"
                | "duplicate_removal"
                | "union_result"
                | "materialized_from_subquery"
                | "materialized"
                | "materialization"
                | "rowid_filter" => 1,
                k if SUBQUERY_KEYS.iter().any(|(s, _)| *s == k) => 2,
                "query_specifications" | "query_optimization" | "cost_info" => return None,
                _ if bears_plan(&map[key]) => 3,
                _ => return None,
            };
            Some((rank, key))
        })
        .collect();
    keys.sort();
    for (_, key) in keys {
        let child = &map[key];
        match key.as_str() {
            "query_block" => {
                visit_block(b, child, parent, "Query block", &[])?;
            }
            "table" => {
                visit_table(b, child, parent)?;
            }
            "nested_loop" => {
                let items = child
                    .as_array()
                    .ok_or_else(|| PlanError::Invalid("nested_loop must be an array".into()))?;
                // One table is not a join: MariaDB writes nested_loop around a single table.
                let join = if items.len() > 1 {
                    Some(b.push(parent, "Nested loop".into(), PlanOperation::Join)?)
                } else {
                    parent
                };
                for item in items {
                    visit_content(b, item, join)?;
                }
            }
            "block-nl-join" => {
                let algorithm = text(child, "join_type")?;
                let id = b.push(parent, "Block nested loop".into(), PlanOperation::Join)?;
                let node = b.node(id);
                node.join_type = algorithm;
                node.filter = text(child, "attached_condition")?;
                node.detail = join_details(&[
                    text(child, "buffer_type")?.map(|v| format!("buffer {v}")),
                    text(child, "buffer_size")?.map(|v| format!("buffer size {v}")),
                ]);
                node.metrics.loops = number(child, "r_loops")?;
                // Unpacking the join buffer and checking the join condition: the block
                // join's own work, apart from reading its table.
                node.metrics.self_time_ms = sum(
                    number(child, "r_unpack_time_ms")?,
                    number(child, "r_other_time_ms")?,
                );
                visit_content(b, child, Some(id))?;
            }
            "union_result" => {
                let temporary = flag(child, "using_temporary_table");
                let id = b.push(
                    parent,
                    "Union result".into(),
                    if temporary {
                        PlanOperation::Temporary
                    } else {
                        PlanOperation::Other
                    },
                )?;
                let node = b.node(id);
                node.detail = join_details(&[
                    text(child, "table_name")?,
                    temporary.then(|| "using a temporary table".into()),
                ]);
                node.metrics.loops = number(child, "r_loops")?;
                node.metrics.actual_rows_per_loop = number(child, "r_rows")?;
                node.metrics.actual_rows =
                    times(node.metrics.actual_rows_per_loop, node.metrics.loops);
                let specs = match child.get("query_specifications") {
                    None => &[][..],
                    Some(Value::Array(a)) => a.as_slice(),
                    Some(_) => {
                        return Err(PlanError::Invalid(
                            "query_specifications must be an array".into(),
                        ));
                    }
                };
                for spec in specs {
                    visit_member(b, spec, Some(id), "Query block", None)?;
                }
                visit_content(b, child, Some(id))?;
            }
            "materialized_from_subquery" | "materialized" | "materialization" => {
                // MariaDB nests materialization inside materialized: one step, not two.
                if key == "materialization"
                    && parent.is_some_and(|p| b.nodes[p as usize].node_type == "Materialize")
                {
                    visit_content(b, child, parent)?;
                    continue;
                }
                let id = b.push(parent, "Materialize".into(), PlanOperation::Temporary)?;
                b.node(id).detail = join_details(&[
                    flag(child, "dependent").then(|| "dependent".into()),
                    flag(child, "unique").then(|| "unique".into()),
                ]);
                visit_content(b, child, Some(id))?;
            }
            "rowid_filter" => {
                let id = b.push(parent, "Rowid filter".into(), PlanOperation::Other)?;
                let range = child.get("range");
                let node = b.node(id);
                if let Some(range) = range {
                    node.index = text(range, "key")?;
                    node.index_condition = list(range, "used_key_parts", ", ")?;
                }
                node.metrics.estimated_rows = number(child, "rows")?;
                node.metrics.loops = number(child, "r_loops")?;
                node.metrics.actual_rows_per_loop = number(child, "r_rows")?;
                node.detail = join_details(&[
                    number(child, "selectivity_pct")?
                        .map(|v| format!("estimated selectivity {v}%")),
                    number(child, "r_selectivity_pct")?.map(|v| format!("actual selectivity {v}%")),
                ]);
            }
            k if SUBQUERY_KEYS.iter().any(|(s, _)| *s == k) => {
                let place = SUBQUERY_KEYS
                    .iter()
                    .find(|(s, _)| *s == k)
                    .map(|(_, p)| *p)
                    .unwrap_or_default();
                let items = child
                    .as_array()
                    .ok_or_else(|| PlanError::Invalid(format!("{k} must be an array")))?;
                for item in items {
                    visit_member(b, item, parent, "Subquery", Some(place))?;
                }
            }
            wrapper => {
                visit_wrapper(b, wrapper, child, parent)?;
            }
        }
    }
    Ok(())
}

/// Plan-bearing: an object or array that holds query blocks or tables somewhere inside.
fn bears_plan(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| {
            matches!(
                k.as_str(),
                "query_block" | "table" | "nested_loop" | "block-nl-join"
            ) || (v.is_object() || v.is_array()) && bears_plan(v)
        }),
        Value::Array(items) => items.iter().any(bears_plan),
        _ => false,
    }
}

fn join_details(parts: &[Option<String>]) -> Option<String> {
    let parts: Vec<&str> = parts
        .iter()
        .flatten()
        .map(String::as_str)
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// A union member or subquery entry: `{dependent, cacheable, query_block}`, or MariaDB's
/// cached subquery `{subquery_cache|expression_cache: {..., query_block}}`.
fn visit_member(
    b: &mut Builder,
    item: &Value,
    parent: Option<u32>,
    label: &str,
    place: Option<&str>,
) -> Result<(), PlanError> {
    for cache in ["subquery_cache", "expression_cache"] {
        if let Some(cached) = item.get(cache) {
            let id = b.push(parent, "Subquery cache".into(), PlanOperation::Other)?;
            b.node(id).detail = join_details(&[
                text(cached, "state")?.map(|s| format!("state {s}")),
                number(cached, "r_hit_ratio")?.map(|r| format!("hit ratio {r}%")),
            ]);
            return visit_member(b, cached, Some(id), label, place);
        }
    }
    let mut flags = vec![];
    if let Some(place) = place.filter(|p| !p.is_empty()) {
        flags.push(place.to_owned());
    }
    if flag(item, "dependent") {
        flags.push("dependent".into());
    }
    if item.get("cacheable") == Some(&Value::Bool(false)) {
        flags.push("not cacheable".into());
    }
    match item.get("query_block") {
        Some(block) => visit_block(b, block, parent, label, &flags).map(|_| ()),
        None => visit_content(b, item, parent),
    }
}

fn visit_block(
    b: &mut Builder,
    block: &Value,
    parent: Option<u32>,
    label: &str,
    flags: &[String],
) -> Result<u32, PlanError> {
    object(block)?;
    let id = b.push(parent, label.into(), PlanOperation::Other)?;
    let mut detail: Vec<Option<String>> =
        vec![number(block, "select_id")?.map(|n| format!("select #{n}"))];
    detail.extend(flags.iter().cloned().map(Some));
    detail.push(text(block, "operation")?);
    detail.push(text(block, "message")?);
    let cost = match block.get("cost_info") {
        Some(info) => number(info, "query_cost")?,
        None => number(block, "cost")?,
    };
    let loops = number(block, "r_loops")?;
    let time = number(block, "r_total_time_ms")?;
    let node = b.node(id);
    node.detail = join_details(&detail);
    node.filter = text(block, "having_condition")?;
    node.metrics.cost = cost;
    node.metrics.loops = loops;
    node.metrics.time_ms = time;
    node.metrics.time_per_loop_ms = per_loop(time, loops);
    visit_content(b, block, Some(id))?;
    finish_leaf(b, id);
    Ok(id)
}

fn humanize(key: &str) -> String {
    let words = key.replace(['_', '-'], " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn visit_wrapper(
    b: &mut Builder,
    key: &str,
    value: &Value,
    parent: Option<u32>,
) -> Result<(), PlanError> {
    if value.is_array() {
        let id = b.push(parent, humanize(key), PlanOperation::Other)?;
        for item in value.as_array().into_iter().flatten() {
            visit_content(b, item, Some(id))?;
        }
        return Ok(());
    }
    let filesort = flag(value, "using_filesort");
    let temporary = flag(value, "using_temporary_table");
    let node_type = match key {
        "ordering_operation" if filesort => "Ordering (filesort)".into(),
        "ordering_operation" => "Ordering".into(),
        "grouping_operation" if temporary => "Grouping (temporary table)".into(),
        "grouping_operation" => "Grouping".into(),
        "duplicates_removal" | "duplicate_removal" => "Duplicates removal".into(),
        "windowing" | "window_functions_computation" => "Window functions".into(),
        "filesort" => "Sort".into(),
        "temporary_table" => "Temporary table".into(),
        "read_sorted_file" => "Read sorted file".into(),
        other => humanize(other),
    };
    let operation = match key {
        "grouping_operation" => PlanOperation::Aggregate,
        _ => operation_of(&node_type),
    };
    let id = b.push(parent, node_type, operation)?;
    let loops = number(value, "r_loops")?;
    let output = number(value, "r_output_rows")?.or(number(value, "r_rows")?);
    let node = b.node(id);
    node.detail = join_details(&[
        text(value, "sort_key")?.map(|k| format!("sort key {k}")),
        (key != "grouping_operation" && temporary).then(|| "using a temporary table".into()),
        (key == "grouping_operation" && filesort).then(|| "using filesort".into()),
        number(value, "r_limit")?.map(|l| format!("limit {l}")),
        text(value, "r_buffer_size")?.map(|s| format!("sort buffer {s}")),
    ]);
    node.sort_method = text(value, "r_sort_mode")?;
    node.metrics.loops = loops;
    node.metrics.actual_rows = output;
    node.metrics.actual_rows_per_loop = per_loop(output, loops);
    if key == "filesort" {
        // MariaDB times the sort itself, not the input it reads.
        node.metrics.self_time_ms = number(value, "r_total_time_ms")?;
    } else {
        node.metrics.time_ms = number(value, "r_total_time_ms")?;
    }
    visit_content(b, value, Some(id))
}

fn access_name(access: &str) -> String {
    match access {
        "ALL" => "Full table scan".into(),
        "index" => "Full index scan".into(),
        "range" => "Index range scan".into(),
        "ref" => "Index lookup".into(),
        "eq_ref" => "Unique index lookup".into(),
        "const" => "Constant row".into(),
        "system" => "System row".into(),
        "fulltext" => "Fulltext search".into(),
        "ref_or_null" => "Index lookup or null".into(),
        "index_merge" => "Index merge".into(),
        "unique_subquery" => "Unique subquery lookup".into(),
        "index_subquery" => "Subquery index lookup".into(),
        other => format!("Table access ({other})"),
    }
}

fn visit_table(b: &mut Builder, table: &Value, parent: Option<u32>) -> Result<(), PlanError> {
    object(table)?;
    let access = text(table, "access_type")?;
    let message = text(table, "message")?;
    let write = ["insert", "update", "delete"]
        .into_iter()
        .find(|k| flag(table, k));
    let node_type = match (write, &access, &message) {
        (Some("insert"), _, _) => "Insert".to_owned(),
        (Some("update"), _, _) => "Update".to_owned(),
        (Some(_), _, _) => "Delete".to_owned(),
        (None, Some(access), _) => access_name(access),
        (None, None, Some(message)) => message.clone(),
        (None, None, None) => "Table access".to_owned(),
    };
    let operation = match write {
        Some(_) => PlanOperation::Other,
        None if access.is_some() => PlanOperation::Scan,
        None => PlanOperation::Other,
    };
    let id = b.push(parent, node_type, operation)?;
    let filtered = number(table, "filtered")?;
    let rows_per_scan = number(table, "rows_examined_per_scan")?.or(number(table, "rows")?);
    let own_cost = match table.get("cost_info") {
        Some(info) => sum(number(info, "read_cost")?, number(info, "eval_cost")?),
        None => number(table, "cost")?,
    };
    let loops = number(table, "r_loops")?;
    let read = number(table, "r_rows")?;
    let kept = number(table, "r_filtered")?;
    let own_time = sum(
        number(table, "r_table_time_ms")?,
        number(table, "r_other_time_ms")?,
    );
    let (hits, reads) = match table.get("r_engine_stats") {
        Some(stats) => {
            // MariaDB prints only the counters that are not zero.
            let accessed = number(stats, "pages_accessed")?.unwrap_or(0.0);
            let from_disk = number(stats, "pages_read_count")?.unwrap_or(0.0);
            (Some((accessed - from_disk).max(0.0)), Some(from_disk))
        }
        None => (None, None),
    };
    let detail = join_details(&[
        write
            .and(access.as_deref())
            .map(|a| format!("access {}", access_name(a).to_lowercase())),
        list(table, "possible_keys", ", ")?.map(|k| format!("possible keys {k}")),
        list(table, "used_key_parts", ", ")?.map(|k| format!("key parts {k}")),
        list(table, "ref", ", ")?.map(|r| format!("ref {r}")),
        flag(table, "using_index").then(|| "covering index".into()),
        text(table, "using_join_buffer")?.map(|j| format!("join buffer ({j})")),
        flag(table, "using_index_for_group_by").then(|| "index for GROUP BY".into()),
        (write.is_some() || access.is_some())
            .then_some(message)
            .flatten(),
    ]);
    let node = b.node(id);
    node.relation = text(table, "table_name")?;
    node.index = text(table, "key")?;
    node.filter = text(table, "attached_condition")?;
    node.index_condition = text(table, "index_condition")?;
    node.detail = detail;
    node.sequential_scan =
        write != Some("insert") && matches!(access.as_deref(), Some("ALL" | "hash_ALL"));
    let m = &mut node.metrics;
    m.estimated_rows = match filtered {
        Some(pct) => times(rows_per_scan, Some(pct / 100.0)),
        None => rows_per_scan,
    };
    m.cost = own_cost;
    m.self_cost = own_cost;
    m.loops = loops;
    m.actual_rows_per_loop = match kept {
        Some(pct) => times(read, Some(pct / 100.0)),
        None => read,
    };
    m.rows_removed = read
        .zip(m.actual_rows_per_loop)
        .filter(|_| kept.is_some())
        .map(|(read, kept)| (read - kept).max(0.0));
    m.actual_rows = times(m.actual_rows_per_loop, loops);
    m.self_time_ms = own_time;
    m.time_ms = own_time;
    m.time_per_loop_ms = per_loop(own_time, loops);
    m.buffers_hit = hits;
    m.buffers_read = reads;
    visit_content(b, table, Some(id))?;
    if !b.nodes[id as usize].children.is_empty() {
        // A table's own cost and time leave out what its children (a materialized
        // subquery, a filter) did: there is no inclusive figure for it.
        let m = &mut b.node(id).metrics;
        m.cost = None;
        m.time_ms = None;
        m.time_per_loop_ms = None;
    }
    Ok(())
}

/// A query block with no children did all its own work.
fn finish_leaf(b: &mut Builder, id: u32) {
    let node = b.node(id);
    if node.children.is_empty() {
        node.metrics.self_cost = node.metrics.cost;
        node.metrics.self_time_ms = node.metrics.time_ms;
    }
}

// ---------------------------------------------------------------------------------------
// MySQL operator trees: format version 2 JSON and the EXPLAIN ANALYZE text.
// ---------------------------------------------------------------------------------------

/// What a MySQL step description (`Index lookup on c using PRIMARY (id=o.customer_id)`)
/// says, without guessing what it does not.
#[derive(Default)]
struct Described {
    node_type: String,
    relation: Option<String>,
    index: Option<String>,
    filter: Option<String>,
    index_condition: Option<String>,
    hash_condition: Option<String>,
    join_type: Option<String>,
    sequential_scan: bool,
    temporary_name: bool,
}

/// `(a = 1)` → `a = 1`, but `(a) OR (b)` stays whole: only one enclosing pair is removed.
fn unwrap_parens(text: &str) -> &str {
    let text = text.trim();
    let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) else {
        return text;
    };
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return text;
                }
            }
            _ => {}
        }
    }
    if depth == 0 { inner } else { text }
}

fn describe(description: &str) -> Described {
    let d = description.trim();
    let mut out = Described::default();
    if let Some(rest) = d.strip_prefix("Filter: ") {
        out.node_type = "Filter".into();
        out.filter = Some(unwrap_parens(rest).into());
        return out;
    }
    for prefix in [
        "Sort row IDs",
        "Sort",
        "Limit/Offset",
        "Limit",
        "Group aggregate",
        "Aggregate",
    ] {
        if let Some(rest) = d.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with(':'))
        {
            out.node_type = prefix.into();
            return out;
        }
    }
    let lower = d.to_ascii_lowercase();
    if let Some(kind) = lower.strip_prefix("nested loop ") {
        out.node_type = d.into();
        out.join_type = Some(kind.trim_end_matches(" join").into());
        return out;
    }
    if let Some(at) = lower.find("hash join") {
        let (head, condition) = match d.find(" (") {
            Some(open) => (&d[..open], Some(unwrap_parens(&d[open..]).to_owned())),
            None => (d, None),
        };
        out.node_type = head.into();
        out.hash_condition = condition;
        let kind = lower[..at].trim();
        if !kind.is_empty() {
            out.join_type = Some(kind.into());
        }
        return out;
    }
    for (prefix, name) in [
        ("Update ", "Update"),
        ("Delete from ", "Delete"),
        ("Insert into ", "Insert"),
    ] {
        if let Some(rest) = d.strip_prefix(prefix) {
            out.node_type = name.into();
            out.relation = rest.split_whitespace().next().map(str::to_owned);
            return out;
        }
    }
    if let Some(on) = d.find(" on ") {
        let kind = &d[..on];
        let kind_lower = kind.to_ascii_lowercase();
        if ["scan", "lookup", "search"]
            .iter()
            .any(|w| kind_lower.contains(w))
        {
            let rest = &d[on + 4..];
            let (table, mut rest) = rest.split_once(' ').unwrap_or((rest, ""));
            out.node_type = kind.into();
            if table.starts_with('<') {
                out.temporary_name = true;
            } else {
                out.relation = Some(table.into());
            }
            if let Some(after) = rest.strip_prefix("using ") {
                let (index, tail) = after.split_once(' ').unwrap_or((after, ""));
                out.index = Some(index.into());
                rest = tail;
            }
            let (condition, _) = rest
                .split_once(", with index condition: ")
                .unwrap_or((rest, ""));
            let condition = condition.strip_prefix("over ").unwrap_or(condition).trim();
            if !condition.is_empty() && condition != "(reverse)" {
                out.index_condition = Some(unwrap_parens(condition).into());
            }
            out.sequential_scan = kind == "Table scan" && out.relation.is_some();
            return out;
        }
    }
    if let Some(rest) = d.strip_prefix("Select #") {
        out.node_type = format!("Select #{}", rest.split_whitespace().next().unwrap_or(""));
        return out;
    }
    let cut = [d.find(" ("), d.find(": ")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(d.len());
    out.node_type = d[..cut].into();
    out
}

fn apply(b: &mut Builder, id: u32, description: &str, d: Described) {
    let node = b.node(id);
    if description.trim() != d.node_type {
        node.detail = Some(description.trim().into());
    }
    node.relation = d.relation;
    node.index = d.index;
    node.filter = d.filter;
    node.index_condition = d.index_condition;
    node.hash_condition = d.hash_condition;
    node.join_type = d.join_type;
    node.sequential_scan = d.sequential_scan;
}

fn visit_v2(b: &mut Builder, value: &Value, parent: Option<u32>) -> Result<(), PlanError> {
    object(value)?;
    let operation = text(value, "operation")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PlanError::Invalid("missing MySQL operation".into()))?;
    let mut d = describe(&operation);
    if let Some(table) = text(value, "table_name")?.filter(|t| !t.starts_with('<')) {
        d.relation = Some(match text(value, "schema_name")? {
            Some(schema) => format!("{schema}.{table}"),
            None => table,
        });
    }
    if let Some(index) = text(value, "index_name")? {
        d.index = Some(index);
    }
    if let Some(condition) = text(value, "condition")?
        && d.node_type == "Filter"
    {
        d.filter = Some(condition);
    }
    if let Some(lookup) = text(value, "lookup_condition")? {
        d.index_condition = Some(lookup);
    } else if let Some(ranges) = list(value, "ranges", " OR ")? {
        d.index_condition = Some(ranges);
    }
    if let Some(hash) = list(value, "hash_condition", " AND ")? {
        d.hash_condition = Some(hash);
    }
    if let Some(join) = text(value, "join_type")? {
        d.join_type = Some(join.trim_end_matches(" join").into());
    }
    let kind = operation_of(&d.node_type);
    let id = b.push(parent, d.node_type.clone(), kind)?;
    apply(b, id, &operation, d);
    let loops = number(value, "actual_loops")?;
    let per_rows = number(value, "actual_rows")?;
    let per_time = number(value, "actual_last_row_ms")?;
    let m = &mut b.node(id).metrics;
    m.cost = number(value, "estimated_total_cost")?;
    m.estimated_rows = number(value, "estimated_rows")?;
    m.loops = loops;
    if loops != Some(0.0) {
        m.actual_rows_per_loop = per_rows;
        m.actual_rows = times(per_rows, loops);
        m.time_per_loop_ms = per_time;
        m.time_ms = times(per_time, loops);
    }
    match value.get("inputs") {
        None | Some(Value::Null) => {}
        Some(Value::Array(inputs)) => {
            for input in inputs {
                visit_v2(b, input, Some(id))?;
            }
        }
        Some(_) => return Err(PlanError::Invalid("inputs must be an array".into())),
    }
    Ok(())
}

/// Self metrics for inclusive operator trees: leaves own everything; a parent subtracts its
/// children only when all ran as often as it did. LIMIT, nested loops and subqueries are
/// not comparable (they stop early, repeat or run apart).
fn derive_self(nodes: &mut [PlanNode]) {
    for id in (0..nodes.len()).rev() {
        let children = nodes[id].children.clone();
        if children.is_empty() {
            nodes[id].metrics.self_cost = nodes[id].metrics.cost;
            nodes[id].metrics.self_time_ms = nodes[id].metrics.time_ms;
            continue;
        }
        let node = &nodes[id];
        let comparable = !node.node_type.starts_with("Limit")
            && !node.node_type.starts_with("Nested loop")
            && children.iter().all(|c| {
                let child = &nodes[*c as usize];
                child.metrics.loops == node.metrics.loops
                    && !child.node_type.starts_with("Select #")
            });
        if !comparable {
            continue;
        }
        let costs: Option<Vec<f64>> = children
            .iter()
            .map(|c| nodes[*c as usize].metrics.cost)
            .collect();
        let times: Option<Vec<f64>> = children
            .iter()
            .map(|c| nodes[*c as usize].metrics.time_ms)
            .collect();
        let m = &nodes[id].metrics;
        let self_cost = m
            .cost
            .zip(costs)
            .and_then(|(total, costs)| finite(total - costs.iter().sum::<f64>()))
            .filter(|n| *n >= 0.0);
        let self_time = m
            .time_ms
            .zip(times)
            .filter(|_| m.loops.is_some_and(|l| l > 0.0))
            .and_then(|(total, times)| finite(total - times.iter().sum::<f64>()))
            .filter(|n| *n >= 0.0);
        nodes[id].metrics.self_cost = self_cost;
        nodes[id].metrics.self_time_ms = self_time;
    }
}

/// `cost=0.103..41485 rows=403201`, `actual time=0.016..0.183 rows=400 loops=1`,
/// `actual rows=400 loops=1` or `never executed`.
fn metric_group(group: &str) -> bool {
    [
        "cost=",
        "rows=",
        "actual time=",
        "actual rows=",
        "never executed",
    ]
    .iter()
    .any(|p| group.starts_with(p))
}

fn tree_number(text: &str) -> Result<f64, PlanError> {
    // A range `first..total`: the total is what the shared model keeps.
    let value = text.rsplit("..").next().unwrap_or(text);
    value
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
        .ok_or_else(|| PlanError::Invalid(format!("unreadable MySQL plan number {text}")))
}

/// Splits trailing metric groups off one step line.
fn split_metrics(line: &str) -> (&str, Vec<&str>) {
    let mut rest = line.trim_end();
    let mut groups = vec![];
    while rest.ends_with(')') {
        let bytes = rest.as_bytes();
        let mut depth = 0usize;
        let mut open = None;
        for i in (0..bytes.len()).rev() {
            match bytes[i] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(open) = open else { break };
        let group = &rest[open + 1..rest.len() - 1];
        if !metric_group(group) {
            break;
        }
        groups.push(group);
        rest = rest[..open].trim_end();
    }
    (rest, groups)
}

pub(super) fn parse_tree(raw: &str, name: &str) -> Result<PlanDocument, PlanError> {
    let text = raw.trim_start_matches('\u{feff}');
    let mut b = Builder { nodes: vec![] };
    let mut stack: Vec<(usize, u32)> = vec![];
    let mut last: Option<u32> = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let Some(step) = line.trim_start().strip_prefix("->") else {
            // A condition that spans lines continues the previous step's description.
            let id =
                last.ok_or_else(|| PlanError::Invalid("expected a MySQL plan step (->)".into()))?;
            let node = b.node(id);
            let detail = node.detail.get_or_insert_with(|| node.node_type.clone());
            detail.push('\n');
            detail.push_str(line.trim());
            continue;
        };
        if step.trim() == "<not executable by iterator executor>" {
            return Err(PlanError::Invalid(
                "MySQL cannot analyze this statement; explain it without ANALYZE".into(),
            ));
        }
        while stack.last().is_some_and(|(level, _)| *level >= indent) {
            stack.pop();
        }
        let parent = stack.last().map(|(_, id)| *id);
        if parent.is_none() && !b.nodes.is_empty() {
            return Err(PlanError::Invalid("a MySQL plan has one root step".into()));
        }
        let (description, groups) = split_metrics(step.trim());
        let d = describe(description);
        let kind = operation_of(&d.node_type);
        let id = b.push(parent, d.node_type.clone(), kind)?;
        apply(&mut b, id, description, d);
        for group in groups {
            let m = &mut b.node(id).metrics;
            if group == "never executed" {
                m.loops = Some(0.0);
                continue;
            }
            let (actual, fields) = match group.strip_prefix("actual ") {
                Some(rest) => (true, rest),
                None => (false, group),
            };
            for field in fields.split_whitespace() {
                let Some((key, value)) = field.split_once('=') else {
                    continue;
                };
                match (actual, key) {
                    (false, "cost") => m.cost = Some(tree_number(value)?),
                    (false, "rows") => m.estimated_rows = Some(tree_number(value)?),
                    (true, "time") => m.time_per_loop_ms = Some(tree_number(value)?),
                    (true, "rows") => m.actual_rows_per_loop = Some(tree_number(value)?),
                    (true, "loops") => m.loops = Some(tree_number(value)?),
                    _ => {}
                }
            }
        }
        let m = &mut b.node(id).metrics;
        m.actual_rows = times(m.actual_rows_per_loop, m.loops);
        m.time_ms = times(m.time_per_loop_ms, m.loops);
        stack.push((indent, id));
        last = Some(id);
    }
    derive_self(&mut b.nodes);
    document(
        raw,
        name,
        Flavor::Mysql,
        PlanRawFormat::Text,
        b.nodes,
        None,
        None,
    )
}

/// For the parser tests: node type, relation, index and index condition of a description.
#[cfg(test)]
pub(super) fn describe_for_test(
    text: &str,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let d = describe(text);
    (d.node_type, d.relation, d.index, d.index_condition)
}
