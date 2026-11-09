//! Real SQLite and DuckDB plan runs (both compiled in: they always run), and SQLite on a
//! server through `sqlite3 -safe` (ADR-018) with `SPELAION_TEST_SSH`.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Engine, LocalPort, Password, ResolvedConnection, Resolver, Source, SystemBackend,
    TrustStore, load,
};
use spelaion_core::plan::{
    self, AnalyzeUnavailable, ExplainOptions, ExplainPolicy, PlanDocument, PlanOperation,
    PlanRawFormat,
};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadLimits, ResultStore, Session, TunnelPool,
    open_connection,
};
use spelaion_driver_api::{ConnectParams, Secret};
use spelaion_ssh::{HostInfo, Interaction, KnownHosts, NonInteractive, SecretRequest};

/// A database file deleted when the test ends.
struct TempFile(PathBuf);

impl TempFile {
    /// An empty database of `engine`, created as the app's "new database" does.
    fn new(name: &str, engine: Engine) -> Self {
        let extension = match engine {
            Engine::Duckdb => "duckdb",
            _ => "db",
        };
        let path = std::env::temp_dir().join(format!(
            "spelaion-plans-{name}-{}.{extension}",
            std::process::id()
        ));
        let file = Self(path);
        file.remove();
        match engine {
            Engine::Duckdb => spelaion_driver_duckdb::create_database(&file.0).unwrap(),
            _ => spelaion_driver_sqlite::create_database(&file.0).unwrap(),
        }
        file
    }

    fn remove(&self) {
        for suffix in ["", ".wal", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        self.remove();
    }
}

async fn open(engine: Engine, path: &Path, read_only: bool) -> OpenConnection {
    let resolved = ResolvedConnection {
        id: ConnectionId {
            source: Source::User,
            path: "plans.toml".into(),
        },
        name: "Plans test".into(),
        engine,
        env: None,
        kind: None,
        params: ConnectParams {
            database: path.display().to_string(),
            read_only,
            ..Default::default()
        },
        password: Password::None,
        tunnel: None,
        kubernetes: None,
        sqlite3: None,
        local_port: LocalPort::Any,
        keepalive: None,
        statement_timeout: Some(Duration::from_secs(5)),
        session: Default::default(),
        mcp: Default::default(),
    };
    open_connection(
        resolved,
        &Engines::builtin(),
        OpenOptions {
            tunnels: Arc::new(TunnelPool::new()),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: std::env::temp_dir().join("unused-plan-known-hosts"),
            },
            interaction: Arc::new(NonInteractive),
            password: None,
            read_only_session: read_only,
        },
        Arc::new(ResultStore::new()),
    )
    .await
    .unwrap()
}

/// Runs a script and returns the first column of its results' rows.
async fn run(session: &Session, sql: &str) -> Vec<String> {
    let mut values = Vec::new();
    for event in session.run_script(sql).collect::<Vec<_>>().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {error:?}"),
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(result).unwrap();
                let stored = stored.read().unwrap();
                for row in 0..stored.rows() {
                    values.push(stored.value_at(row, 0).unwrap().to_string());
                }
                session.results().remove(result);
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    values
}

const SETUP: &str = "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT); \
     CREATE INDEX customers_email ON customers (email); \
     CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, total REAL NOT NULL); \
     INSERT INTO customers VALUES (1, 'Ana', 'ana@example.com'), (2, 'Luis', 'luis@example.com'), (3, 'Eva', NULL); \
     INSERT INTO orders VALUES (1, 1, 10), (2, 1, 250), (3, 2, 40), (4, 3, 300)";

/// A join and two subqueries: one in WHERE, one in the select list.
const JOIN_AND_SUBQUERIES: &str = "SELECT c.name, o.total, \
     (SELECT count(*) FROM orders x WHERE x.customer_id = c.id) AS orders \
     FROM customers c JOIN orders o ON o.customer_id = c.id \
     WHERE c.id IN (SELECT customer_id FROM orders WHERE total > 100) \
     ORDER BY o.total DESC";

fn analyzed() -> ExplainOptions {
    ExplainOptions {
        analyze: true,
        analyze_confirmed: true,
        ..Default::default()
    }
}

/// The tree is whole: preorder, every child points back to its parent.
fn tree(doc: &PlanDocument) {
    assert_eq!(doc.summary().node_count as usize, doc.nodes().len());
    for (i, node) in doc.nodes().iter().enumerate() {
        assert_eq!(node.id as usize, i);
        for child in &node.children {
            let child = &doc.nodes()[*child as usize];
            assert_eq!(child.parent, Some(node.id));
            assert_eq!(child.depth, node.depth + 1);
            assert!(child.id > node.id, "preorder");
        }
    }
}

