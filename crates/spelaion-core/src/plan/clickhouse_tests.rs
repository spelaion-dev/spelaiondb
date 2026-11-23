//! ClickHouse plans in the shared tree (SDB-324): the document ClickHouse 25 writes for
//! `EXPLAIN json = 1, indexes = 1, description = 1`, older plans without `Node Id`, PostgreSQL's
//! look-alike plans kept apart, and malformed input refused. The real server runs in
//! `tests/plans_clickhouse.rs`.
use super::*;

/// A join of two MergeTree tables, filtered, grouped, sorted and limited, as ClickHouse 25.8
/// writes it (headers and expressions left out).
const JOIN: &str = r#"[
  {
    "Plan": {
      "Node Type": "Expression", "Node Id": "Expression_12", "Description": "Project names",
      "Plans": [{
        "Node Type": "Limit", "Node Id": "Limit_11",
        "Description": "preliminary LIMIT (without OFFSET)", "Limit": 5,
        "Plans": [{
          "Node Type": "Sorting", "Node Id": "Sorting_10", "Description": "Sorting for ORDER BY",
          "Sort Description": [{"Column": "sum(__table1.total)", "Ascending": false, "With Fill": false}],
          "Plans": [{
            "Node Type": "Aggregating", "Node Id": "Aggregating_7", "Keys": ["__table2.name"],
            "Plans": [{
              "Node Type": "Join", "Node Id": "Join_22",
              "Plans": [
                {
                  "Node Type": "Expression", "Node Id": "Expression_20",
                  "Description": "(WHERE + Change column names to column identifiers)",
                  "Plans": [{
                    "Node Type": "ReadFromMergeTree", "Node Id": "ReadFromMergeTree_0",
                    "Description": "spelaion.pl_orders",
                    "Indexes": [
                      {"Type": "MinMax", "Keys": ["day"], "Condition": "(day in [19754, +Inf))",
                       "Initial Parts": 2, "Selected Parts": 1, "Initial Granules": 2, "Selected Granules": 1},
                      {"Type": "PrimaryKey", "Keys": ["customer", "id"], "Condition": "(customer in [7, 7])",
                       "Search Algorithm": "binary search",
                       "Initial Parts": 1, "Selected Parts": 1, "Initial Granules": 1, "Selected Granules": 1}
                    ]
                  }]
                },
                {
                  "Node Type": "ReadFromMergeTree", "Node Id": "ReadFromMergeTree_2",
                  "Description": "spelaion.pl_customers",
                  "Indexes": [
                    {"Type": "PrimaryKey", "Keys": ["id"], "Condition": "true",
                     "Initial Parts": 1, "Selected Parts": 1, "Initial Granules": 4, "Selected Granules": 4}
                  ]
                }
              ]
            }]
          }]
        }]
      }]
    }
  }
]"#;

fn node<'a>(doc: &'a PlanDocument, node_type: &str) -> &'a PlanNode {
    doc.nodes
        .iter()
        .find(|n| n.node_type == node_type)
        .unwrap_or_else(|| panic!("no {node_type} in {:?}", doc.nodes))
}

