use super::*;

fn fixture(name: &str) -> PlanDocument {
    let path = format!(
        "{}/tests/fixtures/plans/sqlserver/{name}.sqlplan",
        env!("CARGO_MANIFEST_DIR")
    );
    read_plan(std::path::Path::new(&path)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn real_sqlserver_showplans_have_common_tree_and_runtime_semantics() {
    let seek = fixture("seek");
    assert_eq!(seek.summary.engine, "sqlserver");
    assert_eq!(seek.summary.raw_format, PlanRawFormat::Xml);
    assert!(!seek.summary.analyzed);
    assert!(
        seek.nodes
            .iter()
            .any(|n| n.node_type == "Index Seek" && n.index.is_some())
    );
    assert!(fixture("scan").nodes.iter().any(|n| n.sequential_scan));
    assert!(
        fixture("join")
            .nodes
            .iter()
            .any(|n| n.node_type == "Hash Match" && n.operation == PlanOperation::Join)
    );
    let parallel = fixture("parallel");
    assert!(parallel.summary.analyzed);
    assert!(
        parallel
            .nodes
            .iter()
            .any(|n| n.parallel && n.metrics.actual_rows.is_some())
    );
    for node in parallel.nodes.iter().filter(|n| n.parallel) {
        assert_eq!(node.metrics.time_ms, None);
        assert_eq!(node.metrics.self_time_ms, None);
        assert_eq!(node.metrics.time_per_loop_ms, None);
    }
    let spill = fixture("spill");
    assert!(
        spill
            .nodes
            .iter()
            .any(|n| n.sort_spilled && n.findings.iter().any(|f| f.kind == FindingKind::SortSpill))
    );
    for document in [&seek, &parallel, &spill] {
        for (index, node) in document.nodes.iter().enumerate() {
            assert_eq!(node.id as usize, index);
            for child in &node.children {
                assert_eq!(document.nodes[*child as usize].parent, Some(node.id));
            }
            assert_eq!(node.metrics.heap_fetches, None);
        }
    }
}

#[test]
fn rejects_untrusted_and_malformed_xml() {
    let root = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><StmtSimple StatementType="SELECT"/></ShowPlanXML>"#;
    assert!(parse_plan(root, "ok").is_ok());
    let prefixed = "<p:ShowPlanXML xmlns:p='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><p:StmtSimple StatementType='SELECT'>one &amp; two &#x41;</p:StmtSimple></p:ShowPlanXML>";
    assert!(parse_plan(prefixed, "prefixed").is_ok());
    for bad in [
        "<!DOCTYPE x SYSTEM 'file:///etc/passwd'><x/>",
        "<!DOCTYPE ShowPlanXML [<!ENTITY x SYSTEM 'http://127.0.0.1/'>]><ShowPlanXML>&x;</ShowPlanXML>",
        "<ShowPlanXML xmlns='wrong'><StmtSimple/></ShowPlanXML>",
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple></ShowPlanXML>",
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple StatementType='&xxe;'/></ShowPlanXML>",
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple/><StmtSimple/></ShowPlanXML>",
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple><evil:RelOp xmlns:evil='urn:other' PhysicalOp='Scan'/></StmtSimple></ShowPlanXML>",
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple xmlns='urn:other'/></ShowPlanXML>",
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple>&#0;</StmtSimple></ShowPlanXML>",
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple/></ShowPlanXML><?xml version='1.0'?>",
        "<?xml version='1.0'?><?xml version='1.0'?><ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'><StmtSimple/></ShowPlanXML>",
    ] {
        assert!(parse_plan(bad, "bad").is_err(), "accepted {bad}");
    }
    let nested = format!(
        "<ShowPlanXML xmlns='http://schemas.microsoft.com/sqlserver/2004/07/showplan'>{}<StmtSimple/>{}</ShowPlanXML>",
        "<x>".repeat(1025),
        "</x>".repeat(1025)
    );
    assert!(matches!(
        parse_plan(&nested, "deep"),
        Err(PlanError::Limit(_))
    ));
    assert!(matches!(
        parse_plan(&"x".repeat(MAX_PLAN_BYTES + 1), "large"),
        Err(PlanError::Limit(_))
    ));
}

#[test]
fn utf16_sqlplan_roundtrips_with_original_encoding() {
    let xml = "<?xml version=\"1.0\" encoding=\"utf-16\"?><ShowPlanXML xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/showplan\"><StmtSimple StatementType=\"SELECT\" StatementText=\"élève\"/></ShowPlanXML>";
    for little in [true, false] {
        let mut bytes = if little {
            vec![0xff, 0xfe]
        } else {
            vec![0xfe, 0xff]
        };
        for n in xml.encode_utf16() {
            let word = if little {
                n.to_le_bytes()
            } else {
                n.to_be_bytes()
            };
            bytes.extend_from_slice(&word);
        }
        let dir =
            std::env::temp_dir().join(format!("spelaion-sqlplan-{}-{little}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.sqlplan");
        let output = dir.join("output.sqlplan");
        std::fs::write(&source, &bytes).unwrap();
        let doc = read_plan(&source).unwrap();
        let store = PlanStore::new();
        let id = store.insert(doc).unwrap().id;
        store
            .save_raw(id, &output, &crate::ExportCancel::default())
            .unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        assert!(read_plan(&output).is_ok());
        // The independent platform XML reader honors both the BOM and XML declaration.
        #[cfg(windows)]
        {
            let result = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; $s=New-Object System.Xml.XmlReaderSettings; $s.DtdProcessing=[System.Xml.DtdProcessing]::Prohibit; $r=[System.Xml.XmlReader]::Create($env:SPELAION_TEST_PLAN_XML,$s); while($r.Read()) {}; $r.Dispose()"])
                .env("SPELAION_TEST_PLAN_XML", &output)
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
