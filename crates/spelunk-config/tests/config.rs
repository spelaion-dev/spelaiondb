//! Configuration tests with temporary directories and a fake secret backend
//! (the real keychain and environment are never touched).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use spelunk_config::{
    ConnectionDraft, ConnectionFile, ConnectionId, Engine, HopDraft, KubernetesDraft, Password,
    PasswordChoice, ResolveError, Resolver, SecretBackend, Source, SshAuth, SystemBackend,
    TlsModeConfig, TrustStore, TunnelDraft, WritePolicy, add_connection, add_tunnel,
    connection_account, create_connection, delete_tunnel, load, set_value, slug, tunnel_account,
    update_tunnel,
};
use spelunk_driver_api::{Secret, TlsMode};

/// Temporary directory with a repository and a user directory; deleted at the end.
struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelunk-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelunk")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn repo(&self) -> PathBuf {
        self.base.join("repo")
    }

    fn user(&self) -> PathBuf {
        self.base.join("user")
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.base.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn trusted(&self) -> TrustStore {
        let mut trust = TrustStore::load(&self.user()).unwrap();
        trust.trust(&self.repo()).unwrap();
        trust
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[derive(Default)]
struct FakeBackend {
    keychain: Mutex<BTreeMap<String, String>>,
    env: BTreeMap<String, String>,
    runs: Mutex<Vec<String>>,
    fail_keychain: bool,
}

impl SecretBackend for FakeBackend {
    fn keychain_get(&self, account: &str) -> Result<Option<String>, String> {
        Ok(self.keychain.lock().unwrap().get(account).cloned())
    }

    fn keychain_set(&self, account: &str, secret: &str) -> Result<(), String> {
        if self.fail_keychain {
            return Err("keychain locked".into());
        }
        self.keychain
            .lock()
            .unwrap()
            .insert(account.into(), secret.into());
        Ok(())
    }

    fn keychain_delete(&self, account: &str) -> Result<(), String> {
        if self.fail_keychain {
            return Err("keychain locked".into());
        }
        self.keychain.lock().unwrap().remove(account);
        Ok(())
    }

    fn env(&self, name: &str) -> Option<String> {
        self.env.get(name).cloned()
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.runs
            .lock()
            .unwrap()
            .push(format!("{program} {}", args.join(" ")));
        Ok(format!("output-of-{program}"))
    }

    fn run_shell(&self, command: &str) -> Result<String, String> {
        self.runs.lock().unwrap().push(format!("shell {command}"));
        Ok("output-of-shell".into())
    }
}

/// Keychain entry of a repository connection of the fixture.
fn repo_account(fx: &Fixture, path: &str) -> String {
    connection_account(&load(Some(&fx.repo()), &fx.user()), &ws(path))
}

/// Keychain entry of a hop (from 0) of a repository tunnel of the fixture.
fn repo_hop(fx: &Fixture, tunnel: &str, hop: usize) -> String {
    tunnel_account(
        &load(Some(&fx.repo()), &fx.user()),
        Source::Workspace,
        tunnel,
        hop,
    )
}

fn ws(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::Workspace,
        path: path.into(),
    }
}

const ORDERS: &str = r#"
schema_version = 1
name = "Orders (prod)"
engine = "postgres"
env = "prod"
host = "orders-${env:REGION}.internal"
database = "orders"
user = "${user.db_user}"
password = { secret = "keychain" }
tunnel = "bastion"

[tls]
mode = "verify-full"
ca = "certs/ca.pem"

[mcp]
exposed = true
max_rows = 500
writes = "allow"
"#;

const BASTION: &str = r#"
schema_version = 1

[[hops]]
host = "jump.example.com"
user = "${user.db_user}"

[[hops]]
host = "10.0.0.5"
port = 2222
user = "deploy"
auth = ["key", "password"]
identity_file = "~/.ssh/id_ed25519"
password = { secret = "prompt" }
"#;

#[test]
fn loads_both_layers_with_folders_and_ids() {
    let fx = Fixture::new("layers");
    fx.write("repo/.spelunk/connections/prod/orders.toml", ORDERS);
    fx.write(
        "repo/.spelunk/connections/local.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"data/app.db\"\n",
    );
    fx.write("repo/.spelunk/tunnels/bastion.toml", BASTION);
    fx.write(
        "user/connections/scratch.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \":memory:\"\n",
    );
    fx.write("user/user.toml", "[vars]\ndb_user = \"ana\"\n");
    std::fs::create_dir_all(fx.repo().join("src/deep")).unwrap();

    // The repository is found from a subdirectory, like git.
    let config = load(Some(&fx.repo().join("src/deep")), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(config.workspace_root.as_deref(), Some(fx.repo().as_path()));

    let ids: Vec<String> = config
        .connections
        .iter()
        .map(|c| c.id.to_string())
        .collect();
    assert_eq!(ids, ["local", "prod/orders", "~/scratch"]);
    let orders = config.connection(&ws("prod/orders")).unwrap();
    assert_eq!(orders.name(), "Orders (prod)");
    assert_eq!(orders.folders(), ["prod"]);
    assert_eq!(orders.file.mcp.writes, WritePolicy::Allow);
    assert_eq!(config.user.vars["db_user"], "ana");
    assert_eq!(config.tunnels.len(), 1);
}

#[test]
fn broken_files_are_reported_with_position_and_do_not_hide_others() {
    let fx = Fixture::new("broken");
    fx.write(
        "repo/.spelunk/connections/ok.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"a.db\"\n",
    );
    fx.write(
        "repo/.spelunk/connections/typo.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabse = \"a.db\"\n",
    );
    fx.write(
        "repo/.spelunk/connections/syntax.toml",
        "schema_version = 1\nengine = \n",
    );
    fx.write(
        "repo/.spelunk/connections/future.toml",
        "schema_version = 9\nengine = \"sqlite\"\ndatabase = \"a.db\"\n",
    );
    fx.write(
        "repo/.spelunk/connections/incomplete.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\n",
    );
    fx.write(
        "repo/.spelunk/connections/engine.toml",
        "schema_version = 1\nengine = \"oracle\"\n",
    );

    let config = load(Some(&fx.repo()), &fx.user());
    assert_eq!(config.connections.len(), 1);
    let problem = |file: &str| {
        config
            .problems
            .iter()
            .find(|p| p.file.ends_with(file))
            .unwrap_or_else(|| panic!("no problem for {file}: {:?}", config.problems))
    };
    let typo = problem("typo.toml");
    assert_eq!((typo.line, typo.column), (Some(3), Some(1)), "{typo}");
    assert!(typo.message.contains("databse"), "{typo}");
    assert_eq!(problem("syntax.toml").line, Some(2));
    assert!(problem("future.toml").message.contains("newer version"));
    assert!(
        problem("incomplete.toml")
            .message
            .contains("database, user")
    );
    assert!(problem("engine.toml").message.contains("oracle"));
}

#[test]
fn workspace_connections_need_trust_but_personal_ones_do_not() {
    let fx = Fixture::new("trust");
    fx.write(
        "repo/.spelunk/connections/db.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"a.db\"\n",
    );
    fx.write(
        "user/connections/mine.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"b.db\"\n",
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let backend = FakeBackend::default();

    let untrusted = TrustStore::load(&fx.user()).unwrap();
    let resolver = Resolver::new(&config, &untrusted, &backend);
    assert!(matches!(
        resolver.resolve(&ws("db")),
        Err(ResolveError::Untrusted { .. })
    ));
    let mine = resolver
        .resolve(&ConnectionId {
            source: Source::User,
            path: "mine".into(),
        })
        .unwrap();
    assert_eq!(Path::new(&mine.params.database), fx.user().join("b.db"));

    // Trust is stored and survives a reload.
    fx.trusted();
    let reloaded = TrustStore::load(&fx.user()).unwrap();
    assert!(reloaded.is_trusted(&fx.repo()));
    let resolved = Resolver::new(&config, &reloaded, &backend)
        .resolve(&ws("db"))
        .unwrap();
    assert_eq!(Path::new(&resolved.params.database), fx.repo().join("a.db"));
}

#[test]
fn resolves_variables_paths_tls_and_tunnels() {
    let fx = Fixture::new("resolve");
    fx.write("repo/.spelunk/connections/prod/orders.toml", ORDERS);
    fx.write("repo/.spelunk/tunnels/bastion.toml", BASTION);
    fx.write("user/user.toml", "[vars]\ndb_user = \"ana\"\n");
    let config = load(Some(&fx.repo()), &fx.user());
    let trust = fx.trusted();
    let backend = FakeBackend {
        env: BTreeMap::from([("REGION".into(), "eu".into())]),
        ..Default::default()
    };
    let r = Resolver::new(&config, &trust, &backend)
        .resolve(&ws("prod/orders"))
        .unwrap();

    assert_eq!(r.engine, Engine::Postgres);
    assert!(r.is_production());
    assert_eq!(r.params.host, "orders-eu.internal");
    assert_eq!(r.params.port, 5432);
    assert_eq!(r.params.user, "ana");
    assert_eq!(r.params.tls, TlsMode::VerifyFull);
    assert_eq!(
        r.params.ca_certificate.as_deref(),
        Some(fx.repo().join("certs/ca.pem").as_path())
    );

    let tunnel = r.tunnel.expect("tunnel");
    assert_eq!(tunnel.hops.len(), 2);
    assert_eq!(
        (
            tunnel.hops[0].host.as_str(),
            tunnel.hops[0].port,
            tunnel.hops[0].user.as_str()
        ),
        ("jump.example.com", 22, "ana")
    );
    assert_eq!(tunnel.hops[0].auth, [SshAuth::Agent]);
    assert_eq!(tunnel.hops[1].port, 2222);
    assert_eq!(tunnel.hops[1].auth, [SshAuth::Key, SshAuth::Password]);
    assert!(
        tunnel.hops[1]
            .identity_file
            .as_ref()
            .unwrap()
            .ends_with(".ssh/id_ed25519")
    );
    assert_eq!(tunnel.hops[1].password, Password::Prompt { save_to: None });
}

#[test]
fn missing_variables_point_at_the_file() {
    let fx = Fixture::new("missing-var");
    let file = fx.write("repo/.spelunk/connections/prod/orders.toml", ORDERS);
    fx.write("repo/.spelunk/tunnels/bastion.toml", BASTION);
    let config = load(Some(&fx.repo()), &fx.user());
    let trust = fx.trusted();
    let backend = FakeBackend {
        env: BTreeMap::from([("REGION".into(), "eu".into())]),
        ..Default::default()
    };
    match Resolver::new(&config, &trust, &backend).resolve(&ws("prod/orders")) {
        Err(ResolveError::Config(e)) => {
            assert_eq!(e.file, file);
            assert!(
                e.message.contains("db_user") && e.message.contains("[vars]"),
                "{e}"
            );
        }
        other => panic!("expected a variable error: {other:?}"),
    }
}

fn with_password(fx: &Fixture, secret: &str) -> spelunk_config::Config {
    fx.write(
        "repo/.spelunk/connections/db.toml",
        &format!("schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\npassword = {{ secret = \"{secret}\" }}\n"),
    );
    load(Some(&fx.repo()), &fx.user())
}

#[test]
fn secrets_resolve_from_each_source() {
    let fx = Fixture::new("secrets");
    let trust = fx.trusted();
    let backend = FakeBackend {
        env: BTreeMap::from([("PGPASS".into(), "from-env".into())]),
        ..Default::default()
    };
    let password = |secret: &str| {
        let config = with_password(&fx, secret);
        Resolver::new(&config, &trust, &backend)
            .resolve(&ws("db"))
            .map(|r| r.password)
    };

    assert_eq!(
        password("env:PGPASS").unwrap(),
        Password::Value(Secret::new("from-env"))
    );
    assert_eq!(
        password("op://Infra/db/password").unwrap(),
        Password::Value(Secret::new("output-of-op"))
    );
    assert_eq!(
        password("cmd:pass show db").unwrap(),
        Password::Value(Secret::new("output-of-shell"))
    );
    assert_eq!(
        password("prompt").unwrap(),
        Password::Prompt { save_to: None }
    );
    assert!(matches!(
        password("env:MISSING"),
        Err(ResolveError::Secret { .. })
    ));
    assert!(matches!(
        password("vault://x"),
        Err(ResolveError::Config(_))
    ));

    let runs = backend.runs.lock().unwrap().clone();
    assert_eq!(runs[0], "op read op://Infra/db/password");
    assert_eq!(runs[1], "shell pass show db");
}

#[test]
fn keychain_asks_the_first_time_and_remembers() {
    let fx = Fixture::new("keychain");
    let trust = fx.trusted();
    let backend = FakeBackend::default();
    let config = with_password(&fx, "keychain");
    let resolver = Resolver::new(&config, &trust, &backend);

    let first = resolver.resolve(&ws("db")).unwrap();
    let Password::Prompt {
        save_to: Some(account),
    } = &first.password
    else {
        panic!("the first time it must ask: {:?}", first.password)
    };
    assert_eq!(account, &repo_account(&fx, "db"));
    // Without remembering, the typed password is only used for this connection.
    assert_eq!(
        first.connect_params(Some(Secret::new("typed"))).password,
        Some(Secret::new("typed"))
    );

    resolver
        .remember_password(account, &Secret::new("stored"))
        .unwrap();
    let second = resolver.resolve(&ws("db")).unwrap();
    assert_eq!(second.password, Password::Value(Secret::new("stored")));
    assert_eq!(
        second.connect_params(None).password,
        Some(Secret::new("stored"))
    );
}

#[test]
fn edits_keep_comments_and_layout() {
    let fx = Fixture::new("edit");
    let original = "# Orders database: do not touch on Fridays\nschema_version = 1\nengine = \"postgres\" # engine\nhost = \"old-host\" # change after the migration\ndatabase = \"orders\"\nuser = \"app\"\n";
    let file = fx.write("repo/.spelunk/connections/orders.toml", original);

    set_value(&file, &["host"], "new-host").unwrap();
    set_value(&file, &["tls", "mode"], "verify-full").unwrap();
    let edited = std::fs::read_to_string(&file).unwrap();
    assert!(
        edited.starts_with("# Orders database: do not touch on Fridays\n"),
        "{edited}"
    );
    assert!(
        edited.contains("host = \"new-host\" # change after the migration"),
        "{edited}"
    );
    assert!(
        edited.contains("engine = \"postgres\" # engine"),
        "{edited}"
    );
    assert!(edited.contains("[tls]\nmode = \"verify-full\""), "{edited}");

    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(config.connections[0].file.host.as_deref(), Some("new-host"));

    spelunk_config::remove_value(&file, &["tls", "mode"]).unwrap();
    assert!(
        !std::fs::read_to_string(&file)
            .unwrap()
            .contains("verify-full")
    );
}

#[test]
fn creates_new_connection_files() {
    let fx = Fixture::new("create");
    let dir = fx.repo().join(".spelunk/connections");
    let file = ConnectionFile {
        schema_version: 1,
        name: Some("Analytics".into()),
        engine: Engine::Sqlite,
        env: None,
        host: None,
        port: None,
        database: Some("data/analytics.db".into()),
        user: None,
        password: None,
        tunnel: None,
        kubernetes: None,
        sqlite3: None,
        read_only: true,
        tls: Default::default(),
        mcp: Default::default(),
    };
    let path = create_connection(&dir, "team/analytics", &file).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.starts_with("#:schema https://"), "{content}");
    assert!(
        !content.contains("[tls]") && !content.contains("[mcp]"),
        "default values are not written:\n{content}"
    );

    let config = load(Some(&fx.repo()), &fx.user());
    assert_eq!(config.connections[0].id.to_string(), "team/analytics");
    assert_eq!(config.connections[0].file, file);

    assert!(
        create_connection(&dir, "team/analytics", &file).is_err(),
        "must not overwrite"
    );
    assert!(
        create_connection(&dir, "../outside", &file).is_err(),
        "must not escape the directory"
    );
}

#[test]
fn json_schemas_are_up_to_date() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema");
    let update = std::env::var("SPELUNK_UPDATE_SCHEMA").is_ok();
    for (name, schema) in spelunk_config::json_schemas() {
        let path = dir.join(format!("{name}.schema.json"));
        let expected = serde_json::to_string_pretty(&schema).unwrap() + "\n";
        if update {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(&path, &expected).unwrap();
        } else {
            let actual = std::fs::read_to_string(&path)
                .unwrap_or_default()
                .replace("\r\n", "\n");
            assert_eq!(
                actual,
                expected,
                "{} is out of date: run SPELUNK_UPDATE_SCHEMA=1 cargo test -p spelunk-config",
                path.display()
            );
        }
    }
}

/// The real keychain of this machine (Credential Manager, Keychain or Secret Service). It does
/// not run by default because it writes to the user's keychain and CI usually has none:
/// `cargo test -p spelunk-config -- --ignored`. Creates a test entry and deletes it.
#[test]
#[ignore]
fn system_keychain_round_trip() {
    let account = format!("selftest/{}", std::process::id());
    SystemBackend
        .keychain_set(&account, "test-value")
        .expect("storing in the keychain");
    let read = SystemBackend.keychain_get(&account);
    SystemBackend
        .keychain_delete(&account)
        .expect("deleting the entry");
    assert_eq!(read.unwrap().as_deref(), Some("test-value"));
    assert_eq!(SystemBackend.keychain_get(&account).unwrap(), None);
}

/// Regression: on Windows, Rust escapes quotes as `\"` and `cmd.exe` does not understand it.
/// A command with quotes (paths with spaces) must reach the shell intact.
#[test]
fn shell_commands_keep_their_quotes() {
    let out = SystemBackend
        .run_shell(r#"echo "a b""#)
        .expect("running echo");
    if cfg!(windows) {
        assert_eq!(out, r#""a b""#); // cmd's echo keeps the quotes
    } else {
        assert_eq!(out, "a b");
    }
}

fn postgres_draft() -> ConnectionDraft {
    ConnectionDraft {
        source: Source::Workspace,
        folder: "Prod/EU".into(),
        name: "Orders (prod)".into(),
        engine: Engine::Postgres,
        env: "prod".into(),
        host: " db.internal ".into(),
        port: Some(5432),
        database: "orders".into(),
        user: "app".into(),
        password: PasswordChoice::Keychain,
        tls_mode: TlsModeConfig::VerifyFull,
        tls_ca: String::new(),
        tunnel: String::new(),
        kubernetes: None,
        sqlite3: String::new(),
        read_only: false,
        mcp_exposed: true,
        mcp_writes: false,
    }
}

#[test]
fn slugs_make_safe_file_names() {
    assert_eq!(slug("Orders (prod)"), "orders-prod");
    assert_eq!(slug("  Análisis  2026 "), "analisis-2026");
    assert_eq!(slug("Año Ñandú"), "ano-nandu");
    assert_eq!(slug("日本"), "");
    assert_eq!(slug("a__b--c"), "a__b-c");
    assert_eq!(slug("../.."), "");
}

#[test]
fn new_connections_are_written_without_secrets_and_load_back() {
    let fx = Fixture::new("draft-postgres");
    let config = load(Some(&fx.repo()), &fx.user());
    let backend = FakeBackend::default();
    let draft = postgres_draft();
    let id = add_connection(
        &config,
        &draft,
        Some(&Secret::new("s3cr3t-value")),
        &backend,
    )
    .unwrap();
    assert_eq!(id, ws("prod/eu/orders-prod"));

    let path = fx
        .repo()
        .join(".spelunk/connections/prod/eu/orders-prod.toml");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("s3cr3t-value"), "{content}");
    assert!(content.contains("secret = \"keychain\""), "{content}");
    // The default port is implied, so the file only says what differs.
    assert!(!content.contains("port"), "{content}");
    assert!(content.contains("host = \"db.internal\""), "{content}");
    assert_eq!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&repo_account(&fx, "prod/eu/orders-prod"))
            .map(String::as_str),
        Some("s3cr3t-value")
    );

    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let entry = config.connection(&id).unwrap();
    assert_eq!(entry.name(), "Orders (prod)");
    assert_eq!(entry.file.tls.mode, TlsModeConfig::VerifyFull);
    assert!(entry.file.mcp.exposed);
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve(&id)
        .unwrap();
    assert_eq!(
        resolved.password,
        Password::Value(Secret::new("s3cr3t-value"))
    );
    assert_eq!(resolved.params.port, 5432);
}

