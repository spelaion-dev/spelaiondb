//! Servers that speak MySQL's protocol, through the MySQL connection and its plugin (SDB-170),
//! each against its own image in `testing/docker/mysql-family`:
//!
//! - TiDB (`SPELAION_TEST_TIDB`), OceanBase (`SPELAION_TEST_OCEANBASE`) and SingleStore
//!   (`SPELAION_TEST_SINGLESTORE`) connect, browse, follow transactions, cancel with
//!   `KILL QUERY` and bind a read's values;
//! - OceanBase keeps read-only connections and agents' reads from writing with its own
//!   barriers; TiDB and SingleStore are recognised and get neither;
//! - StarRocks (`SPELAION_TEST_STARROCKS`) does not say what it is: its read-only connection is
//!   refused because the server says its session is still writable;
//! - Databend (`SPELAION_TEST_DATABEND_MYSQL`) accepts MySQL's read-only transaction and session and
//!   writes anyway: it is recognised by its version and gets neither (SDB-350).
//! - GreptimeDB (`SPELAION_TEST_GREPTIME_MYSQL`, the main project's `greptime` service, which CI
//!   runs) is browsed, keyed by its time index, edited by that key and cancelled (SDB-344), and
//!   its ignored transactions get it neither barrier (SDB-343).
//!
//! Each variable is `host:port`; a server without one is skipped, which does not count as
//! tested.

mod engines;
mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::{
    CellInput, CoreEvent, CsvOptions, DEFAULT_BATCH_ROWS, EditError, Engines, ExportCancel,
    ExportError, ExportFormat, OpenConnection, OpenError, OpenOptions, ReadError, ReadLimits,
    ResultId, ResultStore, RowChange, ServerFamily, StreamExport, TablePage, TextEncoding,
    TunnelPool, apply_edits, edit_statements, export_query, open_connection, query_pages_exactly,
    table_page_sql_by_key,
};
use spelaion_driver_api::{
    ExecEvent, ExecOptions, Secret, TransactionState, Value, bound_placeholder,
};
use spelaion_ssh::{KnownHosts, NonInteractive};

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// A server of the family, as its image starts.
struct Server {
    label: &'static str,
    var: &'static str,
    password: Option<&'static str>,
    family: ServerFamily,
    /// What a table of its needs after its columns.
    table_options: &'static str,
    database: Option<&'static str>,
}

const TIDB: Server = Server {
    label: "TiDB",
    var: "SPELAION_TEST_TIDB",
    password: None,
    family: ServerFamily::TiDb,
    table_options: "",
    database: None,
};
const OCEANBASE: Server = Server {
    label: "OceanBase",
    var: "SPELAION_TEST_OCEANBASE",
    password: None,
    family: ServerFamily::OceanBase,
    table_options: "",
    database: None,
};
const SINGLESTORE: Server = Server {
    label: "SingleStore",
    var: "SPELAION_TEST_SINGLESTORE",
    password: Some("spelaion"),
    family: ServerFamily::SingleStore,
    table_options: "",
    database: None,
};
// StarRocks says it is MySQL: only reading its session back tells it apart.
const STARROCKS: Server = Server {
    label: "StarRocks",
    var: "SPELAION_TEST_STARROCKS",
    password: None,
    family: ServerFamily::MySql,
    table_options: " properties ('replication_num' = '1')",
    database: None,
};

const GREPTIME: Server = Server {
    label: "GreptimeDB",
    var: "SPELAION_TEST_GREPTIME_MYSQL",
    password: None,
    family: ServerFamily::GreptimeDb,
    table_options: "",
    database: Some("public"),
};

// Databend says it is MySQL 8.0.90; the rest of its version tells it apart.
const DATABEND: Server = Server {
    label: "Databend",
    var: "SPELAION_TEST_DATABEND_MYSQL",
    password: None,
    family: ServerFamily::Databend,
    table_options: "",
    database: Some("default"),
};

