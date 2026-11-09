//! SQLite and DuckDB plans in the shared tree: the forms each engine writes, files from other
//! tools, and malformed input refused. Real engines run in `tests/plans_sqlite_duckdb.rs`.
use super::*;

fn node<'a>(doc: &'a PlanDocument, node_type: &str) -> &'a PlanNode {
    doc.nodes
        .iter()
        .find(|n| n.node_type == node_type)
        .unwrap_or_else(|| panic!("no {node_type} in {:?}", doc.nodes))
}

/// Preorder, parents before children, ids are positions, depths follow parents.
fn shape(doc: &PlanDocument) {
    assert_eq!(doc.summary.node_count as usize, doc.nodes.len());
    let mut seen = vec![false; doc.nodes.len()];
    let mut stack = vec![0u32];
    let mut order = Vec::new();
    while let Some(id) = stack.pop() {
        order.push(id);
        seen[id as usize] = true;
        stack.extend(doc.nodes[id as usize].children.iter().rev());
    }
    assert!(
        seen.iter().all(|s| *s),
        "every node is reachable from the root"
    );
    assert_eq!(order, (0..doc.nodes.len() as u32).collect::<Vec<_>>());
    for (i, node) in doc.nodes.iter().enumerate() {
        assert_eq!(node.id as usize, i);
        for child in &node.children {
            assert_eq!(doc.nodes[*child as usize].parent, Some(node.id));
            assert_eq!(doc.nodes[*child as usize].depth, node.depth + 1);
        }
    }
}

const SQLITE_ROWS: &str = r#"[
  {"id": 3, "parent": 0, "notused": 0, "detail": "SEARCH p USING INTEGER PRIMARY KEY (rowid=?)"},
  {"id": 7, "parent": 0, "notused": 0, "detail": "LIST SUBQUERY 1"},
  {"id": 12, "parent": 0, "notused": 0, "detail": "SCAN o"},
  {"id": 9, "parent": 7, "notused": 0, "detail": "SCAN product USING COVERING INDEX product_price (price>?)"},
  {"id": 20, "parent": 0, "notused": 0, "detail": "SEARCH c USING INDEX customers_email (email=? AND id>?)"},
  {"id": 32, "parent": 0, "notused": 0, "detail": "USE TEMP B-TREE FOR ORDER BY"}
]"#;

#[test]
fn sqlite_rows_become_a_preorder_tree_under_one_root() {
    let doc = parse_plan(SQLITE_ROWS, "rows").unwrap();
    shape(&doc);
    assert_eq!(doc.summary.engine, "sqlite");
    assert_eq!(doc.summary.raw_format, PlanRawFormat::Json);
    assert!(!doc.summary.analyzed);
    assert_eq!(doc.nodes[0].node_type, "Query plan");
    // The subquery's scan comes right after it, though SQLite wrote it later.
    let types: Vec<&str> = doc.nodes.iter().map(|n| n.node_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "Query plan",
            "Rowid search",
            "List subquery",
            "Covering index scan",
            "Scan",
            "Index search",
            "Temp B-tree"
        ]
    );
    let rowid = node(&doc, "Rowid search");
    assert_eq!(rowid.relation.as_deref(), Some("p"));
    assert_eq!(rowid.index, None);
    assert_eq!(rowid.index_condition.as_deref(), Some("rowid=?"));
    assert!(!rowid.sequential_scan);
    let covering = node(&doc, "Covering index scan");
    assert_eq!(covering.index.as_deref(), Some("product_price"));
    assert_eq!(covering.parent, Some(2));
    let index = node(&doc, "Index search");
    assert_eq!(index.index.as_deref(), Some("customers_email"));
    assert_eq!(index.index_condition.as_deref(), Some("email=? AND id>?"));
    let scan = node(&doc, "Scan");
    assert!(scan.sequential_scan && scan.operation == PlanOperation::Scan);
    assert_eq!(scan.detail.as_deref(), Some("SCAN o"));
    let sort = node(&doc, "Temp B-tree");
    assert_eq!(sort.operation, PlanOperation::Sort);
    assert_eq!(doc.summary.sorts, 1);
    assert_eq!(doc.summary.sequential_scans, 1);
    assert_eq!(doc.summary.relation_count, 4);
    assert_eq!(doc.summary.index_count, 2);
    // No metrics, so no findings and nothing made up.
    assert!(doc.nodes.iter().all(|n| n.findings.is_empty()
        && n.metrics.cost.is_none()
        && n.metrics.estimated_rows.is_none()));
    assert_eq!(doc.summary.total_cost, None);
}

