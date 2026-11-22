//! A MongoDB connection file (ADR-056): it loads with what MongoDB needs, and what MongoDB has
//! not got in SpelaionDB yet is refused when it loads, never let through: read-only mode (its
//! barrier is not built) and session setup statements (they are SQL).

use crate::common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver};

const BASE: &str =
    "schema_version = 1\nname = \"Shop\"\nengine = \"mongodb\"\nhost = \"mongo.internal\"\n";

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_mongodb_connection_loads_and_resolves_with_its_port() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("mongodb-loads");
    fx.write("user/connections/shop.toml", BASE);
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("shop"))
        .unwrap();
    assert_eq!(resolved.engine, Engine::Mongodb);
    assert_eq!(resolved.params.port, 27017);
    assert!(!resolved.params.read_only);
    // No login and no database are needed: some servers have none, and the explorer chooses.
    assert_eq!(resolved.params.user, "");
    assert!(!Engine::Mongodb.is_sql());
    assert!(!Engine::Mongodb.is_file());
}

#[test]
fn a_mongodb_connection_needs_where_its_server_is() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("mongodb-host");
    fx.write(
        "user/connections/shop.toml",
        "schema_version = 1\nname = \"Shop\"\nengine = \"mongodb\"\n",
    );
    assert!(problems(&fx).contains("missing: host"), "{}", problems(&fx));
}

#[test]
fn read_only_is_refused_on_mongodb_rather_than_let_writes_through() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("mongodb-read-only");
    fx.write(
        "user/connections/shop.toml",
        &format!("{BASE}read_only = true\n"),
    );
    let problems = problems(&fx);
    assert!(
        problems.contains("MongoDB has no read-only mode in SpelaionDB yet"),
        "{problems}"
    );
    // It is not loaded at all: nothing can open it.
    assert!(fx.config().connections.iter().all(|c| c.id.path != "shop"));
}

#[test]
fn session_setup_is_refused_on_mongodb() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("mongodb-session");
    for session in [
        "\n[session]\nsetup = \"SET x = 1\"\n",
        "\n[session]\nschema = \"app\"\n",
        "\n[session]\nisolation = \"read-committed\"\n",
    ] {
        fx.write("user/connections/shop.toml", &format!("{BASE}{session}"));
        let problems = problems(&fx);
        assert!(
            problems.contains("MongoDB sessions run no setup statements"),
            "{session}: {problems}"
        );
    }
}