/// The servers that connect and browse through the MySQL plugin.
const CHECKED: [&Server; 3] = [&TIDB, &OCEANBASE, &SINGLESTORE];

impl Server {
    fn addr(&self) -> Option<(String, String)> {
        match std::env::var(self.var) {
            Ok(addr) => {
                let (host, port) = addr.rsplit_once(':').expect("host:port");
                Some((host.to_string(), port.to_string()))
            }
            Err(_) => {
                eprintln!("{} not set: {} skipped", self.var, self.label);
                None
            }
        }
    }

    /// The connection file as the picker's kind writes it: a MySQL connection.
    fn file(&self, read_only: bool) -> Option<String> {
        let (host, port) = self.addr()?;
        let password = match self.password {
            Some(_) => "password = { secret = \"prompt\" }\n",
            None => "",
        };
        let database = self
            .database
            .map_or(String::new(), |name| format!("database = \"{name}\"\n"));
        Some(format!(
            "schema_version = 1\nengine = \"mysql\"\nhost = \"{host}\"\nport = {port}\n\
             user = \"root\"\n{password}{database}tls = {{ mode = \"disable\" }}\nread_only = {read_only}\n"
        ))
    }
}

/// A temporary, trusted repository whose connections the tests write.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base = std::env::temp_dir().join(unique("spelaion-mysql-family"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    async fn open(
        &self,
        server: &Server,
        read_only: bool,
        read_only_session: bool,
    ) -> Option<Result<OpenConnection, OpenError>> {
        let content = server.file(read_only)?;
        let id = unique("conn");
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
                path: id,
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
            password: server.password.map(Secret::new),
            read_only_session,
        };
        Some(
            open_connection(
                resolved,
                &Engines::builtin(),
                options,
                Arc::new(ResultStore::new()),
            )
            .await,
        )
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Runs `sql` as the user would; `Err` with the first error.
async fn run(open: &OpenConnection, sql: &str) -> Result<(), String> {
    let mut events = open.session.run_script(sql);
    while let Some(event) = events.next().await {
        if let CoreEvent::StatementFailed { error, .. } = event {
            return Err(error.message);
        }
    }
    Ok(())
}

/// The first error of `sql` sent straight to the driver, around the core's checks.
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

fn csv() -> StreamExport {
    StreamExport {
        format: ExportFormat::Csv,
        header: true,
        csv: CsvOptions {
            delimiter: ';',
            null_text: "NULL".into(),
            escape_formulas: true,
        },
        encoding: TextEncoding::Utf8,
        columns: None,
        batch_size: DEFAULT_BATCH_ROWS,
    }
}

async fn export(open: &OpenConnection, sql: &str) -> Result<u64, ExportError> {
    let path = std::env::temp_dir().join(format!("{}.csv", unique("sdb-mysql-family")));
    let result = export_query(
        &open.session,
        sql,
        &Safeguards::default(),
        false,
        &path,
        &csv(),
        &ExportCancel::new(),
        &|_| {},
    )
    .await;
    let _ = std::fs::remove_file(&path);
    result.map(|summary| summary.rows)
}

