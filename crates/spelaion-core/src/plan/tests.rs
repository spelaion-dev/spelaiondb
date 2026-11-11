use super::*;

const SIMPLE: &str = include_str!("../../tests/fixtures/plans/postgres/simple.json");
const JOIN: &str = include_str!("../../tests/fixtures/plans/postgres/join.json");
const CTE: &str = include_str!("../../tests/fixtures/plans/postgres/cte.json");
const PARALLEL: &str = include_str!("../../tests/fixtures/plans/postgres/parallel.json");
const SPILL: &str = include_str!("../../tests/fixtures/plans/postgres/spill.json");

#[test]
fn real_postgres_plan_fixtures_preserve_optional_metrics_and_shape() {
    let simple = parse_plan(SIMPLE, "simple").unwrap();
    assert_eq!(simple.nodes.len(), 1);
    assert!(!simple.summary.analyzed);
    assert_eq!(simple.nodes[0].metrics.actual_rows, None);
    assert_eq!(simple.nodes[0].metrics.time_ms, None);
    assert!(simple.nodes[0].index.is_some());
    let join = parse_plan(JOIN, "join").unwrap();
    assert!(
        join.nodes
            .iter()
            .any(|n| n.node_type == "Hash Join" && n.hash_condition.is_some())
    );
    let cte = parse_plan(CTE, "cte").unwrap();
    assert!(cte.nodes.iter().any(|n| n.node_type == "CTE Scan"));
    let parallel = parse_plan(PARALLEL, "parallel").unwrap();
    assert!(parallel.nodes.iter().any(|n| n.node_type == "Gather"
        && n.metrics.workers_planned.is_some_and(|n| n > 0.0)
        && n.metrics.workers_launched.is_some_and(|n| n > 0.0)));
    let spill = parse_plan(SPILL, "spill").unwrap();
    assert!(spill.nodes.iter().any(|n| n.sort_spilled
        && n.metrics.sort_space_kb.is_some_and(|n| n > 0.0)
        && n.findings.iter().any(|f| f.kind == FindingKind::SortSpill)));
    for doc in [simple, join, cte, parallel, spill] {
        assert_eq!(doc.summary.node_count as usize, doc.nodes.len());
        assert_eq!(doc.summary.total_cost, doc.nodes[0].metrics.cost);
        for node in &doc.nodes {
            for id in &node.children {
                assert_eq!(doc.nodes[*id as usize].parent, Some(node.id));
                assert_eq!(doc.nodes[*id as usize].depth, node.depth + 1);
            }
            if let Some((rows, loops)) = node.metrics.actual_rows_per_loop.zip(node.metrics.loops) {
                assert_eq!(node.metrics.actual_rows, Some(rows * loops));
            }
            if !node.children.is_empty() && node.parallel {
                assert_eq!(node.metrics.self_time_ms, None);
                assert_eq!(node.metrics.self_cost, None);
            }
        }
    }
}

