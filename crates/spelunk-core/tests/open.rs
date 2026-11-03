//! End to end: TOML file → resolution → tunnel → driver → query.
//!
//! SQLite always runs. PostgreSQL needs `SPELUNK_TEST_PG` and the tunnels need
//! `SPELUNK_TEST_SSH` (see `testing/docker`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelunk_config::{
    ConnectionId, HopDraft, PasswordChoice, Resolver, Source, SshAuth, SystemBackend, TrustStore,
    TunnelDraft, load,
};
use spelunk_core::{
    CoreEvent, Engines, OpenConnection, OpenError, OpenOptions, ResultStore, TunnelPool,
    open_connection, test_tunnel,
};
use spelunk_driver_api::{Secret, Value};
use spelunk_ssh::{HostInfo, Interaction, KnownHosts, NonInteractive, SecretRequest, TunnelState};

/// Temporary, trusted repository with `.spelunk/` and a user directory.
struct Workspace {
    base: PathBuf,
    tunnels: Arc<TunnelPool>,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("spelunk-open-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelunk")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self {
            base,
            tunnels: Arc::new(TunnelPool::new()),
        }
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.base.join("repo").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    async fn open(
        &self,
        id: &str,
        password: Option<Secret>,
        ui: Arc<dyn Interaction>,
    ) -> Result<OpenConnection, OpenError> {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        // The repository is trusted as the test wrote it (ADR-022 pins its content).
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let id = ConnectionId {
            source: Source::Workspace,
            path: id.into(),
        };
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&id)
            .expect("resolving");
        let options = OpenOptions {
            tunnels: self.tunnels.clone(),
            known_hosts: KnownHosts {
                user_file: None,
                app_file: self.base.join("user/known_hosts"),
            },
            interaction: ui,
            password,
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

struct AcceptHosts;

#[async_trait]
impl Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &HostInfo) -> bool {
        true
    }

    async fn ask_secret(&self, _request: &SecretRequest) -> Option<Secret> {
        None
    }
}

/// First column of the first row of the script's last query.
async fn first_value(conn: &OpenConnection, script: &str) -> String {
    let events: Vec<CoreEvent> = conn.session.run_script(script).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{script}: {error:?}");
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
    let logical = stored.schema().columns[0].logical;
    Value::from_array(batch.column(0).as_ref(), 0, logical)
        .unwrap()
        .to_string()
}

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

/// Path as a TOML literal (single quotes: no escapes, works for Windows paths).
fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

#[tokio::test]
async fn sqlite_from_config() {
    let ws = Workspace::new("sqlite");
    ws.write("data/app.db", "");
    ws.write(
        ".spelunk/connections/local.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"data/app.db\"\n",
    );
    let conn = ws
        .open("local", None, Arc::new(NonInteractive))
        .await
        .unwrap();
    assert!(conn.tunnel_state().is_none());
    assert_eq!(
        first_value(
            &conn,
            "create table t (a int); insert into t values (41), (1); select sum(a) from t"
        )
        .await,
        "42"
    );
}

#[tokio::test]
async fn duckdb_from_config() {
    let ws = Workspace::new("duckdb");
    std::fs::create_dir_all(ws.base.join("repo/data")).unwrap();
    spelunk_core::create_file_database(
        spelunk_config::Engine::Duckdb,
        &ws.base.join("repo/data/app.duckdb"),
    )
    .unwrap();
    ws.write(
        ".spelunk/connections/local.toml",
        "schema_version = 1\nengine = \"duckdb\"\ndatabase = \"data/app.duckdb\"\n",
    );
    let conn = ws
        .open("local", None, Arc::new(NonInteractive))
        .await
        .unwrap();
    assert!(conn.tunnel_state().is_none());
    assert_eq!(
        first_value(
            &conn,
            "create table t (a int); insert into t values (41), (1); select sum(a) from t"
        )
        .await,
        "42"
    );
    // A second connection to the same file (another tab) shares it.
    let other = ws
        .open("local", None, Arc::new(NonInteractive))
        .await
        .unwrap();
    assert_eq!(first_value(&other, "select count(*) from t").await, "2");
}

