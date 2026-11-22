//! HTTP engines deliberately have no read barrier or SQL session setup (ADR-056).
use crate::common;
use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver};

#[test]
fn search_connections_resolve_and_refuse_unsupported_barriers_and_session_setup() {
    let _turn = crate::serial::file_turn(module_path!());
    for (engine, port) in [
        (Engine::Elasticsearch, 9200),
        (Engine::Opensearch, 9200),
        (Engine::Solr, 8983),
        (Engine::Meilisearch, 7700),
    ] {
        let base = format!(
            "schema_version=1\nengine=\"{}\"\nhost=\"search.internal\"\n",
            engine.as_str()
        );
        let fx = Fixture::new(engine.as_str());
        fx.write("user/connections/search.toml", &base);
        let config = fx.config();
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
            .resolve(&personal("search"))
            .unwrap();
        assert_eq!(resolved.params.port, port);
        assert!(!engine.has_read_barrier());
        assert!(!engine.is_sql());
        for extra in [
            "read_only=true\n",
            "[session]\nsetup=\"DELETE /orders\"\n",
            "[session]\nschema=\"elsewhere\"\n",
        ] {
            fx.write("user/connections/search.toml", &format!("{base}{extra}"));
            let config = fx.config();
            assert!(
                !config.problems.is_empty(),
                "{} accepted {extra}",
                engine.as_str()
            );
            assert!(config.connections.is_empty());
        }
    }
}
