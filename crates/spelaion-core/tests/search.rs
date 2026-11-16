//! The request checked by the core is the request its plugin runs; search indexes open as
//! bounded editor requests, with typed Arrow rows stored through the normal session.
mod engines;
use futures_util::StreamExt;
use spelaion_config::{Engine, Safeguards};
use spelaion_core::{CoreEvent, ResultStore, Risk, Session, script_risks};
use spelaion_driver_api::{ConnectParams, TlsMode};
use std::sync::Arc;

#[tokio::test]
async fn each_search_engine_connects_browses_and_runs_a_query_in_a_core_session() {
    for (engine, variable, seed) in [
        (
            Engine::Elasticsearch,
            "SPELAION_TEST_ELASTICSEARCH",
            "PUT /orders/_doc/core?refresh=true\n{\"name\":\"core\"}",
        ),
        (
            Engine::Opensearch,
            "SPELAION_TEST_OPENSEARCH",
            "PUT /orders/_doc/core?refresh=true\n{\"name\":\"core\"}",
        ),
        (
            Engine::Solr,
            "SPELAION_TEST_SOLR",
            "POST /solr/orders/update?commit=true\n[{\"id\":\"core\",\"name\":\"core\"}]",
        ),
        (
            Engine::Meilisearch,
            "SPELAION_TEST_MEILISEARCH",
            "POST /indexes/orders/documents?primaryKey=id\n[{\"id\":\"core\",\"name\":\"core\"}]",
        ),
    ] {
        let Ok(address) = std::env::var(variable) else {
            eprintln!("{variable} unset: skipped");
            continue;
        };
        let (host, port) = address.rsplit_once(':').unwrap();
        let connection = engines::driver(engine)
            .connect(&ConnectParams {
                host: host.into(),
                port: port.parse().unwrap(),
                tls: TlsMode::Disable,
                ..Default::default()
            })
            .await
            .unwrap();
        let dialect = spelaion_core::dialect_of(engine).unwrap();
        let session = Session::new(connection, dialect.clone(), Arc::new(ResultStore::new()));
        let mut events = session.run_script(seed);
        while let Some(event) = events.next().await {
            if let CoreEvent::StatementFailed { error, .. } = event {
                panic!("{}: {}", engine.as_str(), error.message);
            }
        }
        let mut found = false;
        for _ in 0..100 {
            let objects = session.connection().list_objects("indexes").await.unwrap();
            if let Some(object) = objects.iter().find(|o| o.name == "orders") {
                let statement = dialect.browse_statement(object).unwrap();
                assert!(statement.contains("200"));
                let mut events = session.run_script(&statement);
                while let Some(event) = events.next().await {
                    match event {
                        CoreEvent::StatementFailed { error, .. } => {
                            panic!("{}: {}", engine.as_str(), error.message)
                        }
                        CoreEvent::Rows { total_rows, .. } => {
                            found |= total_rows > 0;
                        }
                        _ => {}
                    }
                }
                if found {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(found, "{}: orders missing", engine.as_str());
        // Even a live session cannot pass an arbitrary URL to its plugin.
        let mut events = session.run_script("GET https://elsewhere.invalid/orders");
        let mut refused = false;
        while let Some(event) = events.next().await {
            refused |= matches!(event, CoreEvent::StatementFailed { .. });
        }
        assert!(refused);
    }
}

#[test]
fn destructive_http_requests_ask_in_the_core_and_unknown_routes_ask_on_production() {
    for engine in [
        Engine::Elasticsearch,
        Engine::Opensearch,
        Engine::Solr,
        Engine::Meilisearch,
    ] {
        let dialect = spelaion_core::dialect_of(engine).unwrap();
        assert_eq!(
            script_risks(dialect.as_ref(), "DELETE /orders", &Safeguards::default())[0].risk,
            Risk::Drop
        );
        assert_eq!(
            script_risks(
                dialect.as_ref(),
                "GET /unknown",
                &Safeguards {
                    confirm_data_changes: true,
                    ..Default::default()
                }
            )[0]
            .risk,
            Risk::DataChange
        );
        assert!(!spelaion_core::has_read_barrier(engine));
    }
}
