//! Servers that speak PostgreSQL's protocol, through the PostgreSQL connection and its plugin
//! (SDB-169), each against its own image in `testing/docker/pg-family`:
//!
//! - CockroachDB (`SPELAION_TEST_COCKROACH`), YugabyteDB (`SPELAION_TEST_YUGABYTE`) and
//!   TimescaleDB (`SPELAION_TEST_TIMESCALE`) connect, browse, run transactions and cancels, and
//!   keep read-only connections and agents' reads from writing with their own barriers;
//! - RisingWave (`SPELAION_TEST_RISINGWAVE`) browses, and gets no agents nor read-only
//!   connections: a view hides its table functions that reach other servers (SDB-302);
//! - Materialize (`SPELAION_TEST_MATERIALIZE`) browses, and its read-only transaction is
//!   shown letting DDL through (SDB-303);
//! - CrateDB (`SPELAION_TEST_CRATEDB`) browses, and, like RisingWave, has no transaction its
//!   editor sessions follow: they stay in auto-commit (SDB-304);
//! - RisingWave, Materialize and CrateDB are recognised and get only what they enforce.
//!
//! Each variable is `host:port`; a server without one is skipped, which does not count as
//! tested.

mod engines;
mod support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::{
    CoreEvent, CsvOptions, DEFAULT_BATCH_ROWS, Engines, ExportError, ExportFormat, JobCancel,
    OpenConnection, OpenError, OpenOptions, ReadError, ReadLimits, ResultStore, ServerFamily,
    StreamExport, TextEncoding, TransactionError, TunnelPool, WriteError, WriteLimits,
    classify_read_only, export_query, open_connection,
};
use spelaion_driver_api::{ExecEvent, ExecOptions, TransactionState};
use spelaion_ssh::{KnownHosts, NonInteractive};

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// A server of the family, as its image starts (no password: each trusts local logins).
struct Server {
    label: &'static str,
    var: &'static str,
    user: &'static str,
    database: &'static str,
    family: ServerFamily,
}

const COCKROACH: Server = Server {
    label: "CockroachDB",
    var: "SPELAION_TEST_COCKROACH",
    user: "root",
    database: "defaultdb",
    family: ServerFamily::CockroachDb,
};
const YUGABYTE: Server = Server {
    label: "YugabyteDB",
    var: "SPELAION_TEST_YUGABYTE",
    user: "yugabyte",
    database: "yugabyte",
    family: ServerFamily::YugabyteDb,
};
// TimescaleDB is PostgreSQL with an extension: the server says it is PostgreSQL.
const TIMESCALE: Server = Server {
    label: "TimescaleDB",
    var: "SPELAION_TEST_TIMESCALE",
    user: "postgres",
    database: "postgres",
    family: ServerFamily::PostgreSql,
};
const RISINGWAVE: Server = Server {
    label: "RisingWave",
    var: "SPELAION_TEST_RISINGWAVE",
    user: "root",
    database: "dev",
    family: ServerFamily::RisingWave,
};
const MATERIALIZE: Server = Server {
    label: "Materialize",
    var: "SPELAION_TEST_MATERIALIZE",
    user: "materialize",
    database: "materialize",
    family: ServerFamily::Materialize,
};
const CRATEDB: Server = Server {
    label: "CrateDB",
    var: "SPELAION_TEST_CRATEDB",
    user: "crate",
    database: "doc",
    family: ServerFamily::CrateDb,
};

static GREPTIME: LazyLock<Server> = LazyLock::new(|| Server {
    label: "GreptimeDB",
    var: "SPELAION_TEST_GREPTIME_PG",
    user: "root",
    database: "public",
    family: ServerFamily::Unchecked("GreptimeDB".into()),
});

/// The servers whose barriers are PostgreSQL's.
const CHECKED: [&Server; 3] = [&COCKROACH, &YUGABYTE, &TIMESCALE];

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

    /// The connection file as the picker's kind writes it: a PostgreSQL connection.
    fn file(&self, read_only: bool) -> Option<String> {
        let (host, port) = self.addr()?;
        Some(format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\n\
             database = \"{}\"\nuser = \"{}\"\ntls = {{ mode = \"disable\" }}\nread_only = {read_only}\n",
            self.database, self.user
        ))
    }
}

