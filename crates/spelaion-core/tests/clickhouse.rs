//! ClickHouse, the first engine that only exists as a plugin (ADR-041, stage 3), through the
//! core: agents' reads and their engine-enforced barrier (ADR-020), streaming exports
//! (ADR-034), a read-only connection (ADR-053) and a connection through the two SSH bastions.
//!
//! They need `SPELAION_TEST_CLICKHOUSE` (`testing/docker`, 127.0.0.1:18123) and the plugin:
//! the pinned build (`spelaion-fetch-plugins`) or one built on this machine
//! (`SPELAION_DEV_PLUGIN_CLICKHOUSE`). The tunnel test also needs `SPELAION_TEST_SSH`. Without
//! them the tests are skipped, which does not count as tested. What a test creates on the
//! shared server is dropped even when one of its assertions fails (`support`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, Resolver, Safeguards, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::{
    CoreEvent, CsvOptions, DEFAULT_BATCH_ROWS, Engines, ExportError, ExportFormat, JobCancel,
    OpenConnection, OpenOptions, ReadError, ReadLimits, ResultStore, Session, StreamExport,
    TextEncoding, TunnelPool, WriteError, WriteLimits, export_query, open_connection,
};
use spelaion_driver_api::{
    ConnectParams, Driver, DriverError, ExecOptions, Secret, TlsMode, Value,
};
use spelaion_ssh::KnownHosts;

mod engines;
mod support;
use support::{cleaning_up, run_cleanup};

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_CLICKHOUSE") else {
        eprintln!("SPELAION_TEST_CLICKHOUSE not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("a port")))
}

/// A session as `user`, straight from the driver.
async fn session_as(user: &str, password: &str) -> Option<Session> {
    let (host, port) = server()?;
    let params = ConnectParams {
        host,
        port,
        user: user.into(),
        password: Some(Secret::new(password)),
        database: "spelaion".into(),
        tls: TlsMode::Disable,
        ..Default::default()
    };
    let driver = engines::clickhouse();
    Some(Session::new(
        driver.connect(&params).await.expect("connects"),
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    ))
}

fn limits(max_rows: usize) -> ReadLimits {
    ReadLimits {
        max_rows,
        timeout: Duration::from_secs(10),
    }
}

/// Straight to the driver: what no check of the core sees.
async fn raw(session: &Session, sql: &str) -> Result<(), DriverError> {
    let mut events = session
        .connection()
        .execute(sql, &ExecOptions::default())
        .await?;
    while let Some(event) = events.next().await {
        event?;
    }
    Ok(())
}

async fn must(session: &Session, sql: &str) {
    raw(session, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The server's code for a statement it refused, sent straight to it.
async fn refusal(session: &Session, sql: &str) -> Option<String> {
    match raw(session, sql).await {
        Ok(()) => None,
        Err(DriverError::Database { code, .. }) => code,
        Err(other) => Some(other.to_string()),
    }
}

/// The first value of a statement run as the editor runs it, as text.
async fn value(session: &Session, sql: &str) -> String {
    let mut events = session.run_script(sql);
    let mut last = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => last = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {}", error.message),
            _ => {}
        }
    }
    let id = last.unwrap_or_else(|| panic!("{sql}: no result"));
    let stored = session.results().get(id).expect("the result");
    let cell = stored.read().unwrap().value_at(0, 0).expect("a cell");
    match cell {
        Value::Null => "NULL".into(),
        other => other.to_string(),
    }
}

fn rejected<T: std::fmt::Debug>(result: Result<T, ReadError>, sql: &str) {
    assert!(
        matches!(result, Err(ReadError::Rejected(_))),
        "{sql}: {result:?}"
    );
}

