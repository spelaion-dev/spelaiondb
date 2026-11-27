//! A session made from another one (SDB-426): after the user saves an edit, the source
//! session's resolution is the connection as it was, so the next session is opened from the
//! saved file instead; the typed password goes only to the same login on the same route.
//! Needs nothing external (SQLite is compiled in; the server connections are only resolved).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use spelaion_config::{
    ConnectionDraft, ConnectionId, ResolvedConnection, Resolver, Source, SystemBackend, TrustStore,
    load, update_connection,
};
use spelaion_core::session_source::{SourceSession, carried_password, source_session};
use spelaion_core::{Engines, OpenOptions, ResultStore, TunnelPool, open_connection};
use spelaion_driver_api::Secret;
use spelaion_ssh::{KnownHosts, NonInteractive};

struct Folder {
    base: PathBuf,
}

impl Folder {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "spelaion-session-source-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("user/connections")).unwrap();
        std::fs::create_dir_all(base.join("user/tunnels")).unwrap();
        Self { base }
    }

    fn user(&self) -> PathBuf {
        self.base.join("user")
    }

    fn write(&self, relative: &str, content: &str) {
        std::fs::write(self.user().join(relative), content).unwrap();
    }

    fn config(&self) -> spelaion_config::Config {
        let config = load(None, &self.user());
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        config
    }

    fn resolve(&self, config: &spelaion_config::Config, id: &ConnectionId) -> ResolvedConnection {
        let trust = TrustStore::load(&self.user()).unwrap();
        Resolver::new(config, &trust, &SystemBackend)
            .resolve(id)
            .expect("resolving")
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn personal(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::User,
        path: path.into(),
    }
}

#[tokio::test]
async fn after_a_saved_edit_a_session_made_from_an_open_one_uses_the_saved_file() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("edit");
    let db = folder
        .base
        .join("app.db")
        .display()
        .to_string()
        .replace('\\', "/");
    // An empty file is an empty SQLite database.
    std::fs::write(folder.base.join("app.db"), b"").unwrap();
    folder.write(
        "connections/local.toml",
        &format!(
            "schema_version = 1\nname = \"Local\"\nengine = \"sqlite\"\ndatabase = \"{db}\"\nstatement_timeout = 5\n"
        ),
    );
    let id = personal("local");
    let config = folder.config();
    let opened = config.connection_definition(&id, false).unwrap();
    let source = open_connection(
        folder.resolve(&config, &id),
        &Engines::builtin(),
        OpenOptions {
            tunnels: Arc::new(TunnelPool::new()),
            known_hosts: KnownHosts {
                team_files: Vec::new(),
                user_file: None,
                app_file: folder.user().join("known_hosts"),
            },
            interaction: Arc::new(NonInteractive),
            password: None,
            read_only_session: false,
        },
        Arc::new(ResultStore::new()),
    )
    .await
    .expect("opening");

    // Nothing changed, or only another connection: the source's resolution is still current.
    assert_eq!(
        source_session(&opened, config.connection_definition(&id, false).as_ref()),
        SourceSession::Current
    );
    folder.write(
        "connections/other.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"other.db\"\n",
    );
    let config = folder.config();
    assert_eq!(
        source_session(&opened, config.connection_definition(&id, false).as_ref()),
        SourceSession::Current
    );

    // The user saves an edit. The open session still holds the connection as it was: that
    // is what a session made from it reused before SDB-426.
    let edited = ConnectionDraft {
        statement_timeout: Some(9),
        ..ConnectionDraft::from_entry(config.connection(&id).unwrap())
    };
    update_connection(&config, &id, &edited, None, &SystemBackend).unwrap();
    let config = folder.config();
    assert_eq!(
        source.resolved.statement_timeout,
        Some(Duration::from_secs(5))
    );
    let now = config.connection_definition(&id, false);
    assert_eq!(
        source_session(&opened, now.as_ref()),
        SourceSession::Changed
    );
    assert_eq!(
        folder.resolve(&config, &id).statement_timeout,
        Some(Duration::from_secs(9)),
        "the saved file"
    );

    // What else a session is opened from: user.toml (masking, variables) and whether the
    // repository is trusted. A removed connection is never reused either.
    let opened = now.unwrap();
    folder.write(
        "user.toml",
        "schema_version = 1\n[masking]\nalways = [\"*secret*\"]\n",
    );
    let config = folder.config();
    assert_eq!(
        source_session(&opened, config.connection_definition(&id, false).as_ref()),
        SourceSession::Changed
    );
    let opened = config.connection_definition(&id, false).unwrap();
    assert_eq!(
        source_session(&opened, config.connection_definition(&id, true).as_ref()),
        SourceSession::Changed
    );
    std::fs::remove_file(folder.user().join("connections/local.toml")).unwrap();
    let config = folder.config();
    assert_eq!(
        source_session(&opened, config.connection_definition(&id, false).as_ref()),
        SourceSession::Changed
    );
    source.close().await;
}

