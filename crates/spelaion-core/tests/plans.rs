//! Real PostgreSQL plan execution, with data owned by each session, never suite-order fixtures.
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Engine, LocalPort, Password, ResolvedConnection, Safeguards, Source,
};
use spelaion_core::plan::{self, ExplainOptions, ExplainPolicy};
use spelaion_core::{
    CoreEvent, Engines, HistoryScope, OpenConnection, OpenOptions, QueryHistory, ReadLimits,
    ResultStore, Session, TransactionState, TunnelPool, open_connection,
};
use spelaion_driver_api::{ConnectParams, Secret};
use spelaion_ssh::{KnownHosts, NonInteractive};

async fn open(read_only: bool) -> Option<OpenConnection> {
    open_with_setup(read_only, Default::default()).await
}

async fn open_with_setup(
    read_only: bool,
    setup: spelaion_config::ResolvedSession,
) -> Option<OpenConnection> {
    let Ok(address) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: plans not tested");
        return None;
    };
    let (host, port) = address.rsplit_once(':').unwrap();
    let resolved = ResolvedConnection {
        id: ConnectionId {
            source: Source::User,
            path: "plans.toml".into(),
        },
        name: "Plans test".into(),
        engine: Engine::Postgres,
        env: None,
        kind: None,
        params: ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "spelaion".into(),
            database: "spelaion".into(),
            read_only,
            ..Default::default()
        },
        password: Password::Value(Secret::new("spelaion")),
        tunnel: None,
        kubernetes: None,
        sqlite3: None,
        local_port: LocalPort::Any,
        keepalive: None,
        statement_timeout: Some(Duration::from_secs(5)),
        session: setup,
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
                    app_file: std::env::temp_dir().join("unused-plan-known-hosts"),
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
    let events = session.run_script(sql).collect::<Vec<_>>().await;
    for event in events {
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
        buffers: true,
        analyze_confirmed: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn plans_use_connection_setup_context_without_reopening_or_replaying_it() {
    let setup = spelaion_config::ResolvedSession {
        schema: Some("public".into()),
        setup: Some("CREATE TEMP TABLE setup_plan(value int); INSERT INTO setup_plan VALUES (1); SET search_path=pg_temp".into()),
        ..Default::default()
    };
    let Some(open) = open_with_setup(false, setup).await else {
        return;
    };
    let policy = ExplainPolicy::for_connection(&open);
    run(&open.session, "UPDATE setup_plan SET value=2").await;
    let plan = plan::explain(
        &open.session,
        "SELECT * FROM setup_plan WHERE value=2",
        analyzed(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(plan.nodes()[0].metrics.actual_rows, Some(1.0));
    assert!(
        plan.nodes()[0]
            .relation
            .as_deref()
            .is_some_and(|name| name.starts_with("pg_temp") && name.ends_with(".setup_plan")),
        "PostgreSQL may print its pg_temp alias or physical namespace: {:?}",
        plan.nodes()
    );
    assert_eq!(
        run(&open.session, "SELECT value FROM setup_plan").await,
        ["2"]
    );
    let schema = run(&open.session, "SELECT current_schema()").await;
    assert_eq!(schema.len(), 1);
    assert!(schema[0].starts_with("pg_temp"));
    assert!(open.session.results().is_empty());
}

#[tokio::test]
async fn plans_honor_the_tabs_statement_timeout_override() {
    let Some(open) = open(false).await else {
        return;
    };
    let policy = ExplainPolicy::for_connection(&open);
    let mut timed_out = false;
    let started = std::time::Instant::now();
    let result = plan::explain(
        &open.session,
        "SELECT pg_sleep(30)",
        ExplainOptions {
            timeout_seconds: Some(1),
            ..analyzed()
        },
        &policy,
        None,
        |event| {
            if let CoreEvent::StatementFailed { error, .. } = event {
                timed_out |= error.timed_out;
            }
        },
    )
    .await;
    assert!(result.is_err() && timed_out);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "tab override takes precedence over the connection's five seconds"
    );
    assert!(open.session.results().is_empty());
    assert!(open.session.transaction_status().auto_commit);
}

#[tokio::test]
async fn plans_use_the_selected_session_context_and_underlying_transaction_intent() {
    let Some(open) = open(false).await else {
        return;
    };
    let session = &open.session;
    run(session, "CREATE TEMP TABLE plan_values(id int primary key, value int); INSERT INTO plan_values VALUES (1, 10), (2, 20); SET search_path=pg_temp").await;
    let policy = ExplainPolicy::for_connection(&open);
    let smart = Safeguards {
        smart_commit: true,
        ..Default::default()
    };
    session.follow_transactions(&smart).await;
    let estimated = plan::explain(
        session,
        "SELECT * FROM plan_values WHERE id=1",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(!estimated.summary().analyzed);
    assert!(estimated.nodes().iter().any(|n| {
        n.relation
            .as_deref()
            .is_some_and(|r| r.contains('.') && r.ends_with("plan_values"))
    }));
    let actual = plan::explain(
        session,
        "SELECT * FROM plan_values",
        analyzed(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(actual.summary().analyzed && actual.summary().buffers);
    assert!(session.transaction_status().auto_commit);
    assert_eq!(
        session.transaction_status().state,
        Some(TransactionState::Idle)
    );
    // An estimated UPDATE performs no write and must not trigger smart commit.
    plan::explain(
        session,
        "UPDATE plan_values SET value=99",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(session.transaction_status().auto_commit);
    let risky = "UPDATE plan_values SET value=99";
    assert!(
        !plan::explain_preflight(session, risky, analyzed(), &policy)
            .unwrap()
            .risks
            .is_empty()
    );
    assert!(
        plan::explain(session, risky, analyzed(), &policy, None, |_| {})
            .await
            .unwrap_err()
            .to_string()
            .contains("confirm")
    );
    assert_eq!(
        run(session, "SELECT value FROM plan_values ORDER BY id").await,
        ["10", "20"]
    );
    plan::explain(
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
    let status = session.transaction_status();
    assert!(status.smart_switched && !status.auto_commit && status.is_open());
    assert_eq!(
        run(session, "SELECT value FROM plan_values ORDER BY id").await,
        ["99", "99"]
    );
    session.rollback().await.unwrap();
    assert_eq!(
        run(session, "SELECT value FROM plan_values ORDER BY id").await,
        ["10", "20"]
    );
    session.set_auto_commit(false).await.unwrap();
    plan::explain(
        session,
        "SELECT * FROM plan_values",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(session.transaction_status().is_open());
    assert_eq!(session.transaction_status().statements, 1);
    session.rollback().await.unwrap();
    assert!(
        session.results().is_empty(),
        "internal plan Arrow handles must be released"
    );
}

#[tokio::test]
async fn plans_readonly_is_captured_from_the_real_open_connection_and_never_executes_updates() {
    let Some(open) = open(true).await else {
        return;
    };
    assert!(open.resolved.params.read_only);
    // PG's driver flag alone does not enforce read-only; own temp data proves the core barrier.
    run(&open.session, "CREATE TEMP TABLE readonly_plan_values(value int); INSERT INTO readonly_plan_values VALUES (7)").await;
    let policy = ExplainPolicy::for_connection(&open);
    let capabilities = plan::plan_capabilities(&open.session, &policy);
    assert!(capabilities.estimated && !capabilities.analyze);
    for sql in [
        "UPDATE readonly_plan_values SET value=8",
        "WITH changed AS (UPDATE readonly_plan_values SET value=8 RETURNING *) SELECT * FROM changed",
    ] {
        assert!(
            plan::explain(
                &open.session,
                sql,
                ExplainOptions {
                    confirmed: true,
                    ..analyzed()
                },
                &policy,
                None,
                |_| {}
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("read-only")
        );
    }
    assert_eq!(
        run(&open.session, "SELECT value FROM readonly_plan_values").await,
        ["7"]
    );
    plan::explain(
        &open.session,
        "SELECT * FROM readonly_plan_values",
        Default::default(),
        &policy,
        None,
        |_| {},
    )
    .await
    .unwrap();
    assert!(
        plan::explain(
            &open.session,
            "SELECT * FROM readonly_plan_values",
            analyzed(),
            &policy,
            None,
            |_| {}
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn plans_require_execution_warning_and_reject_multiple_or_non_explainable_statements() {
    let Some(open) = open(false).await else {
        return;
    };
    let policy = ExplainPolicy::for_connection(&open);
    let warning = ExplainOptions {
        analyze: true,
        ..Default::default()
    };
    assert!(
        plan::explain(&open.session, "SELECT 1", warning, &policy, None, |_| {})
            .await
            .unwrap_err()
            .to_string()
            .contains("warning")
    );
    for sql in [
        "SELECT 1; SELECT 2",
        "EXPLAIN ANALYZE SELECT 1",
        "COPY (SELECT 1) TO '/tmp/no'",
        "DO $$BEGIN END$$",
        "SET search_path=public",
    ] {
        assert!(
            plan::explain_preflight(&open.session, sql, Default::default(), &policy).is_err(),
            "{sql}"
        );
    }
    let mut no_analyze = analyzed();
    no_analyze.analyze = false;
    assert!(plan::explain_preflight(&open.session, "SELECT 1", no_analyze, &policy).is_err());
    let read = ReadLimits {
        max_rows: 1,
        timeout: Duration::from_secs(2),
    };
    assert!(
        plan::explain_read_only(&open, "SELECT pg_sleep(30)", &read)
            .await
            .is_ok(),
        "estimated plans do not execute expressions"
    );
    for sql in [
        "SELECT 1; DELETE FROM nonexistent",
        "WITH d AS (DELETE FROM nonexistent RETURNING *) SELECT * FROM d",
        "EXPLAIN ANALYZE SELECT 1",
    ] {
        assert!(
            plan::explain_read_only(&open, sql, &read).await.is_err(),
            "{sql}"
        );
    }
    assert!(open.session.results().is_empty());
}

#[tokio::test]
async fn plans_are_serialized_cancellable_and_record_explicit_runs_in_history() {
    let Some(open) = open(false).await else {
        return;
    };
    let policy = ExplainPolicy::for_connection(&open);
    let session = Arc::new(open.session);
    run(
        &session,
        "CREATE TEMP TABLE plan_queue(value int); INSERT INTO plan_queue VALUES (1)",
    )
    .await;
    let path =
        std::env::temp_dir().join(format!("spelaion-plan-history-{}.db", std::process::id()));
    let history = QueryHistory::open(path.clone()).unwrap();
    let scope = HistoryScope {
        workspace: "plan-test".into(),
        connection: "plans".into(),
        database: Some("spelaion".into()),
        switchable_database: false,
    };
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let first = tokio::spawn({
        let s = session.clone();
        let p = policy.clone();
        let history = history.clone();
        async move {
            let mut started = Some(started_tx);
            plan::explain(
                &s,
                "SELECT pg_sleep(30)",
                analyzed(),
                &p,
                Some((history, scope)),
                |event| {
                    if matches!(event, CoreEvent::StatementStarted { .. })
                        && let Some(tx) = started.take()
                    {
                        let _ = tx.send(());
                    }
                },
            )
            .await
        }
    });
    started_rx.await.unwrap();
    let queued = plan::explain(
        &session,
        "UPDATE plan_queue SET value=2 WHERE value=1",
        ExplainOptions {
            confirmed: true,
            ..analyzed()
        },
        &policy,
        None,
        |_| {},
    );
    futures_util::pin_mut!(queued);
    // Poll once to prove the request is queued behind the first run, before cancellation.
    assert!(futures_util::poll!(queued.as_mut()).is_pending());
    session.cancel().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(run(&session, "SELECT value FROM plan_queue").await, ["1"]);
    assert!(session.transaction_status().auto_commit);
    assert!(session.results().is_empty());
    let entries = history
        .list("plan-test".into(), "".into(), 10, 0)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].statement.starts_with("EXPLAIN (FORMAT JSON"));
    assert!(entries[0].cancelled || entries[0].error.is_some());
    drop(history);
    let _ = std::fs::remove_file(path);
}
