//! ClickHouse plans (SDB-324) on the tab's own connection and for agents: `EXPLAIN json = 1`
//! read into the shared tree, estimated only. Against the Docker ClickHouse
//! (`SPELAION_TEST_CLICKHOUSE`); without it the tests are skipped, which does not count as
//! tested.
use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Engine, LocalPort, Password, ResolvedConnection, Source};
use spelaion_core::plan::{
    self, AnalyzeUnavailable, ExplainOptions, ExplainPolicy, PlanDocument, PlanOperation,
    PlanRawFormat,
};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadLimits, ResultStore, Session, TunnelPool,
    open_connection,
};
use spelaion_driver_api::{ConnectParams, Secret, TlsMode};
use spelaion_ssh::{KnownHosts, NonInteractive};
use std::sync::Arc;
use std::time::Duration;

async fn open(read_only: bool) -> Option<OpenConnection> {
    let Ok(address) = std::env::var("SPELAION_TEST_CLICKHOUSE") else {
        eprintln!("SPELAION_TEST_CLICKHOUSE not set: ClickHouse plans not tested");
        return None;
    };
    let (host, port) = address.rsplit_once(':').unwrap();
    let resolved = ResolvedConnection {
        id: ConnectionId {
            source: Source::User,
            path: "plans.toml".into(),
        },
        name: "ClickHouse plans test".into(),
        engine: Engine::Clickhouse,
        env: None,
        kind: None,
        params: ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "spelaion".into(),
            database: "spelaion".into(),
            tls: TlsMode::Disable,
            read_only,
            ..Default::default()
        },
        password: Password::Value(Secret::new("spelaion")),
        tunnel: None,
        kubernetes: None,
        sqlite3: None,
        file_folder: None,
        databases: None,
        other_databases: spelaion_config::OtherDatabases::None,
        local_port: LocalPort::Any,
        keepalive: None,
        statement_timeout: Some(Duration::from_secs(10)),
        session: Default::default(),
        mcp: Default::default(),
        masking: Default::default(),
    };
    Some(
        open_connection(
            resolved,
            &Engines::builtin(),
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: std::env::temp_dir().join("unused-clickhouse-plan-known-hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: None,
                read_only_session: read_only,
            },
            Arc::new(ResultStore::new()),
        )
        .await
        .unwrap(),
    )
}

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
            | CoreEvent::ResultEnded { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    values
}

/// The tables, renamed by [`named`] for each test: the tests of a file run at once.
const SETUP: [&str; 6] = [
    "DROP TABLE IF EXISTS pl_orders",
    "DROP TABLE IF EXISTS pl_customers",
    "CREATE TABLE pl_orders (id UInt32, customer UInt32, total Decimal(10, 2), day Date) \
     ENGINE = MergeTree ORDER BY (customer, id) PARTITION BY toYYYYMM(day)",
    "INSERT INTO pl_orders SELECT number, number % 100, number / 10, \
     toDate('2024-01-01') + (number % 60) FROM numbers(10000)",
    "CREATE TABLE pl_customers (id UInt32, name String) ENGINE = MergeTree ORDER BY id",
    "INSERT INTO pl_customers SELECT number, concat('c', toString(number)) FROM numbers(100)",
];

const JOIN: &str = "SELECT c.name, sum(o.total) FROM pl_orders o \
    JOIN pl_customers c ON c.id = o.customer \
    WHERE o.customer = 7 GROUP BY c.name ORDER BY 2 DESC LIMIT 5";

/// `sql` with the tables of `prefix` (`pl_`, `plr_`) in place of `pl_`.
fn named(sql: &str, prefix: &str) -> String {
    sql.replace("pl_", prefix)
}

