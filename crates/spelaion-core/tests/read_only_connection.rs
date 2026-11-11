//! A read-only connection (ADR-053), end to end: TOML file with `read_only = true` →
//! resolution → session, on every engine. Reads, session settings and transaction control run;
//! every write is refused before anything is sent, whatever was confirmed, and on PostgreSQL,
//! MySQL and MariaDB the engine's own read-only mode stops what gets past the check (a function
//! that writes behind a `SELECT`). SQLite and DuckDB always run; the servers need
//! `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB` and `SPELAION_TEST_MSSQL`
//! (skipped without them, which does not count as tested).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::plan::{ExplainOptions, ExplainPolicy, explain};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenError, OpenOptions, ResultStore, RunRefused,
    TunnelPool, check_run, open_connection, run_risks,
};
use spelaion_driver_api::{ExecEvent, ExecOptions, Secret, Value};
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
        let base = std::env::temp_dir().join(unique("spelaion-read-only"));
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
            read_only_session: false,
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

/// Runs `sql` on a read-only session and checks it was refused before anything ran: the
/// app's check refuses it even confirmed and asks nothing first, and the session's own run
/// fails at the refused statement without sending a single one. Returns the message.
async fn refused(label: &str, open: &OpenConnection, sql: &str) -> String {
    let safeguards = Safeguards::default();
    match check_run(&open.session, sql, &safeguards, true) {
        Err(RunRefused::ReadOnly(refusal)) => {
            assert!(
                refusal.to_string().contains("the connection is read-only"),
                "{label}: {sql}: {refusal}"
            );
        }
        other => panic!("{label}: {sql}: the app's check let it through: {other:?}"),
    }
    assert!(
        run_risks(&open.session, sql, &safeguards).is_empty(),
        "{label}: {sql}: a refused script asks no confirmation first"
    );
    let events: Vec<CoreEvent> = open.session.run_script(sql).collect().await;
    let mut message = None;
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => message = Some(error.message.clone()),
            CoreEvent::ResultStarted { .. } | CoreEvent::StatementFinished { .. } => {
                panic!("{label}: {sql}: a statement ran: {events:?}")
            }
            CoreEvent::ScriptFinished {
                executed, summary, ..
            } => {
                assert_eq!(*executed, 0, "{label}: {sql}");
                assert_eq!(summary.executed, 0, "{label}: {sql}");
                assert_eq!(summary.failed, 1, "{label}: {sql}");
            }
            _ => {}
        }
    }
    let message = message.unwrap_or_else(|| panic!("{label}: {sql}: it ran: {events:?}"));
    assert!(
        message.contains("the connection is read-only, so nothing was run"),
        "{label}: {sql}: {message}"
    );
    message
}

/// The first error of `sql` sent straight to the driver, around the core's check: what the
/// engine's own read-only mode does.
async fn engine_error(open: &OpenConnection, sql: &str) -> Option<String> {
    let mut events = match open
        .session
        .connection()
        .execute(sql, &ExecOptions::default())
        .await
    {
        Ok(events) => events,
        Err(e) => return Some(e.to_string()),
    };
    while let Some(event) = events.next().await {
        match event {
            Ok(ExecEvent::Done { .. }) => break,
            Ok(_) => {}
            Err(e) => return Some(e.to_string()),
        }
    }
    None
}

/// A database server to test against.
struct Server {
    label: &'static str,
    /// The connection file without `read_only`.
    head: String,
    password: &'static str,
    /// Runs first, in a writable session on the server's default database (SQL Server's
    /// `master`), when the test database may be missing.
    prepare: Option<&'static str>,
    create: fn(&str) -> String,
    drop: fn(&str) -> String,
    /// Settings and `USE` a read-only session runs.
    settings: Vec<String>,
    /// A transaction around a read, as the user writes it.
    transaction: fn(&str) -> String,
    /// Writes in every shape, each of which must be refused.
    writes: fn(&str) -> Vec<String>,
    /// The engine has a read-only mode of its own: a write sent around the check fails.
    engine_mode: bool,
}