#[tokio::test]
async fn postgres_direct_with_a_command_secret() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_PG") else {
        eprintln!("SPELUNK_TEST_PG not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("pg-direct");
    // The secret comes from a real shell command (cmd /C on Windows, sh -c on Unix).
    ws.write(
        ".spelunk/connections/pg.toml",
        &format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = {{ secret = \"cmd:echo spelunk\" }}\n\n[tls]\nmode = \"require\"\n"
        ),
    );
    let conn = ws.open("pg", None, Arc::new(NonInteractive)).await.unwrap();
    assert_eq!(first_value(&conn, "select current_user").await, "spelunk");
    assert_eq!(
        first_value(
            &conn,
            "select ssl from pg_stat_ssl where pid = pg_backend_pid()"
        )
        .await,
        "true"
    );
}

#[tokio::test]
async fn prompted_passwords_must_be_supplied() {
    let ws = Workspace::new("prompt");
    ws.write(
        ".spelunk/connections/pg.toml",
        "schema_version = 1\nengine = \"postgres\"\nhost = \"127.0.0.1\"\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = { secret = \"prompt\" }\n",
    );
    match ws.open("pg", None, Arc::new(NonInteractive)).await {
        Err(OpenError::PasswordRequired { save_to: None }) => {}
        Err(other) => panic!("expected PasswordRequired: {other}"),
        Ok(_) => panic!("should not connect without a password"),
    }
}

/// The typical company setup: two bastions and TLS verify-full against the server's real
/// name, even though the driver connects to the tunnel's local end.
#[tokio::test]
async fn postgres_through_two_bastions_with_verify_full() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelunk/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelunk/connections/prod/orders.toml",
        &format!(
            "schema_version = 1\nengine = \"postgres\"\nenv = \"prod\"\nhost = \"postgres\"\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {}\n",
            toml_path(&keys().join("ca.crt"))
        ),
    );

    let conn = ws
        .open(
            "prod/orders",
            Some(Secret::new("spelunk")),
            Arc::new(AcceptHosts),
        )
        .await
        .unwrap();
    assert!(conn.resolved.is_production());
    let mut state = conn.tunnel_state().expect("goes through a tunnel");
    assert_eq!(*state.borrow(), TunnelState::Up);
    assert_eq!(
        first_value(
            &conn,
            "select ssl from pg_stat_ssl where pid = pg_backend_pid()"
        )
        .await,
        "true"
    );
    assert_eq!(
        first_value(&conn, "select count(*) from big").await,
        "1000000"
    );

    conn.close().await;
    assert!(matches!(
        &*state.borrow_and_update(),
        TunnelState::Down { .. }
    ));
}

/// ADR-004: a shared tunnel. Two tabs against the same database and another database behind
/// the same bastion use a single SSH chain, which closes when the last connection closes.
#[tokio::test]
async fn connections_share_one_tunnel() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("shared");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelunk/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    for (id, database) in [("app", "spelunk"), ("admin", "postgres")] {
        ws.write(
            &format!(".spelunk/connections/{id}.toml"),
            &format!(
                "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"{database}\"\nuser = \"spelunk\"\npassword = {{ secret = \"cmd:echo spelunk\" }}\ntunnel = \"bastions\"\n"
            ),
        );
    }
    let ui = Arc::new(AcceptHosts);
    let tab1 = ws.open("app", None, ui.clone()).await.unwrap();
    let tab2 = ws.open("app", None, ui.clone()).await.unwrap();
    let admin = ws.open("admin", None, ui.clone()).await.unwrap();
    assert_eq!(
        ws.tunnels.active().await,
        1,
        "all three connections should share the tunnel"
    );
    assert_eq!(
        first_value(&tab1, "select current_database()").await,
        "spelunk"
    );
    assert_eq!(
        first_value(&tab2, "select current_database()").await,
        "spelunk"
    );
    assert_eq!(
        first_value(&admin, "select current_database()").await,
        "postgres"
    );

    let state = tab1.tunnel_state().unwrap();
    tab1.close().await;
    admin.close().await;
    assert_eq!(*state.borrow(), TunnelState::Up, "tab2 still uses it");
    assert_eq!(first_value(&tab2, "select 1").await, "1");
    tab2.close().await;
    assert!(matches!(&*state.borrow(), TunnelState::Down { .. }));
    assert_eq!(ws.tunnels.active().await, 0);
}

