//! A Redis connection file (ADR-056): it loads with what Redis needs, and what Redis has not got
//! in SpelaionDB yet is refused when it loads, never let through: read-only mode (its barrier
//! is not built) and session setup statements (they are SQL).

use crate::common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver};

const BASE: &str =
    "schema_version = 1\nname = \"Cache\"\nengine = \"redis\"\nhost = \"cache.internal\"\n";

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_redis_connection_loads_and_resolves_with_its_port() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("redis-loads");
    fx.write("user/connections/cache.toml", BASE);
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("cache"))
        .unwrap();
    assert_eq!(resolved.engine, Engine::Redis);
    assert_eq!(resolved.params.port, 6379);
    assert!(!resolved.params.read_only);
    // No user and no database are needed: Redis's default user, and database 0.
    assert_eq!(resolved.params.user, "");
    assert!(!Engine::Redis.is_sql());
    assert!(!Engine::Redis.is_file());
}

#[test]
fn a_redis_connection_needs_where_its_server_is() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("redis-host");
    fx.write(
        "user/connections/cache.toml",
        "schema_version = 1\nname = \"Cache\"\nengine = \"redis\"\n",
    );
    assert!(problems(&fx).contains("missing: host"), "{}", problems(&fx));
}

#[test]
fn read_only_is_refused_on_redis_rather_than_let_writes_through() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("redis-read-only");
    fx.write(
        "user/connections/cache.toml",
        &format!("{BASE}read_only = true\n"),
    );
    let problems = problems(&fx);
    assert!(
        problems.contains("Redis has no read-only mode in SpelaionDB yet"),
        "{problems}"
    );
    assert!(fx.config().connections.iter().all(|c| c.id.path != "cache"));
}

#[test]
fn session_setup_is_refused_on_redis() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("redis-session");
    for session in [
        "\n[session]\nsetup = \"SET x 1\"\n",
        "\n[session]\nisolation = \"read-committed\"\n",
    ] {
        fx.write("user/connections/cache.toml", &format!("{BASE}{session}"));
        let problems = problems(&fx);
        assert!(
            problems.contains("Redis sessions run no setup statements"),
            "{session}: {problems}"
        );
    }
}