#[test]
fn sqlite_drafts_keep_repository_paths_portable_and_drop_network_fields() {
    let fx = Fixture::new("draft-sqlite");
    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft {
        engine: Engine::Sqlite,
        folder: String::new(),
        name: "Local shop".into(),
        database: fx.repo().join("data").join("shop.db").display().to_string(),
        host: "ignored".into(),
        user: "ignored".into(),
        // A tunnel would mean a file on a server (ADR-018): none here.
        tunnel: String::new(),
        password: PasswordChoice::Prompt,
        mcp_exposed: false,
        mcp_writes: false,
        ..postgres_draft()
    };
    let file = draft.to_file(&config).unwrap();
    assert_eq!(file.database.as_deref(), Some("data/shop.db"));
    assert_eq!(
        (file.host, file.user, file.password, file.tunnel),
        (None, None, None, None)
    );
    assert_eq!(file.tls, Default::default());

    // Outside the repository (or for personal connections) the path is kept as typed.
    let outside = std::env::temp_dir()
        .join("elsewhere.db")
        .display()
        .to_string();
    let personal = ConnectionDraft {
        source: Source::User,
        database: outside.clone(),
        ..draft
    };
    assert_eq!(personal.to_file(&config).unwrap().database, Some(outside));
}

#[test]
fn drafts_never_overwrite_and_undo_the_file_if_the_keychain_fails() {
    let fx = Fixture::new("draft-conflict");
    let config = load(Some(&fx.repo()), &fx.user());
    let backend = FakeBackend::default();
    let draft = postgres_draft();
    add_connection(&config, &draft, Some(&Secret::new("first")), &backend).unwrap();
    // Same identifier: refused, and the stored password of the existing one is untouched.
    assert!(add_connection(&config, &draft, Some(&Secret::new("second")), &backend).is_err());
    assert_eq!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&repo_account(&fx, "prod/eu/orders-prod"))
            .map(String::as_str),
        Some("first")
    );

    let locked = FakeBackend {
        fail_keychain: true,
        ..FakeBackend::default()
    };
    let other = ConnectionDraft {
        name: "Billing".into(),
        ..postgres_draft()
    };
    let err = add_connection(&config, &other, Some(&Secret::new("x")), &locked).unwrap_err();
    assert!(matches!(err, ResolveError::Secret { .. }), "{err}");
    assert!(
        !fx.repo()
            .join(".spelunk/connections/prod/eu/billing.toml")
            .exists()
    );
}