fn common_writes(t: &str) -> Vec<String> {
    vec![
        format!("insert into {t} values (2, 20)"),
        format!("UpDaTe {t} SeT v = 11 WhErE id = 1"),
        format!("/* a read */ delete from {t} where id = 1"),
        format!("-- select 1\ndelete from {t}"),
        format!("create table {t}_new (id int)"),
        format!("alter table {t} add w int"),
        format!("drop table {t}"),
        format!("truncate table {t}"),
    ]
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
            prepare: None,
            create: |t| {
                format!(
                    "create table {t} (id int primary key, v int); insert into {t} values (1, 10); create sequence {t}_seq"
                )
            },
            drop: |t| format!("drop table if exists {t}; drop table if exists {t}_new; drop table if exists {t}_copy; drop sequence if exists {t}_seq"),
            settings: vec![
                "SET search_path TO public".into(),
                "set statement_timeout = 5000".into(),
                "show search_path".into(),
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE".into(),
            ],
            transaction: |t| format!("begin; select v from {t}; savepoint a; rollback to savepoint a; commit"),
            writes: |t| {
                let mut out = common_writes(t);
                out.extend([
                    format!("with d as (delete from {t} returning *) select * from d"),
                    format!("WITH u AS (UPDATE {t} SET v = 0 RETURNING *) SELECT count(*) FROM u"),
                    format!("select * into {t}_copy from {t}"),
                    format!("select * from {t} for update"),
                    format!("explain analyze delete from {t}"),
                    format!("select 1; delete from {t} where id = 1"),
                    format!("do $$ begin delete from {t}; end $$"),
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE".into(),
                    "begin read write".into(),
                    "set default_transaction_read_only = off".into(),
                    "reset all".into(),
                    "select set_config('default_transaction_read_only', 'off', false)".into(),
                    "this is not sql".into(),
                ]);
                out
            },
            engine_mode: true,
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
                prepare: None,
                create: |t| {
                    format!(
                        "create table {t} (id int primary key, v int); insert into {t} values (1, 10)"
                    )
                },
                drop: |t| format!("drop table if exists {t}; drop table if exists {t}_new; drop table if exists {t}_copy"),
                settings: vec![
                    "USE spelaion".into(),
                    "set time_zone = '+00:00'".into(),
                    "show tables".into(),
                    "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED".into(),
                ],
                transaction: |t| format!("start transaction; select v from {t}; commit"),
                writes: |t| {
                    let mut out = common_writes(t);
                    out.extend([
                        format!("replace into {t} values (1, 0)"),
                        format!("select * from {t} into outfile '/tmp/{t}.txt'"),
                        format!("select v into @v from {t} where id = 1"),
                        format!("select 1; delete from {t} where id = 1"),
                        format!("/*!50000 delete from {t} */ select 1"),
                        format!("select 1 /*!, (select v from {t} for update) */"),
                        format!("lock tables {t} write"),
                        "SET SESSION TRANSACTION READ WRITE".into(),
                        "start transaction read write".into(),
                        "set transaction_read_only = 0".into(),
                        "SET @@session.transaction_read_only = OFF".into(),
                        "this is not sql".into(),
                    ]);
                    out
                },
                engine_mode: true,
            });
        }
    }
    if let Some((host, port)) = addr("SPELAION_TEST_MSSQL") {
        out.push(Server {
            label: "sqlserver",
            head: format!(
                "schema_version = 1\nengine = \"sqlserver\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"sa\"\npassword = {{ secret = \"prompt\" }}\n\n[tls]\nmode = \"require\"\n"
            ),
            password: "Spelaion-test-1",
            prepare: Some("if db_id('spelaion') is null create database spelaion"),
            create: |t| {
                format!(
                    "create table dbo.{t} (id int primary key, v int); insert into dbo.{t} values (1, 10)"
                )
            },
            drop: |t| format!("drop table if exists dbo.{t}; drop table if exists dbo.{t}_new; drop table if exists dbo.{t}_copy"),
            settings: vec![
                "USE spelaion".into(),
                "SET NOCOUNT ON".into(),
                "SET LOCK_TIMEOUT 1000".into(),
                "SET TRANSACTION ISOLATION LEVEL READ COMMITTED".into(),
            ],
            transaction: |t| format!("BEGIN TRANSACTION; SELECT v FROM dbo.{t}; COMMIT"),
            writes: |t| {
                let mut out = common_writes(t);
                out.extend([
                    // A write hidden in the same batch, after a read, with and without `;`.
                    format!("SELECT v FROM dbo.{t}\nDELETE FROM dbo.{t} WHERE id = 1"),
                    format!("SELECT 1; UPDATE dbo.{t} SET v = 0 WHERE id = 1"),
                    format!("select 1\ngo\ndelete from dbo.{t}"),
                    format!("select * into dbo.{t}_copy from dbo.{t}"),
                    format!("exec('delete from dbo.{t}')"),
                    format!("BEGIN TRY DELETE FROM dbo.{t} END TRY BEGIN CATCH SELECT 1 END CATCH"),
                    format!("IF 1 = 1 DELETE FROM dbo.{t}"),
                    format!("update dbo.{t} set v = 0 output inserted.* where id = 1"),
                    "SET IMPLICIT_TRANSACTIONS ON".into(),
                    "this is not sql".into(),
                ]);
                out
            },
            engine_mode: false,
        });
    }
    out
}

