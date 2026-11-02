//! Configuration tests with temporary directories and a fake secret backend
//! (the real keychain and environment are never touched).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use spelunk_config::{
    ConnectionFile, ConnectionId, Engine, Password, ResolveError, Resolver, SecretBackend, Source,
    SshAuth, SystemBackend, TrustStore, WritePolicy, create_connection, load, set_value,
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
}

impl SecretBackend for FakeBackend {
    fn keychain_get(&self, account: &str) -> Result<Option<String>, String> {
        Ok(self.keychain.lock().unwrap().get(account).cloned())
    }

    fn keychain_set(&self, account: &str, secret: &str) -> Result<(), String> {
        self.keychain
            .lock()
            .unwrap()
            .insert(account.into(), secret.into());
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
    assert_eq!(account, "db/password");
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