fn relations(doc: &PlanDocument) -> Vec<&str> {
    doc.nodes()
        .iter()
        .filter_map(|n| n.relation.as_deref())
        .collect()
}

/// What SQLite's plan of the join and its subqueries holds, locally and on a server.
fn sqlite_join_plan(doc: &PlanDocument) {
    tree(doc);
    assert_eq!(doc.summary().engine, "sqlite");
    assert_eq!(doc.summary().raw_format, PlanRawFormat::Json);
    assert!(!doc.summary().analyzed);
    assert_eq!(doc.nodes()[0].node_type, "Query plan");
    let names = relations(doc);
    assert!(names.contains(&"c") && names.contains(&"o"), "{names:?}");
    // Each subquery is a step with the scan of `orders` inside it.
    for kind in ["List subquery", "Correlated scalar subquery"] {
        let subquery = doc
            .nodes()
            .iter()
            .find(|n| n.node_type == kind)
            .unwrap_or_else(|| panic!("no {kind} in {:?}", doc.nodes()));
        assert!(
            subquery.children.iter().any(|c| {
                let child = &doc.nodes()[*c as usize];
                child.operation == PlanOperation::Scan
                    && matches!(child.relation.as_deref(), Some("orders" | "x"))
            }),
            "{kind}: {:?}",
            doc.nodes()
        );
    }
    assert!(
        doc.nodes()
            .iter()
            .any(|n| n.node_type == "Temp B-tree" && n.operation == PlanOperation::Sort),
        "{:?}",
        doc.nodes()
    );
    assert!(doc.nodes().iter().all(|n| n.detail.is_some() || n.id == 0));
    assert!(doc.nodes().iter().all(|n| n.metrics.cost.is_none()
        && n.metrics.estimated_rows.is_none()
        && n.findings.is_empty()));
}

