//! Changing a connection's engine keeping its settings (ADR-030), with temporary directories
//! (the real keychain is never touched).

mod common;

use common::{BASTION, FakeBackend, Fixture, personal, team};
use spelaion_config::{
    ConnectionDraft, Engine, PasswordChoice, TlsModeConfig, TrustStore, connection_revision,
    update_connection, update_connection_at_revision,
};

const ENGINES: [Engine; 6] = [
    Engine::Postgres,
    Engine::Mysql,
    Engine::Mariadb,
    Engine::Sqlserver,
    Engine::Sqlite,
    Engine::Duckdb,
];

/// A form as the user would have it, from a file (so settings added later need no change).
fn draft_of(fx: &Fixture, id: &str, text: &str) -> ConnectionDraft {
    fx.write(&format!("user/connections/{id}.toml"), text);
    let config = fx.config();
    ConnectionDraft::from_entry(config.connection(&personal(id)).expect(id))
}

fn server_text(engine: Engine) -> String {
    format!(
        "schema_version = 1\nname = \"Main\"\nengine = \"{}\"\nenv = \"staging\"\nhost = \"db.internal\"\n\
         database = \"app\"\nuser = \"app\"\npassword = {{ secret = \"keychain\" }}\nread_only = true\nkeepalive = 60\n\
         [tls]\nmode = \"require\"\n[mcp]\nexposed = true\nwrites = \"allow\"\n",
        engine.as_str()
    )
}

#[test]
fn every_engine_pair_maps_to_a_connection_the_core_accepts() {
    let fx = Fixture::new("engine-pairs");
    fx.write("user/tunnels/bastion.toml", BASTION);
    for from in ENGINES {
        let text = if from.is_file() {
            format!(
                "schema_version = 1\nname = \"Main\"\nengine = \"{}\"\nenv = \"staging\"\ndatabase = \"data/app.db\"\nread_only = true\n",
                from.as_str()
            )
        } else {
            server_text(from)
        };
        let id = format!("pair-{}", from.as_str());
        let draft = draft_of(&fx, &id, &text);
        for to in ENGINES {
            let (next, change) = draft.with_engine(to);
            assert_eq!((change.from, change.to, next.engine), (from, to, to));
            // Untouched: the name, the folder, the environment, read-only, agents' access.
            assert_eq!((next.name.as_str(), next.env.as_str()), ("Main", "staging"));
            assert!(next.read_only);
            if from == to {
                assert_eq!(next, draft);
                assert!(change.kept.is_empty() && change.changed.is_empty());
                continue;
            }
            assert!(change.kept.contains(&"env".to_string()));
            assert!(change.kept.contains(&"read_only".to_string()));
            let field_changed = |f: &str| change.changed.iter().any(|c| c.field == f);
            match (from.is_file(), to.is_file()) {
                (false, false) => {
                    for field in [
                        "host",
                        "database",
                        "user",
                        "password",
                        "tls.mode",
                        "keepalive",
                    ] {
                        assert!(
                            change.kept.contains(&field.to_string()),
                            "{from:?}→{to:?} {field}"
                        );
                    }
                    assert_eq!(next.host, draft.host);
                    assert_eq!(next.database, draft.database);
                    assert_eq!(next.password, draft.password);
                    assert_eq!(next.tls_mode, draft.tls_mode);
                    assert_eq!(next.mcp_writes, draft.mcp_writes);
                    assert_eq!(next.port, None, "the old default becomes the new default");
                    assert_eq!(
                        field_changed("port"),
                        from.default_port() != to.default_port(),
                        "{from:?}→{to:?}"
                    );
                    assert!(
                        change.required.is_empty(),
                        "{from:?}→{to:?} {:?}",
                        change.required
                    );
                }
                (false, true) => {
                    assert!(
                        next.host.is_empty() && next.user.is_empty() && next.database.is_empty()
                    );
                    assert_eq!(next.password, PasswordChoice::None);
                    assert_eq!(
                        (next.keepalive, next.tls_mode),
                        (None, TlsModeConfig::default())
                    );
                    for field in ["host", "user", "password", "keepalive", "tls.mode"] {
                        assert!(
                            change.dropped.contains(&field.to_string()),
                            "{from:?}→{to:?} {field}"
                        );
                    }
                    assert!(field_changed("database"));
                    assert_eq!(change.required, ["database"]);
                }
                (true, false) => {
                    assert!(next.database.is_empty() && field_changed("database"));
                    assert_eq!(next.password, PasswordChoice::Keychain);
                    assert_eq!(next.keepalive, Some(300));
                    let mut required = vec!["host", "user"];
                    if to == Engine::Postgres {
                        required.push("database");
                    }
                    assert_eq!(change.required, required, "{from:?}→{to:?}");
                }
                (true, true) => {
                    assert_eq!(next.database, draft.database);
                    assert!(change.kept.contains(&"database".to_string()));
                    assert!(change.required.is_empty());
                }
            }
            // Filling in what is required is all it takes for the core to accept it.
            let mut filled = next.clone();
            for field in &change.required {
                match field.as_str() {
                    "host" => filled.host = "db.internal".into(),
                    "user" => filled.user = "app".into(),
                    "database" => filled.database = "data/other.db".into(),
                    other => panic!("unexpected required field {other}"),
                }
            }
            let config = fx.config();
            filled
                .to_file(&config)
                .unwrap_or_else(|e| panic!("{from:?}→{to:?}: {e}"));
        }
    }
}

