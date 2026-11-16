//! ER diagrams (ADR-046) on every engine: SQLite and DuckDB always, PostgreSQL, MySQL, MariaDB
//! and SQL Server with their `SPELAION_TEST_*` variables (without them they are skipped, which
//! does not count as tested).
//!
//! - A small schema with a composite key, a self-reference, a one-to-one key and a table with
//!   no keys is read from each engine's catalog as a schema (with the explorer's table filter),
//!   as chosen tables and around one focused table, and laid out.
//! - A generated 200-table schema is read and laid out once, timed (the PR states the time).
//! - The Mermaid and DBML texts of an awkward model and of the small schema are written to
//!   `apps/desktop/scripts/fixtures/er`, where `scripts/er-text.test.mjs` parses them with
//!   Mermaid's and DBML's own parsers (`npm run build`, so CI runs it). A text that changed
//!   fails here until the fixtures are written again with `SPELAION_UPDATE_ER_FIXTURES=1`.

mod engines;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use spelaion_core::{
    Cardinality, CoreEvent, DiagramScope, ErColumns, ErLayout, ErLayoutOptions, ErMetrics, ErModel,
    ResultStore, Session, TableName, dbml, er_layout, mermaid, read_er_model,
};
use spelaion_driver_api::{
    ColumnInfo, ConnectParams, ConstraintInfo, ConstraintKind, Driver, ForeignKeyInfo, NameFilter,
    ObjectInfo, ObjectKind, Secret, TableInfo, TlsMode,
};

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn login(host: String, port: u16, user: &str, password: &str, database: &str) -> ConnectParams {
    ConnectParams {
        host,
        port,
        user: user.into(),
        password: Some(Secret::new(password)),
        database: database.into(),
        ..Default::default()
    }
}

fn memory() -> ConnectParams {
    ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    }
}

/// Every engine available here, with the schema its tables go in.
async fn engines() -> Vec<(&'static str, Session, &'static str)> {
    let mut out = vec![
        (
            "sqlite",
            session(&*engines::sqlite(), memory()).await,
            "main",
        ),
        (
            "duckdb",
            session(&*engines::duckdb(), memory()).await,
            "main",
        ),
    ];
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        let params = login(host, port, "spelaion", "spelaion", "spelaion");
        out.push((
            "postgres",
            session(&*engines::postgres(), params).await,
            "public",
        ));
    }
    for (label, var, driver) in [
        ("mysql", "SPELAION_TEST_MYSQL", engines::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", engines::mariadb()),
    ] {
        if let Some((host, port)) = server(var) {
            let params = login(host, port, "spelaion", "spelaion", "spelaion");
            out.push((label, session(&driver, params).await, "spelaion"));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push((
            "sqlserver",
            session(&*engines::sqlserver(), params).await,
            "dbo",
        ));
    }
    out
}

/// Runs each statement on its own; panics on a failure unless `tolerate`.
async fn run(label: &str, session: &Session, statements: &[String], tolerate: bool) {
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            if let CoreEvent::StatementFailed { error, .. } = event
                && !tolerate
            {
                panic!("{label}: {statement}\n→ {}", error.message)
            }
        }
    }
}

/// Creates the small schema (children dropped first).
async fn create_schema(label: &str, session: &Session) {
    let q = |name: &str| session.dialect().quote_identifier(name);
    let region = q("order region");
    let order = q("erd_order");
    let drops: Vec<String> = [
        "erd_profile",
        "erd_line",
        "erd_order",
        "erd_customer",
        "erd_country",
        "erd_alone",
    ]
    .iter()
    .map(|t| format!("DROP TABLE {}", q(t)))
    .collect();
    run(label, session, &drops, true).await;
    let statements = vec![
        "CREATE TABLE erd_country (code CHAR(2) PRIMARY KEY, name VARCHAR(40))".to_string(),
        "CREATE TABLE erd_customer (id INT PRIMARY KEY, country CHAR(2) NOT NULL, \
         referrer INT, email VARCHAR(80) UNIQUE, \
         FOREIGN KEY (country) REFERENCES erd_country (code), \
         FOREIGN KEY (referrer) REFERENCES erd_customer (id))"
            .to_string(),
        format!(
            "CREATE TABLE {order} ({region} VARCHAR(10) NOT NULL, number INT NOT NULL, \
             customer INT, PRIMARY KEY ({region}, number), \
             FOREIGN KEY (customer) REFERENCES erd_customer (id))"
        ),
        format!(
            "CREATE TABLE erd_line (id INT PRIMARY KEY, {region} VARCHAR(10), number INT, \
             CONSTRAINT erd_line_order FOREIGN KEY ({region}, number) \
             REFERENCES {order} ({region}, number))"
        ),
        "CREATE TABLE erd_profile (customer INT PRIMARY KEY, bio VARCHAR(200), \
         FOREIGN KEY (customer) REFERENCES erd_customer (id))"
            .to_string(),
        "CREATE TABLE erd_alone (id INT)".to_string(),
    ];
    run(label, session, &statements, false).await;
}

