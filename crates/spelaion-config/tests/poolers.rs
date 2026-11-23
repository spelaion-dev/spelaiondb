//! Poolers and bastions (SDB-396): a PostgreSQL connection's pooler mode, MySQL's cleartext
//! password plugin and the databases a connection shows. Each loads only on the engine that
//! has it, reaches the driver only as an engine option, is saved by the form touching only its
//! key, and is part of what a trusted repository pins (ADR-022).

use crate::common;

use common::{FakeBackend, Fixture, personal, team};
use spelaion_config::{
    ConnectionDraft, PoolerMode, ResolveError, Resolver, engine_options, update_connection,
};

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

const PG: &str = "schema_version = 1\nname = \"Pooled\"\nengine = \"postgres\"\nhost = \"pgbouncer.internal\"\n\
                  port = 6432\ndatabase = \"app\"\nuser = \"app\"\n";
const MYSQL: &str = "schema_version = 1\nname = \"Proxy\"\nengine = \"mysql\"\nhost = \"proxy.internal\"\nuser = \"app\"\n";

#[test]
fn a_pooler_mode_loads_on_postgres_and_reaches_the_driver_as_an_option() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("pooler-loads");
    fx.write(
        "user/connections/pooled.toml",
        &format!("{PG}pooler = \"transaction\"\n"),
    );
    fx.write(
        "user/connections/session.toml",
        &format!("{PG}pooler = \"session\"\nread_only = true\n[session]\nschema = \"reports\"\n"),
    );
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let (trust, backend) = (fx.untrusted(), FakeBackend::default());
    let resolver = Resolver::new(&config, &trust, &backend);
    let resolved = resolver.resolve(&personal("pooled")).unwrap();
    assert_eq!(
        resolved
            .params
            .options
            .get(engine_options::POOLER)
            .map(String::as_str),
        Some("transaction")
    );
    let session = resolver.resolve(&personal("session")).unwrap();
    assert_eq!(
        session
            .params
            .options
            .get(engine_options::POOLER)
            .map(String::as_str),
        Some("session")
    );
}

#[test]
fn a_pooler_is_refused_on_other_engines_and_transaction_mode_refuses_session_state() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("pooler-refused");
    fx.write(
        "user/connections/mysql.toml",
        &format!("{MYSQL}pooler = \"session\"\n"),
    );
    assert!(problems(&fx).contains("pooler"), "{}", problems(&fx));

    // In transaction mode a session's settings would land on whichever server connection the
    // next transaction gets: a read-only mode or a schema set once would not hold.
    for (name, extra) in [
        ("ro", "read_only = true\n"),
        ("schema", "[session]\nschema = \"reports\"\n"),
        (
            "setup",
            "[session]\nsetup = \"SET search_path = reports\"\n",
        ),
        ("isolation", "[session]\nisolation = \"serializable\"\n"),
    ] {
        let fx = Fixture::new(&format!("pooler-tx-{name}"));
        fx.write(
            "user/connections/pooled.toml",
            &format!("{PG}pooler = \"transaction\"\n{extra}"),
        );
        let problems = problems(&fx);
        assert!(problems.contains("transaction mode"), "{name}: {problems}");
    }

    // An unknown mode is a problem too, never a direct connection.
    let fx = Fixture::new("pooler-unknown");
    fx.write(
        "user/connections/pooled.toml",
        &format!("{PG}pooler = \"statement\"\n"),
    );
    assert!(!problems(&fx).is_empty());
}

#[test]
fn the_cleartext_plugin_loads_only_on_mysql_and_mariadb() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("cleartext-loads");
    fx.write(
        "user/connections/proxy.toml",
        &format!("{MYSQL}cleartext_password = true\n[tls]\nmode = \"verify-full\"\n"),
    );
    fx.write(
        "user/connections/maria.toml",
        &MYSQL
            .replace("mysql", "mariadb")
            .replace("Proxy", "Maria")
            .replace("\nuser", "\ncleartext_password = true\nuser"),
    );
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let (trust, backend) = (fx.untrusted(), FakeBackend::default());
    let resolver = Resolver::new(&config, &trust, &backend);
    for id in ["proxy", "maria"] {
        let resolved = resolver.resolve(&personal(id)).unwrap();
        assert_eq!(
            resolved
                .params
                .options
                .get(engine_options::CLEARTEXT_PASSWORD)
                .map(String::as_str),
            Some("on"),
            "{id}"
        );
    }

    let fx = Fixture::new("cleartext-refused");
    fx.write(
        "user/connections/pg.toml",
        &format!("{PG}cleartext_password = true\n"),
    );
    assert!(
        problems(&fx).contains("cleartext_password"),
        "{}",
        problems(&fx)
    );
}