fn node() -> PlanNode {
    let mut n=parse_plan(r#"[{"Plan":{"Node Type":"Index Scan","Total Cost":20,"Plan Rows":100,"Actual Rows":100,"Actual Loops":1,"Actual Total Time":1,"Shared Hit Blocks":1000,"Shared Read Blocks":0,"Rows Removed by Filter":0,"Heap Fetches":0}}]"#,"test").unwrap().nodes.remove(0);
    n.metrics.workers_planned = Some(2.0);
    n.metrics.workers_launched = Some(2.0);
    n
}

#[test]
fn postgres_local_buffers_and_visited_rows_include_filtered_work() {
    let doc = parse_plan(r#"[{"Plan":{"Node Type":"Seq Scan","Plan Rows":9,"Actual Rows":9,"Actual Loops":2,"Actual Total Time":1,"Rows Removed by Filter":119991,"Shared Hit Blocks":0,"Shared Read Blocks":0,"Local Hit Blocks":3,"Local Read Blocks":101,"Temp Read Blocks":2}}]"#, "local").unwrap();
    let n = &doc.nodes[0];
    assert_eq!(n.metrics.buffers_hit, Some(3.0));
    assert_eq!(n.metrics.buffers_read, Some(101.0));
    assert_eq!(n.metrics.temporary_read, Some(2.0));
    assert!(
        n.findings
            .iter()
            .any(|f| f.kind == FindingKind::LargeSequentialScan)
    );
    assert!(n.findings.iter().any(|f| f.kind == FindingKind::DiskReads));
    let mut small = n.clone();
    small.metrics.rows_removed = Some(4900.0);
    assert!(
        !findings(&small, None)
            .iter()
            .any(|f| f.kind == FindingKind::LargeSequentialScan)
    );
    small.metrics.rows_removed = None;
    assert!(
        !findings(&small, None)
            .iter()
            .any(|f| f.kind == FindingKind::LargeSequentialScan)
    );
    let local_only = parse_plan(
        r#"{"Plan":{"Node Type":"Seq Scan","Local Hit Blocks":7,"Local Read Blocks":8}}"#,
        "local only",
    )
    .unwrap();
    assert_eq!(local_only.nodes[0].metrics.buffers_hit, Some(7.0));
    assert_eq!(local_only.nodes[0].metrics.buffers_read, Some(8.0));
}

#[test]
fn self_metrics_only_subtract_comparable_serial_inputs() {
    let raw = r#"{"Plan":{"Node Type":"Sort","Total Cost":50,"Actual Total Time":10,"Actual Loops":1,"Plans":[{"Node Type":"Seq Scan","Total Cost":20,"Actual Total Time":4,"Actual Loops":1}]},"Execution Time":11}"#;
    let doc = parse_plan(raw, "serial").unwrap();
    assert_eq!(doc.nodes[0].metrics.self_cost, Some(30.0));
    assert_eq!(doc.nodes[0].metrics.self_time_ms, Some(6.0));
    assert_eq!(doc.summary.highest_cost.as_ref().unwrap().node, 0);
    assert_eq!(doc.summary.slowest.as_ref().unwrap().node, 0);
    for kind in ["Limit", "Nested Loop"] {
        let ambiguous = parse_plan(&raw.replace("Sort", kind), "partial").unwrap();
        assert_eq!(ambiguous.nodes[0].metrics.self_cost, None);
        assert_eq!(ambiguous.nodes[0].metrics.self_time_ms, None);
    }
    let mut parallel: serde_json::Value = serde_json::from_str(raw).unwrap();
    parallel["Plan"]["Plans"][0]["Parallel Aware"] = true.into();
    let parallel = parse_plan(&parallel.to_string(), "parallel").unwrap();
    assert_eq!(parallel.nodes[0].metrics.self_time_ms, None);
    assert!(parallel.summary.slowest.is_none());
    assert!(
        !parallel
            .nodes
            .iter()
            .flat_map(|n| &n.findings)
            .any(|f| f.kind == FindingKind::Hotspot)
    );
    let mut repeated: serde_json::Value = serde_json::from_str(raw).unwrap();
    repeated["Plan"]["Plans"][0]["Actual Loops"] = 3.into();
    assert_eq!(
        parse_plan(&repeated.to_string(), "repeated").unwrap().nodes[0]
            .metrics
            .self_time_ms,
        None
    );
}

#[test]
fn temporary_operation_counts_do_not_count_inclusive_io_on_ancestors() {
    let raw = r#"{"Plan":{"Node Type":"Aggregate","Temp Written Blocks":12,"Plans":[{"Node Type":"Sort","Temp Written Blocks":12,"Sort Space Type":"Disk","Plans":[{"Node Type":"Seq Scan"}]}]}}"#;
    assert_eq!(
        parse_plan(raw, "spill")
            .unwrap()
            .summary
            .temporary_operations,
        1
    );
    let without_spill = raw.replace("Disk", "Memory");
    assert_eq!(
        parse_plan(&without_spill, "memory")
            .unwrap()
            .summary
            .temporary_operations,
        0
    );
}

#[test]
fn postgres_verbose_names_preserve_schema_identity_and_bitmap_index_namespace() {
    let raw = r#"{"Plan":{"Node Type":"Append","Plans":[{"Node Type":"Index Scan","Schema":"public","Relation Name":"orders","Index Name":"orders_idx"},{"Node Type":"Index Scan","Schema":"archive","Relation Name":"orders","Index Name":"orders_idx"}]}}"#;
    let doc = parse_plan(raw, "schema identity").unwrap();
    assert_eq!(doc.summary.relation_count, 2);
    assert_eq!(doc.summary.index_count, 2);
    assert_eq!(doc.nodes[1].relation.as_deref(), Some("public.orders"));
    assert_eq!(doc.nodes[2].index.as_deref(), Some("archive.orders_idx"));
    let bitmap = r#"{"Plan":{"Node Type":"Bitmap Heap Scan","Schema":"archive","Relation Name":"orders","Plans":[{"Node Type":"BitmapOr","Plans":[{"Node Type":"Bitmap Index Scan","Index Name":"orders_idx"}]}]}}"#;
    assert_eq!(
        parse_plan(bitmap, "bitmap").unwrap().nodes[2]
            .index
            .as_deref(),
        Some("archive.orders_idx")
    );
    let unknown = raw
        .replace("\"Schema\":\"public\",", "")
        .replace("\"Schema\":\"archive\",", "");
    let unknown = parse_plan(&unknown, "saved without verbose").unwrap();
    assert_eq!(unknown.nodes[1].relation.as_deref(), Some("orders"));
    assert_eq!(unknown.nodes[2].index.as_deref(), Some("orders_idx"));
    assert_eq!(
        unknown.summary.relation_count, 1,
        "unknown schemas are never guessed"
    );
    let dotted = raw
        .replace("\"public\"", "\"first.part\"")
        .replace("\"archive\"", "\"first\"")
        .replace(
            "\"Relation Name\":\"orders\",\"Index Name\":\"orders_idx\"}]",
            "\"Relation Name\":\"part.orders\",\"Index Name\":\"orders_idx\"}]",
        );
    let dotted = parse_plan(&dotted, "quoted identity").unwrap();
    assert_eq!(dotted.summary.relation_count, 2);
    assert_eq!(
        dotted.nodes[1].relation.as_deref(),
        Some("\"first.part\".orders")
    );
    assert_eq!(
        dotted.nodes[2].relation.as_deref(),
        Some("first.\"part.orders\"")
    );
}

#[test]
fn every_finding_has_a_trigger_and_a_non_trigger() {
    type FindingCase = (FindingKind, fn(&mut PlanNode), fn(&mut PlanNode));
    let cases: [FindingCase; 11] = [
        (
            FindingKind::Hotspot,
            |n| n.metrics.self_time_ms = Some(50.0),
            |n| n.metrics.self_time_ms = Some(49.0),
        ),
        (
            FindingKind::UnderestimatedRows,
            |n| {
                n.metrics.estimated_rows = Some(10.0);
                n.metrics.actual_rows_per_loop = Some(100.0);
            },
            |n| {
                n.metrics.estimated_rows = Some(11.0);
                n.metrics.actual_rows_per_loop = Some(100.0);
            },
        ),
        (
            FindingKind::OverestimatedRows,
            |n| {
                n.metrics.estimated_rows = Some(100.0);
                n.metrics.actual_rows_per_loop = Some(10.0);
            },
            |n| {
                n.metrics.estimated_rows = Some(100.0);
                n.metrics.actual_rows_per_loop = Some(11.0);
            },
        ),
        (
            FindingKind::SortSpill,
            |n| n.sort_spilled = true,
            |n| n.sort_spilled = false,
        ),
        (
            FindingKind::FilteredRows,
            |n| n.metrics.rows_removed = Some(900.0),
            |n| n.metrics.rows_removed = Some(899.0),
        ),
        (
            FindingKind::LargeSequentialScan,
            |n| {
                n.node_type = "Seq Scan".into();
                n.sequential_scan = true;
                n.metrics.actual_rows_per_loop = Some(10_000.0);
                n.metrics.actual_rows = Some(10_000.0);
            },
            |n| {
                n.node_type = "Seq Scan".into();
                n.sequential_scan = true;
                n.metrics.actual_rows_per_loop = Some(9_999.0);
                n.metrics.actual_rows = Some(9_999.0);
            },
        ),
        (
            FindingKind::HeapFetches,
            |n| n.metrics.heap_fetches = Some(1000.0),
            |n| n.metrics.heap_fetches = Some(999.0),
        ),
        (
            FindingKind::MissingWorkers,
            |n| n.metrics.workers_launched = Some(1.0),
            |n| n.metrics.workers_launched = Some(2.0),
        ),
        (
            FindingKind::HighLoops,
            |n| n.metrics.loops = Some(1000.0),
            |n| n.metrics.loops = Some(999.0),
        ),
        (
            FindingKind::DiskReads,
            |n| {
                n.metrics.buffers_read = Some(101.0);
                n.metrics.buffers_hit = Some(100.0);
            },
            |n| {
                n.metrics.buffers_read = Some(100.0);
                n.metrics.buffers_hit = Some(100.0);
            },
        ),
        (
            FindingKind::NeverExecuted,
            |n| n.metrics.loops = Some(0.0),
            |n| n.metrics.loops = Some(1.0),
        ),
    ];
    for (kind, positive, negative) in cases {
        let mut yes = node();
        positive(&mut yes);
        assert!(
            findings(&yes, Some(100.0)).iter().any(|f| f.kind == kind),
            "missing {kind:?}"
        );
        let mut no = node();
        negative(&mut no);
        assert!(
            !findings(&no, Some(100.0)).iter().any(|f| f.kind == kind),
            "false {kind:?}"
        );
    }
    let mut missing = node();
    missing.metrics = PlanMetrics::default();
    assert!(findings(&missing, None).is_empty());
    let mut never = node();
    never.metrics.loops = Some(0.0);
    never.metrics.estimated_rows = Some(10_000.0);
    never.metrics.actual_rows_per_loop = Some(0.0);
    assert_eq!(findings(&never, Some(100.0)).len(), 1);
}

#[test]
fn plan_pages_sort_siblings_keep_topology_and_bound_all_views() {
    let mut doc = parse_plan(JOIN, "join").unwrap();
    doc.nodes[0].relation = Some("=1+2".into());
    let page = doc.page(&PlanView::default(), 0, 1).unwrap();
    assert_eq!(page.nodes.len(), 1);
    assert_eq!(page.next_offset, Some(1));
    let all = doc.page(&PlanView::default(), 0, 9999).unwrap();
    assert!(all.nodes.len() <= MAX_PLAN_PAGE);
    let collapsed = doc
        .page(
            &PlanView {
                collapsed: vec![0],
                ..Default::default()
            },
            0,
            100,
        )
        .unwrap();
    assert_eq!(collapsed.nodes.len(), 1);
    let ordered = doc
        .page(
            &PlanView {
                sort: PlanSort::Cost,
                descending: true,
                ..Default::default()
            },
            0,
            100,
        )
        .unwrap();
    assert_eq!(ordered.nodes[0].id, 0);
    for n in &ordered.nodes {
        if let Some(parent) = n.parent {
            assert!(
                ordered.nodes.iter().position(|n| n.id == parent)
                    < ordered.nodes.iter().position(|p| p.id == n.id)
            );
        }
    }
    for limit in [1, 2, 50, 1000] {
        let json = doc.tool_json(0, limit, 60_000).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value["page"]["nodes"].as_array().unwrap().len() <= MAX_PLAN_PAGE);
        assert!(json.len() <= 60_000);
    }
    assert!(doc.tool_json(0, 1, 10).is_err());
    let stats = doc.statistics(PlanStatKind::Relations, 0, 1);
    assert!(stats.items.len() <= 1);
}

#[tokio::test]
async fn plan_store_file_io_is_bounded_atomic_and_escapes_csv() {
    let dir = std::env::temp_dir().join(format!("spelaion-plan-store-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = PlanStore::new();
    let mut doc = parse_plan(JOIN, "test").unwrap();
    doc.nodes[0].relation = Some("=1+2".into());
    let summary = store.insert(doc).unwrap();
    let raw = store.raw_window(summary.id, 0).unwrap();
    assert_eq!(raw.text, JOIN);
    assert_eq!(raw.next_offset, None);
    let file = dir.join("raw.json");
    store
        .save_raw(summary.id, &file, &crate::ExportCancel::new())
        .unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), JOIN);
    assert_eq!(read_plan(&file).unwrap().nodes.len(), 5);
    let csv = dir.join("plan.csv");
    store
        .export_csv(
            summary.id,
            &PlanView::default(),
            &csv,
            &crate::ExportCancel::new(),
        )
        .unwrap();
    assert!(std::fs::read_to_string(&csv).unwrap().contains("'=1+2"));
    let cancel = crate::ExportCancel::new();
    cancel.cancel().await;
    std::fs::write(&file, "previous").unwrap();
    assert!(store.save_raw(summary.id, &file, &cancel).is_err());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "previous");
    let impossible = dir.join("folder");
    std::fs::create_dir_all(&impossible).unwrap();
    assert!(
        store
            .save_raw(summary.id, &impossible, &crate::ExportCancel::new())
            .is_err()
    );
    assert!(!std::fs::read_dir(&dir).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains("spelaion-tmp")
    }));
    #[cfg(unix)]
    crate::test_files::assert_keeps_mode_and_links("plan", |path| {
        store
            .save_raw(summary.id, path, &crate::ExportCancel::new())
            .unwrap();
        store
            .export_csv(
                summary.id,
                &PlanView::default(),
                path,
                &crate::ExportCancel::new(),
            )
            .unwrap();
    });
    store.release(summary.id);
    assert!(store.summary(summary.id).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn malformed_and_oversized_plans_are_refused_not_guessed() {
    for raw in [
        "null",
        "[]",
        "[{},{}]",
        r#"[{"Plan":{"Node Type":3}}]"#,
        r#"[{"Plan":{"Node Type":"Scan","Plan Rows":-1}}]"#,
        r#"[{"Plan":{"Node Type":"Scan","Plans":true}}]"#,
    ] {
        assert!(parse_plan(raw, "bad").is_err(), "{raw}");
    }
    assert!(parse_plan(&" ".repeat(MAX_PLAN_BYTES + 1), "too big").is_err());
    let many = serde_json::json!({"Plan":{"Node Type":"Append","Plans":vec![serde_json::json!({"Node Type":"Result"});MAX_PLAN_NODES]}});
    assert!(parse_plan(&many.to_string(), "too many").is_err());
    let mut deep = serde_json::json!({"Node Type":"Result"});
    for _ in 0..MAX_PLAN_DEPTH + 1 {
        deep = serde_json::json!({"Node Type":"Append","Plans":[deep]});
    }
    assert!(parse_plan(&serde_json::json!({"Plan":deep}).to_string(), "too deep").is_err());
}

#[test]
fn raw_windows_keep_unicode_boundaries_and_mcp_never_slices_a_large_node() {
    let raw = format!(
        "\u{feff}{{\"Plan\":{{\"Node Type\":\"Result\",\"Filter\":{}}}}}",
        serde_json::to_string(&"五".repeat(25_000)).unwrap()
    );
    let store = PlanStore::new();
    let summary = store.insert(parse_plan(&raw, "UTF-8").unwrap()).unwrap();
    let mut combined = String::new();
    let mut offset = 0;
    loop {
        let window = store.raw_window(summary.id, offset).unwrap();
        assert!(window.text.len() <= 32_768);
        combined.push_str(&window.text);
        match window.next_offset {
            Some(next) => offset = next as usize,
            None => break,
        }
    }
    assert_eq!(combined, raw);
    assert!(store.raw_window(summary.id, 1).is_err()); // Inside the UTF-8 BOM.
    assert!(store.tool_json(summary.id, 0, 50, 60_000).is_err());
}
