//! H2 and HSQLDB files through the JDBC bridge, through the core (SDB-317, ADR-059): a file the
//! core creates opens from a connection file, a script is split by the core and run statement by
//! statement, the explorer lists schemas, tables and views and describes them, results arrive as
//! their logical types, and what was written is in the file when it opens again. The core builds
//! the JDBC URL from the file's path, refuses one that would carry settings of its own (H2's
//! `INIT`) before the plugin sees it, and offers neither read-only mode nor agents.
//!
//! They need the bridge's plugin (`spelaion-dev/spelaiondb-plugin-jdbc`): the pinned build, or one
//! built on this machine (`SPELAION_DEV_PLUGIN_H2`, `SPELAION_DEV_PLUGIN_HSQLDB`), and a Java
//! runtime (17 or later) for it to start. Without the plugin the tests are skipped, which does not
//! count as tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Engine, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    AgentView, CoreEvent, Engines, OpenConnection, OpenOptions, ResultId, ResultStore, TunnelPool,
    agent_view, open_connection,
};
use spelaion_driver_api::{ObjectKind, Secret, Value};
use spelaion_ssh::KnownHosts;

/// Whether the core has the bridge's plugin for `engine`, saying why a test is skipped.
fn bridged(engine: Engine) -> bool {
    let engines = Engines::builtin();
    if engines.get(engine).is_none() || engines.plugin_unavailable(engine.as_str()) {
        eprintln!(
            "the JDBC bridge is neither pinned nor pointed at ({}): test skipped",
            spelaion_core::plugins::dev_plugin_variable(engine.as_str())
        );
        return false;
    }
    true
}

/// A temporary, trusted repository whose connections a test writes.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir().join(format!("spelaion-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.base.join("repo").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn config(&self) -> spelaion_config::Config {
        load(Some(&self.base.join("repo")), &self.base.join("user"))
    }

    async fn open(&self, id: &str) -> Result<OpenConnection, spelaion_core::OpenError> {
        let config = self.config();
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving");
        open_connection(
            resolved,
            &Engines::builtin(),
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_files: Vec::new(),
                    user_file: None,
                    app_file: self.base.join("user/known_hosts"),
                },
                interaction: Arc::new(NoQuestions),
                password: None,
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        )
        .await
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct NoQuestions;

#[async_trait::async_trait]
impl spelaion_ssh::Interaction for NoQuestions {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        false
    }
    async fn ask_secret(&self, _request: &spelaion_ssh::SecretRequest) -> Option<Secret> {
        None
    }
}

fn toml_path(path: &Path) -> String {
    format!("{:?}", path.display().to_string().replace('\\', "/"))
}

fn connection(engine: Engine, file: &Path, extra: &str) -> String {
    format!(
        "schema_version = 1\nengine = \"{}\"\ndatabase = {}\n{extra}\n",
        engine.as_str(),
        toml_path(file)
    )
}

/// What a script did: each result, and each failed statement's index and message.
#[derive(Default, Debug)]
struct Ran {
    results: Vec<ResultId>,
    errors: Vec<(usize, String)>,
}

async fn run(open: &OpenConnection, script: &str) -> Ran {
    let mut ran = Ran::default();
    let mut events = open.session.run_script(script);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => ran.results.push(result),
            CoreEvent::StatementFailed { index, error } => ran.errors.push((index, error.message)),
            _ => {}
        }
    }
    ran
}

fn cell(open: &OpenConnection, result: ResultId, row: usize, column: usize) -> Value {
    let stored = open.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    stored.value_at(row, column).unwrap_or(Value::Null)
}

