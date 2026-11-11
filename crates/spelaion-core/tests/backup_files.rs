//! SQLite and DuckDB backups with their own commands, and running an `.sql` file through a
//! session of ours (ADR-049). SQLite and DuckDB need nothing external; the file runs on
//! PostgreSQL and MySQL need `SPELAION_TEST_PG` and `SPELAION_TEST_MYSQL`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Engine, ResolvedConnection, Resolver, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::backup::{
    BackupCancel, BackupError, BackupEvent, DuckdbExportFormat, EmbeddedDumpRequest, duckdb_import,
    embedded_dump, plan_sql_file, run_sql_file,
};
use spelaion_core::{
    CoreEvent, Engines, OnError, OpenConnection, OpenOptions, ResultStore, Risk, TunnelPool,
    open_connection,
};
use spelaion_driver_api::{Secret, Value};
use spelaion_ssh::{KnownHosts, NonInteractive};

struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "spelaion-backup-files-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("repo/data")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        std::fs::create_dir_all(base.join("out")).unwrap();
        Self { base }
    }

    fn data(&self, name: &str) -> PathBuf {
        self.base.join("repo/data").join(name)
    }

    fn out(&self) -> PathBuf {
        self.base.join("out")
    }

    fn connection(&self, id: &str, toml: &str) {
        let path = self
            .base
            .join(format!("repo/.spelaion/connections/{id}.toml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("schema_version = 1\n{toml}")).unwrap();
    }

    fn resolve(&self, id: &str) -> ResolvedConnection {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving")
    }

    async fn open(&self, id: &str, password: Option<&str>) -> OpenConnection {
        open_connection(
            self.resolve(id),
            &Engines::builtin(),
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: self.base.join("user/known_hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: password.map(Secret::new),
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        )
        .await
        .expect("opening")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

async fn exec(conn: &OpenConnection, script: &str) {
    let events: Vec<CoreEvent> = conn.session.run_script(script).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{script}: {error:?}");
    }
}

async fn value(conn: &OpenConnection, sql: &str) -> String {
    let events: Vec<CoreEvent> = conn.session.run_script(sql).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{sql}: {error:?}");
    }
    let result = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("no result");
    let stored = conn.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    Value::from_array(
        batch.column(0).as_ref(),
        0,
        stored.schema().columns[0].logical,
    )
    .unwrap()
    .to_string()
}

#[derive(Default)]
struct Log(Mutex<Vec<BackupEvent>>);

impl Log {
    fn sink(&self) -> impl Fn(BackupEvent) + Send + Sync + '_ {
        move |e| self.0.lock().unwrap().push(e)
    }

    fn last_statements(&self) -> Option<(u64, u64)> {
        self.0.lock().unwrap().iter().rev().find_map(|e| match e {
            BackupEvent::Statements { done, total } => Some((*done, *total)),
            _ => None,
        })
    }

    fn lines(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                BackupEvent::Log { line } => Some(line.clone()),
                _ => None,
            })
            .collect()
    }
}

fn no_part_files(folder: &Path) {
    for entry in std::fs::read_dir(folder).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        assert!(!name.ends_with(".part"), "{name} was left behind");
    }
}