#[test]
fn sqlite_rows_written_by_the_core_read_back_the_same() {
    let rows = vec![
        sqlite::Row {
            id: 2,
            parent: 0,
            detail: "SCAN \"odd \"\"name\"\"\"".into(),
        },
        sqlite::Row {
            id: 5,
            parent: 2,
            detail: "CORRELATED SCALAR SUBQUERY 1".into(),
        },
    ];
    let raw = sqlite::raw_json(&rows);
    let doc = parse_plan(&raw, "core").unwrap();
    shape(&doc);
    assert_eq!(doc.raw(), raw);
    assert_eq!(
        doc.nodes[1].relation.as_deref(),
        Some("\"odd \"\"name\"\"\"")
    );
    assert_eq!(doc.nodes[2].node_type, "Correlated scalar subquery");
}

#[test]
fn sqlite_older_and_rarer_steps_keep_their_text() {
    let doc = parse_plan(
        r#"[
          {"id": 1, "parent": 0, "detail": "SCAN TABLE orders AS o"},
          {"id": 2, "parent": 0, "detail": "SEARCH TABLE customers AS c USING PRIMARY KEY (id=?)"},
          {"id": 3, "parent": 0, "detail": "SEARCH t USING AUTOMATIC COVERING INDEX (k=?)"},
          {"id": 4, "parent": 0, "detail": "MATERIALIZE recent"},
          {"id": 5, "parent": 4, "detail": "SCAN CONSTANT ROW"},
          {"id": 6, "parent": 0, "detail": "COMPOUND QUERY"},
          {"id": 7, "parent": 6, "detail": "LEFT-MOST SUBQUERY"},
          {"id": 8, "parent": 6, "detail": "UNION USING TEMP B-TREE"},
          {"id": 9, "parent": 0, "detail": "MULTI-INDEX OR"},
          {"id": 10, "parent": 9, "detail": "INDEX 1"},
          {"id": 11, "parent": 0, "detail": "BLOOM FILTER ON c (id=?)"},
          {"id": 12, "parent": 0, "detail": "USE TEMP B-TREE FOR GROUP BY"},
          {"id": 13, "parent": 0, "detail": "USE TEMP B-TREE FOR DISTINCT"},
          {"id": 14, "parent": 0, "detail": "SCAN v VIRTUAL TABLE INDEX 0:"},
          {"id": 15, "parent": 0, "detail": "SOMETHING NEW IN A LATER VERSION"},
          {"id": 16, "parent": 0, "detail": "CREATE BLOOM FILTER"}
        ]"#,
        "rare",
    )
    .unwrap();
    shape(&doc);
    let scan = node(&doc, "Scan");
    assert_eq!(scan.relation.as_deref(), Some("o"));
    assert!(scan.sequential_scan);
    assert_eq!(
        node(&doc, "Primary key search").relation.as_deref(),
        Some("c")
    );
    let automatic = node(&doc, "Automatic index search");
    assert_eq!(automatic.index, None);
    assert_eq!(automatic.index_condition.as_deref(), Some("k=?"));
    assert_eq!(
        node(&doc, "Materialize").operation,
        PlanOperation::Temporary
    );
    let constant = node(&doc, "Constant row");
    assert_eq!(constant.relation, None);
    assert!(!constant.sequential_scan);
    for label in [
        "Compound query",
        "Left-most subquery",
        "Multi-index OR",
        "Index",
    ] {
        node(&doc, label);
    }
    assert_eq!(node(&doc, "Union").operation, PlanOperation::Temporary);
    assert_eq!(
        node(&doc, "Create Bloom filter").operation,
        PlanOperation::Temporary
    );
    assert_eq!(
        node(&doc, "Bloom filter").index_condition.as_deref(),
        Some("id=?")
    );
    assert!(
        doc.nodes
            .iter()
            .any(|n| n.node_type == "Temp B-tree" && n.operation == PlanOperation::Aggregate)
    );
    assert!(
        doc.nodes
            .iter()
            .any(|n| n.node_type == "Temp B-tree" && n.operation == PlanOperation::Temporary)
    );
    assert_eq!(
        node(&doc, "Virtual table scan").relation.as_deref(),
        Some("v")
    );
    let unknown = node(&doc, "SOMETHING NEW IN A LATER VERSION");
    assert_eq!(unknown.operation, PlanOperation::Other);
}