/// Runs `sqls` on `server` in a writable session of its own, with `run_cleanup`'s rule for
/// failures.
async fn writable(ws: &Workspace, server: &Server, sqls: Vec<String>, panicked: bool) {
    let label = server.label;
    match tokio::time::timeout(
        CLEANUP_TIMEOUT,
        ws.open(label, &server.head, Some(server.password)),
    )
    .await
    {
        Ok(Ok(plain)) => {
            run_cleanup(label, &plain.session, &sqls, panicked).await;
            plain.close().await;
        }
        Ok(Err(e)) if panicked => eprintln!("{label}: `{sqls:?}` failed: {e}"),
        Err(_) if panicked => eprintln!("{label}: `{sqls:?}` timed out"),
        Ok(Err(e)) => panic!("{label}: `{sqls:?}` failed: {e}"),
        Err(_) => panic!("{label}: `{sqls:?}` timed out"),
    }
}

#[tokio::test]
async fn read_only_servers_run_reads_settings_and_transactions_and_refuse_every_write() {
    let ws = Workspace::new();
    let mut tested = 0;
    for server in servers() {
        let label = server.label;
        let t = unique("read_only").to_lowercase();
        if let Some(prepare) = server.prepare {
            let master = server.head.replace("\"spelaion\"", "\"master\"");
            let plain = ws
                .open(label, &master, Some(server.password))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            run(&plain, prepare).await.unwrap();
            plain.close().await;
        }
        let body = async {
            let plain = ws
                .open(label, &server.head, Some(server.password))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(!plain.session.is_read_only(), "{label}");
            run(&plain, &(server.create)(&t))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            // The same write asks for a confirmation on a writable session, never a refusal.
            assert!(
                !run_risks(
                    &plain.session,
                    &format!("delete from {t}"),
                    &Safeguards::default()
                )
                .is_empty(),
                "{label}"
            );
            assert!(matches!(
                check_run(
                    &plain.session,
                    &format!("delete from {t}"),
                    &Safeguards::default(),
                    false
                ),
                Err(RunRefused::NeedsConfirmation(_))
            ));

            // A top-level key, before any table (SQL Server's `[tls]`).
            let read_only = server.head.replacen(
                "schema_version = 1\n",
                "schema_version = 1\nread_only = true\n",
                1,
            );
            // Opening twice: a reconnect is a new session, read-only again.
            for attempt in ["first", "reconnect"] {
                let open = ws
                    .open(label, &read_only, Some(server.password))
                    .await
                    .unwrap_or_else(|e| panic!("{label} {attempt}: {e}"));
                assert!(open.session.is_read_only(), "{label}");
                assert_eq!(
                    value(&open, &format!("select v from {t} where id = 1")).await,
                    "10",
                    "{label} {attempt}"
                );
                for sql in &server.settings {
                    run(&open, sql)
                        .await
                        .unwrap_or_else(|e| panic!("{label} {attempt}: {sql}: {e}"));
                }
                let transaction = (server.transaction)(&t);
                run(&open, &transaction)
                    .await
                    .unwrap_or_else(|e| panic!("{label} {attempt}: {transaction}: {e}"));
                for sql in (server.writes)(&t) {
                    refused(label, &open, &sql).await;
                }
                // The refusal names the statement: the second one here, never the first.
                let message = refused(
                    label,
                    &open,
                    &format!("select 1;\ndelete from {t} where id = 1"),
                )
                .await;
                assert!(message.contains("statement 2"), "{label}: {message}");
                assert!(message.contains("delete from"), "{label}: {message}");

                if label == "mysql" || label == "mariadb" {
                    // A statement written only as executable comments is a statement (the
                    // server runs it, as a dump's views and triggers): refused like any write,
                    // MariaDB's marker too.
                    for sql in [
                        format!("/*!50000 delete from {t} */"),
                        format!("/*M!100000 delete from {t} */"),
                        "/*!50001 CREATE ALGORITHM=UNDEFINED */ /*!50001 VIEW ro_v AS select 1 AS a */"
                            .to_string(),
                    ] {
                        let message = refused(label, &open, &sql).await;
                        assert!(message.contains("executable comment"), "{label}: {message}");
                    }
                }
                if server.engine_mode {
                    // Around the core's check, the engine refuses the write itself.
                    let error = engine_error(&open, &format!("insert into {t} values (3, 30)"))
                        .await
                        .unwrap_or_else(|| panic!("{label}: the engine let a write through"));
                    assert!(
                        error.to_lowercase().contains("read-only")
                            || error.to_lowercase().contains("read only"),
                        "{label}: {error}"
                    );
                }
                open.close().await;
            }

            // Nothing changed.
            assert_eq!(
                value(&plain, &format!("select v from {t} where id = 1")).await,
                "10",
                "{label}"
            );
            assert_eq!(
                value(&plain, &format!("select count(*) from {t}")).await,
                "1",
                "{label}"
            );
            assert_eq!(
                value(
                    &plain,
                    &format!(
                        "select count(*) from information_schema.tables where table_name in ('{t}_new', '{t}_copy')"
                    )
                )
                .await,
                "0",
                "{label}"
            );
            plain.close().await;
        };
        cleaning_up(body, |panicked| {
            writable(&ws, &server, vec![(server.drop)(&t)], panicked)
        })
        .await;
        tested += 1;
    }
    eprintln!("{tested} servers tested");
}