const METRICS: ErMetrics = ErMetrics {
    char_width: 7.2,
    line_height: 18.0,
};

fn all_options() -> ErLayoutOptions {
    ErLayoutOptions {
        columns: ErColumns::All,
        types: true,
        comments: true,
    }
}

/// No two boxes overlap, and every relation starts and ends on its boxes' sides.
fn check_layout(label: &str, model: &ErModel, layout: &ErLayout) {
    assert_eq!(layout.nodes.len(), model.tables.len(), "{label}");
    assert_eq!(layout.edges.len(), model.relations.len(), "{label}");
    let mut boxes: Vec<_> = layout.nodes.iter().collect();
    boxes.sort_by(|a, b| a.x.total_cmp(&b.x));
    for (i, a) in boxes.iter().enumerate() {
        assert!(a.x >= 0.0 && a.y >= 0.0, "{label}: {} outside", a.title);
        assert!(
            a.x + a.width <= layout.width && a.y + a.height <= layout.height,
            "{label}: {} outside the diagram",
            a.title
        );
        for b in &boxes[i + 1..] {
            if b.x >= a.x + a.width {
                break;
            }
            let apart = a.y + a.height <= b.y || b.y + b.height <= a.y;
            assert!(apart, "{label}: {} overlaps {}", a.title, b.title);
        }
    }
    for edge in &layout.edges {
        let (from, to) = (&layout.nodes[edge.from], &layout.nodes[edge.to]);
        let on_side = |p: &spelaion_core::ErPoint, n: &spelaion_core::ErNode| {
            (p.x == n.x || p.x == n.x + n.width) && p.y >= n.y && p.y <= n.y + n.height
        };
        assert!(
            on_side(&edge.points[0], from),
            "{label}: {} start",
            edge.name
        );
        assert!(on_side(&edge.points[5], to), "{label}: {} end", edge.name);
    }
}

fn find(model: &ErModel, name: &str) -> usize {
    model
        .tables
        .iter()
        .position(|t| t.name.eq_ignore_ascii_case(name))
        .unwrap_or_else(|| panic!("no table {name}"))
}

fn relation<'a>(model: &'a ErModel, from: &str, to: &str) -> &'a spelaion_core::ErRelation {
    let (from, to) = (find(model, from), find(model, to));
    model
        .relations
        .iter()
        .find(|r| r.from == from && r.to == to)
        .unwrap_or_else(|| panic!("no relation {from} → {to}"))
}

