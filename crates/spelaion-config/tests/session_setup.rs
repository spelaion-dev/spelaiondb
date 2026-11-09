//! A connection's `[session]` table (ADR-037): how it loads, resolves, is pinned by repository
//! trust, written by the form, and left out of pasted text. Temporary directories and a fake
//! secret backend (the real keychain is never touched).

mod common;

use common::{FakeBackend, Fixture, personal, team};
use spelaion_config::{
    ConnectionDraft, Engine, IsolationLevel, ResolveError, Resolver, SessionDraft,
    connection_clipboard_text, paste_connection, update_connection,
};

const BASE: &str = "schema_version = 1\nname = \"Orders\"\nengine = \"postgres\"\nhost = \"db.internal\"\ndatabase = \"orders\"\nuser = \"app\"\n";

const SESSION: &str = "\n[session]\nsetup = \"\"\"\nSET search_path TO app, public;\nSET TIME ZONE 'UTC';\n\"\"\"\nschema = \"app\"\nisolation = \"repeatable-read\"\n";

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_session_table_loads_and_resolves() {
    let fx = Fixture::new("session-resolves");
    fx.write(
        "user/connections/orders.toml",
        &format!(
            "{BASE}\n[session]\nsetup = \"select '${{user.not_a_variable}}'; SET search_path TO app\"\nignore_setup_errors = true\nschema = \"${{user.schema}}\"\nisolation = \"read-committed\"\n"
        ),
    );
    fx.write(
        "user/user.toml",
        "schema_version = 1\n[vars]\nschema = \"app_pablo\"\n",
    );
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let resolved = Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("orders"))
        .unwrap();
    // The schema is expanded like any text field; the setup is SQL sent as written, so a
    // `${…}` in it is neither expanded nor asked for.
    assert_eq!(resolved.session.schema.as_deref(), Some("app_pablo"));
    assert_eq!(
        resolved.session.setup.as_deref(),
        Some("select '${user.not_a_variable}'; SET search_path TO app")
    );
    assert!(resolved.session.ignore_setup_errors);
    assert_eq!(
        resolved.session.isolation,
        Some(IsolationLevel::ReadCommitted)
    );

    // A missing variable in the schema is asked for, as anywhere else.
    fx.write("user/user.toml", "schema_version = 1\n");
    let config = fx.config();
    match Resolver::new(&config, &fx.untrusted(), &FakeBackend::default())
        .resolve(&personal("orders"))
    {
        Err(ResolveError::MissingVariables { names }) => assert_eq!(names, ["schema"]),
        other => panic!("expected the missing variable: {other:?}"),
    }
}

