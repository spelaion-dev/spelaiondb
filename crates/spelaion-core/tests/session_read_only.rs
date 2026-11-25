//! "Read-only for this session" (SDB-395), end to end on every engine: a writable connection's
//! session made read-only refuses every write before anything is sent, as a read-only
//! connection's does (ADR-053), whatever was confirmed and however the write is spelt (turning
//! the engine's own mode back off included); on PostgreSQL, MySQL and MariaDB the engine's own
//! read-only mode stops a write sent around the check. Turning it off makes the session
//! writable again; a new session starts writable. Nothing loosens a read-only connection: its
//! session cannot be turned writable this way, and stays read-only after any sequence of
//! requests. Nothing changes while a transaction is open.
//!
//! SQLite and DuckDB always run; the servers need `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`,
//! `SPELAION_TEST_MARIADB` and `SPELAION_TEST_MSSQL` (skipped without them, which does not
//! count as tested).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ResultStore, RunRefused, SessionReadOnlyError,
    TunnelPool, check_run, open_connection, set_session_read_only,
};
use spelaion_driver_api::{ExecEvent, ExecOptions, Secret};
use spelaion_ssh::{KnownHosts, NonInteractive};

use crate::engines;
use crate::support;
use support::{cleaning_up, run_cleanup};

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
        let base = std::env::temp_dir().join(unique("spelaion-session-read-only"));
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
    async fn open(&self, id: &str, content: &str, password: Option<&str>) -> OpenConnection {
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
                team_files: Vec::new(),
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
        .unwrap_or_else(|e| panic!("{id}: {e}"))
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
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

/// `sql` is refused before anything runs: by the app's check even confirmed, and by the
/// session's own run without sending a statement.
async fn refused(label: &str, open: &OpenConnection, sql: &str) {
    let refusal_message = if open.session.is_connection_read_only() {
        "the connection is read-only, so nothing was run"
    } else {
        "this session is read-only, so nothing was run"
    };
    match check_run(&open.session, sql, &Safeguards::default(), true) {
        Err(RunRefused::ReadOnly(refusal)) => assert!(
            refusal.to_string().starts_with(refusal_message),
            "{label}: {sql}: {refusal}"
        ),
        other => panic!("{label}: {sql}: the app's check let it through: {other:?}"),
    }
    let events: Vec<CoreEvent> = open.session.run_script(sql).collect().await;
    let mut failed = false;
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => {
                assert!(
                    error.message.contains(refusal_message),
                    "{label}: {sql}: {}",
                    error.message
                );
                failed = true;
            }
            CoreEvent::ResultStarted { .. } | CoreEvent::StatementFinished { .. } => {
                panic!("{label}: {sql}: a statement ran: {events:?}")
            }
            _ => {}
        }
    }
    assert!(failed, "{label}: {sql}: it ran: {events:?}");
}

/// The first error of `sql` sent straight to the driver, around the core's check.
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

/// An engine to test on.
struct Target {
    label: &'static str,
    /// The connection file without `read_only`.
    head: String,
    password: Option<&'static str>,
    /// Runs first on a writable session, when the database may be missing.
    prepare: Option<(String, &'static str)>,
    /// Writes in every shape, each of which must be refused.
    writes: fn(&str) -> Vec<String>,
    /// A transaction around a read.
    transaction: fn(&str) -> String,
    /// The engine has a read-only mode of its own that the session sets.
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
        format!("select 1; delete from {t} where id = 1"),
        "this is not sql".into(),
    ]
}

/// SQLite's and DuckDB's files, which their drivers create (a connection opens one that
/// exists).
async fn create_files(workspace: &Workspace) {
    engines::ensure_database(
        spelaion_config::Engine::Sqlite,
        &workspace.base.join("lite.db"),
    )
    .await;
    engines::ensure_database(
        spelaion_config::Engine::Duckdb,
        &workspace.base.join("duck.duckdb"),
    )
    .await;
}

