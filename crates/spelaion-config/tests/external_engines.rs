//! Connections of an engine served by someone else's plugin (ADR-041, point 5; ADR-055, point
//! 5): the file names the engine, never the plugin. Nothing in a repository's `.spelaion/`, or in
//! a connection file anywhere, can install a plugin, name one or point to one, and such an engine
//! never has a read-only mode.

use crate::common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver, check_external_engine};

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

const BASE: &str = "schema_version = 1\nname = \"Theirs\"\nengine = \"external\"\nhost = \"db.internal\"\nport = 7000\n";

#[test]
fn external_engines_a_connection_names_the_engine_and_resolves() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("external-loads");
    fx.write(
        "user/connections/theirs.toml",
        &format!("{BASE}external_engine = \"acme-testdb\"\n"),
    );
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("theirs"))
        .unwrap();
    assert_eq!(resolved.engine, Engine::External);
    assert_eq!(resolved.external_engine.as_deref(), Some("acme-testdb"));
    assert_eq!(resolved.params.port, 7000);
    // No barrier of ours.
    assert!(!Engine::External.has_read_barrier());
    assert_eq!(Engine::parse("external"), Some(Engine::External));
}

#[test]
fn external_engines_read_only_is_never_offered() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("external-read-only");
    fx.write(
        "user/connections/theirs.toml",
        &format!("{BASE}external_engine = \"acme-testdb\"\nread_only = true\n"),
    );
    let problems = problems(&fx);
    assert!(problems.contains("no read-only mode"), "{problems}");
    assert!(fx.config().connections.is_empty());
}

/// A repository file, or a connection file anywhere, that names a plugin, its file, its address
/// or its hash is refused whole: there is no such field, and `external_engine` takes an
/// engine's name and nothing else.
#[test]
fn external_engines_no_file_names_or_points_to_a_plugin() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("external-points");
    let cases = [
        (
            "plugin",
            "plugin = \"acme-testdb\"\nexternal_engine = \"acme-testdb\"\n",
        ),
        (
            "address",
            "plugin_address = \"https://acme.example/acme-testdb.tar.gz\"\nexternal_engine = \"acme-testdb\"\n",
        ),
        (
            "file",
            "plugin_file = \"C:/plugins/acme.tar.gz\"\nexternal_engine = \"acme-testdb\"\n",
        ),
        (
            "sha",
            "plugin_sha256 = \"00\"\nexternal_engine = \"acme-testdb\"\n",
        ),
        (
            "url",
            "external_engine = \"https://acme.example/acme-testdb.tar.gz\"\n",
        ),
        ("path", "external_engine = \"../../plugins/acme.exe\"\n"),
        ("windows", "external_engine = 'C:\\plugins\\acme.exe'\n"),
        ("ours", "external_engine = \"postgres\"\n"),
        ("kind", "external_engine = \"cockroachdb\"\n"),
        ("brand", "external_engine = \"spelaion-testdb\"\n"),
        ("missing", ""),
    ];
    for (name, extra) in cases {
        fx.write(
            &format!("repo/.spelaion/connections/{name}.toml"),
            &format!("{BASE}{extra}"),
        );
        fx.write(
            &format!("user/connections/{name}.toml"),
            &format!("{BASE}{extra}"),
        );
    }
    // One of our engines cannot name an external one either.
    fx.write(
        "repo/.spelaion/connections/sqlite.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"a.db\"\nexternal_engine = \"acme-testdb\"\n",
    );
    let config = spelaion_config::load(Some(&fx.repo()), &fx.user());
    assert!(
        config.connections.is_empty(),
        "{:?}",
        config
            .connections
            .iter()
            .map(|entry| entry.id.path.clone())
            .collect::<Vec<_>>()
    );
    for (name, _) in cases {
        for layer in ["repo", "user"] {
            assert!(
                config.problems.iter().any(|problem| {
                    problem.file.ends_with(format!("{name}.toml"))
                        && problem.file.to_string_lossy().contains(layer)
                }),
                "{layer}/{name}: {:?}",
                config.problems
            );
        }
    }
    assert!(
        config
            .problems
            .iter()
            .any(|p| p.file.ends_with("sqlite.toml") && p.message.contains("external_engine"))
    );
    // The personal file and the repository's other files have no place for a plugin either.
    fx.write(
        "user/user.toml",
        "schema_version = 1\n[plugins]\nadd = \"https://acme.example/x.tar.gz\"\n",
    );
    let config = spelaion_config::load(None, &fx.user());
    assert!(
        config
            .problems
            .iter()
            .any(|p| p.file.ends_with("user.toml")),
        "{:?}",
        config.problems
    );
}

#[test]
fn external_engines_names_follow_one_rule() {
    let _turn = crate::serial::file_turn(module_path!());
    for good in ["acme-testdb", "foodb", "x1", "a-b-c"] {
        assert!(check_external_engine(good).is_ok(), "{good}");
    }
    for bad in [
        "",
        "a",
        "-foo",
        "foo-",
        "Foo",
        "foo_db",
        "foo.db",
        "foo/db",
        "https://x",
        "external",
        "postgres",
        "sqlite",
        "duckdb",
        "tidb",
        "valkey",
        "spelaion",
        "spelaiondb-x",
        &"a".repeat(41),
    ] {
        assert!(check_external_engine(bad).is_err(), "{bad:?}");
    }
}