#[tokio::test]
async fn sqlite_is_copied_with_vacuum_into_even_read_only() {
    let ws = Workspace::new("sqlite");
    std::fs::write(ws.data("shop.db"), "").unwrap();
    ws.connection("shop", "engine = \"sqlite\"\ndatabase = \"data/shop.db\"\n");
    ws.connection(
        "shop_ro",
        "engine = \"sqlite\"\ndatabase = \"data/shop.db\"\nread_only = true\n",
    );
    let conn = ws.open("shop", None).await;
    exec(
        &conn,
        "create table items (id integer primary key, name text); insert into items (name) values ('it''s'), ('two'), (null);",
    )
    .await;
    // A read: allowed on a read-only connection, and its own folder name with a quote is fine.
    let folder = ws.out().join("it's here");
    let read_only = ws.open("shop_ro", None).await;
    let request = EmbeddedDumpRequest {
        folder: folder.clone(),
        name_pattern: "{database}_copy".into(),
        duckdb_format: DuckdbExportFormat::Parquet,
    };
    let log = Log::default();
    let summary = embedded_dump(
        &read_only.session,
        &read_only.resolved,
        &request,
        &BackupCancel::new(),
        &log.sink(),
    )
    .await
    .unwrap();
    assert_eq!(summary.path, folder.join("shop_copy.sqlite"));
    assert!(summary.bytes > 0);
    no_part_files(&folder);
    // The same name again is refused, not overwritten.
    let again = embedded_dump(
        &read_only.session,
        &read_only.resolved,
        &request,
        &BackupCancel::new(),
        &|_| {},
    )
    .await;
    assert!(matches!(again, Err(BackupError::Invalid(_))), "{again:?}");

    std::fs::copy(&summary.path, ws.data("copy.db")).unwrap();
    ws.connection("copy", "engine = \"sqlite\"\ndatabase = \"data/copy.db\"\n");
    let copy = ws.open("copy", None).await;
    assert_eq!(value(&copy, "select count(*) from items").await, "3");
    assert_eq!(
        value(&copy, "select name from items where id = 1").await,
        "it's"
    );
}