#[test]
fn the_sqlite3_shells_text_tree_opens_as_the_same_plan() {
    let text = "QUERY PLAN\n|--SEARCH p USING INTEGER PRIMARY KEY (rowid=?)\n|--LIST SUBQUERY 1\n|  `--SCAN product\n`--USE TEMP B-TREE FOR ORDER BY\n";
    let doc = parse_plan(text, "shell.txt").unwrap();
    shape(&doc);
    assert_eq!(doc.summary.raw_format, PlanRawFormat::Text);
    let types: Vec<&str> = doc.nodes.iter().map(|n| n.node_type.as_str()).collect();
    assert_eq!(
        types,
        [
            "Query plan",
            "Rowid search",
            "List subquery",
            "Scan",
            "Temp B-tree"
        ]
    );
    assert_eq!(doc.nodes[3].parent, Some(2));
    // Windows line endings and a byte order mark, as a saved file may have.
    let crlf = format!("\u{feff}{}", text.replace('\n', "\r\n"));
    assert_eq!(parse_plan(&crlf, "crlf").unwrap().nodes.len(), 5);
}

#[test]
fn malformed_sqlite_plans_are_refused() {
    for (raw, why) in [
        (r#"[{"id": 2, "parent": 9, "detail": "SCAN t"}]"#, "parent"),
        (
            r#"[{"id": 3, "parent": 4, "detail": "SCAN a"}, {"id": 4, "parent": 0, "detail": "SCAN b"}]"#,
            "parent",
        ),
        (
            r#"[{"id": 2, "parent": 0, "detail": "SCAN a"}, {"id": 2, "parent": 0, "detail": "SCAN b"}]"#,
            "repeated",
        ),
        (
            r#"[{"id": "x", "parent": 0, "detail": "SCAN a"}]"#,
            "integer",
        ),
        (r#"[{"id": 1, "parent": 0, "detail": 5}]"#, "detail"),
        ("QUERY PLAN\n", "no steps"),
        ("QUERY PLAN\n|  `--SCAN a\n", "deeper"),
        ("QUERY PLAN\nSCAN a\n", "not a QUERY PLAN step"),
        ("QUERY PLAN\nxx|--SCAN a\n", "not a QUERY PLAN step"),
    ] {
        let error = parse_plan(raw, "bad").unwrap_err().to_string();
        assert!(error.contains(why), "{raw}: {error}");
    }
    // A cycle cannot be written: a parent must come first.
    let deep: Vec<String> = (1..=70)
        .map(|i| {
            format!(
                r#"{{"id": {i}, "parent": {}, "detail": "SCAN t{i}"}}"#,
                i - 1
            )
        })
        .collect();
    let error = parse_plan(&format!("[{}]", deep.join(",")), "deep")
        .unwrap_err()
        .to_string();
    assert!(error.contains("60 levels"), "{error}");
}

const DUCKDB_ESTIMATED: &str = r##"[
  {"name": "TOP_N", "children": [
    {"name": "HASH_GROUP_BY", "children": [
      {"name": "HASH_JOIN", "children": [
        {"name": "SEQ_SCAN", "children": [], "extra_info": {"Table": "memory.main.orders", "Type": "Sequential Scan", "Projections": ["customer_id", "total"], "Filters": ["total>100.0", "total IS NOT NULL"], "Estimated Cardinality": "200"}},
        {"name": "SEQ_SCAN", "children": [], "extra_info": {"Table": "memory.main.customers", "Type": "Sequential Scan", "Projections": "id", "Estimated Cardinality": "100"}}
      ], "extra_info": {"Join Type": "INNER", "Conditions": "customer_id = id", "Estimated Cardinality": "202"}}
    ], "extra_info": {"Groups": "#0", "Aggregates": "sum(#1)", "Estimated Cardinality": "86"}}
  ], "extra_info": {"Top": "5", "Order By": "sum(o.total) DESC", "Estimated Cardinality": "5"}}
]"##;

#[test]
fn duckdb_estimated_plans_keep_cardinalities_and_details() {
    let doc = parse_plan(DUCKDB_ESTIMATED, "estimated").unwrap();
    shape(&doc);
    assert_eq!(doc.summary.engine, "duckdb");
    assert!(!doc.summary.analyzed);
    assert_eq!(doc.summary.total_cost, None);
    assert_eq!(doc.nodes[0].node_type, "TOP_N");
    assert_eq!(doc.nodes[0].operation, PlanOperation::Sort);
    assert_eq!(doc.nodes[0].metrics.estimated_rows, Some(5.0));
    // Key order follows the JSON map, which may or may not keep DuckDB's order.
    let top = doc.nodes[0].detail.as_deref().unwrap();
    assert!(top.contains("Top: 5") && top.contains("Order By: sum(o.total) DESC"));
    assert_eq!(
        node(&doc, "HASH_GROUP_BY").operation,
        PlanOperation::Aggregate
    );
    let join = node(&doc, "HASH_JOIN");
    assert_eq!(join.operation, PlanOperation::Join);
    assert_eq!(join.join_type.as_deref(), Some("INNER"));
    assert_eq!(join.hash_condition.as_deref(), Some("customer_id = id"));
    assert_eq!(join.detail, None);
    let orders = &doc.nodes[join.children[0] as usize];
    assert_eq!(orders.relation.as_deref(), Some("memory.main.orders"));
    assert_eq!(
        orders.filter.as_deref(),
        Some("total>100.0 AND total IS NOT NULL")
    );
    assert!(orders.sequential_scan);
    assert_eq!(
        orders.detail.as_deref(),
        Some("Projections: customer_id, total")
    );
    assert_eq!(doc.summary.sequential_scans, 2);
    assert!(
        doc.nodes
            .iter()
            .all(|n| n.metrics.actual_rows.is_none() && n.metrics.time_ms.is_none())
    );
}

/// The profiler output of the bundled DuckDB (`operator`, `type`, `timing`), abridged.
const DUCKDB_ANALYZED: &str = r#"{
  "io": {"total_bytes_read": 0, "total_bytes_written": 0},
  "operator": [{"type": "RESULT_COLLECTOR", "timing": 0.0, "intermediate_rows": 0, "extra_info": {}, "children": [
    {"type": "EXECUTE", "timing": 0.0, "intermediate_rows": 0, "extra_info": {}, "children": [
      {"type": "EXPLAIN_ANALYZE", "timing": 6e-7, "intermediate_rows": 0, "extra_info": {}, "children": [
        {"type": "HASH_JOIN", "timing": 0.004, "intermediate_rows": 1000, "extra_info": {"Join Type": "INNER", "Conditions": "customer_id = id", "Estimated Cardinality": "202"}, "children": [
          {"type": "TABLE_SCAN", "timing": 0.003, "intermediate_rows": 1000, "rows_scanned": 120000, "extra_info": {"Table": "memory.main.orders", "Type": "Sequential Scan", "Filters": "total>100.0", "Estimated Cardinality": "1000"}, "children": []},
          {"type": "TABLE_SCAN", "timing": 0.001, "intermediate_rows": 100, "rows_scanned": 100, "extra_info": {"Table": "memory.main.customers", "Type": "Sequential Scan", "Estimated Cardinality": "100"}, "children": []}
        ]}
      ]}
    ]}
  ]}],
  "query": {"cpu_time": 0.008, "total_time": 0.02, "sql": "explain ..."}
}"#;

