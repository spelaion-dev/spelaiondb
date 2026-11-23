//! A TDengine connection file (ADR-056): it loads with what TDengine needs, and what TDengine has not got
//! in SpelaionDB yet is refused when it loads, never let through: read-only mode (its barrier
//! is not built) and session setup statements (they are SQL).

use crate::common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver};

const BASE: &str =
    "schema_version = 1\nname = \"Meters\"\nengine = \"tdengine\"\nhost = \"meters.internal\"\n";

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_tdengine_connection_loads_and_resolves_with_its_port() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("tdengine-loads");
    fx.write("user/connections/meters.toml", BASE);
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("meters"))
        .unwrap();
    assert_eq!(resolved.engine, Engine::Tdengine);
    assert_eq!(resolved.params.port, 6041);
    assert!(!resolved.params.read_only);
    // No user and no database are needed: no keyspace: the explorer chooses.
    assert_eq!(resolved.params.user, "");
    assert!(!Engine::Tdengine.is_sql());
    assert!(!Engine::Tdengine.is_file());
}

#[test]
fn a_tdengine_connection_needs_where_its_server_is() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("tdengine-host");
    fx.write(
        "user/connections/meters.toml",
        "schema_version = 1\nname = \"Meters\"\nengine = \"tdengine\"\n",
    );
    assert!(problems(&fx).contains("missing: host"), "{}", problems(&fx));
}

#[test]
fn read_only_is_refused_on_tdengine_rather_than_let_writes_through() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("tdengine-read-only");
    fx.write(
        "user/connections/meters.toml",
        &format!("{BASE}read_only = true\n"),
    );
    let problems = problems(&fx);
    assert!(
        problems.contains("TDengine has no read-only mode in SpelaionDB yet"),
        "{problems}"
    );
    assert!(
        fx.config()
            .connections
            .iter()
            .all(|c| c.id.path != "meters")
    );
}

#[test]
fn session_setup_is_refused_on_tdengine() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("tdengine-session");
    for session in [
        "\n[session]\nsetup = \"USE x\"\n",
        "\n[session]\nisolation = \"read-committed\"\n",
    ] {
        fx.write("user/connections/meters.toml", &format!("{BASE}{session}"));
        let problems = problems(&fx);
        assert!(
            problems.contains("TDengine sessions run no setup statements"),
            "{session}: {problems}"
        );
    }
}