/// A temporary, trusted repository whose connections the tests write.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base = std::env::temp_dir().join(unique("spelaion-pg-family"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    async fn open(
        &self,
        content: &str,
        read_only_session: bool,
    ) -> Result<OpenConnection, OpenError> {
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
            password: None,
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

/// The first error of `sql` sent straight to the driver, around the core's checks: what the
/// server itself enforces.
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
        gzip: false,
        sql: spelaion_core::SqlOptions::default(),
        json: spelaion_core::JsonOptions::default(),
        columns: None,
        batch_size: DEFAULT_BATCH_ROWS,
    }
}

/// The explorer's calls on a server of the family, through the PostgreSQL plugin.
#[tokio::test]
async fn checked_servers_connect_and_browse() {
    for server in CHECKED {
        let Some(file) = server.file(false) else {
            continue;
        };
        let label = server.label;
        let ws = Workspace::new();
        let open = ws
            .open(&file, false)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            open.session.server_family(),
            Some(&server.family),
            "{label}"
        );
        // Their transactions hold: edits and imports keep their one transaction (SDB-354).
        assert_eq!(open.session.atomic_writes_refusal(), None, "{label}");
        let parent = unique("sdb_parent");
        let child = unique("sdb_child");
        let view = unique("sdb_view");
        let seq = unique("sdb_seq");
        run(
            &open,
            &format!(
                "create table {parent} (id int primary key, name text);
                 create table {child} (id int primary key, parent int references {parent}(id));
                 create view {view} as select id, name from {parent};
                 create sequence {seq};
                 insert into {parent} values (1, 'one'), (2, 'two');"
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        let browse = async {
            let conn = open.session.connection();
            let schemas = conn.list_schemas().await.unwrap();
            assert!(
                schemas.iter().any(|s| s.name == "public" && !s.system),
                "{label}: {schemas:?}"
            );
            assert!(
                schemas.iter().any(|s| s.name == "pg_catalog" && s.system),
                "{label}"
            );
            // CockroachDB leaves `relispartition` NULL: no table may be taken for a partition.
            let objects = conn.list_objects("public").await.unwrap();
            for name in [&parent, &child, &view] {
                assert!(
                    objects.iter().any(|o| &o.name == name),
                    "{label}: {name} in {objects:?}"
                );
            }
            let child_info = conn.describe_table("public", &child).await.unwrap();
            let columns: Vec<_> = child_info.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(columns, ["id", "parent"], "{label}");
            assert_eq!(child_info.primary_key, ["id"], "{label}");
            assert_eq!(child_info.foreign_keys.len(), 1, "{label}");
            let all = conn.list_columns("public").await.unwrap();
            assert!(
                all.iter().any(|c| c.table == parent && c.name == "name"),
                "{label}"
            );
            let keys = conn.list_referencing_keys("public", &parent).await.unwrap();
            assert!(
                keys.iter()
                    .any(|k| k.table == child && k.columns == ["parent"]),
                "{label}: {keys:?}"
            );
            let schema_objects = conn.list_schema_objects("public").await.unwrap();
            let sequence = schema_objects
                .iter()
                .find(|o| o.name == seq)
                .unwrap_or_else(|| panic!("{label}: {seq} in {schema_objects:?}"));
            let definition = conn.object_definition(sequence).await.unwrap();
            assert!(
                definition.contains("CREATE SEQUENCE"),
                "{label}: {definition}"
            );
            // CockroachDB has no `pg_tablespace_location`: its tablespaces come without one.
            let server_objects = conn.list_server_objects().await.unwrap();
            assert!(!server_objects.is_empty(), "{label}");
            let view_sql = conn.relation_definition("public", &view).await.unwrap();
            assert!(view_sql.contains("VIEW"), "{label}: {view_sql}");
        };
        browse.await;
        run(
            &open,
            &format!(
                "drop view {view}; drop table {child}; drop table {parent}; drop sequence {seq}"
            ),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        open.close().await;
    }
}

/// Read-only connections, agents' reads and streaming exports rely on the server's own
/// read-only transaction and session: these servers enforce both.
#[tokio::test]
async fn checked_servers_keep_reads_from_writing() {
    for server in CHECKED {
        let (Some(writable), Some(read_only)) = (server.file(false), server.file(true)) else {
            continue;
        };
        let label = server.label;
        let ws = Workspace::new();
        let table = unique("sdb_ro");
        let owner = ws
            .open(&writable, false)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        run(
            &owner,
            &format!("create table {table} (id int primary key)"),
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));

        // A read-only connection: the server refuses a write sent around the core's check.
        let ro = ws
            .open(&read_only, false)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let error = engine_error(&ro, &format!("insert into {table} values (1)"))
            .await
            .unwrap_or_else(|| panic!("{label}: the read-only session wrote"));
        assert!(error.contains("read-only"), "{label}: {error}");
        ro.close().await;

        // Agents' reads: the session opens, reads run, and a write inside the barrier fails.
        let agent = ws
            .open(&read_only, true)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let read = agent
            .session
            .run_read_only(&format!("select id from {table}"), &ReadLimits::default())
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert!(read.rows.is_empty(), "{label}");
        agent.close().await;

        // The streaming export's barrier, as the app's export session runs it.
        let path = std::env::temp_dir().join(format!("{}.csv", unique("sdb-pg-family")));
        let summary = export_query(
            &owner.session,
            &format!("select id from {table}"),
            &Safeguards::default(),
            false,
            &path,
            &csv(),
            &JobCancel::new(),
            &|_| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(summary.rows, 0, "{label}");
        let _ = std::fs::remove_file(&path);
        run(&owner, "begin read only").await.unwrap();
        let error = engine_error(&owner, &format!("create table {table}_x (id int)"))
            .await
            .unwrap_or_else(|| panic!("{label}: BEGIN READ ONLY let a CREATE TABLE through"));
        assert!(error.contains("read-only"), "{label}: {error}");
        run(&owner, "rollback").await.unwrap();

        run(&owner, &format!("drop table {table}")).await.unwrap();
        owner.close().await;
    }
}

/// Transactions are followed from the server, and a cancel stops a running statement.
#[tokio::test]
async fn checked_servers_follow_transactions_and_cancel() {
    for server in CHECKED {
        let Some(file) = server.file(false) else {
            continue;
        };
        let label = server.label;
        let ws = Workspace::new();
        let open = ws
            .open(&file, false)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        let conn = open.session.connection().clone();
        run(&open, "begin").await.unwrap();
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

        // A cancel that arrives before the statement started is lost (YugabyteDB takes a while
        // over a new connection's first statements): it is sent again until the run ends.
        let session = &open.session;
        let started = std::time::Instant::now();
        let run = session
            .run_script("select pg_sleep(30)")
            .collect::<Vec<CoreEvent>>();
        tokio::pin!(run);
        let events = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                tokio::select! {
                    events = &mut run => break events,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        session.cancel().await.unwrap_or_else(|e| panic!("{label}: {e}"));
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{label}: the cancel did not stop the statement"));
        assert!(started.elapsed() < Duration::from_secs(20), "{label}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, CoreEvent::StatementFinished { .. })),
            "{label}: the statement ran to its end: {events:?}"
        );
        open.close().await;
    }
}

/// RisingWave enforces a read-only transaction but ignores the session's read-only mode;
/// Materialize and CrateDB enforce neither. Each is recognised, opens for normal use, and gets
/// only what it enforces.
#[tokio::test]
async fn servers_without_a_barrier_get_no_read_only_mode_nor_agents() {
    for server in [&RISINGWAVE, &MATERIALIZE, &CRATEDB, &GREPTIME] {
        let (Some(writable), Some(read_only)) = (server.file(false), server.file(true)) else {
            continue;
        };
        let label = server.label;
        let ws = Workspace::new();

        // A read-only connection is refused before anything of it runs, with the reason.
        match ws.open(&read_only, false).await {
            Err(OpenError::NoReadOnlyBarrier(reason)) => {
                assert!(reason.contains(label), "{label}: {reason}");
                assert!(
                    reason.contains("read-only mode is not available"),
                    "{label}: {reason}"
                );
            }
            Err(other) => panic!("{label}: {other}"),
            Ok(_) => panic!("{label}: a read-only connection opened"),
        }

        let open = ws
            .open(&writable, false)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(
            open.session.server_family(),
            Some(&server.family),
            "{label}"
        );
        // Several table edits at once and imports need a transaction that holds (SDB-354):
        // CrateDB and GreptimeDB accept `BEGIN` and ignore it, RisingWave's writes are not
        // transactional.
        assert_eq!(
            open.session.atomic_writes_refusal().is_some(),
            !server.family.enforces_transactions(),
            "{label}"
        );
        if matches!(
            server.family,
            ServerFamily::CrateDb | ServerFamily::Unchecked(_)
        ) {
            let reason = open.session.atomic_writes_refusal().unwrap();
            assert!(reason.contains(label), "{label}: {reason}");
        }
        let transactions = server.family.read_only_transactions();
        let read = open
            .session
            .run_read_only("select 1", &ReadLimits::default())
            .await;
        // Where the transaction is enforced the read is sent (RisingWave then refuses the
        // read's `NO SCROLL` cursor; its agents are refused anyway, SDB-302); elsewhere
        // nothing is.
        match (&read, transactions) {
            (Err(ReadError::Unsupported(reason)), false) => {
                assert!(reason.contains(label), "{label}: {reason}")
            }
            (Err(ReadError::Unsupported(reason)), true) => panic!("{label}: {reason}"),
            (_, true) => {}
            other => panic!("{label}: {other:?}"),
        }
        let path = std::env::temp_dir().join(format!("{}.csv", unique("sdb-pg-family")));
        let export = export_query(
            &open.session,
            "select 1",
            &Safeguards::default(),
            false,
            &path,
            &csv(),
            &JobCancel::new(),
            &|_| {},
        )
        .await;
        let _ = std::fs::remove_file(&path);
        match (&export, transactions) {
            (Ok(_), true) => {}
            (Err(ExportError::Invalid(reason)), false) => {
                assert!(
                    reason.contains("does not enforce read-only transactions"),
                    "{label}: {reason}"
                )
            }
            other => panic!("{label}: {other:?}"),
        }
        open.close().await;

        // A session that only reads (agents' reads) needs the read-only transaction.
        match (ws.open(&writable, true).await, transactions) {
            (Ok(open), true) => open.close().await,
            (Err(OpenError::NoReadOnlyBarrier(reason)), false) => {
                assert!(reason.contains(label), "{label}: {reason}")
            }
            (Err(other), _) => panic!("{label}: {other}"),
            (Ok(_), false) => panic!("{label}: a session that only reads opened"),
        }
    }
}

/// RisingWave browses through the PostgreSQL plugin (SDB-302): its catalog lacks `reltuples`,
/// `LATERAL`, user functions in `pg_proc` and constraint definitions, and its oids are `int4`.
#[tokio::test]
async fn risingwave_connects_and_browses() {
    let Some(file) = RISINGWAVE.file(false) else {
        return;
    };
    let ws = Workspace::new();
    let open = ws.open(&file, false).await.unwrap();
    assert_eq!(
        open.session.server_family(),
        Some(&ServerFamily::RisingWave)
    );
    let table = unique("sdb_rw_table");
    let view = unique("sdb_rw_view");
    let mv = unique("sdb_rw_mv");
    run(
        &open,
        &format!(
            "create table {table} (id int primary key, name varchar);
             create view {view} as select id, name from {table};
             create materialized view {mv} as select count(*) as n from {table};"
        ),
    )
    .await
    .unwrap();
    support::cleaning_up(
        async {
            let conn = open.session.connection();
            let schemas = conn.list_schemas().await.unwrap();
            assert!(
                schemas.iter().any(|s| s.name == "public" && !s.system),
                "{schemas:?}"
            );
            assert!(schemas.iter().any(|s| s.name == "pg_catalog" && s.system));
            let objects = conn.list_objects("public").await.unwrap();
            for name in [&table, &view, &mv] {
                assert!(
                    objects.iter().any(|o| &o.name == name),
                    "{name} in {objects:?}"
                );
            }
            let info = conn.describe_table("public", &table).await.unwrap();
            let columns: Vec<_> = info.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(columns, ["id", "name"]);
            assert_eq!(info.primary_key, ["id"]);
            let all = conn.list_columns("public").await.unwrap();
            assert!(all.iter().any(|c| c.table == table && c.name == "name"));
            assert!(
                conn.list_referencing_keys("public", &table)
                    .await
                    .unwrap()
                    .is_empty()
            );
            conn.list_schema_objects("public").await.unwrap();
            assert!(!conn.list_server_objects().await.unwrap().is_empty());
            let ddl = conn.relation_definition("public", &table).await.unwrap();
            assert!(ddl.starts_with(&format!("CREATE TABLE {table}")), "{ddl}");
            let ddl = conn.relation_definition("public", &view).await.unwrap();
            assert!(ddl.starts_with(&format!("CREATE VIEW {view}")), "{ddl}");
        },
        |panicked| {
            let session = &open.session;
            let statements = vec![
                format!("drop materialized view if exists {mv}"),
                format!("drop view if exists {view}"),
                format!("drop table if exists {table}"),
            ];
            async move {
                support::run_cleanup("RisingWave", session, &statements, panicked).await;
            }
        },
    )
    .await;
    open.close().await;
}

/// RisingWave gets no agents, nor read-only connections (SDB-302, ADR-020's 2026-11-23 note).
/// Its read-only transaction refuses every write and DDL, but not its built-in table functions
/// that reach other servers, buckets and services (`postgres_query`, `mysql_query`,
/// `file_scan`, `openai_embedding`), and the classifier's list of them cannot be the barrier:
/// RisingWave lists none of its built-in functions (`pg_proc` is empty), and any view or
/// function may call one where the statement only names the view. This test keeps both:
/// - a view over `postgres_query` (here, back to RisingWave itself) passes the classifier and
///   runs its query on the other server inside `BEGIN READ ONLY`, while the same call written
///   out is refused;
/// - so an agent's read session is refused when it opens, an agent's write is refused (its
///   writes are not transactional), and a read-only connection is refused.
#[tokio::test]
async fn risingwave_gets_no_agents_because_views_hide_calls_outside() {
    let (Some(writable), Some(read_only)) = (RISINGWAVE.file(false), RISINGWAVE.file(true)) else {
        return;
    };
    let ws = Workspace::new();
    let owner = ws.open(&writable, false).await.unwrap();
    let table = unique("sdb_rw_agents");
    let view = unique("sdb_rw_hidden");
    // RisingWave's own port inside its container, whatever the test reaches it through.
    let call = "postgres_query('127.0.0.1', '4566', 'root', '', 'dev', 'select 42 as answer')";
    run(
        &owner,
        &format!(
            "create table {table} (id int primary key);
             create view {view} as select * from {call};"
        ),
    )
    .await
    .unwrap();
    support::cleaning_up(
        async {
            // The call written out is refused; behind the view's name it is not seen.
            assert!(classify_read_only("postgres", &format!("select * from {call}")).is_err());
            let hidden = format!("select answer from {view}");
            assert!(classify_read_only("postgres", &hidden).is_ok());
            // And RisingWave runs it on the other server inside its read-only transaction.
            assert!(engine_error(&owner, "begin read only").await.is_none());
            let mut events = owner
                .session
                .connection()
                .execute(&hidden, &ExecOptions::default())
                .await
                .unwrap();
            let mut rows = 0;
            while let Some(event) = events.next().await {
                if let ExecEvent::Batch(batch) = event.unwrap() {
                    rows += batch.num_rows();
                }
            }
            assert_eq!(rows, 1, "the view's call ran inside BEGIN READ ONLY");
            assert!(engine_error(&owner, "rollback").await.is_none());

            // An agent's read session, as the MCP opens it: refused before anything runs.
            match ws.open(&read_only, true).await {
                Err(OpenError::NoReadOnlyBarrier(reason)) => {
                    assert!(reason.contains("RisingWave"), "{reason}")
                }
                Err(other) => panic!("an agent's read session: {other}"),
                Ok(_) => panic!("an agent's read session opened"),
            }
            // An agent's write: its transaction would not hold.
            let write = owner
                .session
                .run_write(
                    &format!("insert into {table} values (1)"),
                    &WriteLimits::default(),
                )
                .await;
            match write {
                Err(WriteError::Rejected(reason)) => {
                    assert!(reason.contains("RisingWave"), "{reason}");
                    assert!(reason.contains("does not enforce"), "{reason}");
                }
                other => panic!("an agent's write: {other:?}"),
            }
            // A read-only connection's own sessions.
            match ws.open(&read_only, false).await {
                Err(OpenError::NoReadOnlyBarrier(reason)) => {
                    assert!(reason.contains("RisingWave"), "{reason}")
                }
                Err(other) => panic!("{other}"),
                Ok(_) => panic!("a read-only connection opened"),
            }
        },
        |panicked| {
            let session = &owner.session;
            let statements = vec![
                format!("drop view if exists {view}"),
                format!("drop table if exists {table}"),
            ];
            async move {
                support::run_cleanup("RisingWave", session, &statements, panicked).await;
            }
        },
    )
    .await;
    owner.close().await;
}

#[tokio::test]
async fn greptime_read_only_transaction_does_not_stop_writes() {
    let Some(file) = GREPTIME.file(false) else {
        return;
    };
    let ws = Workspace::new();
    let open = ws.open(&file, false).await.unwrap();
    let table = unique("sdb_greptime_barrier");
    run(
        &open,
        &format!("create table public.{table} (ts timestamp time index, reading double)"),
    )
    .await
    .unwrap();
    let rows = support::cleaning_up(
        async {
            assert!(engine_error(&open, "begin read only").await.is_none());
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

/// Materialize browses through the PostgreSQL plugin (SDB-303): no `format`, no
/// `pg_get_function_identity_arguments`, and its definitions come from `SHOW CREATE`.
#[tokio::test]
async fn materialize_connects_and_browses() {
    let Some(file) = MATERIALIZE.file(false) else {
        return;
    };
    let ws = Workspace::new();
    let open = ws.open(&file, false).await.unwrap();
    assert_eq!(
        open.session.server_family(),
        Some(&ServerFamily::Materialize)
    );
    let table = unique("sdb_mz_table");
    let view = unique("sdb_mz_view");
    let mv = unique("sdb_mz_mv");
    run(
        &open,
        &format!(
            "create table {table} (id int not null, name text);
             create view {view} as select id, name from {table};
             create materialized view {mv} as select count(*) as n from {table};"
        ),
    )
    .await
    .unwrap();
    support::cleaning_up(
        async {
            let conn = open.session.connection();
            let schemas = conn.list_schemas().await.unwrap();
            assert!(
                schemas.iter().any(|s| s.name == "public" && !s.system),
                "{schemas:?}"
            );
            let objects = conn.list_objects("public").await.unwrap();
            for name in [&table, &view, &mv] {
                assert!(
                    objects.iter().any(|o| &o.name == name),
                    "{name} in {objects:?}"
                );
            }
            let info = conn.describe_table("public", &table).await.unwrap();
            let columns: Vec<_> = info.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(columns, ["id", "name"]);
            let all = conn.list_columns("public").await.unwrap();
            assert!(all.iter().any(|c| c.table == table && c.name == "name"));
            assert!(
                conn.list_referencing_keys("public", &table)
                    .await
                    .unwrap()
                    .is_empty()
            );
            conn.list_schema_objects("public").await.unwrap();
            assert!(!conn.list_server_objects().await.unwrap().is_empty());
            let ddl = conn.relation_definition("public", &table).await.unwrap();
            assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
            let ddl = conn.relation_definition("public", &mv).await.unwrap();
            assert!(ddl.starts_with("CREATE MATERIALIZED VIEW"), "{ddl}");
        },
        |panicked| {
            let session = &open.session;
            let statements = vec![
                format!("drop materialized view if exists {mv}"),
                format!("drop view if exists {view}"),
                format!("drop table if exists {table}"),
            ];
            async move {
                support::run_cleanup("Materialize", session, &statements, panicked).await;
            }
        },
    )
    .await;
    open.close().await;
}

/// Why Materialize gets no agents, read-only connections or streaming reads (SDB-169,
/// SDB-303): its read-only transaction refuses `INSERT` but lets DDL through, and its session's
/// read-only mode is refused. If this test fails, Materialize changed what its transaction
/// enforces: ADR-020's table must be read again before anything relies on it.
#[tokio::test]
async fn materialize_read_only_transaction_lets_ddl_through() {
    let Some(file) = MATERIALIZE.file(false) else {
        return;
    };
    let ws = Workspace::new();
    let open = ws.open(&file, false).await.unwrap();
    let table = unique("sdb_mz_ro");
    let made = unique("sdb_mz_made");
    run(&open, &format!("create table {table} (id int)"))
        .await
        .unwrap();
    support::cleaning_up(
        async {
            assert!(engine_error(&open, "begin read only").await.is_none());
            let insert = engine_error(&open, &format!("insert into {table} values (1)")).await;
            assert!(
                insert.as_deref().is_some_and(|e| e.contains("read-only")),
                "{insert:?}"
            );
            assert!(engine_error(&open, "rollback").await.is_none());
            assert!(engine_error(&open, "begin read only").await.is_none());
            assert_eq!(
                engine_error(&open, &format!("create table {made} (id int)")).await,
                None,
                "Materialize's read-only transaction refused a CREATE TABLE"
            );
            let _ = engine_error(&open, "rollback").await;
            let session_mode = engine_error(
                &open,
                "set session characteristics as transaction read only",
            )
            .await;
            assert!(
                session_mode.is_some(),
                "the session's read-only mode was accepted"
            );
        },
        |panicked| {
            let session = &open.session;
            let statements = vec![
                format!("drop table if exists {made}"),
                format!("drop table if exists {table}"),
            ];
            async move {
                support::run_cleanup("Materialize", session, &statements, panicked).await;
            }
        },
    )
    .await;
    open.close().await;
}

/// CrateDB browses through the PostgreSQL plugin (SDB-304): its keys, functions and
/// definitions come from what its catalog has (`information_schema`, `SHOW CREATE TABLE`).
#[tokio::test]
async fn cratedb_connects_and_browses() {
    let Some(file) = CRATEDB.file(false) else {
        return;
    };
    let ws = Workspace::new();
    let open = ws.open(&file, false).await.unwrap();
    assert_eq!(open.session.server_family(), Some(&ServerFamily::CrateDb));
    let table = unique("sdb_crate_table");
    let view = unique("sdb_crate_view");
    run(
        &open,
        &format!(
            "create table doc.{table} (id int primary key, name text);
             create view doc.{view} as select id, name from doc.{table};"
        ),
    )
    .await
    .unwrap();
    support::cleaning_up(
        async {
            let conn = open.session.connection();
            let schemas = conn.list_schemas().await.unwrap();
            assert!(schemas.iter().any(|s| s.name == "doc"), "{schemas:?}");
            let objects = conn.list_objects("doc").await.unwrap();
            for name in [&table, &view] {
                assert!(
                    objects.iter().any(|o| &o.name == name),
                    "{name} in {objects:?}"
                );
            }
            let info = conn.describe_table("doc", &table).await.unwrap();
            let columns: Vec<_> = info.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(columns, ["id", "name"]);
            assert_eq!(info.primary_key, ["id"]);
            let all = conn.list_columns("doc").await.unwrap();
            assert!(all.iter().any(|c| c.table == table && c.name == "name"));
            assert!(
                conn.list_referencing_keys("doc", &table)
                    .await
                    .unwrap()
                    .is_empty()
            );
            conn.list_schema_objects("doc").await.unwrap();
            assert!(!conn.list_server_objects().await.unwrap().is_empty());
            let ddl = conn.relation_definition("doc", &table).await.unwrap();
            assert!(ddl.starts_with("CREATE TABLE"), "{ddl}");
            let ddl = conn.relation_definition("doc", &view).await.unwrap();
            assert!(ddl.contains("VIEW"), "{ddl}");
        },
        |panicked| {
            let session = &open.session;
            let statements = vec![
                format!("drop view if exists doc.{view}"),
                format!("drop table if exists doc.{table}"),
            ];
            async move {
                support::run_cleanup("CrateDB", session, &statements, panicked).await;
            }
        },
    )
    .await;
    open.close().await;
}

/// SDB-304: a server that accepts `BEGIN` and writes at once anyway gets no transaction to
/// follow. Under a connection type that asks for manual commit, its editor session stays in
/// auto-commit, and Commit, Rollback and the auto-commit switch say why. Before, the session
/// showed a transaction "held back" that was already written, and a Rollback that undid
/// nothing (CrateDB: `ROLLBACK` is a syntax error there, and the row stays). A server that keeps
/// its transactions (CockroachDB) still starts in manual commit.
#[tokio::test]
async fn servers_that_ignore_begin_get_no_manual_commit() {
    let manual = Safeguards {
        manual_commit: true,
        ..Safeguards::default()
    };
    if let Some(file) = COCKROACH.file(false) {
        let ws = Workspace::new();
        let open = ws.open(&file, false).await.unwrap();
        let status = open.session.follow_transactions(&manual).await;
        assert!(status.supported && !status.auto_commit, "{status:?}");
        open.close().await;
    }
    for server in [&CRATEDB, &RISINGWAVE] {
        let Some(file) = server.file(false) else {
            continue;
        };
        let label = server.label;
        let ws = Workspace::new();
        let open = ws.open(&file, false).await.unwrap();
        let status = open.session.follow_transactions(&manual).await;
        assert!(!status.supported, "{label}: {status:?}");
        assert!(status.auto_commit, "{label}: {status:?}");
        assert!(
            matches!(
                open.session.set_auto_commit(false).await,
                Err(TransactionError::Unsupported)
            ),
            "{label}"
        );
        assert!(
            matches!(
                open.session.rollback().await,
                Err(TransactionError::Unsupported)
            ),
            "{label}"
        );
        assert!(!open.session.close_needs_confirmation().await, "{label}");
        open.close().await;
    }

    // Why: on CrateDB a write inside `BEGIN` is written at once, and `ROLLBACK` is refused.
    let Some(file) = CRATEDB.file(false) else {
        return;
    };
    let ws = Workspace::new();
    let open = ws.open(&file, false).await.unwrap();
    let table = unique("sdb_crate_tx");
    run(&open, &format!("create table doc.{table} (id int)"))
        .await
        .unwrap();
    support::cleaning_up(
        async {
            assert!(engine_error(&open, "begin").await.is_none());
            assert!(
                engine_error(&open, &format!("insert into doc.{table} values (1)"))
                    .await
                    .is_none()
            );
            assert!(
                engine_error(&open, "rollback").await.is_some(),
                "CrateDB accepted ROLLBACK"
            );
            assert!(
                engine_error(&open, &format!("refresh table doc.{table}"))
                    .await
                    .is_none()
            );
            let mut events = open
                .session
                .connection()
                .execute(
                    &format!("select count(*) from doc.{table}"),
                    &ExecOptions::default(),
                )
                .await
                .unwrap();
            let mut count = None;
            while let Some(event) = events.next().await {
                if let ExecEvent::Batch(batch) = event.unwrap() {
                    count = spelaion_driver_api::Value::from_array(
                        batch.column(0).as_ref(),
                        0,
                        spelaion_driver_api::LogicalType::Int,
                    )
                    .map(|v| v.to_string());
                }
            }
            assert_eq!(count.as_deref(), Some("1"), "the rolled-back row stayed");
        },
        |panicked| {
            let session = &open.session;
            let statements = vec![format!("drop table if exists doc.{table}")];
            async move {
                support::run_cleanup("CrateDB", session, &statements, panicked).await;
            }
        },
    )
    .await;
    open.close().await;
}