fn targets(workspace: &Workspace) -> Vec<Target> {
    let mut out = vec![
        Target {
            label: "sqlite",
            head: format!(
                "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"{}\"\n",
                workspace.file("lite.db")
            ),
            password: None,
            prepare: None,
            writes: |t| {
                let mut out = common_writes(t);
                out.extend([
                    format!("replace into {t} values (1, 0)"),
                    format!("attach database '{t}.db' as other"),
                    "vacuum".into(),
                ]);
                out
            },
            transaction: |t| format!("begin; select v from {t}"),
            engine_mode: false,
        },
        Target {
            label: "duckdb",
            head: format!(
                "schema_version = 1\nengine = \"duckdb\"\ndatabase = \"{}\"\n",
                workspace.file("duck.duckdb")
            ),
            password: None,
            prepare: None,
            writes: |t| {
                let mut out = common_writes(t);
                out.extend([format!("copy {t} to '{t}.csv'")]);
                out
            },
            transaction: |t| format!("begin; select v from {t}"),
            engine_mode: false,
        },
    ];
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
        out.push(Target {
            label: "postgres",
            head: format!(
                "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\n"
            ),
            password: Some("spelaion"),
            prepare: None,
            writes: |t| {
                let mut out = common_writes(t);
                out.extend([
                    format!("with d as (delete from {t} returning *) select * from d"),
                    format!("select * into {t}_copy from {t}"),
                    format!("select * from {t} for update"),
                    format!("do $$ begin delete from {t}; end $$"),
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE".into(),
                    "begin read write".into(),
                    "set default_transaction_read_only = off".into(),
                    "reset all".into(),
                    "select set_config('default_transaction_read_only', 'off', false)".into(),
                ]);
                out
            },
            transaction: |t| format!("begin; select v from {t}"),
            engine_mode: true,
        });
    }
    for (var, engine) in [
        ("SPELAION_TEST_MYSQL", "mysql"),
        ("SPELAION_TEST_MARIADB", "mariadb"),
    ] {
        if let Some((host, port)) = addr(var) {
            out.push(Target {
                label: engine,
                head: format!(
                    "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"root\"\npassword = {{ secret = \"prompt\" }}\n"
                ),
                password: Some("spelaion-root"),
                prepare: None,
                writes: |t| {
                    let mut out = common_writes(t);
                    out.extend([
                        format!("replace into {t} values (1, 0)"),
                        format!("select v into @v from {t} where id = 1"),
                        format!("/*!50000 delete from {t} */ select 1"),
                        format!("lock tables {t} write"),
                        "SET SESSION TRANSACTION READ WRITE".into(),
                        "start transaction read write".into(),
                        "set transaction_read_only = 0".into(),
                        "SET @@session.transaction_read_only = OFF".into(),
                    ]);
                    out
                },
                transaction: |t| format!("start transaction; select v from {t}"),
                engine_mode: true,
            });
        }
    }
    if let Some((host, port)) = addr("SPELAION_TEST_MSSQL") {
        let head = |database: &str| {
            format!(
                "schema_version = 1\nengine = \"sqlserver\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"{database}\"\nuser = \"sa\"\npassword = {{ secret = \"prompt\" }}\n[tls]\nmode = \"require\"\n"
            )
        };
        out.push(Target {
            label: "sqlserver",
            head: head("spelaion"),
            password: Some("Spelaion-test-1"),
            prepare: Some((
                head("master"),
                "if db_id('spelaion') is null create database spelaion",
            )),
            writes: |t| {
                let mut out = common_writes(t);
                out.extend([
                    format!("merge {t} as d using {t} as s on d.id = s.id when matched then update set v = 0;"),
                    format!("select * into {t}_copy from {t}"),
                    format!("exec('delete from {t}')"),
                ]);
                out
            },
            transaction: |t| format!("begin transaction; select v from {t}"),
            engine_mode: false,
        });
    }
    out
}