#[tokio::test]
async fn duckdb_exports_a_folder_and_imports_it_into_another_database() {
    let ws = Workspace::new("duckdb");
    for name in ["source.duckdb", "target.duckdb"] {
        spelaion_core::create_file_database(Engine::Duckdb, &ws.data(name)).unwrap();
    }
    ws.connection(
        "source",
        "engine = \"duckdb\"\ndatabase = \"data/source.duckdb\"\n",
    );
    ws.connection(
        "target",
        "engine = \"duckdb\"\ndatabase = \"data/target.duckdb\"\n",
    );
    ws.connection(
        "target_ro",
        "engine = \"duckdb\"\ndatabase = \"data/target.duckdb\"\nread_only = true\n",
    );
    let source = ws.open("source", None).await;
    exec(
        &source,
        "create table items (id integer, name varchar, price decimal(10,2)); insert into items values (1, 'it''s', 1.50), (2, 'ünï', null);",
    )
    .await;
    for format in [DuckdbExportFormat::Parquet, DuckdbExportFormat::Csv] {
        let summary = embedded_dump(
            &source.session,
            &source.resolved,
            &EmbeddedDumpRequest {
                folder: ws.out(),
                name_pattern: format!("export_{format:?}"),
                duckdb_format: format,
            },
            &BackupCancel::new(),
            &|_| {},
        )
        .await
        .unwrap();
        assert!(summary.path.join("schema.sql").is_file());
        assert!(summary.path.join("load.sql").is_file());
    }
    no_part_files(&ws.out());
    let export = ws.out().join("export_Parquet");

    // Importing writes: refused on a read-only connection and unless confirmed.
    let read_only = ws.open("target_ro", None).await;
    let refused = duckdb_import(
        &read_only.session,
        &read_only.resolved,
        &export,
        true,
        &BackupCancel::new(),
        &|_| {},
    )
    .await;
    assert!(matches!(refused, Err(BackupError::ReadOnly)), "{refused:?}");
    drop(read_only);
    let target = ws.open("target", None).await;
    let unconfirmed = duckdb_import(
        &target.session,
        &target.resolved,
        &export,
        false,
        &BackupCancel::new(),
        &|_| {},
    )
    .await;
    assert!(matches!(unconfirmed, Err(BackupError::NeedsConfirmation)));
    // A folder that is not an export is refused before anything runs.
    let not_export = duckdb_import(
        &target.session,
        &target.resolved,
        &ws.out(),
        true,
        &BackupCancel::new(),
        &|_| {},
    )
    .await;
    assert!(matches!(not_export, Err(BackupError::Invalid(_))));
    duckdb_import(
        &target.session,
        &target.resolved,
        &export,
        true,
        &BackupCancel::new(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(value(&target, "select count(*) from items").await, "2");
    assert_eq!(
        value(&target, "select name from items where id = 1").await,
        "it's"
    );
}

/// A file of statements: a table, rows, and a statement that fails in the middle.
fn write_file(path: &Path, failing: bool) {
    let mut sql = String::from(
        "-- made by a test\ncreate table run_items (id int primary key, name varchar(20));\n",
    );
    for i in 1..=20 {
        sql.push_str(&format!(
            "insert into run_items values ({i}, 'row ''{i}''');\n"
        ));
        if failing && i == 10 {
            sql.push_str("insert into run_items values (1, 'duplicate');\n");
        }
    }
    std::fs::write(path, sql).unwrap();
}

#[tokio::test]
async fn sql_files_run_with_progress_and_are_refused_unconfirmed_or_read_only() {
    let ws = Workspace::new("runfile");
    std::fs::write(ws.data("app.db"), "").unwrap();
    ws.connection("app", "engine = \"sqlite\"\ndatabase = \"data/app.db\"\n");
    ws.connection(
        "app_ro",
        "engine = \"sqlite\"\ndatabase = \"data/app.db\"\nread_only = true\n",
    );
    let file = ws.out().join("load.sql");
    write_file(&file, false);
    let conn = ws.open("app", None).await;

    let plan = plan_sql_file(&conn.session, &file, &conn.resolved.safeguards()).unwrap();
    assert_eq!(plan.statements, 21);
    assert!(plan.risks.is_empty());

    let read_only = ws.open("app_ro", None).await;
    let refused = run_sql_file(
        &read_only.session,
        &file,
        read_only.resolved.params.read_only,
        true,
        OnError::Stop,
        &BackupCancel::new(),
        &|_| {},
    )
    .await;
    assert!(matches!(refused, Err(BackupError::ReadOnly)), "{refused:?}");
    let unconfirmed = run_sql_file(
        &conn.session,
        &file,
        false,
        false,
        OnError::Stop,
        &BackupCancel::new(),
        &|_| {},
    )
    .await;
    assert!(matches!(unconfirmed, Err(BackupError::NeedsConfirmation)));

    let log = Log::default();
    let summary = run_sql_file(
        &conn.session,
        &file,
        false,
        true,
        OnError::Stop,
        &BackupCancel::new(),
        &log.sink(),
    )
    .await
    .unwrap();
    assert_eq!((summary.done, summary.failed, summary.total), (21, 0, 21));
    assert_eq!(log.last_statements(), Some((21, 21)));
    assert_eq!(value(&conn, "select count(*) from run_items").await, "20");
    assert_eq!(
        value(&conn, "select name from run_items where id = 3").await,
        "row '3'"
    );
    // The file's results are not kept.
    let kept = conn.session.results().len();

    // A failure stops the run where it is, or is skipped on demand.
    exec(&conn, "drop table run_items").await;
    let failing = ws.out().join("failing.sql");
    write_file(&failing, true);
    let plan = plan_sql_file(&conn.session, &failing, &conn.resolved.safeguards()).unwrap();
    assert_eq!(plan.statements, 22);
    let log = Log::default();
    let stopped = run_sql_file(
        &conn.session,
        &failing,
        false,
        true,
        OnError::Stop,
        &BackupCancel::new(),
        &log.sink(),
    )
    .await;
    assert!(
        matches!(stopped, Err(BackupError::Failed(_))),
        "{stopped:?}"
    );
    assert_eq!(log.last_statements(), Some((11, 22)));
    assert!(
        log.lines()
            .iter()
            .any(|l| l.starts_with("Statement 12 failed")),
        "{:?}",
        log.lines()
    );
    assert_eq!(value(&conn, "select count(*) from run_items").await, "10");
    exec(&conn, "drop table run_items").await;
    let summary = run_sql_file(
        &conn.session,
        &failing,
        false,
        true,
        OnError::Continue,
        &BackupCancel::new(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!((summary.done, summary.failed), (21, 1));
    assert_eq!(value(&conn, "select count(*) from run_items").await, "20");
    assert!(conn.session.results().len() <= kept + 2);

    // A file the guard would ask about says so in its plan.
    let risky = ws.out().join("risky.sql");
    std::fs::write(&risky, "delete from run_items;\ndrop table run_items;\n").unwrap();
    let plan = plan_sql_file(&conn.session, &risky, &conn.resolved.safeguards()).unwrap();
    assert!(
        plan.risks.contains(&(Risk::DeleteAll, 1)),
        "{:?}",
        plan.risks
    );
    assert!(plan.risks.contains(&(Risk::Drop, 1)), "{:?}", plan.risks);

    // UTF-16 with a BOM is read as the editor reads it.
    let utf16 = ws.out().join("utf16.sql");
    let mut bytes = vec![0xFF, 0xFE];
    for unit in "create table wide (name text); insert into wide values ('ñandú');".encode_utf16()
    {
        bytes.extend(unit.to_le_bytes());
    }
    std::fs::write(&utf16, bytes).unwrap();
    run_sql_file(
        &conn.session,
        &utf16,
        false,
        true,
        OnError::Stop,
        &BackupCancel::new(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(value(&conn, "select name from wide").await, "ñandú");
}

/// The same file on PostgreSQL and MySQL, and a cancel that stops a long statement.
#[tokio::test]
async fn sql_files_run_on_servers_and_stop_on_cancel() {
    let servers = [
        (
            "postgres",
            "SPELAION_TEST_PG",
            "spelaion",
            "spelaion",
            "select pg_sleep(30)",
        ),
        (
            "mysql",
            "SPELAION_TEST_MYSQL",
            "spelaion",
            "spelaion",
            "select sleep(30)",
        ),
    ];
    let mut ran = 0;
    for (engine, var, user, password, sleep) in servers {
        let Ok(addr) = std::env::var(var) else {
            eprintln!("{var} not set: {engine} skipped");
            continue;
        };
        ran += 1;
        let ws = Workspace::new(&format!("runfile-{engine}"));
        let (host, port) = addr.rsplit_once(':').unwrap();
        ws.connection(
            "server",
            &format!(
                "engine = \"{engine}\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"{user}\"\npassword = {{ secret = \"prompt\" }}\n"
            ),
        );
        let conn = ws.open("server", Some(password)).await;
        exec(&conn, "drop table if exists run_items").await;
        let file = ws.out().join("load.sql");
        write_file(&file, false);
        let log = Log::default();
        let summary = run_sql_file(
            &conn.session,
            &file,
            false,
            true,
            OnError::Stop,
            &BackupCancel::new(),
            &log.sink(),
        )
        .await
        .unwrap_or_else(|e| panic!("{engine}: {e}"));
        assert_eq!((summary.done, summary.total), (21, 21), "{engine}");
        assert_eq!(
            value(&conn, "select count(*) from run_items").await,
            "20",
            "{engine}"
        );

        let long = ws.out().join("long.sql");
        std::fs::write(&long, format!("{sleep};\nselect 1;\n")).unwrap();
        let cancel = Arc::new(BackupCancel::new());
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            stopper.cancel().await;
        });
        let cancelled = tokio::time::timeout(
            Duration::from_secs(15),
            run_sql_file(
                &conn.session,
                &long,
                false,
                true,
                OnError::Continue,
                &cancel,
                &|_| {},
            ),
        )
        .await
        .expect("the cancel stopped the run");
        assert!(
            matches!(cancelled, Err(BackupError::Cancelled)),
            "{engine}: {cancelled:?}"
        );
        exec(&conn, "drop table run_items").await;
        conn.close().await;
    }
    if ran == 0 {
        eprintln!("no server variables: test skipped");
    }
}
