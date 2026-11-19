//! PostgreSQL through PgBouncer in transaction mode (SDB-396), against `testing/docker`'s
//! `pgbouncer` (`SPELAION_TEST_PGBOUNCER=127.0.0.1:16432`), which keeps no prepared statement for
//! a client and hands each transaction to the server connection used least recently. With the
//! connection's `pooler = "transaction"`, the driver relies on no prepared statement and the
//! core on no session state: statements, parameters, catalog reads, a transaction, loading rows
//! and an agent's read all work; changing the session's schema is refused with why. A
//! connection without the setting is shown to fail there, so these tests mean something.
//! Runs through the PostgreSQL plugin (`SPELAION_DEV_PLUGIN_POSTGRES` or the pinned build).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ReadLimits, ResultStore, SessionReadOnlyError,
    TunnelPool, open_connection, set_active_schema, set_session_read_only,
};
use spelaion_driver_api::{ExecEvent, ExecOptions, InsertTarget, Secret, TextRows, Value};
use spelaion_ssh::{KnownHosts, NonInteractive};

mod engines;

fn server() -> Option<(String, u16)> {
    let Ok(value) = std::env::var("SPELAION_TEST_PGBOUNCER") else {
        eprintln!("SPELAION_TEST_PGBOUNCER not set: test skipped");
        return None;
    };
    let (host, port) = value.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("a port")))
}

