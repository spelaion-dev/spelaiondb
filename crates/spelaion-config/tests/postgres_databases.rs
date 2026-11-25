//! The other databases a PostgreSQL connection opens (ADR-074, SDB-491): `databases` names
//! them and `all_databases` opens every one the server lists. This is the configuration only:
//! what loads and what is refused, what the resolved connection carries, how the form writes
//! it, what a paste keeps, and what a trusted repository pins. Nothing opens another database
//! yet.

use crate::common;

use common::{FakeBackend, Fixture, personal, team};
use spelaion_config::{
    ConnectionDraft, Engine, OtherDatabases, ResolveError, Resolver, connection_clipboard_text,
    paste_connection, update_connection,
};

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

const PG: &str = "schema_version = 1\nname = \"Tenants\"\nengine = \"postgres\"\nhost = \"db.internal\"\n\
                  port = 5432\ndatabase = \"app\"\nuser = \"app\"\n";
const MYSQL: &str = "schema_version = 1\nname = \"Shop\"\nengine = \"mysql\"\nhost = \"mysql.internal\"\nuser = \"app\"\n";

fn other_databases(fx: &Fixture, name: &str) -> OtherDatabases {
    let config = fx.config();
    Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .other_databases
}

#[test]
fn a_postgres_connection_names_or_opens_all_of_its_servers_databases() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("pgdbs-load");
    fx.write("user/connections/own.toml", PG);
    fx.write(
        "user/connections/named.toml",
        &format!("{PG}databases = [\"billing\", \"reports\"]\n"),
    );
    fx.write(
        "user/connections/all.toml",
        &format!("{PG}all_databases = true\n"),
    );
    fx.write(
        "user/connections/shop.toml",
        &format!("{MYSQL}databases = [\"sales\"]\n"),
    );
    assert_eq!(problems(&fx), "");
    assert_eq!(other_databases(&fx, "own"), OtherDatabases::None);
    assert_eq!(
        other_databases(&fx, "named"),
        OtherDatabases::Named(vec!["billing".into(), "reports".into()])
    );
    assert_eq!(other_databases(&fx, "all"), OtherDatabases::All);
    // MySQL's list is what is shown, never other databases opened.
    assert_eq!(other_databases(&fx, "shop"), OtherDatabases::None);

    // A file without either key reads and writes as before: nothing new in it.
    let config = fx.config();
    let own = config.connection(&personal("own")).unwrap();
    assert!(own.file.databases.is_none() && !own.file.all_databases);
    let written = toml::to_string(&own.file).unwrap();
    assert!(
        !written.contains("databases"),
        "neither key is written when unset: {written}"
    );
}

