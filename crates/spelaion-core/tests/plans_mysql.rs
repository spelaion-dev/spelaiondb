//! Real MySQL (`SPELAION_TEST_MYSQL`) and MariaDB (`SPELAION_TEST_MARIADB`) plan runs. Every
//! table is a session's own TEMPORARY table, so tests never depend on each other's data.
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Engine, LocalPort, Password, ResolvedConnection, Safeguards, Source,
};
use spelaion_core::plan::{self, ExplainOptions, ExplainPolicy, PlanRawFormat};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadLimits, ResultStore, Session, TunnelPool,
    open_connection,
};
use spelaion_driver_api::{ConnectParams, Secret};
use spelaion_ssh::{KnownHosts, NonInteractive};

/// The servers to test, as `(variable, engine the connection is opened as)`.
const SERVERS: [(&str, Engine); 2] = [
    ("SPELAION_TEST_MYSQL", Engine::Mysql),
    ("SPELAION_TEST_MARIADB", Engine::Mariadb),
];

async fn open(variable: &str, engine: Engine, read_only: bool) -> Option<OpenConnection> {
    let Ok(address) = std::env::var(variable) else {
        eprintln!("{variable} not set: MySQL/MariaDB plans not tested there");
        return None;
    };
    let (host, port) = address.rsplit_once(':').unwrap();
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

const SETUP: &str = "CREATE TEMPORARY TABLE plan_values (id INT PRIMARY KEY, value INT NOT NULL, KEY plan_values_value (value)); INSERT INTO plan_values VALUES (1, 10), (2, 20), (3, 30)";

fn analyzed() -> ExplainOptions {
    ExplainOptions {
        analyze: true,
        analyze_confirmed: true,
        ..Default::default()
    }
}

async fn values(session: &Session) -> Vec<String> {
    run(session, "SELECT value FROM plan_values ORDER BY id").await
}

#[tokio::test]
async fn mysql_family_plans_run_on_the_tabs_own_session_estimated_and_analyzed() {
    for (variable, engine) in SERVERS {
        let Some(open) = open(variable, engine, false).await else {
            continue;
        };
        let session = &open.session;
        let policy = ExplainPolicy::for_connection(&open);
        let capabilities = plan::plan_capabilities(session, &policy);
        assert!(capabilities.estimated && capabilities.analyze && !capabilities.buffers);
        // A temporary table only this session can see proves the plan runs on it.
        run(session, SETUP).await;
        let estimated = plan::explain(
            session,
            "SELECT * FROM plan_values WHERE value > 15",
            Default::default(),
            &policy,
            None,
            |_| {},
        )
        .await
        .unwrap();
        let name = engine.as_str();
        assert_eq!(estimated.summary().engine, name);
        assert_eq!(estimated.summary().raw_format, PlanRawFormat::Json);
        assert!(!estimated.summary().analyzed);
        assert!(
            estimated
                .nodes()
                .iter()
                .any(|n| n.relation.as_deref() == Some("plan_values")),
            "{name}: {:?}",
            estimated.nodes()
        );
        assert!(estimated.nodes().iter().all(|n| n.metrics.loops.is_none()));

        let actual = plan::explain(
            session,
            "SELECT * FROM plan_values WHERE value > 15",
            analyzed(),
            &policy,
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert!(actual.summary().analyzed, "{name}");
        assert_eq!(
            actual.summary().raw_format,
            match engine {
                Engine::Mysql => PlanRawFormat::Text,
                _ => PlanRawFormat::Json,
            }
        );
        let kept: f64 = actual
            .nodes()
            .iter()
            .filter(|n| n.relation.as_deref() == Some("plan_values") || n.node_type == "Filter")
            .filter_map(|n| n.metrics.actual_rows)
            .fold(f64::NAN, f64::min);
        assert_eq!(kept, 2.0, "{name}: {:?}", actual.nodes());
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
        assert!(session.transaction_status().auto_commit);
        assert!(
            session.results().is_empty(),
            "internal plan Arrow handles must be released"
        );
    }
}

#[tokio::test]
async fn mysql_family_analyzed_writes_need_confirmation_and_follow_smart_commit() {
    for (variable, engine) in SERVERS {
        let Some(open) = open(variable, engine, false).await else {
            continue;
        };
        let session = &open.session;
        run(session, SETUP).await;
        let policy = ExplainPolicy::for_connection(&open);
        session
            .follow_transactions(&Safeguards {
                smart_commit: true,
                ..Default::default()
            })
            .await;
        // An estimated UPDATE performs no write: no confirmation, no smart-commit switch.
        plan::explain(
            session,
            "UPDATE plan_values SET value = 99",
            Default::default(),
            &policy,
            None,
            |_| {},
        )
        .await
        .unwrap();
        assert!(session.transaction_status().auto_commit);
        let risky = "UPDATE plan_values SET value = 99";
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
        assert_eq!(values(session).await, ["10", "20", "30"]);
        let confirmed = plan::explain(
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
        .await;
        match engine {
            // MariaDB's ANALYZE runs the UPDATE, inside the transaction smart commit opened.
            Engine::Mariadb => {
                let plan = confirmed.unwrap();
                assert!(plan.summary().analyzed);
                let status = session.transaction_status();
                assert!(status.smart_switched && !status.auto_commit && status.is_open());
                assert_eq!(values(session).await, ["99", "99", "99"]);
            }
            // MySQL does not analyze a single-table UPDATE; it says so and changes nothing.
            _ => {
                assert!(
                    confirmed
                        .unwrap_err()
                        .to_string()
                        .contains("cannot analyze"),
                );
                assert_eq!(values(session).await, ["10", "20", "30"]);
            }
        }
        if session.transaction_status().is_open() {
            session.rollback().await.unwrap();
        }
        assert_eq!(values(session).await, ["10", "20", "30"]);
        assert!(session.results().is_empty());
    }
}

#[tokio::test]
async fn mysql_family_read_only_connections_get_estimated_read_plans_only() {
    for (variable, engine) in SERVERS {
        let Some(open) = open(variable, engine, true).await else {
            continue;
        };
        assert!(open.resolved.params.read_only);
        let session = &open.session;
        run(session, SETUP).await;
        let policy = ExplainPolicy::for_connection(&open);
        let capabilities = plan::plan_capabilities(session, &policy);
        assert!(capabilities.estimated && !capabilities.analyze);
        for (sql, options) in [
            ("SELECT * FROM plan_values", analyzed()),
            (
                "UPDATE plan_values SET value = 0",
                ExplainOptions {
                    confirmed: true,
                    ..analyzed()
                },
            ),
            ("UPDATE plan_values SET value = 0", Default::default()),
            (
                "DELETE FROM plan_values",
                ExplainOptions {
                    confirmed: true,
                    ..Default::default()
                },
            ),
        ] {
            let error = plan::explain(session, sql, options, &policy, None, |_| {})
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("read-only"), "{sql}: {error}");
        }
        assert_eq!(values(session).await, ["10", "20", "30"]);
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
    }
}

/// Each flavor's ANALYZE is a syntax error on the other one: a connection opened as the
/// wrong engine never runs the analyzed statement.
#[tokio::test]
async fn mysql_family_analyze_on_the_other_flavor_is_refused_before_running() {
    let (Ok(_), Ok(_)) = (
        std::env::var("SPELAION_TEST_MYSQL"),
        std::env::var("SPELAION_TEST_MARIADB"),
    ) else {
        eprintln!("SPELAION_TEST_MYSQL and SPELAION_TEST_MARIADB needed: not tested");
        return;
    };
    for (variable, opened_as) in [
        ("SPELAION_TEST_MARIADB", Engine::Mysql),
        ("SPELAION_TEST_MYSQL", Engine::Mariadb),
    ] {
        let open = open(variable, opened_as, false).await.unwrap();
        let session = &open.session;
        run(session, SETUP).await;
        let policy = ExplainPolicy::for_connection(&open);
        let error = plan::explain(
            session,
            "UPDATE plan_values SET value = 0 WHERE id > 0",
            ExplainOptions {
                confirmed: true,
                ..analyzed()
            },
            &policy,
            None,
            |_| {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.to_ascii_lowercase().contains("syntax"), "{error}");
        if session.transaction_status().is_open() {
            session.rollback().await.unwrap();
        }
        assert_eq!(values(session).await, ["10", "20", "30"]);
        // Estimated plans are the same statement on both and still work.
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
    }
}

#[tokio::test]
async fn mysql_family_agent_plans_are_estimated_reads_only() {
    for (variable, engine) in SERVERS {
        let Some(open) = open(variable, engine, false).await else {
            continue;
        };
        let session = &open.session;
        run(session, SETUP).await;
        let read = ReadLimits {
            max_rows: 1,
            timeout: Duration::from_secs(2),
        };
        let document = plan::explain_read_only(&open, "SELECT * FROM plan_values", &read)
            .await
            .unwrap();
        assert_eq!(document.summary().engine, engine.as_str());
        assert!(!document.summary().analyzed);
        assert!(
            plan::explain_read_only(&open, "SELECT SLEEP(30)", &read)
                .await
                .is_ok(),
            "estimated plans do not execute expressions"
        );
        for sql in [
            "SELECT 1; DELETE FROM plan_values",
            "DELETE FROM plan_values",
            "UPDATE plan_values SET value = 0",
            "EXPLAIN ANALYZE SELECT 1",
            "ANALYZE FORMAT=JSON SELECT 1",
        ] {
            assert!(
                plan::explain_read_only(&open, sql, &read).await.is_err(),
                "{sql}"
            );
        }
        assert_eq!(values(session).await, ["10", "20", "30"]);
        assert!(session.results().is_empty());
    }
}
