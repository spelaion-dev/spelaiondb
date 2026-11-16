//! HiveServer2's connection files (SDB-176): `hive`, `spark`, `kyuubi` and `impala` load with what
//! they need, run SQL (session setup included), and are refused read-only mode and client
//! certificates when they load: SpelaionDB has no read barrier of its own for them, and the
//! plugin logs in with a user and a password.

mod common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver};

const ENGINES: [(&str, Engine, u16, &str); 4] = [
    ("hive", Engine::Hive, 10000, "Apache Hive"),
    ("spark", Engine::Spark, 10000, "Spark's Thrift server"),
    ("kyuubi", Engine::Kyuubi, 10009, "Apache Kyuubi"),
    ("impala", Engine::Impala, 21050, "Apache Impala"),
];

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
        "schema_version = 1\nname = \"Warehouse\"\nengine = \"{engine}\"\nhost = \"warehouse.internal\"\n"
    )
}

#[test]
fn hiveserver2_connections_load_and_resolve() {
    for (engine, expected, default_port, _) in ENGINES {
        let fx = Fixture::new(&format!("{engine}-loads"));
        fx.write(
            "user/connections/warehouse.toml",
            &format!(
                "{}database = \"sales\"\n\n[session]\nsetup = \"SET hive.exec.dynamic.partition = true\"\n",
                base(engine)
            ),
        );
        assert_eq!(problems(&fx), "", "{engine}");
        let config = fx.config();
        let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
            .resolve(&personal("warehouse"))
            .unwrap();
        assert_eq!(resolved.engine, expected);
        // With no port written, the engine's own.
        assert_eq!(resolved.params.port, default_port, "{engine}");
        assert_eq!(resolved.params.database, "sales", "{engine}");
        // They run SQL, and have no barrier of ours.
        assert!(expected.is_sql());
        assert!(!expected.has_read_barrier());
        assert!(!expected.is_file());
        assert!(!expected.has_session_schema());
    }
}

#[test]
fn read_only_is_refused_rather_than_promised_without_a_barrier() {
    for (engine, _, _, label) in ENGINES {
        let fx = Fixture::new(&format!("{engine}-read-only"));
        fx.write(
            "user/connections/warehouse.toml",
            &format!("{}read_only = true\n", base(engine)),
        );
        let problems = problems(&fx);
        assert!(
            problems.contains(&format!("{label} has no read-only mode in SpelaionDB yet")),
            "{engine}: {problems}"
        );
        assert!(
            fx.config()
                .connections
                .iter()
                .all(|c| c.id.path != "warehouse")
        );
    }
}

#[test]
fn client_certificates_are_refused() {
    for (engine, _, _, _) in ENGINES {
        let fx = Fixture::new(&format!("{engine}-cert"));
        fx.write(
            "user/connections/warehouse.toml",
            &format!(
                "{}\n[tls]\nmode = \"verify-full\"\ncert = \"~/c.pem\"\nkey = \"~/c.key\"\n",
                base(engine)
            ),
        );
        let problems = problems(&fx);
        assert!(
            problems.contains("HiveServer2 connections log in with a user and a password"),
            "{engine}: {problems}"
        );
    }
}