#[test]
fn the_session_table_is_checked_against_the_engine() {
    let fx = Fixture::new("session-checks");
    let cases = [
        ("postgres", "schema = \"\"", "cannot be empty"),
        (
            "postgres",
            "schema = \" app\"",
            "cannot be empty or start or end",
        ),
        ("postgres", "schema = \"a\u{200B}b\"", "hidden characters"),
        ("sqlserver", "schema = \"dbo\"", "no schema per session"),
        ("sqlite", "schema = \"main\"", "no schema per session"),
        (
            "postgres",
            "isolation = \"snapshot\"",
            "no snapshot isolation level",
        ),
        (
            "mysql",
            "isolation = \"snapshot\"",
            "no snapshot isolation level",
        ),
        (
            "sqlite",
            "isolation = \"serializable\"",
            "single isolation level",
        ),
        (
            "duckdb",
            "isolation = \"serializable\"",
            "single isolation level",
        ),
        ("postgres", "isolation = \"chaos\"", "unknown variant"),
        ("postgres", "setup = \"   \"", "session.setup is empty"),
        ("postgres", "setup = \"select 1\\u0000\"", "only hold text"),
        ("postgres", "setup = \"select 1\\u202E\"", "only hold text"),
        (
            "postgres",
            "ignore_setup_errors = true",
            "needs a session.setup",
        ),
        ("postgres", "unknown = 1", "unknown field"),
    ];
    for (engine, line, expected) in cases {
        let file = match engine {
            "sqlite" | "duckdb" => format!(
                "schema_version = 1\nengine = \"{engine}\"\ndatabase = \"x.db\"\n\n[session]\n{line}\n"
            ),
            _ => format!(
                "schema_version = 1\nengine = \"{engine}\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\n\n[session]\n{line}\n"
            ),
        };
        fx.write("user/connections/c.toml", &file);
        let problems = problems(&fx);
        assert!(problems.contains(expected), "{engine} {line}: {problems}");
    }
    let long = "x".repeat(20_001);
    fx.write(
        "user/connections/c.toml",
        &format!("{BASE}\n[session]\nsetup = \"{long}\"\n"),
    );
    assert!(problems(&fx).contains("at most 20000 characters"));

    // What each engine can do is accepted.
    for (engine, line) in [
        ("postgres", "schema = \"App\"\nisolation = \"serializable\""),
        (
            "mysql",
            "schema = \"shop\"\nisolation = \"read-uncommitted\"",
        ),
        (
            "mariadb",
            "schema = \"shop\"\nisolation = \"repeatable-read\"",
        ),
        (
            "sqlserver",
            "isolation = \"snapshot\"\nsetup = \"SET DATEFORMAT dmy\"",
        ),
        ("duckdb", "schema = \"main\""),
        ("sqlite", "setup = \"PRAGMA foreign_keys = ON\""),
    ] {
        let file = match engine {
            "sqlite" | "duckdb" => format!(
                "schema_version = 1\nengine = \"{engine}\"\ndatabase = \"x.db\"\n\n[session]\n{line}\n"
            ),
            _ => format!(
                "schema_version = 1\nengine = \"{engine}\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\n\n[session]\n{line}\n"
            ),
        };
        fx.write("user/connections/c.toml", &file);
        assert_eq!(problems(&fx), "", "{engine} {line}");
    }
}

/// Resolves the repository connection `orders`.
fn resolve_team(fx: &Fixture, trust: &spelaion_config::TrustStore) -> Result<(), ResolveError> {
    let config = fx.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    Resolver::new(&config, trust, &FakeBackend::default())
        .resolve(&team("orders"))
        .map(|_| ())
}

fn changed_fields(
    result: Result<(), ResolveError>,
) -> Vec<(String, Option<String>, Option<String>)> {
    match result {
        Err(ResolveError::Changed { change, .. }) => change
            .fields
            .into_iter()
            .map(|f| (f.field, f.before, f.after))
            .collect(),
        other => panic!("expected a change to review: {other:?}"),
    }
}

#[test]
fn a_repository_connection_whose_setup_changed_is_refused_until_trusted_again() {
    let fx = Fixture::new("session-trust");
    let path = fx.write(
        "repo/.spelaion/connections/orders.toml",
        &format!("{BASE}{SESSION}"),
    );
    let mut trust = fx.trusted();
    resolve_team(&fx, &trust).unwrap();

    // A pull that changes the setup: nothing resolves, and the change shows the SQL.
    let sneaky = format!("{BASE}{SESSION}").replace(
        "SET TIME ZONE 'UTC';",
        "SET TIME ZONE 'UTC';\nDELETE FROM orders;",
    );
    std::fs::write(&path, &sneaky).unwrap();
    let fields = changed_fields(resolve_team(&fx, &trust));
    assert_eq!(fields.len(), 1, "{fields:?}");
    assert_eq!(fields[0].0, "session.setup");
    assert!(
        fields[0]
            .2
            .as_deref()
            .unwrap()
            .contains("DELETE FROM orders")
    );

    // Reloading the trust store from disk does not forget it; accepting the change does.
    let reloaded = spelaion_config::TrustStore::load(&fx.user()).unwrap();
    assert!(resolve_team(&fx, &reloaded).is_err());
    trust.accept(&fx.config(), "connection:orders").unwrap();
    resolve_team(&fx, &trust).unwrap();

    // Each key is pinned, also when it is only switched on, added or removed.
    for (edit, field) in [
        (
            sneaky.replace(
                "isolation = \"repeatable-read\"",
                "isolation = \"read-uncommitted\"",
            ),
            "session.isolation",
        ),
        (
            sneaky.replace("schema = \"app\"", "schema = \"other\""),
            "session.schema",
        ),
        (
            sneaky.replace("[session]\n", "[session]\nignore_setup_errors = true\n"),
            "session.ignore_setup_errors",
        ),
        (sneaky.replace("schema = \"app\"\n", ""), "session.schema"),
    ] {
        std::fs::write(&path, &edit).unwrap();
        let fields = changed_fields(resolve_team(&fx, &trust));
        assert!(
            fields.iter().any(|(f, _, _)| f == field),
            "{field}: {fields:?}"
        );
    }

    // The same SQL written another way (a basic string, other spacing inside the table) is the
    // same value: only content counts. Different SQL, however written, is a change.
    let basic = format!(
        "{BASE}\n[session]\nschema = \"app\"\nisolation = \"repeatable-read\"\nsetup = \"SET search_path TO app, public;\\nSET TIME ZONE 'UTC';\\nDELETE FROM orders;\\n\"\n"
    );
    std::fs::write(&path, &basic).unwrap();
    resolve_team(&fx, &trust).unwrap();
    std::fs::write(
        &path,
        basic.replace("DELETE FROM orders;", "DELETE FROM orders ;"),
    )
    .unwrap();
    assert!(resolve_team(&fx, &trust).is_err());
}