#[tokio::test]
async fn postgres_read_only_mode_stops_a_function_that_writes_and_plans_only_read() {
    let Some(server) = servers().into_iter().find(|s| s.label == "postgres") else {
        return;
    };
    let ws = Workspace::new();
    let t = unique("read_only_pg");
    let body = async {
        let plain = ws
            .open("pg", &server.head, Some(server.password))
            .await
            .unwrap();
        run(&plain, &(server.create)(&t)).await.unwrap();
        plain.close().await;

        // A setup and an isolation level still apply, after the read-only mode.
        let content = format!(
            "{}read_only = true\n\n[session]\nsetup = \"SET statement_timeout = 5000\"\nisolation = \"repeatable-read\"\n",
            server.head
        );
        let open = ws
            .open("pg", &content, Some(server.password))
            .await
            .unwrap();
        assert_eq!(
            value(&open, "show default_transaction_read_only").await,
            "on"
        );
        assert_eq!(
            value(&open, "show transaction_isolation").await,
            "repeatable read"
        );
        assert_eq!(value(&open, "show statement_timeout").await, "5s");
        // `nextval` looks like a read, so it passes the check; the engine stops it.
        let error = run(&open, &format!("select nextval('{t}_seq')"))
            .await
            .expect_err("nextval wrote on a read-only connection");
        assert!(error.contains("read-only transaction"), "{error}");
        assert!(!error.contains("so nothing was run"), "{error}");
        // A transaction the user opens is read-only too.
        run(&open, "begin").await.unwrap();
        assert_eq!(value(&open, "show transaction_read_only").await, "on");
        run(&open, "rollback").await.unwrap();

        // Plans: estimated reads only.
        let policy = ExplainPolicy::for_connection(&open);
        explain(
            &open.session,
            &format!("select v from {t}"),
            ExplainOptions::default(),
            &policy,
            None,
            |_| {},
        )
        .await
        .expect("an estimated read plan");
        for sql in [
            format!("delete from {t}"),
            format!("update {t} set v = 0"),
            format!("insert into {t} values (5, 50)"),
        ] {
            let error = explain(
                &open.session,
                &sql,
                ExplainOptions {
                    confirmed: true,
                    ..Default::default()
                },
                &policy,
                None,
                |_| {},
            )
            .await
            .expect_err("a plan of a write on a read-only connection");
            assert!(error.to_string().contains("read-only"), "{sql}: {error}");
        }
        open.close().await;
    };
    cleaning_up(body, |panicked| {
        writable(&ws, &server, vec![(server.drop)(&t)], panicked)
    })
    .await;
}

