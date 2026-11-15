//! Every engine is listed in `Engine::ALL` and named by its text, both ways:
//! `Engine::parse(engine.as_str())` gives the engine back, so the app's form (which sends that
//! text) reaches every engine there is.

use spelaion_config::Engine;

/// Where each engine stands in `Engine::ALL`. The match has no `_`: a new engine does not
/// compile until it has an arm here, and its arm is its place in the list.
fn position(engine: Engine) -> usize {
    match engine {
        Engine::Postgres => 0,
        Engine::Sqlite => 1,
        Engine::Mysql => 2,
        Engine::Mariadb => 3,
        Engine::Duckdb => 4,
        Engine::Sqlserver => 5,
        Engine::Mongodb => 6,
        Engine::Clickhouse => 7,
        Engine::Redis => 8,
        Engine::Trino => 9,
        Engine::Presto => 10,
        Engine::Oracle => 11,
        Engine::Libsql => 12,
        Engine::Cassandra => 13,
        Engine::Flightsql => 14,
        Engine::Dremio => 15,
        Engine::Gizmosql => 16,
        Engine::Neo4j => 17,
    }
}

/// The number of arms above: one more than the last position.
const ENGINES: usize = 18;

#[test]
fn every_engine_is_listed_once() {
    assert_eq!(Engine::ALL.len(), ENGINES);
    for (index, engine) in Engine::ALL.iter().enumerate() {
        assert_eq!(position(*engine), index, "{engine:?}");
    }
}

#[test]
fn every_engine_is_named_by_its_text() {
    for engine in Engine::ALL {
        assert_eq!(Engine::parse(engine.as_str()), Some(engine), "{engine:?}");
    }
    let mut names: Vec<&str> = Engine::ALL.iter().map(|e| e.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), Engine::ALL.len(), "two engines share a name");
}

#[test]
fn an_unknown_name_is_no_engine() {
    for name in ["", "Postgres", "postgresql", "snowflake", " mysql"] {
        assert_eq!(Engine::parse(name), None, "{name:?}");
    }
}