#[test]
fn engine_changes_keep_a_port_of_its_own_and_drop_what_does_not_apply() {
    let fx = Fixture::new("engine-rules");
    fx.write("user/tunnels/bastion.toml", BASTION);
    let draft = draft_of(
        &fx,
        "pg",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\nport = 6543\ndatabase = \"d\"\nuser = \"u\"\n\
         password = { secret = \"cmd:aws rds generate-db-auth-token --hostname h --port 6543 --username u\" }\n\
         tunnel = \"bastion\"\nlocal_port = 16543\n[tls]\nmode = \"verify-ca\"\nca = \"ca.pem\"\ncert = \"~/c.pem\"\nkey = \"~/k.pem\"\n",
    );
    assert!(matches!(draft.password, PasswordChoice::CloudToken(_)));

    // A port of its own stays; a client certificate and the cloud token go for SQL Server.
    let (next, change) = draft.with_engine(Engine::Sqlserver);
    assert_eq!(next.port, Some(6543));
    assert!(change.kept.contains(&"port".to_string()));
    assert_eq!((next.tls_cert.as_str(), next.tls_key.as_str()), ("", ""));
    assert_eq!(change.dropped, ["tls.cert"]);
    assert_eq!(next.password, PasswordChoice::Prompt);
    let password = change
        .changed
        .iter()
        .find(|c| c.field == "password")
        .unwrap();
    assert_eq!(
        (password.before.as_deref(), password.after.as_deref()),
        (Some("aws-rds"), Some("prompt"))
    );
    assert_eq!(
        (next.tunnel.as_str(), next.local_port),
        ("bastion", Some(16543))
    );
    assert_eq!(next.tls_ca, "ca.pem");
    next.to_file(&fx.config()).unwrap();

    // MySQL takes both.
    let (next, change) = draft.with_engine(Engine::Mysql);
    assert!(change.dropped.is_empty());
    assert!(matches!(next.password, PasswordChoice::CloudToken(_)));
    assert_eq!(next.tls_cert, "~/c.pem");

    // SQLite on a server: the path there means nothing to DuckDB.
    let remote = draft_of(
        &fx,
        "remote",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/srv/app.db\"\ntunnel = \"bastion\"\nsqlite3 = \"/opt/sqlite3\"\n",
    );
    let (next, change) = remote.with_engine(Engine::Duckdb);
    assert_eq!(
        (
            next.database.as_str(),
            next.tunnel.as_str(),
            next.sqlite3.as_str()
        ),
        ("", "", "")
    );
    assert_eq!(change.dropped, ["tunnel", "sqlite3"]);
    assert_eq!(change.required, ["database"]);
    // To a server, the tunnel would mean something else: dropped too.
    let (next, change) = remote.with_engine(Engine::Postgres);
    assert_eq!((next.tunnel.as_str(), next.sqlite3.as_str()), ("", ""));
    assert_eq!(change.dropped, ["tunnel", "sqlite3"]);

    // Kubernetes stays between servers (the host is then optional) and goes for a file.
    let kube = draft_of(
        &fx,
        "kube",
        "schema_version = 1\nengine = \"mysql\"\nuser = \"u\"\n[kubernetes]\nresource = \"svc/mysql\"\n",
    );
    let (next, change) = kube.with_engine(Engine::Mariadb);
    assert!(next.kubernetes.is_some() && change.required.is_empty());
    assert!(
        change.changed.is_empty(),
        "MySQL and MariaDB share the default port"
    );
    let (next, change) = kube.with_engine(Engine::Sqlite);
    assert!(next.kubernetes.is_none());
    assert!(change.dropped.contains(&"kubernetes".to_string()));
}

