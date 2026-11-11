//! A connection's session setup (ADR-037), end to end: TOML file → resolution → session, on
//! every engine. SQLite and DuckDB always run; the servers need `SPELAION_TEST_PG`,
//! `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB` and `SPELAION_TEST_MSSQL` (skipped without
//! them, which does not count as tested).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenError, OpenOptions, ResultStore, SetupError,
    TunnelPool, open_connection,
};
use spelaion_driver_api::{Secret, Value};
use spelaion_ssh::{KnownHosts, NonInteractive};

mod support;
use support::{CLEANUP_TIMEOUT, cleaning_up, run_cleanup};

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// A temporary, trusted repository whose connections the tests write.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base = std::env::temp_dir().join(unique("spelaion-setup"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn file(&self, name: &str) -> String {
        self.base
            .join(name)
            .display()
            .to_string()
            .replace('\\', "/")
    }

    /// Writes `connections/<id>.toml` and opens it as the app does.
    async fn open(
        &self,
        id: &str,
        content: &str,
        password: Option<&str>,
        read_only_session: bool,
    ) -> Result<OpenConnection, OpenError> {
        std::fs::write(
            self.base
                .join(format!("repo/.spelaion/connections/{id}.toml")),
            content,
        )
        .unwrap();
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving");
        let options = OpenOptions {
            tunnels: Arc::new(TunnelPool::new()),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: self.base.join("user/known_hosts"),
            },
            interaction: Arc::new(NonInteractive),
            password: password.map(Secret::new),
            read_only_session,
        };
        open_connection(
            resolved,
            &Engines::builtin(),
            options,
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

/// The first value of the last result of `sql`, as text; panics on an error.
async fn value(open: &OpenConnection, sql: &str) -> String {
    let mut events = open.session.run_script(sql);
    let mut last = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => last = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {}", error.message),
            _ => {}
        }
    }
    let id = last.unwrap_or_else(|| panic!("{sql}: no result"));
    let stored = open.session.results().get(id).expect("the result");
    let cell = stored.read().unwrap().value_at(0, 0).expect("a cell");
    match cell {
        Value::Text(text) => text,
        Value::Null => "NULL".into(),
        other => other.to_string(),
    }
}

/// Runs `sql`; `Err` with the first error.
async fn run(open: &OpenConnection, sql: &str) -> Result<(), String> {
    let mut events = open.session.run_script(sql);
    while let Some(event) = events.next().await {
        if let CoreEvent::StatementFailed { error, .. } = event {
            return Err(error.message);
        }
    }
    Ok(())
}

/// A database server to test against: its connection file (without `[session]`), password,
/// how to create a schema with one table in it (and drop it again), and the setup statement
/// that makes it where unqualified names are found.
struct Server {
    label: &'static str,
    head: String,
    password: &'static str,
    create: fn(&str) -> String,
    drop: fn(&str) -> String,
    search: fn(&str) -> String,
    isolation: &'static str,
    /// The session's isolation level, as the engine names it.
    read_isolation: &'static str,
    expected_isolation: &'static str,
}

/// Runs `sql` (a cleanup's drop) on `server` in a session of its own, without setup, with
/// `run_cleanup`'s rule for failures.
async fn drop_on(ws: &Workspace, server: &Server, sql: String, panicked: bool) {
    let label = server.label;
    match tokio::time::timeout(
        CLEANUP_TIMEOUT,
        ws.open(label, &server.head, Some(server.password), false),
    )
    .await
    {
        Ok(Ok(plain)) => {
            run_cleanup(label, &plain.session, &[sql], panicked).await;
            plain.close().await;
        }
        Ok(Err(e)) if panicked => eprintln!("{label}: cleanup `{sql}` failed: {e}"),
        Err(_) if panicked => eprintln!("{label}: cleanup `{sql}` timed out"),
        Ok(Err(e)) => panic!("{label}: cleanup `{sql}` failed: {e}"),
        Err(_) => panic!("{label}: cleanup `{sql}` timed out"),
    }
}

