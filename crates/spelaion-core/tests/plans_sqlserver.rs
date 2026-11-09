//! SQL Server plans on the tab's own connection; requires SPELAION_TEST_MSSQL.
use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Engine, LocalPort, Password, ResolvedConnection, Safeguards, Source,
};
use spelaion_core::plan::{self, ExplainOptions, ExplainPolicy, PlanRawFormat};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ResultStore, Session, TunnelPool,
    open_connection,
};
use spelaion_driver_api::{ConnectParams, Secret, TlsMode};
use spelaion_ssh::{KnownHosts, NonInteractive};
use std::sync::Arc;
use std::time::Duration;

async fn open(read_only: bool) -> Option<OpenConnection> {
    let Ok(address) = std::env::var("SPELAION_TEST_MSSQL") else {
        eprintln!("SPELAION_TEST_MSSQL not set: SQL Server plans not tested");
        return None;
    };
    let (host, port) = address.rsplit_once(':').unwrap();
    let resolved = ResolvedConnection {
        id: ConnectionId {
            source: Source::User,
            path: "plans.toml".into(),
        },
        name: "SQL Server plans test".into(),
        engine: Engine::Sqlserver,
        env: None,
        kind: None,
        params: ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "sa".into(),
            database: "master".into(),
            tls: TlsMode::Require,
            read_only,
            ..Default::default()
        },
        password: Password::Value(Secret::new("Spelaion-test-1")),
        tunnel: None,
        kubernetes: None,
        sqlite3: None,
        local_port: LocalPort::Any,
        keepalive: None,
        statement_timeout: Some(Duration::from_secs(10)),
        session: Default::default(),
        mcp: Default::default(),
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
                    app_file: std::env::temp_dir().join("unused-sqlserver-plan-known-hosts"),
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
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    values
}

fn analyzed() -> ExplainOptions {
    ExplainOptions {
        analyze: true,
        analyze_confirmed: true,
        confirmed: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn sqlserver_plans_keep_the_session_and_discard_user_results() {
    let Some(open) = open(false).await else {
        return;
    };
    let session = &open.session;
    run(session, "CREATE TABLE #plan_values (id int PRIMARY KEY, value int); INSERT INTO #plan_values VALUES (1, 10), (2, 20), (3, 30)").await;
    let policy = ExplainPolicy::for_connection(&open);
    for options in [ExplainOptions::default(), analyzed()] {
        let plan = plan::explain(
            session,
            "SELECT value FROM #plan_values WHERE value >= 20",
            options,
            &policy,
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(plan.summary().engine, "sqlserver");
        assert_eq!(plan.summary().raw_format, PlanRawFormat::Xml);
        assert_eq!(plan.summary().analyzed, options.analyze);
        assert!(plan.nodes().iter().any(|n| {
            n.relation
                .as_deref()
                .is_some_and(|r| r.contains("plan_values"))
        }));
        assert_eq!(run(session, "SELECT 42").await, ["42"]);
        assert!(session.results().is_empty());
    }
    let missing = "SELECT * FROM #spelaion_plan48_missing_table";
    assert!(
        plan::explain(
            session,
            missing,
            ExplainOptions::default(),
            &policy,
            None,
            |_| {}
        )
        .await
        .is_err()
    );
    assert_eq!(run(session, "SELECT 42").await, ["42"]);
    let divide = "SELECT 1 / (value - 10) FROM #plan_values";
    let error = plan::explain(session, divide, analyzed(), &policy, None, |_| {})
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("ANALYZE may have executed"),
        "{error}"
    );
    assert_eq!(run(session, "SELECT 42").await, ["42"]);
    let fake = "SELECT CAST('<ShowPlanXML xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/showplan\"><StmtSimple StatementType=\"FAKE\"/></ShowPlanXML>' AS xml) AS [Microsoft SQL Server 2005 XML Showplan] FROM #plan_values";
    let plan = plan::explain(session, fake, analyzed(), &policy, None, |_| {})
        .await
        .unwrap();
    assert!(plan.nodes().iter().any(|n| {
        n.relation
            .as_deref()
            .is_some_and(|r| r.contains("plan_values"))
    }));
    let fake_no_plan = "SELECT CAST('<ShowPlanXML xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/showplan\"><StmtSimple StatementType=\"FAKE\"/></ShowPlanXML>' AS xml) AS [Microsoft SQL Server 2005 XML Showplan]";
    assert!(
        plan::explain(session, fake_no_plan, analyzed(), &policy, None, |_| {})
            .await
            .is_err()
    );
    assert_eq!(run(session, "SELECT 42").await, ["42"]);
    let into = "SELECT value INTO #plan_copy FROM #plan_values";
    plan::explain(session, into, analyzed(), &policy, None, |_| {})
        .await
        .unwrap();
    assert_eq!(run(session, "SELECT COUNT(*) FROM #plan_copy").await, ["3"]);
    let update = "UPDATE #plan_values SET value = value + 1 OUTPUT inserted.value WHERE id = 1";
    plan::explain(session, update, analyzed(), &policy, None, |_| {})
        .await
        .unwrap();
    assert_eq!(
        run(session, "SELECT value FROM #plan_values WHERE id = 1").await,
        ["11"]
    );
    let readonly = ExplainPolicy::new(true, Safeguards::default());
    assert!(
        plan::explain(session, update, analyzed(), &readonly, None, |_| {})
            .await
            .is_err()
    );
    assert!(
        plan::explain(
            session,
            update,
            ExplainOptions::default(),
            &readonly,
            None,
            |_| {}
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn sqlserver_read_only_connection_rejects_forged_analyze_confirmation() {
    let Some(writable) = open(false).await else {
        return;
    };
    // Global temporary table lives only while this owning connection remains open.
    let table = format!("##spelaion_plan48_readonly_{}", std::process::id());
    run(
        &writable.session,
        &format!("CREATE TABLE {table} (value int); INSERT INTO {table} VALUES (7)"),
    )
    .await;
    let readonly = open(true).await.unwrap();
    let policy = ExplainPolicy::for_connection(&readonly);
    let update = format!("UPDATE {table} SET value=99");
    let error = plan::explain(
        &readonly.session,
        &update,
        analyzed(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("read-only"));
    assert_eq!(
        run(&writable.session, &format!("SELECT value FROM {table}")).await,
        ["7"]
    );
}