/// SDB-435: a backslash escapes a quote in ClickHouse's strings. A value bound to a placeholder
/// that ends in `\`, or that holds a quote and a statement of its own, stays inside its
/// literal: the script runs the statements it was written with, and the value reads back as
/// it was typed.
#[tokio::test]
async fn a_bound_value_with_a_backslash_stays_inside_its_literal() {
    let Some(session) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let Some(cleanup) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let t = format!("bound_backslash_{}", std::process::id());
    let drops = [format!("drop table if exists {t}")];
    let body = async {
        must(&session, &format!("drop table if exists {t}")).await;
        must(
            &session,
            &format!("create table {t} (n UInt8, v String) engine = MergeTree order by n"),
        )
        .await;
        let template =
            format!("insert into {t} values (1, :v);\ninsert into {t} values (2, 'second')");
        let dialect = session.dialect();
        for typed in ["C:\\", "x'); insert into t values (3, 'injected'); --\\"] {
            must(&session, &format!("truncate table {t}")).await;
            let values = std::collections::HashMap::from([("v".to_string(), typed.to_string())]);
            let script = spelaion_core::bind_params(dialect.as_ref(), &template, &values).unwrap();
            assert_eq!(dialect.split_statements(&script).len(), 2, "{script}");
            let events: Vec<CoreEvent> = session.run_script(&script).collect().await;
            assert!(
                matches!(
                    events.last(),
                    Some(CoreEvent::ScriptFinished {
                        executed: 2,
                        failed: false,
                        ..
                    })
                ),
                "{script}: {events:#?}"
            );
            assert_eq!(
                value(&session, &format!("select count() from {t}")).await,
                "2"
            );
            assert_eq!(
                value(&session, &format!("select v from {t} where n = 1")).await,
                typed,
                "{script}"
            );
        }
    };
    cleaning_up(body, |panicked| {
        run_cleanup("clickhouse", &cleanup, &drops, panicked)
    })
    .await;
}