#[test]
fn saving_an_engine_change_rewrites_only_what_changed() {
    let fx = Fixture::new("engine-save");
    let text = "#:schema https://spelaion.dev/schema/db/connection.schema.json\n\n\
                # Reporting replica.\nschema_version = 1\nname = \"Reports\"\n\
                engine = \"mysql\" # the old one\nhost = \"reports.internal\" # replica\n\
                database = \"sales\"\nuser = \"reader\"\npassword = { secret = \"prompt\" }\n\n\
                [mcp]\nexposed = true\nmax_rows = 50\n";
    let draft = draft_of(&fx, "reports", text);
    let backend = FakeBackend::default();
    let (next, _) = draft.with_engine(Engine::Mariadb);
    let config = fx.config();
    let revision = connection_revision(config.connection(&personal("reports")).unwrap()).unwrap();
    update_connection_at_revision(
        &config,
        &personal("reports"),
        &next,
        None,
        &backend,
        &revision,
    )
    .unwrap();
    assert_eq!(
        fx.read("user/connections/reports.toml"),
        text.replace("engine = \"mysql\"", "engine = \"mariadb\"")
    );

    // To PostgreSQL: the implied port changes with the engine, nothing is written for it.
    let config = fx.config();
    let draft = ConnectionDraft::from_entry(config.connection(&personal("reports")).unwrap());
    let (next, _) = draft.with_engine(Engine::Postgres);
    update_connection(&config, &personal("reports"), &next, None, &backend).unwrap();
    let saved = fx.read("user/connections/reports.toml");
    assert!(
        saved.contains("engine = \"postgres\" # the old one"),
        "{saved}"
    );
    assert!(!saved.contains("port ="), "{saved}");
    assert!(saved.contains("max_rows = 50"));

    // To SQLite: the server keys go, the database must be filled in first.
    let config = fx.config();
    let draft = ConnectionDraft::from_entry(config.connection(&personal("reports")).unwrap());
    let (mut next, change) = draft.with_engine(Engine::Sqlite);
    assert_eq!(change.required, ["database"]);
    let err = update_connection(&config, &personal("reports"), &next, None, &backend).unwrap_err();
    assert!(err.to_string().contains("missing: database"), "{err}");
    assert_eq!(
        fx.read("user/connections/reports.toml"),
        saved,
        "a refused change writes nothing"
    );
    next.database = "data/reports.db".into();
    update_connection(&config, &personal("reports"), &next, None, &backend).unwrap();
    let saved = fx.read("user/connections/reports.toml");
    for gone in ["host", "user", "password"] {
        assert!(!saved.contains(&format!("{gone} =")), "{gone}: {saved}");
    }
    assert!(saved.contains("# Reporting replica.") && saved.contains("max_rows = 50"));
    let config = fx.config();
    assert_eq!(
        config.connection(&personal("reports")).unwrap().file.engine,
        Engine::Sqlite
    );
}

#[test]
fn a_team_engine_change_goes_through_trust_like_any_edit() {
    let fx = Fixture::new("engine-team");
    fx.write(
        "repo/.spelaion/connections/db.toml",
        &server_text(Engine::Mysql),
    );
    let backend = FakeBackend::default();
    let mut trust = fx.trusted();
    let config = fx.config();
    let draft = ConnectionDraft::from_entry(config.connection(&team("db")).unwrap());
    let (next, _) = draft.with_engine(Engine::Mariadb);
    let written = update_connection(&config, &team("db"), &next, None, &backend).unwrap();
    assert_eq!(written.engine, Engine::Mariadb);
    trust
        .accept_connection_edit(&config, &team("db"), &written)
        .unwrap();
    assert!(
        TrustStore::load(&fx.user())
            .unwrap()
            .changes(&fx.config())
            .is_empty()
    );

    // The same change arriving by a pull is shown before the connection is used.
    let text = std::fs::read_to_string(fx.repo().join(".spelaion/connections/db.toml")).unwrap();
    std::fs::write(
        fx.repo().join(".spelaion/connections/db.toml"),
        text.replace("mariadb", "postgres"),
    )
    .unwrap();
    let changes = TrustStore::load(&fx.user()).unwrap().changes(&fx.config());
    assert_eq!(changes.len(), 1);
    assert!(changes[0].fields.iter().any(|f| f.field == "engine"));
}