#[test]
fn duckdb_analyzed_plans_drop_the_explain_wrappers_and_add_measures() {
    let doc = parse_plan(DUCKDB_ANALYZED, "analyzed").unwrap();
    shape(&doc);
    assert!(doc.summary.analyzed);
    assert_eq!(doc.summary.execution_ms, Some(20.0));
    assert_eq!(doc.nodes[0].node_type, "HASH_JOIN");
    assert_eq!(doc.nodes.len(), 3);
    let orders = &doc.nodes[1];
    assert_eq!(orders.metrics.actual_rows, Some(1000.0));
    assert_eq!(orders.metrics.actual_rows_per_loop, Some(1000.0));
    assert_eq!(orders.metrics.loops, Some(1.0));
    assert_eq!(orders.metrics.self_time_ms, Some(3.0));
    assert_eq!(orders.metrics.rows_removed, Some(119_000.0));
    assert!(
        orders
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::FilteredRows)
    );
    assert!(
        orders
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::LargeSequentialScan)
    );
    // Cumulative time sums the operator's own time and its children's.
    let join = &doc.nodes[0].metrics;
    assert_eq!(join.self_time_ms, Some(4.0));
    assert!((join.time_ms.unwrap() - 8.0).abs() < 1e-9);
    assert_eq!(doc.summary.slowest.as_ref().map(|s| s.node), Some(0));
    // A step's estimate against what it returned.
    assert_eq!(join.estimated_rows, Some(202.0));
    assert!(doc.summary.estimate_gap.is_some());
}