/// A tunnel typed in the editor (not saved) is tested by opening and closing its hops. A
/// wrong user fails at the hop that rejects it, and nothing is written to disk.
#[tokio::test]
async fn unsaved_tunnels_can_be_tested_hop_by_hop() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("tunnel-draft");
    let config = load(Some(&ws.base.join("repo")), &ws.base.join("user"));
    let mut trust = TrustStore::load(&ws.base.join("user")).unwrap();
    trust.trust(&config).unwrap();
    let identity = keys().join("id_ed25519").display().to_string();
    let hop = |host: &str, port: Option<u16>, user: &str| HopDraft {
        host: host.into(),
        port,
        user: user.into(),
        auth: vec![SshAuth::Key],
        identity_file: identity.clone(),
        password: PasswordChoice::None,
    };
    let mut draft = TunnelDraft {
        source: Source::Workspace,
        name: "Bastions".into(),
        hops: vec![
            hop(host, Some(port.parse().unwrap()), "spelunk"),
            hop("bastion2", None, "spelunk"),
        ],
    };
    let known_hosts = KnownHosts {
        user_file: None,
        app_file: ws.base.join("user/known_hosts"),
    };
    let resolved = Resolver::new(&config, &trust, &SystemBackend)
        .resolve_tunnel_draft(&draft)
        .unwrap();
    test_tunnel(&resolved, vec![], &known_hosts, Arc::new(AcceptHosts))
        .await
        .unwrap();

    draft.hops[1].user = "nobody".into();
    let resolved = Resolver::new(&config, &trust, &SystemBackend)
        .resolve_tunnel_draft(&draft)
        .unwrap();
    match test_tunnel(&resolved, vec![], &known_hosts, Arc::new(AcceptHosts)).await {
        Err(OpenError::Tunnel(spelunk_ssh::SshError::Auth { hop: 1, .. })) => {}
        other => panic!("expected an authentication error at hop 2: {other:?}"),
    }
    assert!(
        !ws.base.join("repo/.spelunk/tunnels").exists(),
        "testing writes nothing"
    );
}

/// MySQL behind the two bastions with verify-full: the driver connects to the tunnel's local
/// end but checks the certificate against the server's real name ("mysql").
#[tokio::test]
async fn mysql_through_two_bastions_with_verify_full() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    if std::env::var("SPELUNK_TEST_MYSQL").is_err() {
        eprintln!("SPELUNK_TEST_MYSQL not set: test skipped");
        return;
    }
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("mysql-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelunk/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    ws.write(
        ".spelunk/connections/shop.toml",
        &format!(
            "schema_version = 1\nengine = \"mysql\"\nhost = \"mysql\"\ndatabase = \"spelunk\"\nuser = \"spelunk\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {}\n",
            toml_path(&keys().join("ca.crt"))
        ),
    );
    let conn = ws
        .open("shop", Some(Secret::new("spelunk")), Arc::new(AcceptHosts))
        .await
        .unwrap();
    assert_eq!(
        first_value(
            &conn,
            "select variable_value <> '' from performance_schema.session_status where variable_name = 'Ssl_cipher'"
        )
        .await,
        "1"
    );
    conn.close().await;
}

/// The tunnel to the Docker server that holds a SQLite file (ADR-018): bastion1, then
/// `last_hop` (`sqlitehost` runs commands; `bastion2` only forwards).
fn sqlite_server(ws: &Workspace, addr: &str, last_hop: &str) {
    let (host, port) = addr.rsplit_once(':').unwrap();
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelunk/tunnels/server.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"{last_hop}\"\nuser = \"spelunk\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
}