#[tokio::test]
async fn a_session_made_read_only_refuses_writes_on_every_engine_and_can_be_turned_back() {
    let _turn = crate::serial::file_turn(module_path!());
    let workspace = Workspace::new();
    create_files(&workspace).await;
    let mut tested = Vec::new();
    for target in targets(&workspace) {
        let label = target.label;
        let t = unique("session_ro");
        if let Some((head, sql)) = &target.prepare {
            let master = workspace
                .open(&format!("{label}-prepare"), head, target.password)
                .await;
            run(&master, sql).await.unwrap();
            master.close().await;
        }
        let open = workspace.open(label, &target.head, target.password).await;
        // An editor tab's session: it follows its transaction (ADR-031).
        open.session
            .follow_transactions(&Safeguards::default())
            .await;
        run(
            &open,
            &format!("create table {t} (id int primary key, v int)"),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        let body = async {
            run(&open, &format!("insert into {t} values (1, 10)"))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(!open.session.is_read_only(), "{label}");

            set_session_read_only(&open, true)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(open.session.is_read_only(), "{label}");
            assert!(open.session.is_session_read_only(), "{label}");
            assert!(!open.session.is_connection_read_only(), "{label}");
            // Reads still run.
            run(&open, &format!("select v from {t}"))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            for sql in (target.writes)(&t) {
                refused(label, &open, &sql).await;
            }
            // The engine's own mode stops what gets around the check.
            if target.engine_mode {
                let error = engine_error(&open, &format!("insert into {t} values (3, 30)")).await;
                assert!(error.is_some(), "{label}: the engine let a write through");
            }
            // Asking again changes nothing.
            set_session_read_only(&open, true).await.unwrap();
            assert!(open.session.is_read_only(), "{label}");

            // Nothing changes while a transaction is open.
            run(&open, &(target.transaction)(&t))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let status = open.session.transaction_status();
            assert!(
                !status.supported || status.in_transaction(),
                "{label}: an editor session sees its transaction"
            );
            if status.in_transaction() {
                assert_eq!(
                    set_session_read_only(&open, false).await,
                    Err(SessionReadOnlyError::TransactionOpen),
                    "{label}"
                );
                assert!(open.session.is_read_only(), "{label}");
                run(&open, "rollback").await.unwrap();
            }

            // Turned off: writable again, the engine's mode too.
            set_session_read_only(&open, false)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(!open.session.is_read_only(), "{label}");
            run(&open, &format!("insert into {t} values (2, 20)"))
                .await
                .unwrap_or_else(|e| panic!("{label}: writable again: {e}"));
            if target.engine_mode {
                assert_eq!(
                    engine_error(&open, &format!("insert into {t} values (4, 40)")).await,
                    None,
                    "{label}: the engine is writable again"
                );
            }
            // A transaction is open: it cannot be turned on either.
            run(&open, &(target.transaction)(&t)).await.unwrap();
            if open.session.transaction_status().in_transaction() {
                assert_eq!(
                    set_session_read_only(&open, true).await,
                    Err(SessionReadOnlyError::TransactionOpen),
                    "{label}"
                );
                assert!(!open.session.is_read_only(), "{label}");
                run(&open, "rollback").await.unwrap();
            }

            // Read-only again, then a new session of the same connection: it starts writable,
            // as the connection says.
            set_session_read_only(&open, true).await.unwrap();
            let again = workspace.open(label, &target.head, target.password).await;
            assert!(!again.session.is_read_only(), "{label}");
            run(&again, &format!("delete from {t} where id = 2"))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            again.close().await;
            set_session_read_only(&open, false).await.unwrap();
        };
        let drop = vec![format!("drop table {t}")];
        let open_ref = &open;
        cleaning_up(body, |panicked| async move {
            if open_ref.session.is_session_read_only() {
                let _ = set_session_read_only(open_ref, false).await;
            }
            run_cleanup(label, &open_ref.session, &drop, panicked).await
        })
        .await;
        open.close().await;
        tested.push(label);
    }
    eprintln!("tested on {tested:?}");
}

#[tokio::test]
async fn nothing_makes_a_read_only_connections_session_writable() {
    let _turn = crate::serial::file_turn(module_path!());
    let workspace = Workspace::new();
    create_files(&workspace).await;
    let mut tested = Vec::new();
    for target in targets(&workspace) {
        let label = target.label;
        let t = unique("connection_ro");
        if let Some((head, sql)) = &target.prepare {
            let master = workspace
                .open(&format!("{label}-prepare"), head, target.password)
                .await;
            run(&master, sql).await.unwrap();
            master.close().await;
        }
        // The table, from a writable session; SQLite's and DuckDB's file must exist before a
        // read-only connection opens it.
        let writer = workspace
            .open(&format!("{label}-writer"), &target.head, target.password)
            .await;
        run(
            &writer,
            &format!("create table {t} (id int primary key, v int)"),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        writer.close().await;
        // A top-level key, before any table (SQL Server's `[tls]`).
        let content = target.head.replacen(
            "schema_version = 1\n",
            "schema_version = 1\nread_only = true\n",
            1,
        );
        let open = workspace
            .open(&format!("{label}-ro"), &content, target.password)
            .await;
        let body = async {
            assert!(open.session.is_connection_read_only(), "{label}");
            // Turning it off is refused, before and after turning it on.
            assert_eq!(
                set_session_read_only(&open, false).await,
                Ok(()),
                "{label}: nothing to turn off yet"
            );
            assert!(open.session.is_read_only(), "{label}");
            set_session_read_only(&open, true)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(
                set_session_read_only(&open, false).await,
                Err(SessionReadOnlyError::ConnectionReadOnly),
                "{label}"
            );
            for _ in 0..3 {
                let _ = set_session_read_only(&open, true).await;
                let _ = set_session_read_only(&open, false).await;
            }
            assert!(open.session.is_read_only(), "{label}");
            for sql in (target.writes)(&t) {
                refused(label, &open, &sql).await;
            }
            if target.engine_mode {
                let error = engine_error(&open, &format!("insert into {t} values (1, 1)")).await;
                assert!(error.is_some(), "{label}: the engine let a write through");
            }
        };
        let drop = vec![format!("drop table {t}")];
        let open_ref = &open;
        let workspace_ref = &workspace;
        let head = target.head.clone();
        let password = target.password;
        cleaning_up(body, |panicked| async move {
            // A file is in the workspace's folder, removed with it; DuckDB would not open it
            // writable while the read-only connection holds it anyway.
            if password.is_none() {
                return;
            }
            let cleaner = workspace_ref
                .open(&format!("{label}-cleaner"), &head, password)
                .await;
            run_cleanup(label, &cleaner.session, &drop, panicked).await;
            cleaner.close().await;
            let _ = open_ref;
        })
        .await;
        open.close().await;
        tested.push(label);
    }
    eprintln!("tested on {tested:?}");
}
