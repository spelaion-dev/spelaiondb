//! H2 and HSQLDB connection files (ADR-059, SDB-317): files on this machine, opened through the
//! JDBC bridge. They load with a file's path, and what the bridge does not have is refused when
//! they load, never let through: read-only mode (no barrier of ours), a tunnel or a Kubernetes
//! route (the file is local) and a client certificate.

use crate::common;

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
        "schema_version = 1\nname = \"Shop\"\nengine = \"{engine}\"\ndatabase = \"~/shop.mv.db\"\n"
    )
}

#[test]
fn an_h2_or_hsqldb_connection_is_a_local_file() {
    let _turn = crate::serial::file_turn(module_path!());
    for (name, engine) in [("h2", Engine::H2), ("hsqldb", Engine::Hsqldb)] {
        let fx = Fixture::new(&format!("jdbc-loads-{name}"));
        fx.write("user/connections/shop.toml", &base(name));
        assert_eq!(problems(&fx), "", "{name}");
        let config = fx.config();
        let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
            .resolve(&personal("shop"))
            .unwrap();
        assert_eq!(resolved.engine, engine);
        // The path is resolved, as SQLite's and DuckDB's are: the core makes the URL from it.
        assert!(
            resolved.params.database.ends_with("shop.mv.db"),
            "{}",
            resolved.params.database
        );
        assert!(!resolved.params.database.starts_with('~'));
        assert_eq!(resolved.params.port, 0);
        assert!(engine.is_file() && engine.is_sql() && !engine.has_read_barrier());
        assert_eq!(Engine::parse(name), Some(engine));
    }
}

#[test]
fn an_h2_or_hsqldb_connection_needs_its_file() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("jdbc-database");
    fx.write(
        "user/connections/shop.toml",
        "schema_version = 1\nname = \"Shop\"\nengine = \"h2\"\n",
    );
    assert!(
        problems(&fx).contains("missing: database"),
        "{}",
        problems(&fx)
    );
}

#[test]
fn what_the_bridge_has_not_got_is_refused_when_the_file_loads() {
    let _turn = crate::serial::file_turn(module_path!());
    for name in ["h2", "hsqldb"] {
        let fx = Fixture::new(&format!("jdbc-refused-{name}"));
        for (extra, refusal) in [
            ("read_only = true\n", "no read-only mode"),
            ("[kubernetes]\nresource = \"svc/db\"\n", "is a local file"),
            (
                "[tls]\ncert = \"~/c.pem\"\nkey = \"~/k.pem\"\n",
                "a local file has no client certificate",
            ),
        ] {
            fx.write(
                "user/connections/shop.toml",
                &format!("{}{extra}", base(name)),
            );
            let problems = problems(&fx);
            assert!(problems.contains(refusal), "{name}, {extra}: {problems}");
            assert!(fx.config().connections.iter().all(|c| c.id.path != "shop"));
        }
    }
}
