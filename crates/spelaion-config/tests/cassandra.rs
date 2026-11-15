//! A Cassandra connection file (ADR-056): it loads with what Cassandra needs, and what Cassandra has not got
//! in SpelaionDB yet is refused when it loads, never let through: read-only mode (its barrier
//! is not built) and session setup statements (they are SQL).

mod common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver};

const BASE: &str =
    "schema_version = 1\nname = \"Ring\"\nengine = \"cassandra\"\nhost = \"ring.internal\"\n";

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_cassandra_connection_loads_and_resolves_with_its_port() {
    let fx = Fixture::new("cassandra-loads");
    fx.write("user/connections/ring.toml", BASE);
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("ring"))
        .unwrap();
    assert_eq!(resolved.engine, Engine::Cassandra);
    assert_eq!(resolved.params.port, 9042);
    assert!(!resolved.params.read_only);
    // No user and no database are needed: no keyspace: the explorer chooses.
    assert_eq!(resolved.params.user, "");
    assert!(!Engine::Cassandra.is_sql());
    assert!(!Engine::Cassandra.is_file());
}

#[test]
fn a_cassandra_connection_needs_where_its_server_is() {
    let fx = Fixture::new("cassandra-host");
    fx.write(
        "user/connections/ring.toml",
        "schema_version = 1\nname = \"Ring\"\nengine = \"cassandra\"\n",
    );
    assert!(problems(&fx).contains("missing: host"), "{}", problems(&fx));
}

#[test]
fn read_only_is_refused_on_cassandra_rather_than_let_writes_through() {
    let fx = Fixture::new("cassandra-read-only");
    fx.write(
        "user/connections/ring.toml",
        &format!("{BASE}read_only = true\n"),
    );
    let problems = problems(&fx);
    assert!(
        problems.contains("Cassandra has no read-only mode in SpelaionDB yet"),
        "{problems}"
    );
    assert!(fx.config().connections.iter().all(|c| c.id.path != "ring"));
}

#[test]
fn session_setup_is_refused_on_cassandra() {
    let fx = Fixture::new("cassandra-session");
    for session in [
        "\n[session]\nsetup = \"USE x\"\n",
        "\n[session]\nisolation = \"read-committed\"\n",
    ] {
        fx.write("user/connections/ring.toml", &format!("{BASE}{session}"));
        let problems = problems(&fx);
        assert!(
            problems.contains("Cassandra sessions run no setup statements"),
            "{session}: {problems}"
        );
    }
}