/// Every event of a script, and the error of its failed statement if any.
async fn script(
    conn: &OpenConnection,
    sql: &str,
) -> (Vec<CoreEvent>, Option<spelunk_core::StatementError>) {
    let events: Vec<CoreEvent> = conn.session.run_script(sql).collect().await;
    let error = events.iter().find_map(|e| match e {
        CoreEvent::StatementFailed { error, .. } => Some(error.clone()),
        _ => None,
    });
    (events, error)
}

fn first_result(events: &[CoreEvent]) -> spelunk_core::ResultId {
    events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result")
}

#[tokio::test]
async fn sqlite_on_a_server_through_two_hops() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let ws = Workspace::new("sqlite-remote");
    sqlite_server(&ws, &addr, "sqlitehost");
    ws.write(
        ".spelunk/connections/shop.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelunk/shop.db\"\ntunnel = \"server\"\n",
    );
    let conn = ws
        .open("shop", None, Arc::new(AcceptHosts))
        .await
        .expect("opening the remote file");
    let version = conn.session.connection().server_version().await.unwrap();
    assert!(version.starts_with("3."), "{version}");

    // Values keep their storage class: integers, text, reals and blobs.
    let (events, error) = script(
        &conn,
        "select id, name, price, photo from product order by id",
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    let stored = conn.session.results().get(first_result(&events)).unwrap();
    {
        use spelunk_driver_api::LogicalType;
        let stored = stored.read().unwrap();
        let logical: Vec<_> = stored.schema().columns.iter().map(|c| c.logical).collect();
        assert_eq!(
            logical,
            [
                LogicalType::Int,
                LogicalType::Text,
                LogicalType::Float,
                LogicalType::Bytes
            ]
        );
        let batch = &stored.window(0, 3)[0];
        let cell = |c: usize, r: usize| {
            Value::from_array(
                batch.column(c).as_ref(),
                r,
                stored.schema().columns[c].logical,
            )
            .unwrap()
            .to_string()
        };
        assert_eq!(
            (cell(1, 0), cell(2, 0), cell(3, 0)),
            ("Lamp".into(), "19.5".into(), "\\x00ff".into())
        );
        assert_eq!(cell(1, 2), "it's a chair");
    }

    // The explorer reads the server's catalog.
    let objects = conn
        .session
        .connection()
        .list_objects("main")
        .await
        .unwrap();
    assert!(objects.iter().any(|o| o.name == "product"), "{objects:?}");
    let product = conn
        .session
        .connection()
        .describe_table("main", "product")
        .await
        .unwrap();
    assert_eq!(product.primary_key, ["id"]);

    // Writes, row counts, a value with a line break and a transaction across statements.
    let (events, error) = script(
        &conn,
        "create table if not exists remote_t (id integer primary key, note text);\n\
         delete from remote_t;\n\
         insert into remote_t values (1, 'a'), (2, 'it''s\nmultiline');\n\
         begin; delete from remote_t; rollback",
    )
    .await;
    assert!(error.is_none(), "{error:?}");
    assert!(events.iter().any(|e| matches!(
        e,
        CoreEvent::StatementFinished {
            index: 2,
            rows_affected: Some(2),
            ..
        }
    )));
    assert_eq!(
        first_value(&conn, "select note from remote_t where id = 2").await,
        "it's\nmultiline"
    );

    // Editing a page of the table, in one transaction.
    let table = conn
        .session
        .connection()
        .describe_table("main", "remote_t")
        .await
        .unwrap();
    let page = spelunk_core::TablePage {
        schema: "main".into(),
        name: "remote_t".into(),
        filter: String::new(),
        order_by: "id".into(),
        limit: 10,
        offset: 0,
    };
    let sql = spelunk_core::table_page_sql(conn.session.dialect().as_ref(), &page).unwrap();
    let (events, _) = script(&conn, &sql).await;
    let stored = conn.session.results().get(first_result(&events)).unwrap();
    let statements = spelunk_core::edit_statements(
        conn.session.dialect().as_ref(),
        &table,
        &stored.read().unwrap(),
        &[spelunk_core::RowChange::Update {
            row: 0,
            values: vec![(1, spelunk_core::CellInput::Text("edited".into()))],
        }],
    )
    .unwrap();
    assert_eq!(
        spelunk_core::apply_edits(&conn.session, &statements).await,
        Ok(1)
    );
    assert_eq!(
        first_value(&conn, "select note from remote_t where id = 1").await,
        "edited"
    );

    // The shell never runs commands of its own, nor a statement that would swallow the rest.
    let (_, error) = script(&conn, ".shell echo pwned").await;
    assert!(error.unwrap().message.contains("not run on the server"));
    let (_, error) = script(&conn, "select 'open").await;
    assert!(error.unwrap().message.contains("not complete"));

    // Cancelling interrupts the statement and keeps the session.
    let cancel = conn.session.connection().cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    let (_, error) = script(
        &conn,
        "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c",
    )
    .await;
    canceller.await.unwrap().unwrap();
    assert!(error.as_ref().is_some_and(|e| e.cancelled), "{error:?}");
    // sqlite3 ends when interrupted: the session says so, and a new one goes on.
    let (_, error) = script(&conn, "select 1").await;
    let message = error.unwrap().message;
    assert!(message.contains("reconnect"), "{message}");
    let conn = ws.open("shop", None, Arc::new(AcceptHosts)).await.unwrap();
    assert_eq!(
        first_value(&conn, "select count(*) from remote_t").await,
        "2"
    );
    let (_, error) = script(&conn, "drop table remote_t").await;
    assert!(error.is_none(), "{error:?}");

    // Safe mode: what touches the server's files ends sqlite3, and the session says so.
    let (_, error) = script(&conn, "select writefile('/tmp/x', 1)").await;
    let message = error.unwrap().message;
    assert!(message.contains("safe mode"), "{message}");
    let (_, error) = script(&conn, "select 1").await;
    assert!(error.is_some(), "the session ended with sqlite3");
}