#[test]
fn what_adr_074_refuses_is_refused_when_the_file_loads() {
    let _turn = crate::serial::file_turn(module_path!());
    for (name, text, says) in [
        (
            "both",
            format!("{PG}databases = [\"billing\"]\nall_databases = true\n"),
            "cannot both be set",
        ),
        (
            "mysql-all",
            format!("{MYSQL}all_databases = true\n"),
            "all_databases is for PostgreSQL",
        ),
        (
            "sqlite-all",
            "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"a.db\"\nall_databases = true\n"
                .to_string(),
            "all_databases is for PostgreSQL",
        ),
        (
            "pooler-named",
            format!("{PG}pooler = \"session\"\ndatabases = [\"billing\"]\n"),
            "pooler opens only its own database",
        ),
        (
            "pooler-all",
            format!("{PG}pooler = \"transaction\"\nall_databases = true\n"),
            "pooler opens only its own database",
        ),
        // The limits of SDB-396 hold on PostgreSQL too.
        (
            "empty",
            format!("{PG}databases = []\n"),
            "databases names at most",
        ),
        (
            "long",
            format!("{PG}databases = [\"{}\"]\n", "x".repeat(129)),
            "databases names at most",
        ),
        (
            "many",
            format!(
                "{PG}databases = [{}]\n",
                (0..501)
                    .map(|i| format!("\"d{i}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            "databases names at most",
        ),
    ] {
        let fx = Fixture::new(&format!("pgdbs-refused-{name}"));
        fx.write("user/connections/db.toml", &text);
        let problems = problems(&fx);
        assert!(problems.contains(says), "{name}: {problems}");
    }
}

#[test]
fn the_form_saves_and_reads_back_each_of_the_three_choices() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("pgdbs-form");
    let text = format!("# Tenants' server.\n{PG}[tls]\nmode = \"verify-full\" # checked\n");
    let path = fx.write("user/connections/tenants.toml", &text);
    let backend = FakeBackend::default();
    let config = fx.config();
    let id = personal("tenants");
    let own = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    assert!(own.databases.is_empty() && !own.all_databases);

    // These databases: trimmed, blanks left out, each once; PostgreSQL's names are
    // case-sensitive, so `Reports` and `reports` are two databases.
    let named = ConnectionDraft {
        databases: vec![
            " billing ".into(),
            String::new(),
            "reports".into(),
            "billing".into(),
            "Reports".into(),
        ],
        ..own.clone()
    };
    update_connection(&config, &id, &named, None, &backend).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(
        saved.contains("databases = [\"billing\", \"reports\", \"Reports\"]"),
        "{saved}"
    );
    assert!(!saved.contains("all_databases"), "{saved}");
    assert!(saved.contains("# Tenants' server.") && saved.contains("# checked"));
    let config = fx.config();
    let back = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    assert_eq!(
        (back.databases.as_slice(), back.all_databases),
        (
            &[
                "billing".to_string(),
                "reports".to_string(),
                "Reports".to_string()
            ][..],
            false
        )
    );

    // All of them: `databases` goes, whatever the form still held.
    let all = ConnectionDraft {
        all_databases: true,
        ..back.clone()
    };
    update_connection(&config, &id, &all, None, &backend).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("all_databases = true"), "{saved}");
    assert!(!saved.contains("databases = ["), "{saved}");
    let config = fx.config();
    let back = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    assert!(back.all_databases && back.databases.is_empty());

    // Only its own: the file is again exactly what it was.
    update_connection(&config, &id, &own, None, &backend).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

    // Behind a pooler the choice is refused, never quietly dropped.
    let config = fx.config();
    let pooled = ConnectionDraft {
        pooler: Some(spelaion_config::PoolerMode::Session),
        all_databases: true,
        ..own.clone()
    };
    let refused = update_connection(&config, &id, &pooled, None, &backend)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(refused.contains("pooler"), "{refused}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

    // Another engine's form never writes `all_databases`; changing the engine drops it.
    let (mysql, change) = all.with_engine(Engine::Mysql);
    assert!(!mysql.all_databases);
    assert!(
        change.dropped.iter().any(|f| f == "all_databases"),
        "{change:?}"
    );
    let (mysql, change) = named.with_engine(Engine::Mysql);
    assert!(
        mysql.databases.is_empty(),
        "PostgreSQL's list does not become MySQL's shown databases"
    );
    assert!(
        change.dropped.iter().any(|f| f == "databases"),
        "{change:?}"
    );
}

#[test]
fn a_paste_never_widens_a_connection_by_itself() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("pgdbs-paste");
    let backend = FakeBackend::default();
    for (name, extra, left_out) in [
        ("named", "databases = [\"billing\"]\n", "databases"),
        ("all", "all_databases = true\n", "all_databases"),
    ] {
        let text = format!("{PG}{extra}").replace("Tenants", name);
        let pasted = paste_connection(&fx.config(), &text, &backend).unwrap();
        assert!(
            pasted.left_out.iter().any(|f| f == left_out),
            "{name}: {:?}",
            pasted.left_out
        );
        let config = fx.config();
        let file = &config.connection(&pasted.id).unwrap().file;
        assert!(file.databases.is_none() && !file.all_databases, "{name}");
    }
    // MySQL's list only narrows what is shown: kept.
    let pasted = paste_connection(
        &fx.config(),
        &format!("{MYSQL}databases = [\"sales\"]\n"),
        &backend,
    )
    .unwrap();
    assert!(pasted.left_out.is_empty(), "{:?}", pasted.left_out);
    let config = fx.config();
    assert_eq!(
        config.connection(&pasted.id).unwrap().file.databases,
        Some(vec!["sales".to_string()])
    );

    // A copy leaves them out too, and says so.
    fx.write(
        "user/connections/wide.toml",
        &format!("{PG}all_databases = true\n"),
    );
    let config = fx.config();
    let copied =
        connection_clipboard_text(&config, &fx.untrusted(), &personal("wide"), &backend).unwrap();
    assert!(!copied.text.contains("all_databases"), "{}", copied.text);
    assert!(copied.left_out.iter().any(|f| f == "all_databases"));
}

#[test]
fn a_pulled_change_to_either_key_needs_the_repository_trusted_again() {
    let _turn = crate::serial::file_turn(module_path!());
    for (name, before, after, field) in [
        (
            "named",
            format!("{PG}databases = [\"billing\"]\n"),
            format!("{PG}databases = [\"billing\", \"payroll\"]\n"),
            "databases",
        ),
        (
            "added",
            PG.to_string(),
            format!("{PG}databases = [\"payroll\"]\n"),
            "databases",
        ),
        (
            "all",
            PG.to_string(),
            format!("{PG}all_databases = true\n"),
            "all_databases",
        ),
    ] {
        let fx = Fixture::new(&format!("pgdbs-trust-{name}"));
        fx.write("repo/.spelaion/connections/db.toml", &before);
        let trust = fx.trusted();
        let backend = FakeBackend::default();
        let config = fx.config();
        Resolver::new(&config, &trust, &backend)
            .resolve(&team("db"))
            .unwrap();
        fx.write("repo/.spelaion/connections/db.toml", &after);
        let config = fx.config();
        match Resolver::new(&config, &trust, &backend).resolve(&team("db")) {
            // Shown field by field: a list's entries one by one (`databases.1`).
            Err(ResolveError::Changed { change, .. }) => {
                let fields: Vec<&str> = change.fields.iter().map(|f| f.field.as_str()).collect();
                assert!(
                    !fields.is_empty()
                        && fields
                            .iter()
                            .all(|f| { *f == field || f.starts_with(&format!("{field}.")) }),
                    "{name}: {fields:?}"
                );
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}