#[tokio::test]
async fn sqlite_plans_are_estimated_trees_of_steps_on_the_tabs_own_session() {
    let file = TempFile::new("sqlite", Engine::Sqlite);
    let open = open(Engine::Sqlite, &file.0, false).await;
    let session = &open.session;
    run(session, SETUP).await;
    let policy = ExplainPolicy::for_connection(&open);
    let capabilities = plan::plan_capabilities(session, &policy);
    assert!(capabilities.estimated && !capabilities.analyze && !capabilities.buffers);
    assert_eq!(
        capabilities.analyze_unavailable,
        Some(AnalyzeUnavailable::Engine)
    );
    let doc = plan::explain(
        session,
        JOIN_AND_SUBQUERIES,
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    sqlite_join_plan(&doc);
    // The index is named, with the condition it is searched by.
    let indexed = plan::explain(
        session,
        "SELECT name FROM customers WHERE email = 'ana@example.com'",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    tree(&indexed);
    assert!(
        indexed
            .nodes()
            .iter()
            .any(|n| n.index.as_deref() == Some("customers_email")
                && n.index_condition.as_deref() == Some("email=?")),
        "{:?}",
        indexed.nodes()
    );
    // A temporary table only this session sees proves the plan runs on it.
    run(session, "CREATE TEMP TABLE scratch (k INTEGER)").await;
    let temp = plan::explain(
        session,
        "SELECT * FROM scratch",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(temp.nodes()[1].sequential_scan);
    // A saved plan reads back the same.
    let saved = plan::parse_plan(doc.raw(), "saved").unwrap();
    assert_eq!(saved.nodes().len(), doc.nodes().len());

    // There is no ANALYZE to run, even when a caller asks past the disabled toggle.
    for options in [
        analyzed(),
        ExplainOptions {
            confirmed: true,
            ..analyzed()
        },
    ] {
        let error = plan::explain(session, "SELECT 1", options, &policy, None, |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("SQLite has no ANALYZE"), "{error}");
    }
    // An estimated write plans without running.
    plan::explain(
        session,
        "DELETE FROM orders WHERE total > 100",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(run(session, "SELECT count(*) FROM orders").await, ["4"]);
    assert!(session.transaction_status().auto_commit);
    assert!(
        session.results().is_empty(),
        "internal plan Arrow handles must be released"
    );
}

#[tokio::test]
async fn sqlite_read_only_and_agent_plans_are_estimated_reads() {
    let file = TempFile::new("sqlite-ro", Engine::Sqlite);
    {
        let writer = open(Engine::Sqlite, &file.0, false).await;
        run(&writer.session, SETUP).await;
        writer.close().await;
    }
    let open = open(Engine::Sqlite, &file.0, true).await;
    let session = &open.session;
    let policy = ExplainPolicy::for_connection(&open);
    let capabilities = plan::plan_capabilities(session, &policy);
    assert!(capabilities.estimated && !capabilities.analyze);
    // The engine has no ANALYZE at all: that, not read-only mode, is why it is off.
    assert_eq!(
        capabilities.analyze_unavailable,
        Some(AnalyzeUnavailable::Engine)
    );
    let doc = plan::explain(
        session,
        JOIN_AND_SUBQUERIES,
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    sqlite_join_plan(&doc);
    let error = plan::explain(
        session,
        "UPDATE orders SET total = 0",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("read-only"), "{error}");

    // Agents: the caller's row limit does not cut a plan of several steps.
    let read = ReadLimits {
        max_rows: 1,
        timeout: Duration::from_secs(5),
    };
    let agent = plan::explain_read_only(&open, JOIN_AND_SUBQUERIES, &read)
        .await
        .unwrap();
    sqlite_join_plan(&agent);
    for sql in [
        "SELECT 1; DELETE FROM orders",
        "DELETE FROM orders",
        "UPDATE orders SET total = 0",
        "SELECT load_extension('x')",
    ] {
        assert!(
            plan::explain_read_only(&open, sql, &read).await.is_err(),
            "{sql}"
        );
    }
    assert_eq!(run(session, "SELECT count(*) FROM orders").await, ["4"]);
    assert!(session.results().is_empty());
}

/// What DuckDB's plan of the join and its subqueries holds.
fn duckdb_join_plan(doc: &PlanDocument) {
    tree(doc);
    assert_eq!(doc.summary().engine, "duckdb");
    assert_eq!(doc.summary().raw_format, PlanRawFormat::Json);
    let names = relations(doc);
    assert!(
        names.iter().any(|n| n.ends_with("customers"))
            && names.iter().any(|n| n.ends_with("orders")),
        "{names:?}"
    );
    let joins: Vec<_> = doc
        .nodes()
        .iter()
        .filter(|n| n.operation == PlanOperation::Join)
        .collect();
    // The join itself, and the IN subquery as a semi join.
    assert!(joins.len() >= 2, "{:?}", doc.nodes());
    assert!(joins.iter().any(|n| n.join_type.is_some()));
    assert!(
        doc.nodes()
            .iter()
            .any(|n| n.metrics.estimated_rows.is_some())
    );
    assert!(
        doc.nodes()
            .iter()
            .any(|n| n.operation == PlanOperation::Sort)
    );
    assert!(doc.nodes().iter().all(|n| n.metrics.cost.is_none()));
}

#[tokio::test]
async fn duckdb_plans_run_estimated_and_analyzed_on_the_tabs_own_session() {
    let file = TempFile::new("duckdb", Engine::Duckdb);
    let open = open(Engine::Duckdb, &file.0, false).await;
    let session = &open.session;
    run(session, SETUP).await;
    let policy = ExplainPolicy::for_connection(&open);
    let capabilities = plan::plan_capabilities(session, &policy);
    assert!(capabilities.estimated && capabilities.analyze && !capabilities.buffers);
    assert_eq!(capabilities.analyze_unavailable, None);

    let estimated = plan::explain(
        session,
        JOIN_AND_SUBQUERIES,
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    duckdb_join_plan(&estimated);
    assert!(!estimated.summary().analyzed);
    assert!(
        estimated
            .nodes()
            .iter()
            .all(|n| n.metrics.actual_rows.is_none())
    );

    let actual = plan::explain(
        session,
        JOIN_AND_SUBQUERIES,
        analyzed(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    duckdb_join_plan(&actual);
    let summary = actual.summary();
    assert!(summary.analyzed);
    assert!(summary.execution_ms.is_some_and(|ms| ms > 0.0));
    // Below DuckDB's own EXPLAIN_ANALYZE wrappers: the query's operators only.
    assert!(
        actual
            .nodes()
            .iter()
            .all(|n| !matches!(n.node_type.as_str(), "EXPLAIN_ANALYZE" | "RESULT_COLLECTOR"))
    );
    // The root returns the query's rows: Ana's two orders and Eva's one.
    assert_eq!(actual.nodes()[0].metrics.actual_rows, Some(3.0));
    assert!(
        actual
            .nodes()
            .iter()
            .any(|n| n.metrics.self_time_ms.is_some())
    );

    // A temporary table only this session sees proves the plan runs on it.
    run(session, "CREATE TEMP TABLE scratch (k INTEGER)").await;
    plan::explain(
        session,
        "SELECT * FROM scratch",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(
        plan::explain_preflight(
            session,
            "SELECT 1",
            ExplainOptions {
                buffers: true,
                ..analyzed()
            },
            &policy
        )
        .unwrap_err()
        .to_string()
        .contains("PostgreSQL")
    );
    let saved = plan::parse_plan(actual.raw(), "saved").unwrap();
    assert_eq!(saved.nodes().len(), actual.nodes().len());
    assert!(session.transaction_status().auto_commit);
    assert!(session.results().is_empty());
}

#[tokio::test]
async fn duckdb_analyzed_writes_execute_only_once_confirmed() {
    let file = TempFile::new("duckdb-write", Engine::Duckdb);
    let open = open(Engine::Duckdb, &file.0, false).await;
    let session = &open.session;
    run(session, SETUP).await;
    let policy = ExplainPolicy::for_connection(&open);
    let risky = "UPDATE orders SET total = 0";
    // Estimated, nothing runs and nothing needs confirming.
    plan::explain(session, risky, Default::default(), &policy, None, |_| {})
        .await
        .unwrap();
    let totals = || async {
        run(
            session,
            "SELECT CAST(total AS INTEGER) FROM orders ORDER BY id",
        )
        .await
    };
    assert_eq!(totals().await, ["10", "250", "40", "300"]);
    let preflight = plan::explain_preflight(session, risky, analyzed(), &policy).unwrap();
    assert!(preflight.executes_query && !preflight.risks.is_empty());
    // Without the execution warning, then without the write confirmation: refused.
    for options in [
        ExplainOptions {
            analyze_confirmed: false,
            confirmed: true,
            ..analyzed()
        },
        analyzed(),
    ] {
        assert!(
            plan::explain(session, risky, options, &policy, None, |_| {})
                .await
                .is_err()
        );
    }
    assert_eq!(totals().await, ["10", "250", "40", "300"]);
    let plan = plan::explain(
        session,
        risky,
        ExplainOptions {
            confirmed: true,
            ..analyzed()
        },
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(plan.summary().analyzed);
    // ANALYZE really ran the UPDATE (auto-commit: nothing to roll back).
    assert_eq!(totals().await, ["0", "0", "0", "0"]);
    assert!(session.results().is_empty());
}

#[tokio::test]
async fn duckdb_read_only_and_agent_plans_are_estimated_reads() {
    let file = TempFile::new("duckdb-ro", Engine::Duckdb);
    {
        let writer = open(Engine::Duckdb, &file.0, false).await;
        run(&writer.session, SETUP).await;
        writer.close().await;
    }
    let open = open(Engine::Duckdb, &file.0, true).await;
    let session = &open.session;
    let policy = ExplainPolicy::for_connection(&open);
    let capabilities = plan::plan_capabilities(session, &policy);
    assert!(capabilities.estimated && !capabilities.analyze);
    assert_eq!(
        capabilities.analyze_unavailable,
        Some(AnalyzeUnavailable::ReadOnly)
    );
    for (sql, options) in [
        (JOIN_AND_SUBQUERIES, analyzed()),
        (
            "UPDATE orders SET total = 0",
            ExplainOptions {
                confirmed: true,
                ..analyzed()
            },
        ),
        ("UPDATE orders SET total = 0", Default::default()),
    ] {
        let error = plan::explain(session, sql, options, &policy, None, |_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("read-only"), "{sql}: {error}");
    }
    let doc = plan::explain(
        session,
        JOIN_AND_SUBQUERIES,
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    duckdb_join_plan(&doc);

    let read = ReadLimits {
        max_rows: 1,
        timeout: Duration::from_secs(5),
    };
    let agent = plan::explain_read_only(&open, JOIN_AND_SUBQUERIES, &read)
        .await
        .unwrap();
    duckdb_join_plan(&agent);
    assert!(!agent.summary().analyzed);
    for sql in [
        "SELECT 1; DELETE FROM orders",
        "DELETE FROM orders",
        "EXPLAIN ANALYZE SELECT 1",
        "SELECT * FROM read_csv('/etc/passwd')",
    ] {
        assert!(
            plan::explain_read_only(&open, sql, &read).await.is_err(),
            "{sql}"
        );
    }
    assert_eq!(
        run(session, "SELECT count(*) FROM orders WHERE total = 0").await,
        ["0"]
    );
    assert!(session.results().is_empty());
}

// ---------------------------------------------------------------------------------------
// SQLite on a server (ADR-018), through the Docker bastion and `sqlitehost`.
// ---------------------------------------------------------------------------------------

struct AcceptHosts;

#[async_trait]
impl Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &HostInfo) -> bool {
        true
    }

    async fn ask_secret(&self, _request: &SecretRequest) -> Option<Secret> {
        None
    }
}

/// A trusted repository with the tunnel to `sqlitehost` and a connection to its file.
async fn open_remote(addr: &str, read_only: bool) -> (OpenConnection, PathBuf) {
    let base = std::env::temp_dir().join(format!(
        "spelaion-plans-remote-{read_only}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("repo/.spelaion/tunnels")).unwrap();
    std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
    std::fs::create_dir_all(base.join("user")).unwrap();
    let (host, port) = addr.rsplit_once(':').unwrap();
    let identity = format!(
        "'{}'",
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testing/docker/keys/id_ed25519")
            .display()
    );
    std::fs::write(
        base.join("repo/.spelaion/tunnels/server.toml"),
        format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"sqlitehost\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    )
    .unwrap();
    // The copy tests only read (`other.db`): nothing here writes to the file.
    std::fs::write(
        base.join("repo/.spelaion/connections/shop.toml"),
        format!(
            "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelaion/other.db\"\ntunnel = \"server\"\nread_only = {read_only}\n"
        ),
    )
    .unwrap();
    let config = load(Some(&base.join("repo")), &base.join("user"));
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let mut trust = TrustStore::load(&base.join("user")).unwrap();
    trust.trust(&config).unwrap();
    let resolved = Resolver::new(&config, &trust, &SystemBackend)
        .resolve(&ConnectionId {
            source: Source::Workspace,
            path: "shop".into(),
        })
        .expect("resolving");
    let open = open_connection(
        resolved,
        &Engines::builtin(),
        OpenOptions {
            tunnels: Arc::new(TunnelPool::new()),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: base.join("user/known_hosts"),
            },
            interaction: Arc::new(AcceptHosts),
            password: None,
            read_only_session: read_only,
        },
        Arc::new(ResultStore::new()),
    )
    .await
    .expect("opening the remote file");
    (open, base)
}

/// A join and two subqueries over the server file's only table.
const REMOTE_QUERY: &str = "SELECT a.name, b.price, \
     (SELECT count(*) FROM product x WHERE x.price > a.price) AS dearer \
     FROM product a JOIN product b ON b.id = a.id \
     WHERE a.id IN (SELECT id FROM product WHERE price > 1) \
     ORDER BY a.name";

#[tokio::test]
async fn sqlite_on_a_server_gets_the_same_plans_through_sqlite3() {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: remote SQLite plans not tested");
        return;
    };
    for read_only in [false, true] {
        let (open, base) = open_remote(&addr, read_only).await;
        let session = &open.session;
        let policy = ExplainPolicy::for_connection(&open);
        let capabilities = plan::plan_capabilities(session, &policy);
        assert!(capabilities.estimated && !capabilities.analyze);
        assert_eq!(
            capabilities.analyze_unavailable,
            Some(AnalyzeUnavailable::Engine)
        );
        let doc = plan::explain(
            session,
            REMOTE_QUERY,
            Default::default(),
            &policy,
            None,
            |_| {},
        )
        .await
        .unwrap();
        tree(&doc);
        assert_eq!(doc.summary().engine, "sqlite");
        let names = relations(&doc);
        assert!(names.contains(&"a") && names.contains(&"b"), "{names:?}");
        for kind in ["List subquery", "Correlated scalar subquery", "Temp B-tree"] {
            assert!(
                doc.nodes().iter().any(|n| n.node_type == kind),
                "{kind}: {:?}",
                doc.nodes()
            );
        }
        // `b` is found by its rowid for each row of `a`.
        assert!(
            doc.nodes()
                .iter()
                .any(|n| n.relation.as_deref() == Some("b") && n.node_type == "Rowid search"),
            "{:?}",
            doc.nodes()
        );
        assert!(
            plan::explain(session, "SELECT 1", analyzed(), &policy, None, |_| {})
                .await
                .unwrap_err()
                .to_string()
                .contains("SQLite has no ANALYZE")
        );
        // Agents get it through the read-only path too.
        let agent = plan::explain_read_only(
            &open,
            REMOTE_QUERY,
            &ReadLimits {
                max_rows: 1,
                timeout: Duration::from_secs(10),
            },
        )
        .await
        .unwrap();
        assert_eq!(agent.nodes().len(), doc.nodes().len());
        // Ordinary results still come back as values after a plan.
        assert_eq!(run(session, "SELECT count(*) FROM product").await.len(), 1);
        assert!(session.results().is_empty());
        open.close().await;
        let _ = std::fs::remove_dir_all(base);
    }
}
