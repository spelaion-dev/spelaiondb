//! Flight SQL connection files (ADR-041): the generic `flightsql`, `dremio` and `gizmosql` load
//! with what they need, run SQL (session setup included), and are refused read-only mode when
//! they load: SpelaionDB has no read barrier of its own for them.

mod common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver};

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn base(engine: &str) -> String {
    format!(
        "schema_version = 1\nname = \"Lake\"\nengine = \"{engine}\"\nhost = \"lake.internal\"\nport = 31337\n"
    )
}

#[test]
fn flight_sql_connections_load_and_resolve() {
    for (engine, expected, default_port) in [
        ("flightsql", Engine::Flightsql, None),
        ("dremio", Engine::Dremio, Some(32010)),
        ("gizmosql", Engine::Gizmosql, Some(31337)),
    ] {
        let fx = Fixture::new(&format!("{engine}-loads"));
        fx.write(
            "user/connections/lake.toml",
            &format!("{}\n[session]\nsetup = \"SET threads = 2\"\n", base(engine)),
        );
        assert_eq!(problems(&fx), "", "{engine}");
        let config = fx.config();
        let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
            .resolve(&personal("lake"))
            .unwrap();
        assert_eq!(resolved.engine, expected);
        assert_eq!(resolved.params.port, 31337);
        assert_eq!(expected.default_port(), default_port, "{engine}");
        // They run SQL, and have no barrier of ours.
        assert!(expected.is_sql());
        assert!(!expected.has_read_barrier());
        assert!(!expected.is_file());
    }
}

#[test]
fn read_only_is_refused_rather_than_promised_without_a_barrier() {
    for (engine, label) in [
        ("flightsql", "Arrow Flight SQL"),
        ("dremio", "Dremio"),
        ("gizmosql", "GizmoSQL"),
    ] {
        let fx = Fixture::new(&format!("{engine}-read-only"));
        fx.write(
            "user/connections/lake.toml",
            &format!("{}read_only = true\n", base(engine)),
        );
        let problems = problems(&fx);
        assert!(
            problems.contains(&format!("{label} has no read-only mode in SpelaionDB yet")),
            "{engine}: {problems}"
        );
        assert!(fx.config().connections.iter().all(|c| c.id.path != "lake"));
    }
}

#[test]
fn engines_with_a_barrier_keep_their_read_only_mode() {
    for engine in [
        Engine::Postgres,
        Engine::Mysql,
        Engine::Sqlserver,
        Engine::Clickhouse,
        Engine::Trino,
        Engine::Presto,
    ] {
        assert!(engine.has_read_barrier(), "{}", engine.as_str());
    }
    for engine in [Engine::Mongodb, Engine::Redis] {
        assert!(!engine.has_read_barrier(), "{}", engine.as_str());
    }
}