#[test]
fn a_connection_trusted_before_the_session_table_stays_trusted_and_a_new_one_is_a_change() {
    let fx = Fixture::new("session-trust-old");
    let path = fx.write("repo/.spelaion/connections/orders.toml", BASE);
    let trust = fx.trusted();
    resolve_team(&fx, &trust).unwrap();
    // An empty table is no table.
    std::fs::write(&path, format!("{BASE}\n[session]\n")).unwrap();
    resolve_team(&fx, &trust).unwrap();
    // Adding a setup to a connection trusted without one needs a review.
    std::fs::write(
        &path,
        format!("{BASE}\n[session]\nsetup = \"SET ROLE admin\"\n"),
    )
    .unwrap();
    let fields = changed_fields(resolve_team(&fx, &trust));
    assert_eq!(
        fields,
        [(
            "session.setup".to_string(),
            None,
            Some("SET ROLE admin".to_string())
        )]
    );
}

#[test]
fn the_form_reads_and_writes_only_the_session_keys_it_changed() {
    let fx = Fixture::new("session-form");
    let path = fx.write(
        "user/connections/orders.toml",
        &format!("# the team's orders\n{BASE}\n[session]\n# needed by the reports\nschema = \"app\" # theirs\n"),
    );
    let config = fx.config();
    let entry = config.connection(&personal("orders")).unwrap();
    let mut draft = ConnectionDraft::from_entry(entry);
    assert_eq!(draft.session.schema, "app");
    assert_eq!(draft.session.setup, "");

    // Line breaks as Windows sends them, blank lines around: stored with `\n`, trimmed.
    draft.session = SessionDraft {
        setup: "\r\n  SET search_path TO app, public;\r\nSET TIME ZONE 'UTC';\r\n\r\n".into(),
        ignore_setup_errors: true,
        schema: "app".into(),
        isolation: Some(IsolationLevel::Serializable),
    };
    update_connection(
        &config,
        &personal("orders"),
        &draft,
        None,
        &FakeBackend::default(),
    )
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# needed by the reports"), "{text}");
    assert!(text.contains("schema = \"app\" # theirs"), "{text}");
    assert!(text.contains("isolation = \"serializable\""), "{text}");
    assert!(text.contains("ignore_setup_errors = true"), "{text}");
    assert!(!text.contains('\r'), "{text}");
    let config = fx.config();
    let file = &config.connection(&personal("orders")).unwrap().file;
    assert_eq!(
        file.session.setup.as_deref(),
        Some("SET search_path TO app, public;\nSET TIME ZONE 'UTC';")
    );

    // Clearing everything removes the table; "ignore errors" goes with the setup.
    let mut draft = ConnectionDraft::from_entry(config.connection(&personal("orders")).unwrap());
    draft.session = SessionDraft {
        setup: "  \n".into(),
        ignore_setup_errors: true,
        ..SessionDraft::default()
    };
    update_connection(
        &config,
        &personal("orders"),
        &draft,
        None,
        &FakeBackend::default(),
    )
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("[session]"), "{text}");
    assert!(!text.contains("ignore_setup_errors"), "{text}");
    assert!(fx.config().problems.is_empty());
}

