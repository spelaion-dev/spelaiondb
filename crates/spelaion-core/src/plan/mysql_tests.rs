//! Real MySQL 8.4 and MariaDB 11.4 plans (tests/fixtures/plans/{mysql,mariadb}) in the
//! shared tree, and malformed input refused.
use super::mysql::describe_for_test as describe;
use super::*;

fn fixture(engine: &str, file: &str) -> PlanDocument {
    let path = format!(
        "{}/tests/fixtures/plans/{engine}/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    read_plan(std::path::Path::new(&path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn node<'a>(doc: &'a PlanDocument, node_type: &str) -> &'a PlanNode {
    doc.nodes
        .iter()
        .find(|n| n.node_type == node_type)
        .unwrap_or_else(|| panic!("no {node_type} in {:?}", doc.nodes))
}

/// What every MySQL/MariaDB document must hold, whatever the format.
fn shared_shape(doc: &PlanDocument) {
    assert_eq!(doc.summary.node_count as usize, doc.nodes.len());
    assert_eq!(doc.summary.total_cost, doc.nodes[0].metrics.cost);
    for (i, node) in doc.nodes.iter().enumerate() {
        assert_eq!(node.id as usize, i);
        for id in &node.children {
            assert_eq!(doc.nodes[*id as usize].parent, Some(node.id));
            assert_eq!(doc.nodes[*id as usize].depth, node.depth + 1);
        }
        let m = &node.metrics;
        if let Some((rows, loops)) = m.actual_rows_per_loop.zip(m.loops) {
            assert_eq!(m.actual_rows, finite(rows * loops));
        }
        // Counters these engines never report stay unavailable, never zero.
        assert_eq!(m.heap_fetches, None);
        assert_eq!(m.workers_planned, None);
        assert_eq!(m.workers_launched, None);
        assert_eq!(m.temporary_read, None);
        assert_eq!(m.temporary_written, None);
        assert_eq!(m.sort_space_kb, None);
        assert!(!node.parallel && !node.sort_spilled);
    }
}

fn no_execution_metrics(doc: &PlanDocument) {
    assert!(!doc.summary.analyzed && !doc.summary.buffers);
    assert_eq!(doc.summary.execution_ms, None);
    for n in &doc.nodes {
        let m = &n.metrics;
        assert_eq!(
            (
                m.actual_rows,
                m.actual_rows_per_loop,
                m.time_ms,
                m.self_time_ms,
                m.loops
            ),
            (None, None, None, None, None),
            "{}",
            n.node_type
        );
        assert_eq!(
            (m.buffers_hit, m.buffers_read, m.rows_removed),
            (None, None, None)
        );
    }
}

#[test]
fn real_mysql_estimated_plans_parse_into_the_shared_tree() {
    let simple = fixture("mysql", "simple.json");
    let row = node(&simple, "Constant row");
    assert_eq!(
        (row.relation.as_deref(), row.index.as_deref()),
        (Some("orders"), Some("PRIMARY"))
    );

    let join = fixture("mysql", "join.json");
    assert_eq!(join.summary.engine, "mysql");
    assert_eq!(join.summary.raw_format, PlanRawFormat::Json);
    assert!(join.summary.total_cost.is_some_and(|c| c > 0.0));
    let nested = node(&join, "Nested loop");
    assert_eq!(nested.operation, PlanOperation::Join);
    assert_eq!(nested.children.len(), 2, "both tables, in join order");
    let scan = node(&join, "Full table scan");
    assert!(scan.sequential_scan && scan.relation.as_deref() == Some("o"));
    assert!(scan.filter.as_deref().is_some_and(|f| f.contains("'paid'")));
    // Estimated output per scan: rows examined × filtered %.
    assert!(
        scan.metrics
            .estimated_rows
            .is_some_and(|r| r > 10_000.0 && r < 12_000.0)
    );
    assert_eq!(scan.metrics.self_cost, scan.metrics.cost);
    assert_eq!(
        node(&join, "Unique index lookup").index.as_deref(),
        Some("PRIMARY")
    );
    assert_eq!(
        node(&join, "Ordering (filesort)").operation,
        PlanOperation::Sort
    );
    assert_eq!(join.summary.sorts, 1);

    let subquery = fixture("mysql", "subquery.json");
    assert_eq!(node(&subquery, "Union result").children.len(), 2);
    assert_eq!(
        node(&subquery, "Materialize").operation,
        PlanOperation::Temporary
    );
    let dependent = node(&subquery, "Subquery");
    assert!(
        dependent
            .detail
            .as_deref()
            .is_some_and(|d| d.contains("dependent"))
    );
    assert!(
        subquery
            .nodes
            .iter()
            .any(|n| n.detail.as_deref() == Some("select #4; No tables used"))
    );

    let v2 = fixture("mysql", "join-v2.json");
    assert!(
        v2.nodes.iter().any(|n| n
            .relation
            .as_deref()
            .is_some_and(|r| r.ends_with(".orders"))),
        "format version 2 names schema and table"
    );
    assert!(node(&v2, "Filter").filter.is_some());
    assert!(node(&v2, "Nested loop inner join").metrics.cost.is_some());

    for doc in [&simple, &join, &subquery, &v2] {
        shared_shape(doc);
        no_execution_metrics(doc);
    }
}

#[test]
fn real_mysql_analyze_trees_parse_into_the_shared_tree() {
    let analyzed = fixture("mysql", "analyze-join.txt");
    assert_eq!(analyzed.summary.raw_format, PlanRawFormat::Text);
    assert!(analyzed.summary.analyzed && !analyzed.summary.buffers);
    let lookup = node(&analyzed, "Single-row index lookup");
    assert_eq!(lookup.index_condition.as_deref(), Some("id=o.customer_id"));
    assert_eq!(lookup.metrics.loops, Some(30_000.0));
    assert!(
        lookup
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::HighLoops)
    );
    let temporary = node(&analyzed, "Table scan");
    assert_eq!(
        temporary.detail.as_deref(),
        Some("Table scan on <temporary>")
    );
    assert!(temporary.relation.is_none() && !temporary.sequential_scan);
    let scan = analyzed
        .nodes
        .iter()
        .find(|n| n.relation.as_deref() == Some("o"))
        .unwrap();
    assert!(scan.sequential_scan && scan.metrics.actual_rows == Some(120_000.0));
    assert!(
        scan.findings
            .iter()
            .any(|f| f.kind == FindingKind::LargeSequentialScan)
    );
    // The temporary table scanned for the sort is not a base-table sequential scan.
    assert_eq!(analyzed.summary.sequential_scans, 1);

    let never = fixture("mysql", "analyze-never.txt");
    let skipped = node(&never, "Single-row index lookup");
    assert_eq!(skipped.metrics.loops, Some(0.0));
    assert_eq!(
        (skipped.metrics.actual_rows, skipped.metrics.time_ms),
        (None, None)
    );
    assert_eq!(skipped.findings.len(), 1);
    assert_eq!(skipped.findings[0].kind, FindingKind::NeverExecuted);

    let hash = fixture("mysql", "analyze-hash.txt");
    let join = node(&hash, "Inner hash join");
    assert_eq!(join.join_type.as_deref(), Some("inner"));
    assert!(
        join.hash_condition
            .as_deref()
            .is_some_and(|c| c.contains("concat("))
    );
    assert!(
        join.metrics.cost.is_some_and(|c| c > 3_000_000.0),
        "scientific notation"
    );
    let build = node(&hash, "Hash");
    assert_eq!(
        (build.metrics.cost, build.metrics.loops),
        (None, None),
        "MySQL prints no figures for it"
    );

    let sub = fixture("mysql", "analyze-subquery.txt");
    assert_eq!(
        node(&sub, "Covering index range scan")
            .index_condition
            .as_deref(),
        Some("(country = 'ES') OR (country = 'FR')")
    );
    assert_eq!(
        node(&sub, "Materialize").operation,
        PlanOperation::Temporary
    );
    assert!(sub.nodes.iter().any(|n| n.node_type == "Select #2"));
    assert_eq!(
        node(&sub, "Index lookup").metrics.actual_rows_per_loop,
        Some(0.25)
    );

    for doc in [&analyzed, &never, &hash, &sub] {
        shared_shape(doc);
        assert_eq!(doc.summary.engine, "mysql");
        assert!(
            doc.nodes
                .iter()
                .all(|n| n.metrics.buffers_hit.is_none() && n.metrics.rows_removed.is_none())
        );
    }
}