#[test]
fn duckdb_older_profiler_output_and_several_roots_are_read() {
    let doc = parse_plan(
        r#"{"latency": 0.005, "query_name": "q", "children": [
          {"operator_type": "PROJECTION", "operator_timing": 0.001, "operator_cardinality": 3, "extra_info": {"Projections": ["a"]}, "children": [
            {"operator_name": "SEQ_SCAN ", "operator_timing": 0.002, "operator_cardinality": 3, "extra_info": "t\n\nEC: 3", "children": []}
          ]}
        ]}"#,
        "old",
    )
    .unwrap();
    shape(&doc);
    assert_eq!(doc.summary.execution_ms, Some(5.0));
    assert_eq!(doc.nodes[0].node_type, "PROJECTION");
    assert_eq!(doc.nodes[1].node_type, "SEQ_SCAN");
    assert!(doc.nodes[1].sequential_scan);
    assert_eq!(doc.nodes[1].detail.as_deref(), Some("t\n\nEC: 3"));
    assert_eq!(doc.nodes[1].metrics.actual_rows, Some(3.0));
    // Two roots go under one synthetic step, as every tree needs one root.
    let two = parse_plan(
        r#"[{"name": "A", "children": []}, {"name": "B", "children": [{"name": "C", "children": []}]}]"#,
        "two",
    )
    .unwrap();
    shape(&two);
    assert_eq!(two.nodes[0].node_type, "Query plan");
    assert_eq!(two.nodes[0].children.len(), 2);
}

#[test]
fn malformed_duckdb_plans_are_refused() {
    for (raw, why) in [
        (r#"[{"name": "A", "children": 3}]"#, "children"),
        (r#"[{"name": "A", "children": [5]}]"#, "object"),
        (
            r#"[{"name": "A", "children": [], "extra_info": {"Estimated Cardinality": "-1"}}]"#,
            "finite",
        ),
        (
            r#"[{"name": "A", "children": [{"children": []}]}]"#,
            "operator name",
        ),
        (
            r#"{"operator": [{"type": "A", "timing": "x", "children": []}]}"#,
            "finite",
        ),
        (r#"{"operator": []}"#, "no steps"),
    ] {
        let error = parse_plan(raw, "bad").unwrap_err().to_string();
        assert!(error.contains(why), "{raw}: {error}");
    }
    let mut deep = String::from(r#"{"name": "leaf", "children": []}"#);
    // 61 levels: past the tree bound, inside serde_json's own nesting limit.
    for _ in 0..61 {
        deep = format!(r#"{{"name": "P", "children": [{deep}]}}"#);
    }
    let error = parse_plan(&format!("[{deep}]"), "deep")
        .unwrap_err()
        .to_string();
    assert!(error.contains("60 levels"), "{error}");
}