#[test]
fn invalid_drafts_explain_what_is_wrong() {
    let fx = Fixture::new("draft-invalid");
    let config = load(Some(&fx.repo()), &fx.user());
    let error = |draft: ConnectionDraft| draft.to_file(&config).unwrap_err().message;
    assert!(
        error(ConnectionDraft {
            host: " ".into(),
            ..postgres_draft()
        })
        .contains("host")
    );
    assert!(
        error(ConnectionDraft {
            name: "!!".into(),
            ..postgres_draft()
        })
        .contains("name")
    );
    assert!(
        error(ConnectionDraft {
            folder: "ok/!!".into(),
            ..postgres_draft()
        })
        .contains("folder")
    );
    assert!(
        error(ConnectionDraft {
            tunnel: "nope".into(),
            ..postgres_draft()
        })
        .contains("nope")
    );
    for var in ["", "1ABC", "A-B", "A B", "$X"] {
        let draft = ConnectionDraft {
            password: PasswordChoice::Env(var.into()),
            ..postgres_draft()
        };
        assert!(error(draft).contains("environment variable"), "{var}");
    }
    let no_repo = load(None, &fx.user());
    assert!(
        postgres_draft()
            .to_file(&no_repo)
            .unwrap_err()
            .message
            .contains("repository")
    );
}