/// The join's plan: both tables read, the orders through their primary key, a join, an
/// aggregate and a sort, and no metric ClickHouse does not report.
fn join_plan(doc: &PlanDocument, prefix: &str) {
    let orders_table = named("spelaion.pl_orders", prefix);
    let customers_table = named("spelaion.pl_customers", prefix);
    assert_eq!(doc.summary().engine, "clickhouse");
    assert_eq!(doc.summary().raw_format, PlanRawFormat::Json);
    assert!(!doc.summary().analyzed);
    let nodes = doc.nodes();
    let orders = nodes
        .iter()
        .find(|n| n.relation.as_deref() == Some(orders_table.as_str()))
        .unwrap_or_else(|| panic!("no read of {orders_table} in {nodes:?}"));
    assert_eq!(orders.operation, PlanOperation::Scan);
    assert!(
        orders
            .index
            .as_deref()
            .is_some_and(|i| i.contains("PrimaryKey")),
        "{orders:?}"
    );
    assert!(
        orders
            .index_condition
            .as_deref()
            .is_some_and(|c| c.contains("customer")),
        "{orders:?}"
    );
    assert!(
        nodes
            .iter()
            .any(|n| n.relation.as_deref() == Some(customers_table.as_str()))
    );
    for operation in [
        PlanOperation::Join,
        PlanOperation::Aggregate,
        PlanOperation::Sort,
    ] {
        assert!(
            nodes.iter().any(|n| n.operation == operation),
            "{operation:?}"
        );
    }
    assert!(nodes.iter().all(|n| n.metrics.cost.is_none()));
}

#[tokio::test]
async fn clickhouse_plans_are_estimated_on_the_tabs_own_session() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(open) = open(false).await else {
        return;
    };
    let session = &open.session;
    let prefix = "pl_";
    for statement in SETUP {
        run(session, &named(statement, prefix)).await;
    }
    let join = named(JOIN, prefix);
    let policy = ExplainPolicy::for_connection(&open);
    let capabilities = plan::plan_capabilities(session, &policy);
    assert!(capabilities.estimated && !capabilities.analyze && !capabilities.buffers);
    assert_eq!(
        capabilities.analyze_unavailable,
        Some(AnalyzeUnavailable::Engine)
    );
    let doc = plan::explain(
        session,
        &join,
        ExplainOptions::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    join_plan(&doc, prefix);
    // The plan's own results are gone, and the session goes on.
    assert!(session.results().is_empty());
    assert_eq!(run(session, "SELECT 42").await, ["42"]);

    // No ANALYZE: it is refused before anything runs.
    let analyze = ExplainOptions {
        analyze: true,
        analyze_confirmed: true,
        confirmed: true,
        ..Default::default()
    };
    let error = plan::explain(session, &join, analyze, &policy, None, |_| {})
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("estimated only"), "{error}");
    // A table that is not there is the server's error.
    assert!(
        plan::explain(
            session,
            "SELECT * FROM pl_missing",
            ExplainOptions::default(),
            &policy,
            None,
            |_| {}
        )
        .await
        .is_err()
    );
    assert_eq!(run(session, "SELECT 42").await, ["42"]);
}

#[tokio::test]
async fn clickhouse_read_only_and_agent_plans_are_estimated_reads() {
    let _turn = crate::serial::file_turn(module_path!());
    let prefix = "plr_";
    let join = named(JOIN, prefix);
    {
        let Some(writer) = open(false).await else {
            return;
        };
        for statement in SETUP {
            run(&writer.session, &named(statement, prefix)).await;
        }
        writer.close().await;
    }
    let open = open(true).await.unwrap();
    let session = &open.session;
    let policy = ExplainPolicy::for_connection(&open);
    let capabilities = plan::plan_capabilities(session, &policy);
    assert!(capabilities.estimated && !capabilities.analyze);
    let doc = plan::explain(
        session,
        &join,
        ExplainOptions::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    join_plan(&doc, prefix);
    // A write is not explained on a read-only connection.
    let error = plan::explain(
        session,
        &named("INSERT INTO pl_customers VALUES (1000, 'x')", prefix),
        ExplainOptions::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("read-only"), "{error}");

    let read = ReadLimits {
        max_rows: 1,
        timeout: Duration::from_secs(10),
    };
    let agent = plan::explain_read_only(&open, &join, &read).await.unwrap();
    join_plan(&agent, prefix);
    for sql in [
        "SELECT 1; DROP TABLE pl_orders",
        "DROP TABLE pl_orders",
        "INSERT INTO pl_customers VALUES (1000, 'x')",
    ] {
        let sql = named(sql, prefix);
        assert!(
            plan::explain_read_only(&open, &sql, &read).await.is_err(),
            "{sql}"
        );
    }
    assert_eq!(
        run(session, &named("SELECT count() FROM pl_customers", prefix)).await,
        ["100"]
    );
}