#[test]
fn real_mariadb_plans_parse_into_the_shared_tree() {
    let simple = fixture("mariadb", "simple.json");
    assert_eq!(
        simple.summary.engine, "mariadb",
        "recognized without a cost"
    );
    assert_eq!(
        node(&simple, "Constant row").index.as_deref(),
        Some("PRIMARY")
    );
    assert!(
        !simple.nodes.iter().any(|n| n.node_type == "Nested loop"),
        "one table is not a join"
    );

    let join = fixture("mariadb", "join.json");
    assert_eq!(node(&join, "Sort").operation, PlanOperation::Sort);
    assert_eq!(
        node(&join, "Temporary table").operation,
        PlanOperation::Temporary
    );
    let subquery = fixture("mariadb", "subquery.json");
    assert!(
        subquery
            .nodes
            .iter()
            .any(|n| n.node_type == "Subquery cache")
    );
    for doc in [&simple, &join, &subquery] {
        assert_eq!(doc.summary.engine, "mariadb");
        shared_shape(doc);
        no_execution_metrics(doc);
    }

    let analyzed = fixture("mariadb", "analyze-join.json");
    assert!(analyzed.summary.analyzed && analyzed.summary.buffers);
    assert!(analyzed.summary.planning_ms.is_some() && analyzed.summary.execution_ms.is_some());
    let m = &node(&analyzed, "Index lookup").metrics;
    assert_eq!(m.loops, Some(1000.0));
    // r_rows 120 per loop, r_filtered 25 %: 30 kept, 90 removed by the attached condition.
    assert_eq!(m.actual_rows_per_loop, Some(30.0));
    assert_eq!(m.rows_removed, Some(90.0));
    assert_eq!(m.actual_rows, Some(30_000.0));
    assert_eq!(m.buffers_hit, Some(242_184.0));
    assert_eq!(m.buffers_read, Some(0.0));
    assert!(m.time_ms.is_some() && m.time_ms == m.self_time_ms);
    let sort = node(&analyzed, "Sort");
    assert!(
        sort.metrics.self_time_ms.is_some() && sort.metrics.time_ms.is_none(),
        "a sort's time is its own"
    );
    assert_eq!(sort.sort_method.as_deref(), Some("sort_key,rowid"));

    let hash = fixture("mariadb", "analyze-hash.json");
    let block = node(&hash, "Block nested loop");
    assert_eq!(block.join_type.as_deref(), Some("BNL"));
    assert!(block.filter.is_some());
    assert!(
        block
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::Hotspot),
        "most of the time is the block join's own"
    );

    let never = fixture("mariadb", "analyze-never.json");
    let filtered = node(&never, "Index lookup");
    assert_eq!(filtered.metrics.actual_rows_per_loop, Some(0.0));
    assert!(
        filtered
            .findings
            .iter()
            .any(|f| f.kind == FindingKind::FilteredRows)
    );

    let sub = fixture("mariadb", "analyze-subquery.json");
    assert_eq!(node(&sub, "Subquery").metrics.loops, Some(100.0));
    assert!(sub.nodes.iter().any(|n| n.node_type == "No tables used"));
    let derived = sub
        .nodes
        .iter()
        .find(|n| n.relation.as_deref() == Some("<derived3>"))
        .unwrap();
    assert_eq!(
        derived.metrics.time_ms, None,
        "a table with a materialized child has no inclusive time"
    );
    assert!(derived.metrics.self_time_ms.is_some());

    for doc in [&analyzed, &hash, &never, &sub] {
        assert_eq!(doc.summary.engine, "mariadb");
        shared_shape(doc);
    }
}