#[test]
fn drafts_resolve_for_testing_with_the_same_trust_rules() {
    let fx = Fixture::new("draft-resolve");
    let config = load(Some(&fx.repo()), &fx.user());
    let backend = FakeBackend::default();
    let untrusted = TrustStore::load(&fx.user()).unwrap();
    assert!(matches!(
        Resolver::new(&config, &untrusted, &backend).resolve_draft(&postgres_draft()),
        Err(ResolveError::Untrusted { .. })
    ));
    // Nothing is stored yet, so a keychain draft asks for the typed password.
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve_draft(&postgres_draft())
        .unwrap();
    assert_eq!(
        resolved.password,
        Password::Prompt {
            save_to: Some(repo_account(&fx, "prod/eu/orders-prod"))
        }
    );
    assert_eq!(resolved.params.host, "db.internal");
    assert!(
        !fx.repo().join(".spelunk/connections/prod").exists(),
        "testing writes nothing"
    );

    let personal = ConnectionDraft {
        source: Source::User,
        ..postgres_draft()
    };
    let resolved = Resolver::new(&config, &untrusted, &backend)
        .resolve_draft(&personal)
        .unwrap();
    assert_eq!(resolved.id.to_string(), "~/prod/eu/orders-prod");
}

const HAND_WRITTEN: &str = "# Orders database: owned by the data team\nschema_version = 1\nname = \"Orders\"\nengine = \"postgres\"\nhost = \"db.internal\" # primary\ndatabase = \"orders\"\nuser = \"app\"\npassword = { secret = \"keychain\" }\n\n[mcp]\nexposed = true\nmax_rows = 50 # agents get few rows\nwrites = \"deny\"\n";