/// The explorer's calls, transactions, `KILL QUERY` and a bound read on each server.
#[tokio::test]
async fn checked_servers_connect_browse_and_cancel() {
    for server in CHECKED {
        let ws = Workspace::new();
        let Some(open) = ws.open(server, false, false).await else {
            continue;
        };
        let label = server.label;
        let open = open.unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            open.session.server_family(),
            Some(&server.family),
            "{label}"
        );
        let db = unique("sdb170");
        let options = server.table_options;
        run(
            &open,
            &format!(
                "create database {db};
                 create table {db}.parent (id int primary key, name varchar(20)){options};
                 create table {db}.child (id int primary key, parent int){options};
                 create view {db}.v as select id, name from {db}.parent;
                 insert into {db}.parent values (1, 'one'), (2, 'two');"
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        let checks = async {
            let conn = open.session.connection();
            assert!(
                conn.list_databases().await.unwrap().contains(&db),
                "{label}"
            );
            let objects = conn.list_objects(&db).await.unwrap();
            for name in ["parent", "child", "v"] {
                assert!(
                    objects.iter().any(|o| o.name == name),
                    "{label}: {name} in {objects:?}"
                );
            }
            // SingleStore has no CHECK catalog: the table is read without one.
            let child = conn.describe_table(&db, "child").await.unwrap();
            let columns: Vec<_> = child.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(columns, ["id", "parent"], "{label}");
            assert_eq!(child.primary_key, ["id"], "{label}");
            let all = conn.list_columns(&db).await.unwrap();
            assert!(
                all.iter().any(|c| c.table == "parent" && c.name == "name"),
                "{label}"
            );
            // SingleStore has no `mysql` database: it lists no accounts.
            conn.list_server_objects().await.unwrap();
            let view = conn.relation_definition(&db, "v").await.unwrap();
            assert!(
                view.to_ascii_uppercase().contains("VIEW"),
                "{label}: {view}"
            );

            // A read with a bound value, as the data search runs it.
            let mut events = conn
                .execute_bound(
                    &format!(
                        "select id from {db}.parent where name = {}",
                        bound_placeholder("mysql", 1)
                    ),
                    vec!["two".into()],
                    &ExecOptions::default(),
                )
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let mut rows = 0;
            while let Some(event) = events.next().await {
                match event {
                    Ok(ExecEvent::Batch(batch)) => rows += batch.num_rows(),
                    Ok(_) => {}
                    Err(e) => panic!("{label}: {e}"),
                }
            }
            assert_eq!(rows, 1, "{label}");

            run(&open, "start transaction").await.unwrap();
            assert_eq!(
                conn.transaction_state().await.unwrap(),
                TransactionState::Open,
                "{label}"
            );
            run(&open, "rollback").await.unwrap();
            assert_eq!(
                conn.transaction_state().await.unwrap(),
                TransactionState::Idle,
                "{label}"
            );

            // `KILL QUERY` from a second connection, sent again until the run ends.
            let session = &open.session;
            let started = Instant::now();
            let sleep = session
                .run_script("select sleep(30)")
                .collect::<Vec<CoreEvent>>();
            tokio::pin!(sleep);
            tokio::time::timeout(Duration::from_secs(20), async {
                loop {
                    tokio::select! {
                        events = &mut sleep => break events,
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {
                            session.cancel().await.unwrap_or_else(|e| panic!("{label}: {e}"));
                        }
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{label}: the cancel did not stop the statement"));
            assert!(started.elapsed() < Duration::from_secs(20), "{label}");
        };
        checks.await;
        run(&open, &format!("drop database {db}"))
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        open.close().await;
    }
}

/// OceanBase enforces MySQL's read-only session and transaction: read-only connections,
/// agents' reads and streaming exports run, and a write sent around the core's check fails.
#[tokio::test]
async fn oceanbase_keeps_reads_from_writing() {
    let ws = Workspace::new();
    let Some(owner) = ws.open(&OCEANBASE, false, false).await else {
        return;
    };
    let owner = owner.unwrap();
    let db = unique("sdb170_ro");
    run(
        &owner,
        &format!("create database {db}; create table {db}.t (id int primary key)"),
    )
    .await
    .unwrap();

    let ro = ws.open(&OCEANBASE, true, false).await.unwrap().unwrap();
    let error = engine_error(&ro, &format!("insert into {db}.t values (1)"))
        .await
        .expect("the read-only session wrote");
    assert!(error.contains("READ ONLY"), "{error}");
    ro.close().await;

    let agent = ws.open(&OCEANBASE, true, true).await.unwrap().unwrap();
    let read = agent
        .session
        .run_read_only(&format!("select id from {db}.t"), &ReadLimits::default())
        .await
        .unwrap();
    assert!(read.rows.is_empty());
    agent.close().await;

    assert_eq!(
        export(&owner, &format!("select id from {db}.t"))
            .await
            .unwrap(),
        0
    );
    run(&owner, "start transaction read only").await.unwrap();
    let error = engine_error(&owner, &format!("insert into {db}.t values (1)"))
        .await
        .expect("START TRANSACTION READ ONLY let an INSERT through");
    assert!(error.contains("READ ONLY"), "{error}");
    run(&owner, "rollback").await.unwrap();

    run(&owner, &format!("drop database {db}")).await.unwrap();
    owner.close().await;
}

/// TiDB, SingleStore, GreptimeDB and Databend are recognised and get no read-only connection,
/// no agents' reads and no streaming reads; StarRocks, which says it is MySQL, has its read-only connection
/// refused because its session stays writable.
#[tokio::test]
async fn servers_without_a_barrier_get_no_read_only_mode_nor_agents() {
    for server in [&TIDB, &SINGLESTORE, &GREPTIME, &DATABEND] {
        let ws = Workspace::new();
        let Some(read_only) = ws.open(server, true, false).await else {
            continue;
        };
        let label = server.label;
        match read_only {
            Err(OpenError::NoReadOnlyBarrier(reason)) => {
                assert!(reason.contains(label), "{label}: {reason}");
                assert!(
                    reason.contains("SET SESSION TRANSACTION READ ONLY"),
                    "{label}: {reason}"
                );
            }
            Err(other) => panic!("{label}: {other}"),
            Ok(_) => panic!("{label}: a read-only connection opened"),
        }
        match ws.open(server, false, true).await.unwrap() {
            Err(OpenError::NoReadOnlyBarrier(reason)) => {
                assert!(
                    reason.contains("START TRANSACTION READ ONLY"),
                    "{label}: {reason}"
                )
            }
            Err(other) => panic!("{label}: {other}"),
            Ok(_) => panic!("{label}: a session that only reads opened"),
        }
        let open = ws.open(server, false, false).await.unwrap().unwrap();
        assert_eq!(
            open.session.server_family(),
            Some(&server.family),
            "{label}"
        );
        match open
            .session
            .run_read_only("select 1", &ReadLimits::default())
            .await
        {
            Err(ReadError::Unsupported(reason)) => assert!(reason.contains(label), "{label}"),
            other => panic!("{label}: {other:?}"),
        }
        match export(&open, "select 1").await {
            Err(ExportError::Invalid(reason)) => {
                assert!(
                    reason.contains("does not enforce read-only transactions"),
                    "{label}"
                )
            }
            other => panic!("{label}: {other:?}"),
        }
        open.close().await;
    }

    let ws = Workspace::new();
    if let Some(read_only) = ws.open(&STARROCKS, true, false).await {
        match read_only {
            Err(OpenError::NoReadOnlyBarrier(reason)) => {
                assert!(reason.contains("its session is still writable"), "{reason}")
            }
            Err(other) => panic!("StarRocks: {other}"),
            Ok(_) => panic!("StarRocks: a read-only connection opened"),
        }
        // Agents' reads open read-only connections: refused the same way.
        assert!(matches!(
            ws.open(&STARROCKS, true, true).await.unwrap(),
            Err(OpenError::NoReadOnlyBarrier(_))
        ));
        let open = ws.open(&STARROCKS, false, false).await.unwrap().unwrap();
        assert_eq!(open.session.server_family(), Some(&STARROCKS.family));
        // StarRocks has no `START TRANSACTION READ ONLY`: the export fails before it reads.
        assert!(export(&open, "select 1").await.is_err());
        open.close().await;
    }
}

/// The transaction's spelling is accepted, but a write reaches disk and rollback does not
/// undo it. Protected reads must be refused by the core even on an ordinary writable tab.
#[tokio::test]
async fn greptime_read_only_transaction_does_not_stop_writes() {
    let ws = Workspace::new();
    let Some(open) = ws.open(&GREPTIME, false, false).await else {
        return;
    };
    let open = open.unwrap();
    let table = unique("sdb_greptime_barrier");
    run(
        &open,
        &format!("create table public.{table} (ts timestamp time index, reading double)"),
    )
    .await
    .unwrap();
    let rows = support::cleaning_up(
        async {
            assert!(
                engine_error(&open, "start transaction read only")
                    .await
                    .is_none()
            );
            assert!(
                engine_error(
                    &open,
                    &format!("insert into public.{table} values ('2026-11-15 00:00:00', 1.5)")
                )
                .await
                .is_none()
            );
            assert!(engine_error(&open, "rollback").await.is_none());
            let mut events = open
                .session
                .connection()
                .execute(
                    &format!("select * from public.{table}"),
                    &ExecOptions::default(),
                )
                .await
                .unwrap();
            let mut rows = 0;
            while let Some(event) = events.next().await {
                if let ExecEvent::Batch(batch) = event.unwrap() {
                    rows += batch.num_rows();
                }
            }
            rows
        },
        |panicked| {
            let session = &open.session;
            let drop_table = format!("drop table if exists public.{table}");
            async move {
                support::run_cleanup("GreptimeDB", session, &[drop_table], panicked).await;
            }
        },
    )
    .await;
    open.close().await;
    assert_eq!(
        rows, 1,
        "GreptimeDB unexpectedly enforced its transaction barrier"
    );
}

/// Runs `sql` and returns its stored result.
async fn stored(open: &OpenConnection, sql: &str) -> ResultId {
    let mut found = None;
    let mut events = open.session.run_script(sql);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => found = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {}", error.message),
            _ => {}
        }
    }
    found.unwrap_or_else(|| panic!("{sql}: no result"))
}

/// Every row of a stored result, each value as text.
fn rows_of(open: &OpenConnection, id: ResultId) -> Vec<Vec<String>> {
    let stored = open.session.results().get(id).unwrap();
    let stored = stored.read().unwrap();
    (0..stored.rows())
        .map(|row| {
            (0..stored.schema().columns.len())
                .map(|column| match stored.value_at(row, column).unwrap() {
                    Value::Text(text) => text,
                    other => other.to_string(),
                })
                .collect()
        })
        .collect()
}

/// GreptimeDB as a MySQL connection (SDB-344): the explorer's calls work without the catalog
/// tables it lacks; a table's rows are identified by their primary key and time index (a
/// second insert with both replaces the row), and an append-only table, which keeps equal rows,
/// has no key; the table data tab's pages are ordered by that key, and the editor never pages a
/// query by it (a timestamp's literal may not compare exactly, ADR-054); the data search and
/// imports, which it cannot bind, are refused; and a long query is cancelled.
#[tokio::test]
async fn greptime_is_browsed_and_edited_by_its_time_index_and_cancelled() {
    let ws = Workspace::new();
    let Some(open) = ws.open(&GREPTIME, false, false).await else {
        return;
    };
    let open = open.unwrap();
    assert_eq!(
        open.session.server_family(),
        Some(&ServerFamily::GreptimeDb)
    );
    let db = unique("sdb344");
    run(&open, &format!("create database {db}")).await.unwrap();
    support::cleaning_up(
        async {
            // 3 hosts × 200 instants, some with milliseconds (a page must start after the
            // exact instant); then (b, 8 s) again, which replaces that row.
            let values: Vec<String> = ["a", "b", "c"]
                .iter()
                .flat_map(|host| {
                    (1..=200)
                        .map(move |s| format!("('{host}', {}, {s}.5)", s * 1000 + (s % 4) * 125))
                })
                .collect();
            run(
                &open,
                &format!(
                    "create table {db}.cpu (host string, ts timestamp(3) time index, \
                        val double, primary key (host));
                     insert into {db}.cpu values {};
                     insert into {db}.cpu values ('b', 8000, 80.5);
                     create table {db}.logs (host string, ts timestamp(3) time index, \
                        msg string, primary key (host)) with (append_mode = 'true');
                     insert into {db}.logs values ('a', 1000, 'x'), ('a', 1000, 'x');",
                    values.join(", ")
                ),
            )
            .await
            .unwrap();
            let conn = open.session.connection();

            // The explorer.
            assert!(conn.list_databases().await.unwrap().contains(&db));
            let objects = conn.list_objects(&db).await.unwrap();
            for name in ["cpu", "logs"] {
                assert!(objects.iter().any(|o| o.name == name), "{name}");
            }
            assert!(conn.list_schema_objects(&db).await.unwrap().is_empty());
            assert!(conn.list_server_objects().await.unwrap().is_empty());
            let cpu = conn.describe_table(&db, "cpu").await.unwrap();
            let columns: Vec<_> = cpu.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(columns, ["host", "ts", "val"]);
            assert_eq!(cpu.primary_key, ["host", "ts"]);
            assert!(
                conn.describe_table(&db, "logs")
                    .await
                    .unwrap()
                    .primary_key
                    .is_empty()
            );
            let all = conn.list_columns(&db).await.unwrap();
            assert!(all.iter().any(|c| c.table == "cpu" && c.name == "val"));

            // Values as the server writes them (whole seconds without their milliseconds).
            let id = stored(
                &open,
                &format!(
                    "select host, ts, val from {db}.cpu \
                     where host = 'b' and ts >= 7000 and ts <= 8000 order by ts"
                ),
            )
            .await;
            assert_eq!(
                rows_of(&open, id),
                [
                    ["b", "1970-01-01 00:00:07.375", "7.5"],
                    ["b", "1970-01-01 00:00:08", "80.5"]
                ]
            );
            open.session.results().remove(id);
            let id = stored(&open, &format!("select * from {db}.cpu order by host, ts")).await;
            let ordered = rows_of(&open, id);
            open.session.results().remove(id);
            assert_eq!(ordered.len(), 600);

            // The editor's query pages need a key whose literals compare exactly (ADR-054): a
            // GreptimeDB key always ends with its time index, a timestamp, so its queries run as
            // written; an append-only table has no key at all.
            for table in ["cpu", "logs"] {
                assert!(
                    !query_pages_exactly(
                        &open.session,
                        &format!("select * from {db}.{table}"),
                        &[]
                    )
                    .await,
                    "{table}"
                );
            }

            // The table data tab's page, ordered by the key.
            let page = table_page_sql_by_key(
                &open.session,
                &TablePage {
                    schema: db.clone(),
                    name: "cpu".into(),
                    filter: String::new(),
                    order_by: String::new(),
                    limit: 3,
                    offset: 200,
                },
            )
            .await
            .unwrap();
            assert!(page.contains("`host`, `ts`"), "{page}");
            let id = stored(&open, &page).await;
            let rows = rows_of(&open, id);
            assert_eq!(rows, ordered[200..203]);

            // An edit of that page addresses one row by host and time: deleting the first row
            // shown deletes it alone, not the host's other 199. GreptimeDB has no UPDATE, so a
            // changed cell fails with its error.
            let target = open.session.edit_target().await.unwrap();
            let edits = |changes: &[RowChange]| {
                let stored = open.session.results().get(id).unwrap();
                let stored = stored.read().unwrap();
                edit_statements(&target, &cpu, &stored, changes).unwrap()
            };
            let delete = edits(&[RowChange::Delete { row: 0 }]);
            assert_eq!(apply_edits(&open.session, &delete).await, Ok(1));
            let id_after =
                stored(&open, &format!("select * from {db}.cpu order by host, ts")).await;
            let after = rows_of(&open, id_after);
            open.session.results().remove(id_after);
            let mut expected = ordered.clone();
            expected.remove(200);
            assert_eq!(after, expected);
            let update = edits(&[RowChange::Update {
                row: 1,
                values: vec![(2, CellInput::Text("1.0".into()))],
            }]);
            assert!(
                matches!(
                    apply_edits(&open.session, &update).await,
                    Err(EditError::Failed { index: 1, .. })
                ),
                "{update:?}"
            );
            // GreptimeDB accepts BEGIN and ignores it: a delete followed by that update used
            // to keep the delete although the app said it rolled back. Several changes at
            // once are refused before anything runs (SDB-354).
            let both = edits(&[
                RowChange::Delete { row: 0 },
                RowChange::Update {
                    row: 1,
                    values: vec![(2, CellInput::Text("1.0".into()))],
                },
            ]);
            match apply_edits(&open.session, &both).await {
                Err(EditError::NotAtomic(reason)) => {
                    assert!(reason.contains("GreptimeDB"), "{reason}")
                }
                other => panic!("{other:?}"),
            }
            let id_kept = stored(&open, &format!("select * from {db}.cpu order by host, ts")).await;
            assert_eq!(rows_of(&open, id_kept), expected, "nothing was applied");
            open.session.results().remove(id_kept);
            open.session.results().remove(id);

            // The data search binds its value to a user variable, which GreptimeDB does not
            // have, and an import binds every value as text, which its prepared inserts do not
            // convert: the plugin says it can do neither, so both are refused before they run.
            let capabilities = conn.capabilities();
            assert!(!capabilities.bound_params);
            assert!(!capabilities.insert_rows);

            // A long query, cancelled by its process id.
            let session = &open.session;
            let started = Instant::now();
            let long = session
                .run_script(
                    "select count(*) from generate_series(1, 100000000) a, \
                     generate_series(1, 1000) b",
                )
                .collect::<Vec<CoreEvent>>();
            tokio::pin!(long);
            let events = tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    tokio::select! {
                        events = &mut long => break events,
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {
                            session.cancel().await.unwrap();
                        }
                    }
                }
            })
            .await
            .expect("the cancel did not stop the statement");
            assert!(started.elapsed() < Duration::from_secs(30));
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
                "{events:?}"
            );
            // The session goes on.
            run(&open, "select 1").await.unwrap();
        },
        |panicked| {
            let session = &open.session;
            let drop_db = format!("drop database if exists {db}");
            async move {
                support::run_cleanup("GreptimeDB", session, &[drop_db], panicked).await;
            }
        },
    )
    .await;
    open.close().await;
}