#[tokio::test]
async fn mysql_read_only_mode_is_set_before_the_setup() {
    for server in servers()
        .into_iter()
        .filter(|s| s.label == "mysql" || s.label == "mariadb")
    {
        let ws = Workspace::new();
        let label = server.label;
        let content = format!(
            "{}read_only = true\n\n[session]\nsetup = \"set time_zone = '+00:00'\"\nisolation = \"read-committed\"\n",
            server.head
        );
        let open = ws
            .open(label, &content, Some(server.password))
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            value(&open, "select @@session.transaction_read_only").await,
            "1",
            "{label}"
        );
        assert_eq!(
            value(&open, "select @@session.transaction_isolation").await,
            "READ-COMMITTED",
            "{label}"
        );
        assert_eq!(
            value(&open, "select @@session.time_zone").await,
            "+00:00",
            "{label}"
        );
        open.close().await;
    }
}

#[tokio::test]
async fn read_only_sqlite_and_duckdb_files_refuse_writes_in_the_core_too() {
    let ws = Workspace::new();
    let lite = ws.file("read_only.sqlite");
    let duck = ws.file("read_only.duckdb");
    std::fs::write(&lite, b"").unwrap();
    spelaion_driver_duckdb::create_database(std::path::Path::new(&duck)).unwrap();
    for (label, head) in [
        (
            "sqlite",
            format!("schema_version = 1\nengine = \"sqlite\"\ndatabase = \"{lite}\"\n"),
        ),
        (
            "duckdb",
            format!("schema_version = 1\nengine = \"duckdb\"\ndatabase = \"{duck}\"\n"),
        ),
    ] {
        let plain = ws.open(label, &head, None).await.unwrap();
        run(
            &plain,
            "create table t (id int primary key, v int); insert into t values (1, 10)",
        )
        .await
        .unwrap();
        plain.close().await;
        let open = ws
            .open(label, &format!("{head}read_only = true\n"), None)
            .await
            .unwrap();
        assert_eq!(value(&open, "select v from t").await, "10", "{label}");
        run(&open, "begin; select count(*) from t; commit")
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        for sql in [
            "insert into t values (2, 20)",
            "update t set v = 0 where id = 1",
            "with d as (delete from t returning *) select * from d",
            "create table u (a int)",
            "attach 'other.db' as other",
            "select 1; delete from t",
        ] {
            refused(label, &open, sql).await;
        }
        open.close().await;
        let plain = ws.open(label, &head, None).await.unwrap();
        assert_eq!(
            value(&plain, "select count(*) from t").await,
            "1",
            "{label}"
        );
        plain.close().await;
    }
}