#[test]
fn the_databases_shown_are_bounded_and_only_where_an_engine_chooses_them() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("databases-loads");
    fx.write(
        "user/connections/proxy.toml",
        &format!("{MYSQL}databases = [\"sales\", \"hr\"]\n"),
    );
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("proxy"))
        .unwrap();
    assert_eq!(
        resolved.databases.as_deref(),
        Some(&["sales".to_string(), "hr".to_string()][..])
    );

    for (name, text) in [
        ("pg", format!("{PG}databases = [\"app\"]\n")),
        ("empty", format!("{MYSQL}databases = []\n")),
        ("blank", format!("{MYSQL}databases = [\"  \"]\n")),
        ("control", format!("{MYSQL}databases = [\"a\\u0007b\"]\n")),
        (
            "long",
            format!("{MYSQL}databases = [\"{}\"]\n", "x".repeat(129)),
        ),
        (
            "many",
            format!(
                "{MYSQL}databases = [{}]\n",
                (0..501)
                    .map(|i| format!("\"d{i}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
    ] {
        let fx = Fixture::new(&format!("databases-{name}"));
        fx.write("user/connections/proxy.toml", &text);
        assert!(
            problems(&fx).contains("databases"),
            "{name}: {}",
            problems(&fx)
        );
    }
}

#[test]
fn the_form_writes_each_setting_alone_and_takes_it_out_again() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("sdb396-form");
    let text = format!("# Behind ProxySQL.\n{MYSQL}[tls]\nmode = \"verify-full\" # checked\n");
    let path = fx.write("user/connections/proxy.toml", &text);
    let backend = FakeBackend::default();
    let config = fx.config();
    let id = personal("proxy");
    let draft = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    assert!(!draft.cleartext_password && draft.databases.is_empty() && draft.pooler.is_none());

    let changed = ConnectionDraft {
        cleartext_password: true,
        // Typed with spaces, a blank line and a repeat: kept trimmed, each once.
        databases: vec![" sales ".into(), String::new(), "hr".into(), "SALES".into()],
        // Another engine's setting is not written.
        pooler: Some(PoolerMode::Transaction),
        ..draft.clone()
    };
    update_connection(&config, &id, &changed, None, &backend).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("cleartext_password = true"), "{saved}");
    assert!(saved.contains("databases = [\"sales\", \"hr\"]"), "{saved}");
    assert!(!saved.contains("pooler"), "{saved}");
    assert!(saved.contains("# Behind ProxySQL.") && saved.contains("# checked"));

    let config = fx.config();
    let back = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    assert_eq!(back.databases, ["sales", "hr"]);
    update_connection(&config, &id, &draft, None, &backend).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

    // PostgreSQL's pooler, written and removed the same way.
    let text = PG.to_string();
    let path = fx.write("user/connections/pooled.toml", &text);
    let config = fx.config();
    let id = personal("pooled");
    let draft = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    let pooled = ConnectionDraft {
        pooler: Some(PoolerMode::Transaction),
        databases: vec!["other".into()],
        ..draft.clone()
    };
    update_connection(&config, &id, &pooled, None, &backend).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("pooler = \"transaction\""), "{saved}");
    assert!(!saved.contains("databases"), "{saved}");
    let config = fx.config();
    update_connection(&config, &id, &draft, None, &backend).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn a_pulled_change_to_any_of_them_needs_the_repository_trusted_again() {
    let _turn = crate::serial::file_turn(module_path!());
    for (name, before, after) in [
        (
            "cleartext",
            format!("{MYSQL}[tls]\nmode = \"verify-full\"\n"),
            format!("{MYSQL}cleartext_password = true\n[tls]\nmode = \"verify-full\"\n"),
        ),
        (
            "databases",
            format!("{MYSQL}databases = [\"sales\"]\n"),
            format!("{MYSQL}databases = [\"sales\", \"hr\"]\n"),
        ),
        (
            "pooler",
            format!("{PG}pooler = \"session\"\n"),
            format!("{PG}pooler = \"transaction\"\n"),
        ),
    ] {
        let fx = Fixture::new(&format!("sdb396-trust-{name}"));
        fx.write("repo/.spelaion/connections/db.toml", &before);
        let trust = fx.trusted();
        let backend = FakeBackend::default();
        let config = fx.config();
        Resolver::new(&config, &trust, &backend)
            .resolve(&team("db"))
            .unwrap();
        fx.write("repo/.spelaion/connections/db.toml", &after);
        let config = fx.config();
        assert!(
            matches!(
                Resolver::new(&config, &trust, &backend).resolve(&team("db")),
                Err(ResolveError::Changed { .. })
            ),
            "{name}"
        );
    }
}