#[tokio::test]
async fn schemas_tables_and_focus_on_every_engine() {
    for (label, session, schema) in engines().await {
        create_schema(label, &session).await;
        let conn = session.connection();
        let filter = NameFilter {
            include: vec!["erd_*".into()],
            exclude: vec![],
        };
        let mut progress = Vec::new();
        let model = read_er_model(
            conn.as_ref(),
            &DiagramScope::Schema {
                schema: schema.into(),
                filter: Some(filter),
            },
            |p| progress.push(p),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(model.tables.len(), 6, "{label}: {:?}", model.tables);
        assert_eq!(model.unreadable, 0, "{label}: {:?}", model.first_error);
        assert_eq!(
            progress.last().map(|p| (p.read, p.total)),
            Some((6, 6)),
            "{label}"
        );
        assert_eq!(model.relations.len(), 5, "{label}: {:?}", model.relations);
        assert_eq!(model.outside_keys, 0, "{label}");

        let country = relation(&model, "erd_customer", "erd_country");
        assert_eq!(country.to_cardinality, Cardinality::One, "{label}");
        assert_eq!(country.from_cardinality, Cardinality::ZeroOrMany, "{label}");
        assert!(!country.identifying, "{label}");
        let referrer = relation(&model, "erd_customer", "erd_customer");
        assert_eq!(referrer.to_cardinality, Cardinality::ZeroOrOne, "{label}");
        let line = relation(&model, "erd_line", "erd_order");
        assert_eq!(line.from_columns.len(), 2, "{label}");
        assert_eq!(line.to_columns.len(), 2, "{label}");
        assert_eq!(line.to_cardinality, Cardinality::ZeroOrOne, "{label}");
        let profile = relation(&model, "erd_profile", "erd_customer");
        assert_eq!(profile.from_cardinality, Cardinality::ZeroOrOne, "{label}");
        assert_eq!(profile.to_cardinality, Cardinality::One, "{label}");
        assert!(profile.identifying, "{label}");
        let customer = &model.tables[find(&model, "erd_customer")];
        assert!(
            customer
                .unique
                .iter()
                .any(|key| key.len() == 1 && key[0].eq_ignore_ascii_case("email")),
            "{label}: {:?}",
            customer.unique
        );
        for columns in [ErColumns::All, ErColumns::Keys, ErColumns::None] {
            let options = ErLayoutOptions {
                columns,
                ..all_options()
            };
            check_layout(label, &model, &er_layout(&model, &options, &METRICS));
        }
        if label == "sqlite" {
            fixture("shop.mmd", &mermaid(&model));
            fixture("shop.dbml", &dbml(&model));
        }

        // Chosen tables: the key to a table left out is counted, not drawn.
        let chosen = |name: &str| TableName {
            schema: customer.schema.clone(),
            name: model.tables[find(&model, name)].name.clone(),
        };
        let pair = read_er_model(
            conn.as_ref(),
            &DiagramScope::Tables {
                tables: vec![chosen("erd_line"), chosen("erd_order"), chosen("erd_line")],
            },
            |_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(pair.tables.len(), 2, "{label}");
        assert_eq!(pair.relations.len(), 1, "{label}");
        assert_eq!(pair.outside_keys, 1, "{label}");

        // Focus: the table, what it points to and what points to it; nothing further.
        let focus = read_er_model(
            conn.as_ref(),
            &DiagramScope::Focus {
                table: chosen("erd_customer"),
            },
            |_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        let mut names: Vec<_> = focus.tables.iter().map(|t| t.name.to_lowercase()).collect();
        names.sort();
        assert_eq!(
            names,
            ["erd_country", "erd_customer", "erd_order", "erd_profile"],
            "{label}"
        );
        assert_eq!(focus.focus, Some(find(&focus, "erd_customer")), "{label}");
        // erd_order's key to... nothing else; erd_line is two steps away.
        assert_eq!(focus.relations.len(), 4, "{label}: {:?}", focus.relations);
        check_layout(label, &focus, &er_layout(&focus, &all_options(), &METRICS));

        // A table that is gone is counted, and the others are still read.
        let missing = read_er_model(
            conn.as_ref(),
            &DiagramScope::Tables {
                tables: vec![
                    chosen("erd_alone"),
                    TableName {
                        schema: customer.schema.clone(),
                        name: "erd_not_there".into(),
                    },
                ],
            },
            |_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(missing.tables.len(), 1, "{label}");
        assert_eq!(missing.unreadable, 1, "{label}");
        assert!(missing.first_error.is_some(), "{label}");
    }
}

/// 200 tables: chains, hubs, a cycle, self-references, composite keys and tables with no keys,
/// the same every run.
fn big_schema() -> Vec<String> {
    let mut seed: u64 = 0x5eed;
    let mut next = |n: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let mut statements = Vec::new();
    for i in 0..200u64 {
        let mut columns = vec![
            "id INT NOT NULL".to_string(),
            "code INT NOT NULL".to_string(),
            "name VARCHAR(60)".to_string(),
        ];
        let mut keys = vec!["PRIMARY KEY (id, code)".to_string()];
        if i % 17 == 0 {
            keys = vec!["PRIMARY KEY (id)".to_string()];
            columns.push("parent INT".into());
            keys.push(format!("FOREIGN KEY (parent) REFERENCES t{i:03} (id)"));
        }
        // Every 23rd table stands alone; the others point to one or two earlier tables, mostly
        // the first few (hubs).
        if i > 0 && i % 23 != 0 {
            for k in 0..(1 + next(2)) {
                let target = if next(3) == 0 { next(5) } else { next(i) };
                if target % 17 == 0 {
                    continue;
                }
                columns.push(format!("ref{k}_id INT"));
                columns.push(format!("ref{k}_code INT"));
                keys.push(format!(
                    "FOREIGN KEY (ref{k}_id, ref{k}_code) REFERENCES t{target:03} (id, code)"
                ));
            }
        }
        for c in 0..next(6) {
            columns.push(format!("extra_{c} VARCHAR(30)"));
        }
        statements.push(format!(
            "CREATE TABLE t{i:03} ({}, {})",
            columns.join(", "),
            keys.join(", ")
        ));
    }
    statements
}

#[tokio::test]
async fn a_200_table_schema_is_laid_out_once() {
    let session = session(&*engines::sqlite(), memory()).await;
    run("sqlite", &session, &big_schema(), false).await;
    let conn = session.connection();
    let started = Instant::now();
    let model = read_er_model(
        conn.as_ref(),
        &DiagramScope::Schema {
            schema: "main".into(),
            filter: None,
        },
        |_| {},
    )
    .await
    .expect("reading the catalog");
    let read = started.elapsed();
    assert_eq!(model.tables.len(), 200);
    assert!(model.relations.len() > 200, "{}", model.relations.len());

    let started = Instant::now();
    let layout = er_layout(&model, &all_options(), &METRICS);
    let laid_out = started.elapsed();
    check_layout("200 tables", &model, &layout);
    let keys = er_layout(
        &model,
        &ErLayoutOptions {
            columns: ErColumns::Keys,
            ..all_options()
        },
        &METRICS,
    );
    check_layout("200 tables, keys only", &model, &keys);
    // A sensible shape: neither a single row nor a single column of boxes.
    let ratio = layout.width / layout.height;
    assert!(
        (0.2..=5.0).contains(&ratio),
        "{} × {}",
        layout.width,
        layout.height
    );
    eprintln!(
        "200 tables, {} relations: catalog read in {read:?}, layout in {laid_out:?} \
         ({} × {} px)",
        model.relations.len(),
        layout.width,
        layout.height
    );
    // The same model and options always give the same layout.
    assert_eq!(er_layout(&model, &all_options(), &METRICS), layout);
}

/// A model with everything the text formats find hard, built without a database.
fn awkward_model() -> ErModel {
    let column = |name: &str, ty: &str, nullable: bool, comment: Option<&str>| ColumnInfo {
        name: name.into(),
        native_type: ty.into(),
        nullable,
        default: None,
        comment: comment.map(Into::into),
    };
    let object = |schema: &str, name: &str, comment: Option<&str>| ObjectInfo {
        schema: schema.into(),
        name: name.into(),
        kind: ObjectKind::Table,
        estimated_rows: None,
        comment: comment.map(Into::into),
    };
    let fk = |name: &str, columns: &[&str], schema: &str, table: &str, referenced: &[&str]| {
        ForeignKeyInfo {
            name: name.into(),
            columns: columns.iter().map(|c| (*c).into()).collect(),
            referenced_schema: schema.into(),
            referenced_table: table.into(),
            referenced_columns: referenced.iter().map(|c| (*c).into()).collect(),
        }
    };
    let unique = |columns: &[&str]| ConstraintInfo {
        name: None,
        kind: ConstraintKind::Unique,
        columns: columns.iter().map(|c| (*c).into()).collect(),
        definition: String::new(),
    };
    let tables = vec![
        TableInfo {
            object: object(
                "sales",
                "order lines",
                Some("Lines of an order: it's \"quoted\"\nand on two lines \\ with a backslash"),
            ),
            columns: vec![
                column("id", "integer", false, Some("the line's number")),
                column("order no", "character varying(80)", false, None),
                column(
                    "pk",
                    "numeric(12,2)",
                    true,
                    Some("a column named like a key"),
                ),
                column("1st", "\"char\"", true, None),
                column(
                    "name\"quote",
                    "text",
                    true,
                    Some("a \"double\" quote in the name"),
                ),
                column("日本語", "text", true, Some("non-Latin name")),
                column("Ref", "int", true, None),
                column("Note", "int", true, None),
                column("indexes", "int", true, None),
                column(
                    "back\\slash",
                    "int",
                    true,
                    Some("a \\' pair, and a backslash at the end \\"),
                ),
                column("untyped", "", true, None),
                column("Table", "timestamp with time zone", true, None),
                column("uk", "uk", true, None),
                column("arr", "int[]", true, None),
            ],
            primary_key: vec!["id".into(), "order no".into()],
            indexes: vec![],
            foreign_keys: vec![
                fk("fk_1", &["order no"], "sales", "Orders", &["number"]),
                // The same key twice, as an engine may hold it.
                fk("fk_1_again", &["order no"], "sales", "Orders", &["number"]),
                fk(
                    "lines→product",
                    &["Ref", "Note"],
                    "Ventas ñ",
                    "producto",
                    &["id", "variante"],
                ),
            ],
            constraints: vec![unique(&["pk"]), unique(&["Ref", "Note"])],
        },
        TableInfo {
            object: object("sales", "Orders", None),
            columns: vec![
                column("number", "character varying(80)", false, None),
                column("parent", "character varying(80)", true, None),
                column("customer", "int", true, None),
            ],
            primary_key: vec!["number".into()],
            indexes: vec![],
            foreign_keys: vec![
                fk("fk_1", &["parent"], "sales", "Orders", &["number"]),
                fk("fk_customer", &["customer"], "sales", "orders", &["id"]),
            ],
            constraints: vec![],
        },
        TableInfo {
            // Same name but for case.
            object: object("sales", "orders", Some("lower case")),
            columns: vec![column("id", "int", false, None)],
            primary_key: vec!["id".into()],
            indexes: vec![],
            foreign_keys: vec![],
            constraints: vec![],
        },
        TableInfo {
            object: object("Ventas ñ", "producto", None),
            columns: vec![
                column("id", "int", false, None),
                column("variante", "int", false, None),
                column("ficha", "int", false, Some("one-to-one")),
            ],
            primary_key: vec!["id".into(), "variante".into()],
            indexes: vec![],
            foreign_keys: vec![fk(
                "producto_ficha",
                &["ficha"],
                "Ventas ñ",
                "ficha",
                &["id"],
            )],
            constraints: vec![unique(&["ficha"])],
        },
        TableInfo {
            object: object("Ventas ñ", "ficha", None),
            columns: vec![column("id", "int", false, None)],
            primary_key: vec!["id".into()],
            indexes: vec![],
            foreign_keys: vec![],
            constraints: vec![],
        },
        TableInfo {
            object: object("Ventas ñ", "empty", None),
            columns: vec![],
            primary_key: vec![],
            indexes: vec![],
            foreign_keys: vec![],
            constraints: vec![],
        },
    ];
    ErModel::from_tables(tables, None)
}

#[test]
fn awkward_names_are_written_as_text() {
    let model = awkward_model();
    assert_eq!(model.relations.len(), 6);
    let mermaid_text = mermaid(&model);
    let dbml_text = dbml(&model);
    // Qualified names across schemas, the two keys with the same columns written once.
    assert!(mermaid_text.contains("\"sales.order lines\" {"));
    assert_eq!(dbml_text.matches("Ref ").count(), 5);
    assert!(dbml_text.contains("// \"Ventas ñ\".\"empty\" has no columns"));
    fixture("awkward.mmd", &mermaid_text);
    fixture("awkward.dbml", &dbml_text);
}

/// Checks `text` against the fixture of that name, or writes it with
/// `SPELAION_UPDATE_ER_FIXTURES=1`.
fn fixture(name: &str, text: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/desktop/scripts/fixtures/er")
        .join(name);
    if std::env::var_os("SPELAION_UPDATE_ER_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        return;
    }
    let current = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| {
            panic!(
                "{}: {e} (write it with SPELAION_UPDATE_ER_FIXTURES=1)",
                path.display()
            )
        })
        .replace("\r\n", "\n");
    assert_eq!(
        current, text,
        "{name} is out of date: write it again with SPELAION_UPDATE_ER_FIXTURES=1 and check \
         that `node --test scripts/er-text.test.mjs` still parses it"
    );
}