fn servers() -> Vec<Server> {
    let mut out = Vec::new();
    let addr = |var: &str| match std::env::var(var) {
        Ok(addr) => {
            let (host, port) = addr.rsplit_once(':').expect("host:port");
            Some((host.to_string(), port.to_string()))
        }
        Err(_) => {
            eprintln!("{var} not set: that engine is skipped");
            None
        }
    };
    if let Some((host, port)) = addr("SPELAION_TEST_PG") {
        out.push(Server {
            label: "postgres",
            head: format!(
                "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\n"
            ),
            password: "spelaion",
            create: |s| {
                format!(
                    "drop schema if exists {s} cascade; create schema {s}; create table {s}.only_here (id int); insert into {s}.only_here values (7)"
                )
            },
            drop: |s| format!("drop schema if exists {s} cascade"),
            search: |s| format!("SET search_path TO {s}, public"),
            isolation: "repeatable-read",
            read_isolation: "show transaction_isolation",
            expected_isolation: "repeatable read",
        });
    }
    for (var, engine) in [
        ("SPELAION_TEST_MYSQL", "mysql"),
        ("SPELAION_TEST_MARIADB", "mariadb"),
    ] {
        if let Some((host, port)) = addr(var) {
            out.push(Server {
                label: engine,
                head: format!(
                    "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"root\"\npassword = {{ secret = \"prompt\" }}\n"
                ),
                password: "spelaion-root",
                create: |s| {
                    format!(
                        "drop database if exists {s}; create database {s}; create table {s}.only_here (id int); insert into {s}.only_here values (7)"
                    )
                },
                drop: |s| format!("drop database if exists {s}"),
                search: |s| format!("USE {s}"),
                isolation: "read-committed",
                read_isolation: "select @@session.transaction_isolation",
                expected_isolation: "READ-COMMITTED",
            });
        }
    }
    out
}

#[tokio::test]
async fn setup_and_default_schema_open_sessions_in_that_schema_on_every_engine_with_schemas() {
    let ws = Workspace::new();
    let mut tested = 0;
    for server in servers() {
        let label = server.label;
        let schema = unique("setup_app").to_lowercase();
        let body = async {
            let plain = ws
                .open(label, &server.head, Some(server.password), false)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            run(&plain, &(server.create)(&schema))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            plain.close().await;

            let content = format!(
                "{}\n[session]\nsetup = \"\"\"\n{};\nSELECT 1;\n\"\"\"\nschema = \"{schema}\"\nisolation = \"{}\"\n",
                server.head,
                (server.search)(&schema),
                server.isolation
            );
            // Opening twice: a reconnect is a new session, set up again in the same way.
            for attempt in ["first", "reconnect"] {
                let open = ws
                    .open(label, &content, Some(server.password), false)
                    .await
                    .unwrap_or_else(|e| panic!("{label} {attempt}: {e}"));
                assert_eq!(open.setup.statements, 2, "{label}");
                assert!(open.setup.skipped.is_empty(), "{label}");
                // The setup's SELECT left no result behind.
                assert_eq!(open.session.results().len(), 0, "{label}");
                assert_eq!(
                    spelaion_core::active_schema(&open.session)
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(schema.as_str()),
                    "{label} {attempt}"
                );
                // Unqualified names are found there.
                assert_eq!(
                    value(&open, "select id from only_here").await,
                    "7",
                    "{label} {attempt}"
                );
                assert_eq!(
                    value(&open, server.read_isolation).await,
                    server.expected_isolation,
                    "{label} {attempt}"
                );
                open.close().await;
            }
        };
        // The schema goes even when an assertion fails: left behind, it would pile up in the
        // shared database (one more per run) and push the explorer's other folders down.
        cleaning_up(body, |panicked| {
            drop_on(&ws, &server, (server.drop)(&schema), panicked)
        })
        .await;
        tested += 1;
    }
    eprintln!("{tested} servers tested");
}

