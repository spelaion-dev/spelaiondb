//! Another database of a PostgreSQL connection (ADR-074, SDB-493), against the real PostgreSQL
//! of `testing/docker`: the databases a connection opens, a session on one of them through the
//! connection's own tunnel, and every refusal tried from the outside. Needs `SPELAION_TEST_PG`,
//! and `SPELAION_TEST_SSH` for the tunnel; without them the tests are skipped, which does not
//! count as tested. The PostgreSQL plugin must answer `list_databases` (SDB-489).

use crate::engines;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_config::{Config, ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    Configuration, ConnectionDatabases, CoreEvent, Engines, OpenConnection, OpenOptions,
    OtherDatabaseError, ResultStore, Session, TunnelPool, connection_databases, database_owner,
    open_connection, open_other_database,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, Value};
use spelaion_ssh::{HostInfo, Interaction, KnownHosts, SecretRequest};

/// This file's databases and schema, apart from every other test's.
const BILLING: &str = "core_other_billing";
const REPORTS: &str = "core_other_reports";
const OWNED: &str = "core_other_owned";
const OWN_SCHEMA: &str = "core_other_only_here";

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

/// A trusted repository of connection files, its configuration and its tunnels.
struct Workspace {
    base: PathBuf,
    tunnels: Arc<TunnelPool>,
    config: Config,
    trust: TrustStore,
}