#[test]
fn drafts_round_trip_existing_connections() {
    let fx = Fixture::new("draft-roundtrip");
    fx.write("repo/.spelunk/connections/team/orders.toml", HAND_WRITTEN);
    fx.write(
        "repo/.spelunk/connections/vault.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\npassword = { secret = \"op://Infra/db/password\" }\n",
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft::from_entry(config.connection(&ws("team/orders")).unwrap());
    assert_eq!(draft.folder, "team");
    assert_eq!(draft.name, "Orders");
    assert_eq!(draft.password, PasswordChoice::Keychain);
    assert!(draft.mcp_exposed);
    // A reference the form cannot build is shown and kept as is.
    let vault = ConnectionDraft::from_entry(config.connection(&ws("vault")).unwrap());
    assert_eq!(
        vault.password,
        PasswordChoice::Other("op://Infra/db/password".into())
    );
    assert_eq!(vault.name, "vault");
}

#[test]
fn editing_keeps_comments_and_settings_the_form_does_not_show() {
    let fx = Fixture::new("draft-edit");
    let path = fx.write("repo/.spelunk/connections/team/orders.toml", HAND_WRITTEN);
    let config = load(Some(&fx.repo()), &fx.user());
    let id = ws("team/orders");
    let backend = FakeBackend::default();
    backend
        .keychain_set(&repo_account(&fx, "team/orders"), "old")
        .unwrap();
    let draft = ConnectionDraft {
        name: "Orders (EU)".into(),
        host: "db-eu.internal".into(),
        port: Some(6543),
        tls_mode: TlsModeConfig::Require,
        // Folder and layer cannot move a connection: the file stays where it is.
        folder: "elsewhere".into(),
        source: Source::User,
        ..ConnectionDraft::from_entry(config.connection(&id).unwrap())
    };
    spelunk_config::update_connection(&config, &id, &draft, None, &backend).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.starts_with("# Orders database: owned by the data team\n"),
        "{content}"
    );
    assert!(
        content.contains("host = \"db-eu.internal\" # primary"),
        "{content}"
    );
    assert!(
        content.contains("max_rows = 50 # agents get few rows"),
        "{content}"
    );
    assert!(content.contains("writes = \"deny\""), "{content}");
    assert!(content.contains("port = 6543"), "{content}");
    assert!(content.contains("[tls]\nmode = \"require\""), "{content}");
    assert!(!fx.user().join("connections/elsewhere").exists());
    // An empty password keeps the stored one.
    assert_eq!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&repo_account(&fx, "team/orders"))
            .map(String::as_str),
        Some("old")
    );

    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(config.connection(&id).unwrap().name(), "Orders (EU)");
    assert_eq!(config.connection(&id).unwrap().file.mcp.max_rows, Some(50));
}