#[tokio::test]
async fn duckdb_opens_in_its_default_schema_and_sqlite_runs_its_setup() {
    let ws = Workspace::new();
    let duck = ws.file("setup.duckdb");
    spelaion_driver_duckdb::create_database(std::path::Path::new(&duck)).unwrap();
    let head = format!("schema_version = 1\nengine = \"duckdb\"\ndatabase = \"{duck}\"\n");
    let plain = ws.open("duck", &head, None, false).await.unwrap();
    run(
        &plain,
        "create schema app; create table app.only_here (id int); insert into app.only_here values (7)",
    )
    .await
    .unwrap();
    plain.close().await;
    let open = ws
        .open(
            "duck",
            &format!("{head}\n[session]\nschema = \"app\"\nsetup = \"SET TimeZone = 'UTC'\"\n"),
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        spelaion_core::active_schema(&open.session)
            .await
            .unwrap()
            .as_deref(),
        Some("app")
    );
    assert_eq!(value(&open, "select id from only_here").await, "7");
    open.close().await;

    let lite = ws.file("setup.sqlite");
    std::fs::write(&lite, b"").unwrap();
    let open = ws
        .open(
            "lite",
            &format!(
                "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"{lite}\"\n\n[session]\nsetup = \"PRAGMA foreign_keys = ON\"\n"
            ),
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(value(&open, "pragma foreign_keys").await, "1");
    open.close().await;
}

#[tokio::test]
async fn a_setup_that_writes_is_refused_on_a_read_only_connection_and_nothing_of_it_runs() {
    let ws = Workspace::new();
    let lite = ws.file("refuse.sqlite");
    std::fs::write(&lite, b"").unwrap();
    let head = format!("schema_version = 1\nengine = \"sqlite\"\ndatabase = \"{lite}\"\n");
    let plain = ws.open("lite", &head, None, false).await.unwrap();
    run(&plain, "create table t (a int)").await.unwrap();
    plain.close().await;

    // A setting first and a write second: nothing runs, not even the setting.
    let writes =
        "\n[session]\nsetup = \"select 1; insert into t values (1)\"\nignore_setup_errors = true\n";
    for (case, content, read_only_session) in [
        (
            "read-only connection",
            format!("{head}read_only = true\n{writes}"),
            false,
        ),
        (
            "production",
            format!("{head}env = \"prod\"\n{writes}"),
            false,
        ),
        ("agent's read session", format!("{head}{writes}"), true),
    ] {
        match ws.open("lite", &content, None, read_only_session).await {
            Err(OpenError::Setup(SetupError::Refused {
                index,
                total,
                reason,
            })) => {
                assert_eq!((index, total), (2, 2), "{case}");
                assert!(
                    reason.contains("only reads and session settings"),
                    "{case}: {reason}"
                );
            }
            Err(other) => panic!("{case}: {other}"),
            Ok(_) => panic!("{case}: a setup that writes must be refused"),
        }
    }
    let plain = ws.open("lite", &head, None, false).await.unwrap();
    assert_eq!(value(&plain, "select count(*) from t").await, "0");
    plain.close().await;

    // The same setup runs on the user's own writable session.
    let open = ws
        .open("lite", &format!("{head}{writes}"), None, false)
        .await
        .unwrap();
    assert_eq!(value(&open, "select count(*) from t").await, "1");
    open.close().await;

    // Settings and reads are fine on a read-only connection.
    let open = ws
        .open(
            "lite",
            &format!("{head}read_only = true\n\n[session]\nsetup = \"select count(*) from t\"\n"),
            None,
            false,
        )
        .await
        .unwrap();
    open.close().await;
}

#[tokio::test]
async fn postgres_read_only_setup_takes_settings_and_refuses_writes() {
    let Some(server) = servers().into_iter().find(|s| s.label == "postgres") else {
        return;
    };
    let ws = Workspace::new();
    let table = unique("setup_ro");
    let body = async {
        let plain = ws
            .open("pg", &server.head, Some(server.password), false)
            .await
            .unwrap();
        run(&plain, &format!("create table {table} (a int)"))
            .await
            .unwrap();
        plain.close().await;

        let open = ws
            .open(
                "pg",
                &format!(
                    "{}read_only = true\n\n[session]\nsetup = \"SET search_path TO public; SET TIME ZONE 'UTC'; SET statement_timeout = '30s'\"\n",
                    server.head
                ),
                Some(server.password),
                false,
            )
            .await
            .unwrap();
        assert_eq!(value(&open, "show timezone").await, "UTC");
        open.close().await;

        for setup in [
            format!("insert into {table} values (1)"),
            "SET default_transaction_read_only = off".to_string(),
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE".to_string(),
            "select set_config('default_transaction_read_only', 'off', false)".to_string(),
            format!("SET search_path TO public; create table {table}_x (a int)"),
        ] {
            let content = format!(
                "{}read_only = true\n\n[session]\nsetup = \"{setup}\"\nignore_setup_errors = true\n",
                server.head
            );
            match ws.open("pg", &content, Some(server.password), false).await {
                Err(OpenError::Setup(SetupError::Refused { .. })) => {}
                Err(other) => panic!("{setup}: {other}"),
                Ok(_) => panic!("{setup}: must be refused"),
            }
        }
        let plain = ws
            .open("pg", &server.head, Some(server.password), false)
            .await
            .unwrap();
        assert_eq!(
            value(&plain, &format!("select count(*) from {table}")).await,
            "0"
        );
        assert_eq!(
            value(&plain, &format!("select to_regclass('{table}_x') is null")).await,
            "true"
        );
        plain.close().await;
    };
    cleaning_up(body, |panicked| {
        drop_on(
            &ws,
            &server,
            format!("drop table if exists {table}; drop table if exists {table}_x"),
            panicked,
        )
    })
    .await;
}

#[tokio::test]
async fn ignore_errors_skips_failing_statements_and_otherwise_the_session_is_refused() {
    let ws = Workspace::new();
    let lite = ws.file("ignore.sqlite");
    std::fs::write(&lite, b"").unwrap();
    let head = format!("schema_version = 1\nengine = \"sqlite\"\ndatabase = \"{lite}\"\n");
    let setup = "\n[session]\nsetup = \"select * from missing_table; PRAGMA foreign_keys = ON\"\n";

    match ws
        .open("lite", &format!("{head}{setup}"), None, false)
        .await
    {
        Err(OpenError::Setup(SetupError::Failed {
            index,
            total,
            message,
        })) => {
            assert_eq!((index, total), (1, 2));
            assert!(message.contains("missing_table"), "{message}");
        }
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("a failing setup must refuse the session"),
    }

    let open = ws
        .open(
            "lite",
            &format!("{head}{setup}ignore_setup_errors = true\n"),
            None,
            false,
        )
        .await
        .unwrap();
    assert_eq!(open.setup.statements, 2);
    assert_eq!(open.setup.skipped.len(), 1);
    assert_eq!(open.setup.skipped[0].index, 1);
    assert_eq!(
        open.setup.skipped[0].statement,
        "select * from missing_table"
    );
    // The statements after the failed one still ran.
    assert_eq!(value(&open, "pragma foreign_keys").await, "1");
    open.close().await;
}

#[tokio::test]
async fn a_setup_that_leaves_a_transaction_open_refuses_the_session() {
    let ws = Workspace::new();
    let lite = ws.file("tx.sqlite");
    std::fs::write(&lite, b"").unwrap();
    let content = format!(
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"{lite}\"\n\n[session]\nsetup = \"begin\"\n"
    );
    match ws.open("lite", &content, None, false).await {
        Err(OpenError::Setup(SetupError::TransactionLeftOpen)) => {}
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("a session must not start inside the setup's transaction"),
    }
}

#[tokio::test]
async fn a_default_schema_that_does_not_exist_refuses_the_session() {
    let ws = Workspace::new();
    let mut tested = 0;
    for server in servers() {
        let content = format!(
            "{}\n[session]\nschema = \"{}\"\n",
            server.head,
            unique("no_such_schema")
        );
        match ws
            .open(server.label, &content, Some(server.password), false)
            .await
        {
            Err(OpenError::Setup(SetupError::Schema { .. })) => {}
            Err(other) => panic!("{}: {other}", server.label),
            Ok(_) => panic!("{}: a missing schema must be said", server.label),
        }
        tested += 1;
    }
    eprintln!("{tested} servers tested");
}

#[tokio::test]
async fn sql_server_sets_its_isolation_level_and_options() {
    let Ok(addr) = std::env::var("SPELAION_TEST_MSSQL") else {
        eprintln!("SPELAION_TEST_MSSQL not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let ws = Workspace::new();
    let head = format!(
        "schema_version = 1\nengine = \"sqlserver\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"master\"\nuser = \"sa\"\npassword = {{ secret = \"prompt\" }}\n\n[tls]\nmode = \"require\"\n"
    );
    let open = ws
        .open(
            "mssql",
            &format!(
                "{head}\n[session]\nisolation = \"serializable\"\nsetup = \"SET DATEFORMAT dmy\"\n"
            ),
            Some("Spelaion-test-1"),
            false,
        )
        .await
        .unwrap();
    // 4 = SERIALIZABLE.
    assert_eq!(
        value(
            &open,
            "select transaction_isolation_level from sys.dm_exec_sessions where session_id = @@SPID"
        )
        .await,
        "4"
    );
    assert_eq!(
        value(
            &open,
            "select date_format from sys.dm_exec_sessions where session_id = @@SPID"
        )
        .await,
        "dmy"
    );
    open.close().await;
}