#[tokio::test]
async fn mysql_plan_files_keep_their_text_and_format() {
    let dir = std::env::temp_dir().join(format!("spelaion-mysql-plan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = format!(
        "{}/tests/fixtures/plans/mysql/analyze-join.txt",
        env!("CARGO_MANIFEST_DIR")
    );
    let original = std::fs::read_to_string(&source).unwrap();
    let store = PlanStore::new();
    let summary = store
        .insert(read_plan(std::path::Path::new(&source)).unwrap())
        .unwrap();
    assert_eq!(summary.raw_format, PlanRawFormat::Text);
    assert_eq!(store.raw_window(summary.id, 0).unwrap().text, original);
    let saved = dir.join("plan.txt");
    store
        .save_raw(summary.id, &saved, &crate::ExportCancel::new())
        .unwrap();
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), original);
    assert_eq!(
        read_plan(&saved).unwrap().nodes.len(),
        summary.node_count as usize
    );
    let csv = dir.join("plan.csv");
    store
        .export_csv(
            summary.id,
            &PlanView::default(),
            &csv,
            &crate::ExportCancel::new(),
        )
        .unwrap();
    let csv = std::fs::read_to_string(&csv).unwrap();
    assert!(csv.lines().next().unwrap().contains("Detail"));
    assert!(csv.contains("Single-row index lookup on c using PRIMARY (id=o.customer_id)"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mysql_step_descriptions_are_read_without_guessing() {
    type Case<'a> = (
        &'a str,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );
    let cases: [Case; 8] = [
        (
            "Index range scan on o using PRIMARY over (id < 100)",
            "Index range scan",
            Some("o"),
            Some("PRIMARY"),
            Some("id < 100"),
        ),
        (
            "Index lookup on c using idx (a=b), with index condition: (x > 1)",
            "Index lookup",
            Some("c"),
            Some("idx"),
            Some("a=b"),
        ),
        ("Table scan on <temporary>", "Table scan", None, None, None),
        (
            "Index scan on t using idx (reverse)",
            "Index scan",
            Some("t"),
            Some("idx"),
            None,
        ),
        ("Update o (buffered)", "Update", Some("o"), None, None),
        ("Limit: 20 row(s)", "Limit", None, None, None),
        (
            "Aggregate using temporary table",
            "Aggregate using temporary table",
            None,
            None,
            None,
        ),
        (
            "Remove duplicates from input grouped on t.a",
            "Remove duplicates from input grouped on t.a",
            None,
            None,
            None,
        ),
    ];
    for (text, node_type, relation, index, condition) in cases {
        let d = describe(text);
        assert_eq!(
            (d.0.as_str(), d.1.as_deref(), d.2.as_deref(), d.3.as_deref()),
            (node_type, relation, index, condition),
            "{text}"
        );
    }
    let left = parse_plan(
        "-> Left hash join (a = b)  (cost=1 rows=1)\n    -> Table scan on t  (cost=1 rows=1)\n    -> Hash\n        -> Table scan on u  (cost=1 rows=1)\n",
        "t",
    )
    .unwrap();
    assert_eq!(left.nodes[0].join_type.as_deref(), Some("left"));
    assert_eq!(left.nodes[0].hash_condition.as_deref(), Some("a = b"));
    assert!(
        !left.summary.analyzed,
        "FORMAT=TREE without ANALYZE is an estimate"
    );
    let anti = parse_plan(
        "-> Nested loop antijoin  (cost=1 rows=1)\n    -> Table scan on t  (cost=1 rows=1)\n",
        "t",
    )
    .unwrap();
    assert_eq!(anti.nodes[0].join_type.as_deref(), Some("antijoin"));
}

#[test]
fn malformed_mysql_and_mariadb_plans_are_refused_not_guessed() {
    assert!(
        parse_plan("-> <not executable by iterator executor>\n", "x")
            .unwrap_err()
            .to_string()
            .contains("cannot analyze")
    );
    for raw in [
        "-> A\n-> B\n",
        "-> Table scan on t  (cost=-1 rows=1)\n",
        "-> Table scan on t  (actual time=1..x rows=1 loops=1)\n",
        r#"{"query_block":{"table":{"table_name":"t","rows_examined_per_scan":-5}}}"#,
        r#"{"query_block":{"table":{"table_name":"t","filtered":"lots"}}}"#,
        r#"{"query_block":{"nested_loop":true}}"#,
        r#"{"query_block":{"table":3}}"#,
        r#"{"query_block":{"select_list_subqueries":{"query_block":{}}}}"#,
        r#"{"operation":"Table scan on t","inputs":3}"#,
        r#"{"operation":"Filter: x","inputs":[{"table_name":"t"}]}"#,
    ] {
        assert!(parse_plan(raw, "bad").is_err(), "{raw}");
    }
    let mut deep = String::new();
    for level in 0..=MAX_PLAN_DEPTH + 1 {
        deep.push_str(&format!(
            "{}-> Filter: (a)\n",
            " ".repeat(4 * level as usize)
        ));
    }
    assert!(matches!(
        parse_plan(&deep, "deep"),
        Err(PlanError::Limit(_))
    ));
    let wide = serde_json::json!({"operation":"Append","inputs":vec![serde_json::json!({"operation":"Table scan on t"}); MAX_PLAN_NODES]});
    assert!(matches!(
        parse_plan(&wide.to_string(), "wide"),
        Err(PlanError::Limit(_))
    ));
    let mut nested = serde_json::json!({"table":{"table_name":"t","access_type":"ALL"}});
    for _ in 0..MAX_PLAN_DEPTH + 1 {
        nested = serde_json::json!({ "query_block": nested });
    }
    assert!(matches!(
        parse_plan(&nested.to_string(), "nested"),
        Err(PlanError::Limit(_))
    ));
}