impl Workspace {
    fn new(name: &str, files: &[(&str, String)]) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-other-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        for (relative, content) in files {
            let path = base.join("repo/.spelaion").join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let config = load(Some(&base.join("repo")), &base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        // Trusted as the test wrote it (ADR-022 pins its content).
        let mut trust = TrustStore::load(&base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        Self {
            base,
            tunnels: Arc::new(TunnelPool::new()),
            config,
            trust,
        }
    }

    fn options(&self) -> OpenOptions {
        OpenOptions {
            tunnels: self.tunnels.clone(),
            known_hosts: KnownHosts {
                team_files: Vec::new(),
                user_file: None,
                app_file: self.base.join("user/known_hosts"),
            },
            interaction: Arc::new(AcceptHosts),
            password: None,
            read_only_session: false,
        }
    }

    fn configuration(&self) -> Configuration<'_> {
        Configuration {
            config: &self.config,
            trust: &self.trust,
            backend: &SystemBackend,
        }
    }

    async fn open(&self, id: &str) -> OpenConnection {
        let id = ConnectionId {
            source: Source::Workspace,
            path: id.into(),
        };
        let resolved = Resolver::new(&self.config, &self.trust, &SystemBackend)
            .resolve(&id)
            .expect("resolving");
        open_connection(
            resolved,
            &Engines::builtin(),
            self.options(),
            Arc::new(ResultStore::new()),
        )
        .await
        .unwrap_or_else(|e| panic!("opening {}: {e}", id.path))
    }

    async fn other(
        &self,
        own: &OpenConnection,
        database: &str,
    ) -> Result<OpenConnection, OtherDatabaseError> {
        open_other_database(
            own,
            database,
            &self.configuration(),
            &Engines::builtin(),
            self.options(),
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

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

/// Path as a TOML literal (single quotes: no escapes, works for Windows paths).
fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// The first column of the first row of `sql`, or the server's refusal.
async fn value(session: &Session, sql: &str) -> Result<String, String> {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        return Err(error.message.clone());
    }
    let result = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .ok_or("no result")?;
    let stored = session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    let logical = stored.schema().columns[0].logical;
    Ok(Value::from_array(batch.column(0).as_ref(), 0, logical)
        .unwrap()
        .to_string())
}

/// The test's databases and the schema that exists only in the connection's own database,
/// made by the server's superuser directly, once for the file's tests (which run together).
async fn prepare(host: &str, port: u16) {
    static PREPARED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    PREPARED.get_or_init(|| make(host, port)).await;
}

async fn make(host: &str, port: u16) {
    let driver = engines::postgres();
    let conn = driver
        .connect(&ConnectParams {
            host: host.into(),
            port,
            user: "spelaion".into(),
            password: Some(Secret::new("spelaion")),
            database: "spelaion".into(),
            ..Default::default()
        })
        .await
        .expect("connecting as the superuser");
    let admin = Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    for database in [BILLING, REPORTS, OWNED] {
        // CREATE DATABASE cannot run in a transaction and has no IF NOT EXISTS: an error that
        // it exists is fine.
        let _: Vec<CoreEvent> = admin
            .run_script(&format!("CREATE DATABASE {database}"))
            .collect()
            .await;
    }
    value(
        &admin,
        &format!("CREATE SCHEMA IF NOT EXISTS {OWN_SCHEMA}; SELECT 1"),
    )
    .await
    .expect("the own database's schema");
}

/// What `connection_databases` answers, as names.
async fn opened(own: &OpenConnection) -> Vec<String> {
    match connection_databases(own).await {
        ConnectionDatabases::Several(names) => names,
        other => panic!("not several databases: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_other_database_opens_through_the_connections_tunnel_as_that_connection() {
    let _turn = crate::serial::file_turn(module_path!());
    let (Some((pg_host, pg_port)), Some((ssh_host, ssh_port))) =
        (server("SPELAION_TEST_PG"), server("SPELAION_TEST_SSH"))
    else {
        return;
    };
    prepare(&pg_host, pg_port).await;
    let identity = toml_path(&keys().join("id_ed25519"));
    let tunnel = format!(
        "schema_version = 1\n\n[[hops]]\nhost = \"{ssh_host}\"\nport = {ssh_port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
    );
    // Read-only, with a default schema that exists only in its own database, and a list of
    // the others it opens. `owned` is another connection, same login, whose database is one of
    // them.
    let connection = |database: &str, extra: &str| {
        format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"{database}\"\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\ntunnel = \"bastions\"\n{extra}"
        )
    };
    let ws = Workspace::new(
        "tunnel",
        &[
            ("tunnels/bastions.toml", tunnel),
            (
                "connections/tenants.toml",
                connection(
                    "spelaion",
                    &format!(
                        "read_only = true\ndatabases = [\"{BILLING}\", \"{OWNED}\"]\n\n[session]\nschema = \"{OWN_SCHEMA}\"\n"
                    ),
                ),
            ),
            ("connections/owned.toml", connection(OWNED, "")),
        ],
    );
    let own = ws.open("tenants").await;
    assert_eq!(
        value(&own.session, "select current_schema()")
            .await
            .unwrap(),
        OWN_SCHEMA
    );
    // Its own first, then what the file names and the server lists.
    assert_eq!(opened(&own).await, ["spelaion", BILLING, OWNED]);
    // The explorer shows which one has a connection of its own, and leads there (SDB-504).
    let owner = database_owner(&own, OWNED, &ws.configuration()).map(|(_, name)| name);
    assert!(
        owner.is_some_and(|n| n.contains("owned")),
        "{OWNED} belongs to `owned`"
    );
    assert_eq!(database_owner(&own, BILLING, &ws.configuration()), None);
    assert_eq!(database_owner(&own, "spelaion", &ws.configuration()), None);

    let billing = ws.other(&own, BILLING).await.expect("opening billing");
    assert_eq!(
        value(&billing.session, "select current_database()")
            .await
            .unwrap(),
        BILLING
    );
    assert_eq!(
        ws.tunnels.active().await,
        1,
        "the other database goes through the connection's own tunnel"
    );
    // The connection's read-only mode holds there: the server's and the core's.
    assert_eq!(
        value(&billing.session, "show transaction_read_only")
            .await
            .unwrap(),
        "on"
    );
    assert!(
        value(&billing.session, "create table core_other_written (id int)")
            .await
            .is_err(),
        "a read-only connection stays read-only in another database"
    );
    // The default schema is its own database's: here the server's search_path applies.
    assert_eq!(
        value(&billing.session, "select current_schema()")
            .await
            .unwrap(),
        "public"
    );

    // Trying to get around the list: another case, a database it does not name, one that has
    // a connection of its own.
    let upper = BILLING.to_uppercase();
    assert!(matches!(
        ws.other(&own, &upper).await,
        Err(OtherDatabaseError::NotNamed { database }) if database == upper
    ));
    assert!(matches!(
        ws.other(&own, REPORTS).await,
        Err(OtherDatabaseError::NotNamed { .. })
    ));
    match ws.other(&own, OWNED).await {
        Err(OtherDatabaseError::HasOwnConnection { name, .. }) => {
            assert!(name.contains("owned"), "the connection to use: {name}")
        }
        other => panic!(
            "{OWNED} opens only as its own connection: {:?}",
            other.err()
        ),
    }
    // Its own database opens as the connection does, with its schema.
    let again = ws.other(&own, "spelaion").await.unwrap();
    assert_eq!(
        value(&again.session, "select current_schema()")
            .await
            .unwrap(),
        OWN_SCHEMA
    );

    // A pooler (which a file cannot have with other databases) is refused here too.
    let mut pooled = ws.open("tenants").await;
    pooled
        .resolved
        .params
        .options
        .insert("pooler".into(), "session".into());
    assert!(matches!(
        ws.other(&pooled, BILLING).await,
        Err(OtherDatabaseError::Pooler)
    ));
    assert!(matches!(
        connection_databases(&pooled).await,
        ConnectionDatabases::OwnOnly { .. }
    ));

    for open in [billing, again, pooled, own] {
        open.close().await;
    }
    assert_eq!(ws.tunnels.active().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn postgres_all_databases_are_what_the_server_lists_and_nothing_else() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server("SPELAION_TEST_PG") else {
        return;
    };
    prepare(&host, port).await;
    let file = |database: &str, extra: &str| {
        format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"{database}\"\nuser = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\n{extra}"
        )
    };
    let ws = Workspace::new(
        "all",
        &[
            (
                "connections/all.toml",
                file("spelaion", "all_databases = true\n"),
            ),
            ("connections/plain.toml", file("spelaion", "")),
        ],
    );
    let all = ws.open("all").await;
    let names = opened(&all).await;
    assert_eq!(names[0], "spelaion", "its own first");
    for name in [BILLING, REPORTS, OWNED, "postgres"] {
        assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
    }
    assert!(
        !names.iter().any(|n| n.starts_with("template")),
        "{names:?}"
    );
    let reports = ws.other(&all, REPORTS).await.expect("opening reports");
    assert_eq!(
        value(&reports.session, "select current_database()")
            .await
            .unwrap(),
        REPORTS
    );
    // A template accepts connections, but the server does not list it: refused, as is a name
    // the server does not have.
    assert!(matches!(
        ws.other(&all, "template1").await,
        Err(OtherDatabaseError::NotListed { .. })
    ));
    assert!(matches!(
        ws.other(&all, "core_other_no_such_database").await,
        Err(OtherDatabaseError::NotListed { .. })
    ));

    // A connection that names no other database opens only its own.
    let plain = ws.open("plain").await;
    assert_eq!(
        connection_databases(&plain).await,
        ConnectionDatabases::Own("spelaion".into())
    );
    assert!(matches!(
        ws.other(&plain, BILLING).await,
        Err(OtherDatabaseError::NoneNamed)
    ));
    for open in [reports, all, plain] {
        open.close().await;
    }
}