#[test]
fn pasted_text_never_brings_setup_sql_along() {
    let fx = Fixture::new("session-paste");
    let backend = FakeBackend::default();
    // A connection someone sends: its setup would run on the user's server at every open.
    let text = format!(
        "{BASE}\n[session]\nsetup = \"DROP TABLE orders\"\nignore_setup_errors = true\nschema = \"app\"\nisolation = \"read-committed\"\n"
    );
    let pasted = paste_connection(&fx.config(), &text, &backend).unwrap();
    assert!(
        pasted.left_out.contains(&"session.setup".to_string()),
        "{:?}",
        pasted.left_out
    );
    let config = fx.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let file = &config.connection(&pasted.id).unwrap().file;
    assert_eq!(file.session.setup, None);
    assert!(!file.session.ignore_setup_errors);
    // Choosing settings is harmless: kept.
    assert_eq!(file.session.schema.as_deref(), Some("app"));
    assert_eq!(file.session.isolation, Some(IsolationLevel::ReadCommitted));

    // Written another way (a sub-table, an inline table), it is still left out.
    for text in [
        format!("{BASE}session.setup = \"DROP TABLE orders\"\n"),
        format!("{BASE}session = {{ setup = \"DROP TABLE orders\" }}\n"),
    ] {
        let pasted = paste_connection(&fx.config(), &text, &backend).unwrap();
        let config = fx.config();
        assert_eq!(
            config.connection(&pasted.id).unwrap().file.session.setup,
            None
        );
    }

    // Copying a connection with a setup leaves it out of the clipboard text too.
    let original = config.connection(&pasted.id).unwrap().id.clone();
    fx.write(
        "user/connections/with-setup.toml",
        &format!("{BASE}\n[session]\nsetup = \"SET ROLE reporting\"\n"),
    );
    let config = fx.config();
    let copied =
        connection_clipboard_text(&config, &fx.untrusted(), &personal("with-setup"), &backend)
            .unwrap();
    assert!(!copied.text.contains("SET ROLE"), "{}", copied.text);
    assert!(copied.left_out.contains(&"session.setup".to_string()));
    let _ = original;
}

#[test]
fn changing_the_engine_drops_what_the_new_engine_cannot_set() {
    let fx = Fixture::new("session-engine");
    fx.write("user/connections/orders.toml", &format!("{BASE}{SESSION}"));
    let config = fx.config();
    let draft = ConnectionDraft::from_entry(config.connection(&personal("orders")).unwrap());

    let (mysql, report) = draft.with_engine(Engine::Mysql);
    assert_eq!(mysql.session, draft.session);
    for kept in ["session.setup", "session.schema", "session.isolation"] {
        assert!(report.kept.contains(&kept.to_string()), "{report:?}");
    }

    let (sqlserver, report) = draft.with_engine(Engine::Sqlserver);
    assert_eq!(sqlserver.session.schema, "");
    assert_eq!(sqlserver.session.setup, draft.session.setup);
    assert!(report.dropped.contains(&"session.schema".to_string()));
    assert!(report.kept.contains(&"session.isolation".to_string()));

    let (sqlite, report) = draft.with_engine(Engine::Sqlite);
    assert_eq!(sqlite.session.schema, "");
    assert_eq!(sqlite.session.isolation, None);
    assert!(report.dropped.contains(&"session.isolation".to_string()));

    let mut snapshot = draft.clone();
    snapshot.engine = Engine::Sqlserver;
    snapshot.session.schema.clear();
    snapshot.session.isolation = Some(IsolationLevel::Snapshot);
    let (postgres, report) = snapshot.with_engine(Engine::Postgres);
    assert_eq!(postgres.session.isolation, None);
    assert!(report.dropped.contains(&"session.isolation".to_string()));
}