#[test]
fn a_clickhouse_plan_reads_into_steps_with_their_indexes() {
    let doc = parse_plan(JOIN, "plan").unwrap();
    assert_eq!(doc.summary.engine, "clickhouse");
    assert_eq!(doc.summary.raw_format, PlanRawFormat::Json);
    assert!(!doc.summary.analyzed);
    assert_eq!(doc.nodes.len(), 8);
    assert_eq!(doc.nodes[0].node_type, "Expression");
    assert_eq!(doc.nodes[0].detail.as_deref(), Some("Project names"));
    // Parents before children, each child naming its parent.
    for node in &doc.nodes {
        for &child in &node.children {
            assert_eq!(doc.nodes[child as usize].parent, Some(node.id));
            assert_eq!(doc.nodes[child as usize].depth, node.depth + 1);
        }
    }
    assert_eq!(node(&doc, "Join").operation, PlanOperation::Join);
    assert_eq!(
        node(&doc, "Aggregating").operation,
        PlanOperation::Aggregate
    );
    assert_eq!(
        node(&doc, "Aggregating").detail.as_deref(),
        Some("Keys: __table2.name")
    );
    let sorting = node(&doc, "Sorting");
    assert_eq!(sorting.operation, PlanOperation::Sort);
    assert_eq!(
        sorting.detail.as_deref(),
        Some("Sorting for ORDER BY; Sort: sum(__table1.total) DESC")
    );
    assert_eq!(
        node(&doc, "Limit").detail.as_deref(),
        Some("preliminary LIMIT (without OFFSET); Limit: 5")
    );

    let reads: Vec<&PlanNode> = doc
        .nodes
        .iter()
        .filter(|n| n.node_type == "ReadFromMergeTree")
        .collect();
    assert_eq!(reads.len(), 2);
    let orders = reads[0];
    assert_eq!(orders.operation, PlanOperation::Scan);
    assert_eq!(orders.relation.as_deref(), Some("spelaion.pl_orders"));
    assert_eq!(orders.index.as_deref(), Some("MinMax, PrimaryKey"));
    assert_eq!(
        orders.index_condition.as_deref(),
        Some("(day in [19754, +Inf)) AND (customer in [7, 7])")
    );
    assert_eq!(
        orders.detail.as_deref(),
        Some(
            "MinMax (day): 1/2 parts, 1/2 granules; PrimaryKey (customer, id): 1/1 parts, 1/1 granules"
        )
    );
    // An index left out granules: not the whole table.
    assert!(!orders.sequential_scan);
    // Every granule kept, the condition `true`: the whole table.
    let customers = reads[1];
    assert_eq!(customers.relation.as_deref(), Some("spelaion.pl_customers"));
    assert_eq!(customers.index_condition, None);
    assert!(customers.sequential_scan);
    assert_eq!(doc.summary.sequential_scans, 1);
    assert_eq!(doc.summary.relation_count, 2);
    // ClickHouse reports no metrics: none is made up.
    for node in &doc.nodes {
        assert_eq!(node.metrics.cost, None);
        assert_eq!(node.metrics.estimated_rows, None);
        assert_eq!(node.metrics.time_ms, None);
    }
    assert!(doc.summary.note.contains("ClickHouse"));
}

#[test]
fn older_plans_without_node_ids_and_several_roots_read_too() {
    // ClickHouse before 24 wrote no `Node Id`; a read from a table engine tells it apart.
    let old = r#"[{"Plan": {"Node Type": "Expression", "Plans": [
        {"Node Type": "ReadFromStorage", "Description": "SystemNumbers"}]}}]"#;
    let doc = parse_plan(old, "plan").unwrap();
    assert_eq!(doc.summary.engine, "clickhouse");
    assert_eq!(doc.nodes[1].relation.as_deref(), Some("SystemNumbers"));
    // A read that uses no index reads everything.
    assert!(doc.nodes[1].sequential_scan);
    // Two statements' plans go under one root.
    let two = r#"[
        {"Plan": {"Node Type": "Expression", "Node Id": "Expression_1"}},
        {"Plan": {"Node Type": "Expression", "Node Id": "Expression_2"}}]"#;
    let doc = parse_plan(two, "plan").unwrap();
    assert_eq!(doc.nodes[0].node_type, "Query plan");
    assert_eq!(doc.nodes[0].children, [1, 2]);
}

#[test]
fn postgres_plans_are_not_taken_for_clickhouse_ones() {
    let postgres = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "t",
        "Startup Cost": 0.0, "Total Cost": 1.5, "Plan Rows": 10, "Plan Width": 4}}]"#;
    assert_eq!(
        parse_plan(postgres, "plan").unwrap().summary.engine,
        "postgres"
    );
}

#[test]
fn malformed_clickhouse_plans_are_refused() {
    for bad in [
        r#"[{"Plan": {"Node Id": "Expression_1"}}]"#,
        r#"[{"Plan": {"Node Type": "Expression", "Node Id": "Expression_1", "Plans": {}}}]"#,
        r#"[{"Plan": {"Node Type": "ReadFromMergeTree", "Node Id": "R", "Indexes": "PrimaryKey"}}]"#,
        r#"[{"Plan": {"Node Type": "Expression", "Node Id": "E", "Plans": [1]}}]"#,
    ] {
        assert!(parse_plan(bad, "plan").is_err(), "{bad}");
    }
    // Deeper than the bound.
    let mut deep = String::from(r#"{"Node Type": "Expression", "Node Id": "E"}"#);
    for _ in 0..=MAX_PLAN_DEPTH {
        deep = format!(r#"{{"Node Type": "Expression", "Node Id": "E", "Plans": [{deep}]}}"#);
    }
    let deep = format!(r#"[{{"Plan": {deep}}}]"#);
    assert!(matches!(
        parse_plan(&deep, "plan"),
        Err(PlanError::Limit(_))
    ));
}