#[test]
fn the_typed_password_goes_only_to_the_same_login_on_the_same_route() {
    let _turn = crate::serial::file_turn(module_path!());
    let folder = Folder::new("password");
    folder.write(
        "tunnels/jump.toml",
        "schema_version = 1\n[[hops]]\nhost = \"jump.internal\"\nuser = \"ops\"\nauth = [\"agent\"]\n",
    );
    folder.write(
        "tunnels/other.toml",
        "schema_version = 1\n[[hops]]\nhost = \"elsewhere.example\"\nuser = \"ops\"\nauth = [\"agent\"]\n",
    );
    let base = "schema_version = 1\nengine = \"postgres\"\nhost = \"db.internal\"\ndatabase = \"app\"\nuser = \"app\"\npassword = { secret = \"prompt\" }\ntunnel = \"jump\"\n";
    let resolved = |text: &str| {
        folder.write("connections/pg.toml", text);
        let config = folder.config();
        folder.resolve(&config, &personal("pg"))
    };
    let old = resolved(base);
    let typed = Secret::new("typed-for-db.internal");

    // A setting that does not change where it logs in keeps the typed password.
    let same = resolved(&format!("{base}statement_timeout = 30\n"));
    assert_eq!(
        carried_password(&old, &same, Some(&typed)).map(|s| s.expose().to_string()),
        Some("typed-for-db.internal".into())
    );
    // Nothing typed, nothing carried.
    assert!(carried_password(&old, &same, None).is_none());

    // Another server, port, user, network profile or engine: asked again, never sent there.
    for (what, text) in [
        ("host", base.replace("db.internal", "evil.example")),
        ("port", format!("{base}port = 6543\n")),
        ("user", base.replace("user = \"app\"", "user = \"admin\"")),
        (
            "tunnel",
            base.replace("tunnel = \"jump\"", "tunnel = \"other\""),
        ),
        ("no tunnel", base.replace("tunnel = \"jump\"\n", "")),
        (
            "kubernetes",
            base.replace(
                "tunnel = \"jump\"\n",
                "[kubernetes]\nresource = \"svc/db\"\n",
            ),
        ),
        (
            "engine",
            base.replace("\"postgres\"", "\"mysql\"")
                .replace("database = \"app\"\n", ""),
        ),
    ] {
        let new = resolved(&text);
        assert!(
            carried_password(&old, &new, Some(&typed)).is_none(),
            "{what}: the typed password went to a changed login"
        );
    }
    // A password that now comes from somewhere else (an environment variable here, one that is always set) is that
    // one, never the typed one.
    let from_env = resolved(&base.replace("{ secret = \"prompt\" }", "{ secret = \"env:PATH\" }"));
    assert!(carried_password(&old, &from_env, Some(&typed)).is_none());
}

