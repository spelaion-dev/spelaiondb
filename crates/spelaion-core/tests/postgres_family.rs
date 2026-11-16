//! Servers that speak PostgreSQL's protocol, through the PostgreSQL connection and its plugin
//! (SDB-169), each against its own image in `testing/docker/pg-family`:
//!
//! - CockroachDB (`SPELAION_TEST_COCKROACH`), YugabyteDB (`SPELAION_TEST_YUGABYTE`) and
//!   TimescaleDB (`SPELAION_TEST_TIMESCALE`) connect, browse, run transactions and cancels, and
//!   keep read-only connections and agents' reads from writing with their own barriers;
//! - RisingWave (`SPELAION_TEST_RISINGWAVE`), Materialize (`SPELAION_TEST_MATERIALIZE`) and
//!   CrateDB (`SPELAION_TEST_CRATEDB`) are recognised and get only what they enforce.
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
    CoreEvent, CsvOptions, DEFAULT_BATCH_ROWS, Engines, ExportCancel, ExportError, ExportFormat,
    OpenConnection, OpenError, OpenOptions, ReadError, ReadLimits, ResultStore, ServerFamily,
    StreamExport, TextEncoding, TunnelPool, export_query, open_connection,
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
            &ExportCancel::new(),
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
        let transactions = server.family.read_only_transactions();
        let read = open
            .session
            .run_read_only("select 1", &ReadLimits::default())
            .await;
        // Where the transaction is enforced the read is sent (RisingWave then refuses the
        // read's `NO SCROLL` cursor: SDB-169 leaves it to RisingWave's own item); elsewhere
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
            &ExportCancel::new(),
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