fn unique(prefix: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let base = std::env::temp_dir().join(unique("spelaion-pgbouncer"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    async fn open(&self, (host, port): &(String, u16), pooler: Option<&str>) -> OpenConnection {
        let id = unique("db");
        let pooler = pooler
            .map(|mode| format!("pooler = \"{mode}\"\n"))
            .unwrap_or_default();
        std::fs::write(
            self.base
                .join(format!("repo/.spelaion/connections/{id}.toml")),
            format!(
                "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\n{pooler}[tls]\nmode = \"disable\"\n"
            ),
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
        open_connection(
            resolved,
            &Engines::builtin(),
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: self.base.join("user/known_hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: Some(Secret::new("spelaion")),
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        )
        .await
        .expect("opening through PgBouncer")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Runs `sql` as the editor does; the first cell of its last result, or its first error.
async fn run(open: &OpenConnection, sql: &str) -> Result<Option<String>, String> {
    let mut events = open.session.run_script(sql);
    let mut last = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => last = Some(result),
            CoreEvent::StatementFailed { error, .. } => return Err(error.message),
            _ => {}
        }
    }
    let Some(id) = last else { return Ok(None) };
    let stored = open.session.results().get(id).expect("the result");
    let cell = stored.read().unwrap().value_at(0, 0);
    Ok(cell.map(|cell| match cell {
        Value::Text(text) => text,
        other => other.to_string(),
    }))
}

/// The first cell of `sql` run with `params` bound as text, through the driver.
async fn bound(open: &OpenConnection, sql: &str, params: &[&str]) -> Result<String, String> {
    let mut events = open
        .session
        .connection()
        .execute_bound(
            sql,
            params.iter().map(|p| p.to_string()).collect(),
            &ExecOptions::default(),
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut first = None;
    while let Some(event) = events.next().await {
        if let ExecEvent::Batch(batch) = event.map_err(|e| e.to_string())?
            && first.is_none()
            && batch.num_rows() > 0
        {
            first = Some(arrow::util::display::array_value_to_string(batch.column(0), 0).unwrap());
        }
    }
    first.ok_or_else(|| "no row".into())
}

#[tokio::test]
async fn statements_parameters_and_the_catalog_work_behind_a_transaction_pooler() {
    let Some(server) = server() else { return };
    let ws = Workspace::new();
    let open = ws.open(&server, Some("transaction")).await;
    // Many statements in a row: each one is prepared and run, and the pooler sends them to
    // different server connections. None may find a statement missing.
    for n in 0..40 {
        assert_eq!(
            run(&open, &format!("select {n} + 1"))
                .await
                .unwrap()
                .as_deref(),
            Some((n + 1).to_string().as_str())
        );
        assert_eq!(
            bound(&open, "select $1::int * 2", &[&n.to_string()])
                .await
                .unwrap(),
            (n * 2).to_string()
        );
    }
    // The explorer's reads (catalog queries with parameters).
    let connection = open.session.connection();
    let schemas = connection.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "public"), "{schemas:?}");
    for _ in 0..5 {
        connection.list_objects("public").await.unwrap();
        connection.list_columns("public").await.unwrap();
    }
    open.close().await;
}

#[tokio::test]
async fn a_transaction_an_import_and_an_agent_read_work_behind_a_transaction_pooler() {
    let Some(server) = server() else { return };
    let ws = Workspace::new();
    let open = ws.open(&server, Some("transaction")).await;
    let table = unique("pooled");
    run(
        &open,
        &format!("create table {table} (id int primary key, name text)"),
    )
    .await
    .unwrap();

    // A transaction keeps one server connection from BEGIN to COMMIT: what it wrote is read
    // inside it, and is there after it.
    run(&open, "begin").await.unwrap();
    run(&open, &format!("insert into {table} values (1, 'one')"))
        .await
        .unwrap();
    assert_eq!(
        run(&open, &format!("select count(*) from {table}"))
            .await
            .unwrap()
            .as_deref(),
        Some("1")
    );
    run(&open, "commit").await.unwrap();

    // Loading rows (COPY … FROM STDIN), as an import does.
    let rows: Vec<Vec<Option<String>>> = (2..=1001)
        .map(|n| vec![Some(n.to_string()), Some(format!("row {n}"))])
        .collect();
    let loaded = open
        .session
        .connection()
        .insert_rows(
            &InsertTarget {
                schema: "public".into(),
                table: table.clone(),
                columns: vec!["id".into(), "name".into()],
            },
            TextRows::from_rows(2, &rows).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(loaded, 1000);

    // An agent's read: a read-only transaction with a server-side cursor, all inside it.
    let read = open
        .session
        .run_read_only(
            &format!("select count(*) from {table}"),
            &ReadLimits::default(),
        )
        .await
        .unwrap();
    assert_eq!(read.rows[0][0].to_string(), "1001");

    run(&open, &format!("drop table {table}")).await.unwrap();
    open.close().await;
}

#[tokio::test]
async fn the_session_s_schema_is_refused_with_why_behind_a_transaction_pooler() {
    let Some(server) = server() else { return };
    let ws = Workspace::new();
    let open = ws.open(&server, Some("transaction")).await;
    assert!(open.session.is_transaction_pooled());
    let error = set_active_schema(&open.session, "public")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("transaction mode"), "{error}");
    // "Read-only for this session" would hold for one transaction: refused, nothing changed.
    let refused = set_session_read_only(&open, true).await.unwrap_err();
    assert!(
        matches!(&refused, SessionReadOnlyError::ServerRefuses(why) if why.contains("transaction mode")),
        "{refused}"
    );
    assert!(!open.session.is_session_read_only());
    open.close().await;

    // In session mode the pooler keeps the session: the schema can be set.
    let open = ws.open(&server, Some("session")).await;
    assert!(!open.session.is_transaction_pooled());
    open.close().await;
}

/// The control: without the setting, the driver's named prepared statements meet a server
/// connection that never prepared them, so the setup above is hard enough to catch a driver
/// that relies on its session.
#[tokio::test]
async fn without_the_setting_a_transaction_pooler_loses_the_driver_s_statements() {
    let Some(server) = server() else { return };
    let ws = Workspace::new();
    let open = ws.open(&server, None).await;
    let mut failure = None;
    for n in 0..40 {
        if let Err(error) = bound(&open, "select $1::int", &[&n.to_string()]).await {
            failure = Some(error);
            break;
        }
    }
    let failure = failure.expect("a named statement was lost at least once");
    assert!(failure.contains("prepared statement"), "{failure}");
    open.close().await;
}