#[test]
fn the_form_lets_agents_write_or_only_read() {
    let fx = Fixture::new("draft-mcp-writes");
    let path = fx.write("repo/.spelunk/connections/orders.toml", HAND_WRITTEN);
    let id = ws("orders");
    let backend = FakeBackend::default();
    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    assert!(!draft.mcp_writes);

    let allow = ConnectionDraft {
        mcp_writes: true,
        ..draft.clone()
    };
    spelunk_config::update_connection(&config, &id, &allow, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("writes = \"allow\""), "{content}");
    assert!(
        content.contains("max_rows = 50 # agents get few rows"),
        "{content}"
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.connection(&id).unwrap();
    assert_eq!(entry.file.mcp.writes, WritePolicy::Allow);
    assert!(ConnectionDraft::from_entry(entry).mcp_writes);

    // Back to reading only: the default, so the key goes away.
    spelunk_config::update_connection(&config, &id, &draft, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("writes"), "{content}");

    // A hand-written `confirm` the form cannot show is kept by unrelated edits.
    fx.write(
        "repo/.spelunk/connections/orders.toml",
        &HAND_WRITTEN.replace("writes = \"deny\"", "writes = \"confirm\""),
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let renamed = ConnectionDraft {
        name: "Orders (EU)".into(),
        ..ConnectionDraft::from_entry(config.connection(&id).unwrap())
    };
    spelunk_config::update_connection(&config, &id, &renamed, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("writes = \"confirm\""), "{content}");

    // New connections that allow writes say so in their file.
    let file = ConnectionDraft {
        mcp_writes: true,
        ..postgres_draft()
    }
    .to_file(&config)
    .unwrap();
    assert_eq!(file.mcp.writes, WritePolicy::Allow);
}

#[test]
fn editing_the_password_updates_or_forgets_the_keychain_entry() {
    let fx = Fixture::new("draft-edit-password");
    let path = fx.write("repo/.spelunk/connections/orders.toml", HAND_WRITTEN);
    let id = ws("orders");
    let backend = FakeBackend::default();
    backend
        .keychain_set(&repo_account(&fx, "orders"), "old")
        .unwrap();

    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    spelunk_config::update_connection(&config, &id, &draft, Some(&Secret::new("new")), &backend)
        .unwrap();
    assert_eq!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&repo_account(&fx, "orders"))
            .map(String::as_str),
        Some("new")
    );
    assert!(!std::fs::read_to_string(&path).unwrap().contains("new\""));

    let prompt = ConnectionDraft {
        password: PasswordChoice::Prompt,
        ..draft.clone()
    };
    spelunk_config::update_connection(&config, &id, &prompt, None, &backend).unwrap();
    assert!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&repo_account(&fx, "orders"))
            .is_none()
    );
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("secret = \"prompt\"")
    );

    // If the keychain fails, the file goes back to what it was.
    let config = load(Some(&fx.repo()), &fx.user());
    let before = std::fs::read_to_string(&path).unwrap();
    let locked = FakeBackend {
        fail_keychain: true,
        ..FakeBackend::default()
    };
    let back_to_keychain = ConnectionDraft {
        password: PasswordChoice::Keychain,
        ..draft
    };
    assert!(
        spelunk_config::update_connection(
            &config,
            &id,
            &back_to_keychain,
            Some(&Secret::new("x")),
            &locked
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn invalid_edits_leave_the_file_untouched() {
    let fx = Fixture::new("draft-edit-invalid");
    let path = fx.write("repo/.spelunk/connections/orders.toml", HAND_WRITTEN);
    let config = load(Some(&fx.repo()), &fx.user());
    let id = ws("orders");
    let draft = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    let backend = FakeBackend::default();
    for broken in [
        ConnectionDraft {
            host: String::new(),
            ..draft.clone()
        },
        ConnectionDraft {
            name: "  ".into(),
            ..draft.clone()
        },
        ConnectionDraft {
            tunnel: "missing".into(),
            ..draft.clone()
        },
    ] {
        assert!(spelunk_config::update_connection(&config, &id, &broken, None, &backend).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_WRITTEN);
    }
    assert!(
        spelunk_config::update_connection(&config, &ws("missing"), &draft, None, &backend).is_err()
    );
}

#[test]
fn deleting_removes_the_file_its_password_and_empty_folders() {
    let fx = Fixture::new("draft-delete");
    let path = fx.write(
        "repo/.spelunk/connections/prod/eu/orders.toml",
        HAND_WRITTEN,
    );
    let keep = fx.write(
        "repo/.spelunk/connections/prod/other.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"a.db\"\n",
    );
    let backend = FakeBackend::default();
    backend
        .keychain_set(&repo_account(&fx, "prod/eu/orders"), "secret")
        .unwrap();
    backend
        .keychain_set(&repo_account(&fx, "prod/other"), "unrelated")
        .unwrap();
    let config = load(Some(&fx.repo()), &fx.user());

    spelunk_config::delete_connection(&config, &ws("prod/eu/orders"), &backend).unwrap();
    assert!(!path.exists());
    assert!(!fx.repo().join(".spelunk/connections/prod/eu").exists());
    assert!(keep.exists(), "folders still in use are kept");
    assert!(fx.repo().join(".spelunk/connections").exists());
    let keychain = backend.keychain.lock().unwrap();
    assert!(keychain.get(&repo_account(&fx, "prod/eu/orders")).is_none());
    assert_eq!(
        keychain
            .get(&repo_account(&fx, "prod/other"))
            .map(String::as_str),
        Some("unrelated")
    );
    drop(keychain);

    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.connection(&ws("prod/eu/orders")).is_none());
    assert!(spelunk_config::delete_connection(&config, &ws("prod/eu/orders"), &backend).is_err());
}

#[test]
fn testing_an_edit_finds_the_stored_password_even_after_renaming() {
    let fx = Fixture::new("draft-edit-resolve");
    fx.write("repo/.spelunk/connections/orders.toml", HAND_WRITTEN);
    let config = load(Some(&fx.repo()), &fx.user());
    let backend = FakeBackend::default();
    backend
        .keychain_set(&repo_account(&fx, "orders"), "stored")
        .unwrap();
    let id = ws("orders");
    let draft = ConnectionDraft {
        name: "Renamed".into(),
        ..ConnectionDraft::from_entry(config.connection(&id).unwrap())
    };
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve_edit(&id, &draft)
        .unwrap();
    assert_eq!(resolved.password, Password::Value(Secret::new("stored")));
    assert_eq!(resolved.name, "Renamed");
}

fn hop(host: &str, user: &str, auth: Vec<SshAuth>, password: PasswordChoice) -> HopDraft {
    HopDraft {
        host: host.into(),
        port: None,
        user: user.into(),
        auth,
        identity_file: String::new(),
        password,
    }
}

fn two_hop_tunnel() -> TunnelDraft {
    TunnelDraft {
        source: Source::Workspace,
        name: "Bastion EU".into(),
        hops: vec![
            hop(
                "jump.example.com",
                "ana",
                vec![SshAuth::Agent],
                PasswordChoice::None,
            ),
            hop(
                "10.0.0.5",
                "deploy",
                vec![SshAuth::Password],
                PasswordChoice::Keychain,
            ),
        ],
    }
}

fn kubernetes_connection(resource: &str) -> ConnectionDraft {
    ConnectionDraft {
        source: Source::Workspace,
        folder: String::new(),
        name: "Orders in cluster".into(),
        engine: Engine::Postgres,
        env: String::new(),
        host: String::new(),
        port: Some(5432),
        database: "orders".into(),
        user: "app".into(),
        password: PasswordChoice::Prompt,
        tls_mode: TlsModeConfig::Disable,
        tls_ca: String::new(),
        tunnel: String::new(),
        sqlite3: String::new(),
        kubernetes: Some(KubernetesDraft {
            context: "kind-orders".into(),
            namespace: "shop".into(),
            resource: resource.into(),
            kubeconfig: String::new(),
            kubectl: String::new(),
        }),
        read_only: false,
        mcp_exposed: false,
        mcp_writes: false,
    }
}

#[test]
fn kubernetes_connections_are_written_resolved_and_validated() {
    let fx = Fixture::new("conn-k8s");
    let config = load(Some(&fx.repo()), &fx.user());
    let backend = FakeBackend::default();
    let id = add_connection(
        &config,
        &kubernetes_connection("svc/orders-db"),
        None,
        &backend,
    )
    .unwrap();
    let path = fx
        .repo()
        .join(format!(".spelunk/connections/{}.toml", id.path));
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("[kubernetes]") && content.contains("resource = \"svc/orders-db\""),
        "{content}"
    );
    assert!(!content.contains("host"), "no host needed: {content}");
    assert!(
        !content.contains("port"),
        "the default port is implied: {content}"
    );

    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve(&id)
        .unwrap();
    assert!(resolved.tunnel.is_none());
    let k = resolved.kubernetes.unwrap();
    assert_eq!(
        (k.context.as_deref(), k.namespace.as_deref()),
        (Some("kind-orders"), Some("shop"))
    );
    assert_eq!(
        (k.resource.as_str(), k.kubectl.as_str()),
        ("svc/orders-db", "kubectl")
    );
    assert_eq!(
        (resolved.params.host.as_str(), resolved.params.port),
        ("localhost", 5432)
    );

    // Editing: another resource and port, then back to a plain connection.
    let entry = config.connection(&id).unwrap();
    let mut draft = ConnectionDraft::from_entry(entry);
    assert_eq!(draft.kubernetes.as_ref().unwrap().resource, "svc/orders-db");
    draft.kubernetes.as_mut().unwrap().resource = "deployment/orders".into();
    draft.port = Some(6432);
    spelunk_config::update_connection(&config, &id, &draft, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("deployment/orders") && content.contains("port = 6432"),
        "{content}"
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let mut plain = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    plain.kubernetes = None;
    plain.host = "db.internal".into();
    spelunk_config::update_connection(&config, &id, &plain, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        !content.contains("kubernetes") && content.contains("db.internal"),
        "{content}"
    );

    for bad in ["orders-db", "secret/orders", "svc/", "svc/Orders_DB"] {
        let mut draft = kubernetes_connection(bad);
        draft.name = format!("bad {bad}");
        assert!(
            add_connection(&config, &draft, None, &backend).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn kubernetes_and_an_ssh_tunnel_do_not_mix() {
    let fx = Fixture::new("conn-k8s-both");
    std::fs::create_dir_all(fx.repo().join(".spelunk/connections")).unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/mixed.toml"),
        "schema_version = 1\nengine = \"postgres\"\ndatabase = \"d\"\nuser = \"u\"\ntunnel = \"jump\"\n[kubernetes]\nresource = \"svc/db\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/local.toml"),
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\n[kubernetes]\nresource = \"svc/db\"\n",
    )
    .unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let problems = format!("{:?}", config.problems);
    assert!(problems.contains("not both"), "{problems}");
    assert!(problems.contains("SQLite is a local file"), "{problems}");
}

#[test]
fn new_tunnels_are_written_without_secrets_and_load_back() {
    let fx = Fixture::new("tunnel-add");
    let config = load(Some(&fx.repo()), &fx.user());
    let backend = FakeBackend::default();
    let mut draft = two_hop_tunnel();
    draft.hops[0].port = Some(22);
    draft.hops[1].port = Some(2222);
    let name = add_tunnel(
        &config,
        &draft,
        &[None, Some(Secret::new("hop-secret"))],
        &backend,
    )
    .unwrap();
    assert_eq!(name, "bastion-eu");
    let path = fx.repo().join(".spelunk/tunnels/bastion-eu.toml");
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.starts_with("#:schema "), "{content}");
    assert!(!content.contains("hop-secret"), "{content}");
    assert_eq!(
        content.matches("port").count(),
        1,
        "the default SSH port is implied: {content}"
    );
    assert_eq!(
        backend
            .keychain
            .lock()
            .unwrap()
            .get(&repo_hop(&fx, "bastion-eu", 1))
            .map(String::as_str),
        Some("hop-secret")
    );

    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let entry = config.tunnel_for(Source::Workspace, "bastion-eu").unwrap();
    assert_eq!(TunnelDraft::from_entry(entry).hops.len(), 2);
    // Resolving finds the stored password for hop 2 and nothing for the agent hop.
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve_tunnel_draft(&TunnelDraft::from_entry(entry))
        .unwrap();
    assert_eq!(resolved.hops[0].password, Password::None);
    assert_eq!(
        resolved.hops[1].password,
        Password::Value(Secret::new("hop-secret"))
    );
    assert_eq!(resolved.hops[1].port, 2222);

    assert!(
        add_tunnel(&config, &draft, &[], &backend).is_err(),
        "never overwrites"
    );
}

#[test]
fn invalid_tunnels_explain_what_is_wrong() {
    let fx = Fixture::new("tunnel-invalid");
    let config = load(Some(&fx.repo()), &fx.user());
    let error = |draft: TunnelDraft| draft.to_file(&config).unwrap_err().message;
    assert!(
        error(TunnelDraft {
            name: "??".into(),
            ..two_hop_tunnel()
        })
        .contains("name")
    );
    assert!(
        error(TunnelDraft {
            hops: vec![],
            ..two_hop_tunnel()
        })
        .contains("hop")
    );
    let mut no_host = two_hop_tunnel();
    no_host.hops[1].host = " ".into();
    assert!(error(no_host).contains("hop 2: the host"));
    let mut no_auth = two_hop_tunnel();
    no_auth.hops[0].auth.clear();
    assert!(error(no_auth).contains("hop 1"));
    // A password is only kept for methods that use it.
    let mut agent = two_hop_tunnel();
    agent.hops[0].password = PasswordChoice::Prompt;
    assert_eq!(agent.to_file(&config).unwrap().hops[0].password, None);
    // Untrusted repository: testing is refused, like for connections.
    let untrusted = TrustStore::load(&fx.user()).unwrap();
    assert!(matches!(
        Resolver::new(&config, &untrusted, &FakeBackend::default())
            .resolve_tunnel_draft(&two_hop_tunnel()),
        Err(ResolveError::Untrusted { .. })
    ));
}

const HAND_WRITTEN_TUNNEL: &str = "# Production bastions: ask the infra team before changing\nschema_version = 1\n\n[[hops]]\nhost = \"jump.example.com\" # public\nuser = \"ana\"\nauth = [\"agent\"]\ncommand = \"ssh -W %h:%p corp-gateway\"\n\n[[hops]]\nhost = \"10.0.0.5\"\nuser = \"deploy\"\nauth = [\"password\"]\npassword = { secret = \"keychain\" }\n";

#[test]
fn editing_tunnels_keeps_comments_and_commands_and_never_reuses_a_password_elsewhere() {
    let fx = Fixture::new("tunnel-edit");
    let path = fx.write("repo/.spelunk/tunnels/prod.toml", HAND_WRITTEN_TUNNEL);
    let backend = FakeBackend::default();
    backend
        .keychain_set(&repo_hop(&fx, "prod", 1), "old")
        .unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();

    // Same servers, new port on hop 1: comments, `command` and the stored password stay.
    let mut draft = TunnelDraft::from_entry(entry);
    draft.hops[0].port = Some(2200);
    update_tunnel(&config, Source::Workspace, "prod", &draft, &[], &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.starts_with("# Production bastions"), "{content}");
    assert!(
        content.contains("host = \"jump.example.com\" # public"),
        "{content}"
    );
    assert!(
        content.contains("command = \"ssh -W %h:%p corp-gateway\""),
        "{content}"
    );
    assert!(content.contains("port = 2200"), "{content}");
    assert!(
        backend
            .keychain
            .lock()
            .unwrap()
            .contains_key(&repo_hop(&fx, "prod", 1))
    );

    // Hop 2 now points to another server without a new password: the old one is forgotten,
    // so it is never sent to a host it was not meant for.
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    let mut moved = TunnelDraft::from_entry(entry);
    moved.hops[1].host = "10.0.0.9".into();
    update_tunnel(&config, Source::Workspace, "prod", &moved, &[], &backend).unwrap();
    assert!(
        !backend
            .keychain
            .lock()
            .unwrap()
            .contains_key(&repo_hop(&fx, "prod", 1))
    );

    // A third hop is appended; removing it again leaves two.
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    let mut longer = TunnelDraft::from_entry(entry);
    longer.hops.push(hop(
        "db-gw",
        "ana",
        vec![SshAuth::Key],
        PasswordChoice::None,
    ));
    update_tunnel(&config, Source::Workspace, "prod", &longer, &[], &backend).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    assert_eq!(entry.file.hops.len(), 3);
    let mut shorter = TunnelDraft::from_entry(entry);
    shorter.hops.truncate(2);
    update_tunnel(&config, Source::Workspace, "prod", &shorter, &[], &backend).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    assert_eq!(
        config
            .tunnel_for(Source::Workspace, "prod")
            .unwrap()
            .file
            .hops
            .len(),
        2
    );

    // Invalid edits leave the file as it was.
    let before = std::fs::read_to_string(&path).unwrap();
    let mut broken = shorter.clone();
    broken.hops[0].user = String::new();
    assert!(update_tunnel(&config, Source::Workspace, "prod", &broken, &[], &backend).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn tunnels_in_use_cannot_be_deleted() {
    let fx = Fixture::new("tunnel-delete");
    let path = fx.write("repo/.spelunk/tunnels/prod.toml", HAND_WRITTEN_TUNNEL);
    fx.write(
        "repo/.spelunk/connections/orders.toml",
        "schema_version = 1\nname = \"Orders\"\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\ntunnel = \"prod\"\n",
    );
    let backend = FakeBackend::default();
    backend
        .keychain_set(&repo_hop(&fx, "prod", 1), "old")
        .unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let err = delete_tunnel(&config, Source::Workspace, "prod", &backend).unwrap_err();
    assert!(err.to_string().contains("Orders"), "{err}");
    assert!(path.exists());

    std::fs::remove_file(fx.repo().join(".spelunk/connections/orders.toml")).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    delete_tunnel(&config, Source::Workspace, "prod", &backend).unwrap();
    assert!(!path.exists());
    assert!(
        !backend
            .keychain
            .lock()
            .unwrap()
            .contains_key(&repo_hop(&fx, "prod", 1))
    );
}

#[test]
fn shared_key_paths_work_on_every_machine() {
    let fx = Fixture::new("tunnel-paths");
    let config = load(Some(&fx.repo()), &fx.user());
    let home =
        PathBuf::from(std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap());
    let mut draft = two_hop_tunnel();
    draft.hops[0].auth = vec![SshAuth::Key];
    draft.hops[0].identity_file = home.join(".ssh").join("id_ed25519").display().to_string();
    draft.hops[1].auth = vec![SshAuth::Key];
    draft.hops[1].identity_file = fx.repo().join("keys").join("deploy").display().to_string();
    let file = draft.to_file(&config).unwrap();
    assert_eq!(
        file.hops[0].identity_file.as_deref(),
        Some("~/.ssh/id_ed25519")
    );
    assert_eq!(file.hops[1].identity_file.as_deref(), Some("keys/deploy"));
    // Personal tunnels never leave this computer: paths are kept as typed.
    let personal = TunnelDraft {
        source: Source::User,
        ..draft.clone()
    };
    assert_eq!(
        personal.to_file(&config).unwrap().hops[0].identity_file,
        Some(draft.hops[0].identity_file.clone())
    );
}

/// Two projects with a connection of the same name used to share its keychain entry, so the
/// password of one project's server could be sent to the other's. Now each repository has
/// its own entries, named after its path (which its files cannot choose).
#[test]
fn projects_never_share_keychain_passwords() {
    let first = Fixture::new("keychain-project-a");
    let second = Fixture::new("keychain-project-b");
    let backend = FakeBackend::default();
    let draft = ConnectionDraft {
        folder: "prod".into(),
        name: "orders".into(),
        ..postgres_draft()
    };
    let first_config = load(Some(&first.repo()), &first.user());
    add_connection(
        &first_config,
        &draft,
        Some(&Secret::new("first-project")),
        &backend,
    )
    .unwrap();
    let second_config = load(Some(&second.repo()), &second.user());
    add_connection(&second_config, &draft, None, &backend).unwrap();

    let id = ws("prod/orders");
    let first_config = load(Some(&first.repo()), &first.user());
    let second_config = load(Some(&second.repo()), &second.user());
    assert_ne!(
        connection_account(&first_config, &id),
        connection_account(&second_config, &id)
    );
    let resolved = Resolver::new(&second_config, &second.trusted(), &backend)
        .resolve(&id)
        .unwrap();
    assert_eq!(
        resolved.password,
        Password::Prompt {
            save_to: Some(connection_account(&second_config, &id))
        },
        "the other project's password must not be used"
    );
    let resolved = Resolver::new(&first_config, &first.trusted(), &backend)
        .resolve(&id)
        .unwrap();
    assert_eq!(
        resolved.password,
        Password::Value(Secret::new("first-project"))
    );
}

#[test]
fn personal_and_repository_entries_are_separate() {
    let fx = Fixture::new("keychain-layers");
    let config = load(Some(&fx.repo()), &fx.user());
    // Personal connections keep the names they always had.
    let personal = ConnectionId {
        source: Source::User,
        path: "scratch".into(),
    };
    assert_eq!(connection_account(&config, &personal), "~/scratch/password");
    assert!(connection_account(&config, &ws("scratch")).starts_with("repo:"));
    // A personal tunnel and a repository tunnel with the same name never share a password.
    assert_ne!(
        tunnel_account(&config, Source::User, "bastion", 0),
        tunnel_account(&config, Source::Workspace, "bastion", 0)
    );
    let backend = FakeBackend::default();
    let tunnel = |source| TunnelDraft {
        source,
        ..two_hop_tunnel()
    };
    add_tunnel(
        &config,
        &tunnel(Source::User),
        &[None, Some(Secret::new("personal"))],
        &backend,
    )
    .unwrap();
    add_tunnel(&config, &tunnel(Source::Workspace), &[], &backend).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve_tunnel_draft(&tunnel(Source::Workspace))
        .unwrap();
    assert!(matches!(resolved.hops[1].password, Password::Prompt { .. }));
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve_tunnel_draft(&tunnel(Source::User))
        .unwrap();
    assert_eq!(
        resolved.hops[1].password,
        Password::Value(Secret::new("personal"))
    );
}

/// SQLite with a tunnel is a file on the server (ADR-018): its path is the server's, kept as
/// written. DuckDB may not have a tunnel, and `sqlite3` only goes with one.
#[test]
fn sqlite_files_on_a_server_go_through_a_tunnel() {
    let fx = Fixture::new("conn-sqlite-remote");
    std::fs::create_dir_all(fx.repo().join(".spelunk/connections")).unwrap();
    std::fs::create_dir_all(fx.repo().join(".spelunk/tunnels")).unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/tunnels/jump.toml"),
        "schema_version = 1\n[[hops]]\nhost = \"files.internal\"\nuser = \"deploy\"\nauth = [\"agent\"]\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/remote.toml"),
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"data/app.db\"\ntunnel = \"jump\"\nsqlite3 = \"/opt/sqlite/bin/sqlite3\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/duck.toml"),
        "schema_version = 1\nengine = \"duckdb\"\ndatabase = \"x.duckdb\"\ntunnel = \"jump\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/local.toml"),
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\nsqlite3 = \"sqlite3\"\n",
    )
    .unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let problems = format!("{:?}", config.problems);
    assert!(problems.contains("DuckDB is a local file"), "{problems}");
    assert!(problems.contains("sqlite3 only applies"), "{problems}");
    assert!(!problems.contains("remote.toml"), "{problems}");

    let backend = FakeBackend::default();
    let id = ConnectionId {
        source: Source::Workspace,
        path: "remote".into(),
    };
    let resolved = Resolver::new(&config, &fx.trusted(), &backend)
        .resolve(&id)
        .unwrap();
    // Not resolved against the repository: it is a path on the server.
    assert_eq!(resolved.params.database, "data/app.db");
    assert_eq!(resolved.sqlite3.as_deref(), Some("/opt/sqlite/bin/sqlite3"));
    assert_eq!(resolved.tunnel.unwrap().hops[0].host, "files.internal");
}