/// Why Databend gets no barrier: its read-only transaction and session are accepted without an
/// error, and the writes made inside them stay. If a release starts enforcing them, this test
/// says so, and Databend's barriers can be checked again.
#[tokio::test]
async fn databend_read_only_modes_do_not_stop_writes() {
    let ws = Workspace::new();
    let Some(open) = ws.open(&DATABEND, false, false).await else {
        return;
    };
    let open = open.unwrap();
    let db = unique("sdb350");
    run(
        &open,
        &format!("create database {db}; create table {db}.t (id int)"),
    )
    .await
    .unwrap();
    let rows = support::cleaning_up(
        async {
            for sql in [
                "start transaction read only".to_string(),
                format!("insert into {db}.t values (1)"),
                "rollback".into(),
                "set session transaction read only".into(),
                format!("insert into {db}.t values (2)"),
            ] {
                assert_eq!(engine_error(&open, &sql).await, None, "{sql}");
            }
            let mut events = open
                .session
                .connection()
                .execute(&format!("select id from {db}.t"), &ExecOptions::default())
                .await
                .unwrap();
            let mut rows = 0;
            while let Some(event) = events.next().await {
                if let ExecEvent::Batch(batch) = event.unwrap() {
                    rows += batch.num_rows();
                }
            }
            rows
        },
        |panicked| {
            let session = &open.session;
            let drop_database = format!("drop database if exists {db}");
            async move {
                support::run_cleanup("Databend", session, &[drop_database], panicked).await;
            }
        },
    )
    .await;
    open.close().await;
    assert_eq!(rows, 2, "Databend unexpectedly enforced a read-only mode");
}