/// Several folders open (ADR-075, SDB-526): a personal connection's session is not reused
/// once a folder it reads changed (here folder B's types, which tighten it), while folder A's
/// connection, which never reads B, keeps reusing its own.
#[test]
fn a_change_in_another_open_folder_stops_reusing_a_personal_connections_session() {
    let _turn = crate::serial::file_turn(module_path!());
    let base = std::env::temp_dir().join(format!(
        "spelaion-session-source-folders-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let write = |relative: &str, content: &str| {
        let path = base.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    let sqlite = |env: &str| {
        format!("schema_version = 1\nengine = \"sqlite\"\nenv = \"{env}\"\ndatabase = \"app.db\"\n")
    };
    write("user/connections/mine.toml", &sqlite("prod"));
    write("a/.spelaion/connections/orders.toml", &sqlite("prod"));
    write(
        "b/.spelaion/connection-types.toml",
        "schema_version = 1\n\n[types.prod]\nconfirm_execution = true\n",
    );
    let user = base.join("user");
    let definition = |id: &ConnectionId| {
        let a = load(Some(&base.join("a")), &user);
        let b = load(Some(&base.join("b")), &user);
        a.connection_definition_open(id, true, &[(&a, true), (&b, true)])
    };
    let mine = personal("mine");
    let orders = ConnectionId {
        source: Source::Workspace,
        path: "orders".into(),
    };
    let (opened_mine, opened_orders) = (definition(&mine).unwrap(), definition(&orders).unwrap());
    assert_eq!(
        source_session(&opened_mine, definition(&mine).as_ref()),
        SourceSession::Current
    );

    write(
        "b/.spelaion/connection-types.toml",
        "schema_version = 1\n\n[types.prod]\nconfirm_execution = true\nmanual_commit = true\n",
    );
    assert_eq!(
        source_session(&opened_mine, definition(&mine).as_ref()),
        SourceSession::Changed
    );
    assert_eq!(
        source_session(&opened_orders, definition(&orders).as_ref()),
        SourceSession::Current
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// SDB-527: a team change to the folder's `known_hosts` (a server's new key) stops a session of
/// a tunnelled connection from being made from one opened before: it is opened from the saved
/// configuration, which goes through ADR-022's review. A connection without a tunnel keeps
/// reusing its source's.
#[test]
fn a_changed_team_known_hosts_stops_reusing_a_tunnelled_connections_session() {
    let _turn = crate::serial::file_turn(module_path!());
    let base = std::env::temp_dir().join(format!(
        "spelaion-session-source-known-hosts-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let write = |relative: &str, content: &str| {
        let path = base.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };
    write(
        "repo/.spelaion/tunnels/bastion.toml",
        "schema_version = 1\n\n[[hops]]\nhost = \"bastion.internal\"\nuser = \"ops\"\nauth = [\"agent\"]\n",
    );
    write(
        "repo/.spelaion/connections/orders.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"db.internal\"\ndatabase = \"orders\"\nuser = \"app\"\ntunnel = \"bastion\"\n",
    );
    write(
        "repo/.spelaion/connections/local.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"app.db\"\n",
    );
    write(
        "repo/.spelaion/known_hosts",
        "bastion.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC0Cx5aAdFPmIIPx6ZxdIYFKbARZbBbB5NV0QIO3Qdp2\n",
    );
    std::fs::create_dir_all(base.join("user")).unwrap();
    let definition = |path: &str| {
        let config = load(Some(&base.join("repo")), &base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let id = ConnectionId {
            source: Source::Workspace,
            path: path.into(),
        };
        config.connection_definition(&id, true)
    };
    let (orders, local) = (definition("orders").unwrap(), definition("local").unwrap());
    assert_eq!(
        source_session(&orders, definition("orders").as_ref()),
        SourceSession::Current
    );

    write(
        "repo/.spelaion/known_hosts",
        "bastion.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X\n",
    );
    assert_eq!(
        source_session(&orders, definition("orders").as_ref()),
        SourceSession::Changed
    );
    assert_eq!(
        source_session(&local, definition("local").as_ref()),
        SourceSession::Current
    );
    let _ = std::fs::remove_dir_all(&base);
}
