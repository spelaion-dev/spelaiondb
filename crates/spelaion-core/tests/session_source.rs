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
                team_file: None,
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