/// ClickHouse has no transactions: its barrier is the session's `readonly` setting (ADR-020),
/// which the first agent's read sets for the rest of the session. What the classifier lets
/// through, the server stops; a write sent straight to the session after a read, which no
/// check of the core ever sees, is refused by the server too.
#[tokio::test]
async fn agents_reads_make_the_session_read_only_in_the_engine() {
    let Some(session) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let Some(cleanup) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let t = format!("agent_read_{}", std::process::id());
    let drops = [format!("drop table if exists {t}")];
    let body = async {
        must(&session, &format!("drop table if exists {t}")).await;
        must(
            &session,
            &format!("create table {t} (id UInt32) engine = MergeTree order by id"),
        )
        .await;
        must(&session, &format!("insert into {t} values (1), (2), (3)")).await;

        // Capped: only the rows that fit travel, and the statement stops.
        let started = Instant::now();
        let r = session
            .run_read_only("select number from system.numbers", &limits(5))
            .await
            .unwrap();
        assert_eq!((r.rows.len(), r.truncated), (5, true));
        assert!(started.elapsed() < Duration::from_secs(5));

        // The session is read-only in the server now, for good.
        assert_eq!(
            value(&session, "select toString(getSetting('readonly'))").await,
            "2"
        );
        for sql in [
            format!("insert into {t} values (4)"),
            format!("alter table {t} delete where 1"),
            format!("truncate table {t}"),
            format!("drop table {t}"),
            format!("create table {t}_copy engine = Memory as select * from {t}"),
            "set readonly = 0".to_string(),
            "select 1 settings readonly = 0".to_string(),
        ] {
            assert_eq!(
                refusal(&session, &sql).await.as_deref(),
                Some("164"),
                "{sql}"
            );
        }
        // A read that asks the server to be writable again: the classifier refuses what it
        // cannot read, and the server refuses it anyway (above).
        let loosened = session
            .run_read_only("select 1 settings readonly = 0", &limits(5))
            .await;
        assert!(
            matches!(
                loosened,
                Err(ReadError::Rejected(_)) | Err(ReadError::Database(_))
            ),
            "{loosened:?}"
        );

        // Reading again works: the barrier is set only where it is not set yet.
        let r = session
            .run_read_only(&format!("select count() from {t}"), &limits(5))
            .await
            .unwrap();
        assert_eq!(r.rows[0][0].to_string(), "3");

        for sql in [
            format!("insert into {t} values (5)"),
            format!("alter table {t} delete where id = 1"),
            format!("truncate table {t}"),
            format!("drop table {t}"),
            format!("select 1; select * from {t}"),
            "select * from url('http://example.com/data.csv', CSV, 'a String')".into(),
            "select * from file('data.csv', CSV, 'a String')".into(),
            "select * from remote('other:9000', system.one)".into(),
            "select * from s3('https://bucket.s3.amazonaws.com/data.csv')".into(),
            "select * from mysql('db:3306', 'd', 't', 'u', 'p')".into(),
            "select * from postgresql('db:5432', 'd', 't', 'u', 'p')".into(),
            "select * from executable('script.sh', CSV, 'a String')".into(),
            "select * from icebergS3('https://bucket/table')".into(),
            format!("optimize table {t}"),
            "system flush logs".into(),
            "kill query where 1".into(),
        ] {
            rejected(session.run_read_only(&sql, &limits(5)).await, &sql);
        }

        // A timeout stops the statement and leaves the session usable.
        let started = Instant::now();
        let slow = session
            .run_read_only(
                "select count() from system.numbers",
                &ReadLimits {
                    max_rows: 1,
                    timeout: Duration::from_secs(1),
                },
            )
            .await;
        assert!(
            matches!(
                slow,
                Err(ReadError::Timeout(_)) | Err(ReadError::Database(_))
            ),
            "{slow:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(6));
        let r = session
            .run_read_only("select 42", &limits(5))
            .await
            .unwrap();
        assert_eq!(r.rows[0][0].to_string(), "42");

        // Agents' writes run in a transaction of their own: ClickHouse has none.
        assert!(matches!(
            session
                .run_write(
                    &format!("insert into {t} values (6)"),
                    &WriteLimits {
                        timeout: Duration::from_secs(2)
                    }
                )
                .await,
            Err(WriteError::Unsupported(_))
        ));
    };
    cleaning_up(body, |panicked| {
        run_cleanup("clickhouse", &cleanup, &drops, panicked)
    })
    .await;
}

/// SDB-359: an agent's read stops past its row cap, and the statement the editor runs right
/// after it on the same session is not refused because the server still held the session for
/// the stopped one (`SESSION_IS_LOCKED`). Many times over, since that is a race.
#[tokio::test]
async fn a_statement_right_after_a_capped_read_finds_the_session_free() {
    let Some(session) = session_as("spelaion", "spelaion").await else {
        return;
    };
    for round in 0..30 {
        let r = session
            .run_read_only("select number from system.numbers", &limits(5))
            .await
            .unwrap_or_else(|e| panic!("round {round}: {e:?}"));
        assert_eq!((r.rows.len(), r.truncated), (5, true), "round {round}");
        assert_eq!(
            value(&session, &format!("select toString({round})")).await,
            round.to_string()
        );
    }
}

/// ClickHouse's primary key is a sorting key: two rows may share it, so a query is never paged
/// by it (ADR-054, SDB-405). Paged, a page boundary between two such rows would hide one; the
/// query runs as written and every row is there.
#[tokio::test]
async fn a_query_is_not_paged_by_a_primary_key_rows_share() {
    let Some(session) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let Some(cleanup) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let t = format!("shared_key_{}", std::process::id());
    let drops = [format!("drop table if exists {t}")];
    let body = async {
        must(&session, &format!("drop table if exists {t}")).await;
        must(
            &session,
            &format!(
                "create table {t} (id UInt32, label String) engine = MergeTree primary key id order by id"
            ),
        )
        .await;
        must(
            &session,
            &format!("insert into {t} values (1, 'a'), (1, 'b'), (2, 'c'), (2, 'd')"),
        )
        .await;
        // The plugin reports no primary key for it today (checked 2026-11-17); the core's rule
        // refuses ClickHouse even if a later plugin reports the sorting key as one.
        for sql in [
            format!("select id, label from {t}"),
            format!("select * from {t} order by id"),
        ] {
            assert!(
                !spelaion_core::query_pages_exactly(&session, &sql, &[]).await,
                "{sql} must run as written"
            );
            assert!(
                spelaion_core::first_query_page_sql(&session, &sql, 1, &[])
                    .await
                    .is_none(),
                "{sql}"
            );
        }
        assert_eq!(
            value(&session, &format!("select count() from {t}")).await,
            "4"
        );
    };
    cleaning_up(body, |panicked| {
        run_cleanup("clickhouse", &cleanup, &drops, panicked)
    })
    .await;
}

/// A user the server already keeps read-only (`readonly = 1` in its profile: the setting cannot
/// be set again) reads as any other: the barrier is already there.
#[tokio::test]
async fn agents_read_as_a_user_the_server_keeps_read_only() {
    let Some(admin) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let user = format!("agent_ro_{}", std::process::id());
    let drops = [format!("drop user if exists {user}")];
    let body = async {
        must(&admin, &format!("drop user if exists {user}")).await;
        must(
            &admin,
            &format!("create user {user} identified by 'agent-ro-1' settings readonly = 1"),
        )
        .await;
        must(&admin, &format!("grant select on system.* to {user}")).await;
        must(&admin, &format!("grant select on spelaion.* to {user}")).await;
        // Granted, so what refuses it below is the profile's `readonly`, not a missing grant.
        must(
            &admin,
            &format!("grant create temporary table on *.* to {user}"),
        )
        .await;
        let session = session_as(&user, "agent-ro-1").await.unwrap();
        let r = session
            .run_read_only("select number from system.numbers limit 3", &limits(5))
            .await
            .unwrap();
        assert_eq!(r.rows.len(), 3);
        assert_eq!(
            refusal(&session, "create temporary table x (a UInt8)")
                .await
                .as_deref(),
            Some("164")
        );
    };
    cleaning_up(body, |panicked| {
        run_cleanup("clickhouse", &admin, &drops, panicked)
    })
    .await;
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

fn folder(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spelaion-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A query streams to a file in the export's own session, which the barrier leaves read-only;
/// writes never reach the server through the export path.
#[tokio::test]
async fn exports_stream_rows_from_a_read_only_session() {
    let Some(session) = session_as("spelaion", "spelaion").await else {
        return;
    };
    let dir = folder("clickhouse-export");
    let path = dir.join("numbers.csv");
    let summary = export_query(
        &session,
        "select number, if(number % 2 = 0, NULL, concat('=n', toString(number))) as name \
         from numbers(25000) order by number",
        &Safeguards::default(),
        false,
        &path,
        &csv(),
        &JobCancel::new(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(summary.rows, 25_000);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("number;name\r\n0;NULL\r\n1;'=n1\r\n"),
        "{}",
        &text[..60]
    );
    assert!(text.ends_with("24999;'=n24999\r\n"));
    assert_eq!(
        value(&session, "select toString(getSetting('readonly'))").await,
        "2"
    );
    for sql in [
        "insert into t values (1)",
        "select 1; select 2",
        "alter table t delete where 1",
        "select * from url('http://example.com/x.csv', CSV, 'a String')",
    ] {
        let result = export_query(
            &session,
            sql,
            &Safeguards::default(),
            false,
            &dir.join("refused.csv"),
            &csv(),
            &JobCancel::new(),
            &|_| {},
        )
        .await;
        assert!(
            matches!(result, Err(ExportError::NotARead(_))),
            "{sql}: {result:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
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

    fn write(&self, relative: &str, content: &str) {
        let path = self.base.join("repo").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    async fn open(&self, id: &str) -> OpenConnection {
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
            interaction: Arc::new(AcceptHosts),
            password: Some(Secret::new("spelaion")),
            read_only_session: false,
        };
        open_connection(
            resolved,
            &Engines::builtin(),
            options,
            Arc::new(ResultStore::new()),
        )
        .await
        .expect("opens")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct AcceptHosts;

#[async_trait::async_trait]
impl spelaion_ssh::Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        true
    }
    async fn ask_secret(&self, _request: &spelaion_ssh::SecretRequest) -> Option<Secret> {
        None
    }
}

fn toml_path(path: &Path) -> String {
    format!("{:?}", path.display().to_string().replace('\\', "/"))
}

fn keys() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

/// A read-only connection (ADR-053): opening it makes its session read-only in ClickHouse
/// itself, so a write that gets past the core's checks is refused by the server.
#[tokio::test]
async fn a_read_only_connection_is_read_only_in_the_server() {
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("clickhouse-read-only");
    ws.write(
        ".spelaion/connections/ro.toml",
        &format!(
            "schema_version = 1\nengine = \"clickhouse\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\nread_only = true\n\n[tls]\nmode = \"disable\"\n"
        ),
    );
    let open = ws.open("ro").await;
    assert_eq!(value(&open.session, "select 1 + 1").await, "2");
    assert_eq!(
        value(&open.session, "select toString(getSetting('readonly'))").await,
        "2"
    );
    // The core refuses it first; the server would too.
    let mut events = open
        .session
        .run_script("create table nope (a UInt8) engine = Memory");
    let mut failed = false;
    while let Some(event) = events.next().await {
        failed |= matches!(event, CoreEvent::StatementFailed { .. });
    }
    assert!(failed);
    assert_eq!(
        refusal(&open.session, "create table nope (a UInt8) engine = Memory")
            .await
            .as_deref(),
        Some("164")
    );
    open.close().await;
}

/// Through the two bastions of `testing/docker`: the plugin connects to the tunnel's local end,
/// and the core opened the tunnel (ADR-041: a plugin opens none).
#[tokio::test]
async fn clickhouse_through_two_bastions() {
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("clickhouse-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/events.toml",
        "schema_version = 1\nengine = \"clickhouse\"\nhost = \"clickhouse\"\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = { secret = \"prompt\" }\ntunnel = \"bastions\"\n\n[tls]\nmode = \"disable\"\n",
    );
    let open = ws.open("events").await;
    // The server behind the bastions, by the name only they resolve.
    assert_eq!(
        value(&open.session, "select currentDatabase()").await,
        "spelaion"
    );
    assert_eq!(
        value(&open.session, "select count() from numbers(100000)").await,
        "100000"
    );
    let schemas = open.session.connection().list_schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "spelaion"));
    open.close().await;
}