/// Creates the file through the core, opens it from a connection file, writes, browses and
/// reads it, then opens it again and finds what was written.
async fn a_file_opens_browses_and_queries(engine: Engine, picked: &str, schema: &str) {
    if !bridged(engine) {
        return;
    }
    let ws = Workspace::new(&format!("jdbc-{}", engine.as_str()));
    let file = ws.file(picked);
    spelaion_core::create_file_database(engine, &file)
        .await
        .unwrap_or_else(|e| panic!("creating {}: {e}", file.display()));
    assert!(file.exists(), "the core created {}", file.display());
    // An existing file is never touched.
    assert!(
        spelaion_core::create_file_database(engine, &file)
            .await
            .is_err()
    );
    ws.write(
        ".spelaion/connections/shop.toml",
        &connection(engine, &file, ""),
    );

    let open = ws.open("shop").await.expect("opens");
    // Split by the core: the semicolons in strings, quoted names and comments are not ends.
    let ran = run(
        &open,
        "-- the shop; with a semicolon in a comment\n\
         CREATE TABLE customers (id INT PRIMARY KEY, email VARCHAR(80) NOT NULL);\n\
         CREATE TABLE \"or;ders\" (id INT PRIMARY KEY, customer INT REFERENCES customers(id), \
         total DECIMAL(10, 2), placed DATE);\n\
         CREATE VIEW big AS SELECT * FROM \"or;ders\" WHERE total > 100;\n\
         INSERT INTO customers VALUES (1, 'a;b@example.com');\n\
         INSERT INTO \"or;ders\" VALUES (10, 1, 250.50, DATE '2026-11-28');\n\
         /* a block; comment */ SELECT c.email, o.total, o.placed FROM \"or;ders\" o \
         JOIN customers c ON c.id = o.customer",
    )
    .await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    let rows = *ran.results.last().unwrap();
    assert_eq!(
        cell(&open, rows, 0, 0),
        Value::Text("a;b@example.com".into())
    );
    assert_eq!(cell(&open, rows, 0, 1), Value::Decimal("250.50".into()));
    assert_eq!(cell(&open, rows, 0, 2), Value::Date(20_785));

    let connection = open.session.connection();
    let schemas = connection.list_schemas().await.unwrap();
    assert!(
        schemas.iter().any(|s| s.name == schema && !s.system),
        "{schemas:?}"
    );
    assert!(
        schemas
            .iter()
            .any(|s| s.name == "INFORMATION_SCHEMA" && s.system)
    );
    let objects = connection.list_objects(schema).await.unwrap();
    let kinds: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("BIG", ObjectKind::View),
            ("CUSTOMERS", ObjectKind::Table),
            ("or;ders", ObjectKind::Table)
        ]
    );
    let orders = connection.describe_table(schema, "or;ders").await.unwrap();
    assert_eq!(orders.primary_key, ["ID"]);
    assert_eq!(orders.foreign_keys[0].referenced_table, "CUSTOMERS");

    // An error stops the script at its statement, and the session goes on.
    let ran = run(
        &open,
        "SELECT 1 FROM customers; SELECT * FROM not_there; SELECT 3",
    )
    .await;
    assert_eq!(ran.errors.len(), 1, "{:?}", ran.errors);
    assert_eq!(ran.errors[0].0, 1);
    let ran = run(&open, "SELECT COUNT(*) FROM customers").await;
    assert_eq!(cell(&open, ran.results[0], 0, 0), Value::Int(1));
    open.close().await;

    // Opened again: what was written is in the file.
    let open = ws.open("shop").await.expect("opens again");
    let ran = run(&open, "SELECT email FROM customers").await;
    assert!(ran.errors.is_empty(), "{:?}", ran.errors);
    assert_eq!(
        cell(&open, ran.results[0], 0, 0),
        Value::Text("a;b@example.com".into())
    );
    open.close().await;
}

#[tokio::test]
async fn an_h2_file_opens_browses_and_queries_through_the_bridge() {
    let _turn = crate::serial::file_turn(module_path!());
    a_file_opens_browses_and_queries(Engine::H2, "shop.mv.db", "PUBLIC").await;
}

#[tokio::test]
async fn an_hsqldb_file_opens_browses_and_queries_through_the_bridge() {
    let _turn = crate::serial::file_turn(module_path!());
    a_file_opens_browses_and_queries(Engine::Hsqldb, "shop.script", "PUBLIC").await;
}

/// ADR-059: the core builds the URL, and a path that would carry settings of its own (H2's
/// `INIT`, which runs a script as the database opens) never reaches the plugin.
#[tokio::test]
async fn a_path_that_would_carry_settings_never_reaches_the_bridge() {
    let _turn = crate::serial::file_turn(module_path!());
    if !bridged(Engine::H2) {
        return;
    }
    let ws = Workspace::new("jdbc-init");
    let marker = ws.file("init-ran.sql");
    let sneaky = format!(
        "{};INIT=SCRIPT TO '{}'",
        ws.file("shop").display().to_string().replace('\\', "/"),
        marker.display().to_string().replace('\\', "/")
    );
    ws.write(
        ".spelaion/connections/shop.toml",
        &format!("schema_version = 1\nengine = \"h2\"\ndatabase = {sneaky:?}\n"),
    );
    let error = ws.open("shop").await.err().expect("refused");
    assert!(error.to_string().contains("cannot hold a `;`"), "{error}");
    let created = spelaion_core::create_file_database(Engine::H2, Path::new(&sneaky)).await;
    assert!(created.is_err());
    assert!(!marker.exists(), "an INIT ran");
    assert!(!ws.file("shop.mv.db").exists());
}

/// ADR-059: an engine through the bridge has no read-only mode and agents never see it.
#[tokio::test]
async fn no_read_only_mode_and_no_agents() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in [Engine::H2, Engine::Hsqldb] {
        let ws = Workspace::new(&format!("jdbc-barrier-{}", engine.as_str()));
        let file = ws.file("shop.db");
        ws.write(
            ".spelaion/connections/shop.toml",
            &connection(engine, &file, "read_only = true"),
        );
        assert!(
            !ws.config().problems.is_empty(),
            "{engine:?}: read_only accepted"
        );

        ws.write(
            ".spelaion/connections/shop.toml",
            &connection(engine, &file, "[mcp]\nexposed = true"),
        );
        let config = ws.config();
        let entry = config
            .connections
            .iter()
            .find(|c| c.id.path == "shop")
            .expect("loaded");
        assert_eq!(
            agent_view(entry, false, false),
            AgentView::EngineWithoutBarrier,
            "{engine:?}"
        );
    }
}