#[tokio::test]
async fn sqlite_on_a_server_says_what_is_wrong() {
    let Ok(addr) = std::env::var("SPELUNK_TEST_SSH") else {
        eprintln!("SPELUNK_TEST_SSH not set: test skipped");
        return;
    };
    let ws = Workspace::new("sqlite-remote-errors");
    sqlite_server(&ws, &addr, "sqlitehost");
    ws.write(
        ".spelunk/connections/missing.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelunk/missing.db\"\ntunnel = \"server\"\n",
    );
    ws.write(
        ".spelunk/connections/readonly.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelunk/other.db\"\ntunnel = \"server\"\nread_only = true\n",
    );
    match ws.open("missing", None, Arc::new(AcceptHosts)).await {
        Err(e) => assert!(
            e.to_string().contains("does not exist on the server"),
            "{e}"
        ),
        Ok(_) => panic!("a missing file must not open (nor be created)"),
    }
    let conn = ws
        .open("readonly", None, Arc::new(AcceptHosts))
        .await
        .unwrap();
    let (_, error) = script(&conn, "insert into product (name) values ('x')").await;
    let message = error.unwrap().message;
    assert!(message.to_lowercase().contains("readonly"), "{message}");

    // A last hop that only forwards ports (bastion2 runs no commands).
    let ws = Workspace::new("sqlite-remote-noshell");
    sqlite_server(&ws, &addr, "bastion2");
    ws.write(
        ".spelunk/connections/shop.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/home/spelunk/shop.db\"\ntunnel = \"server\"\n",
    );
    match ws.open("shop", None, Arc::new(AcceptHosts)).await {
        Err(e) => {
            let message = e.to_string();
            assert!(
                message.contains("sqlite3 on the server ended") || message.contains("refused"),
                "{message}"
            )
        }
        Ok(_) => panic!("a bastion that runs no commands cannot open a file"),
    }
}
