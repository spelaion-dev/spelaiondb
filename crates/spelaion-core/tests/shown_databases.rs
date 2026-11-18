//! The databases a connection shows (SDB-396), against real servers through their plugins:
//! MySQL and MariaDB (whose databases are the explorer's schemas) and SQL Server (whose
//! database picker lists them). Only the named databases and the connection's own are listed;
//! it is what is shown, not a permission. Needs `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB`
//! and `SPELAION_TEST_MSSQL` (each server is skipped without its variable).

use std::path::PathBuf;
use std::sync::Arc;

use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    Engines, OpenConnection, OpenOptions, ResultStore, TunnelPool, open_connection,
};
use spelaion_driver_api::Secret;
use spelaion_ssh::{KnownHosts, NonInteractive};

mod engines;

struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "spelaion-shown-databases-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion/connections")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    async fn open(&self, id: &str, content: &str, password: &str) -> OpenConnection {
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
                password: Some(Secret::new(password)),
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

fn addr(var: &str) -> Option<(String, u16)> {
    let value = std::env::var(var).ok()?;
    let (host, port) = value.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

fn lower(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = names.into_iter().map(|n| n.to_lowercase()).collect();
    names.sort();
    names
}

#[tokio::test]
async fn mysql_and_mariadb_show_only_the_named_databases_as_schemas() {
    for (var, engine) in [
        ("SPELAION_TEST_MYSQL", "mysql"),
        ("SPELAION_TEST_MARIADB", "mariadb"),
    ] {
        let Some((host, port)) = addr(var) else {
            eprintln!("{var} not set: {engine} skipped");
            continue;
        };
        let ws = Workspace::new(engine);
        let head = format!(
            "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"root\"\npassword = {{ secret = \"prompt\" }}\n"
        );
        // Without a list: every database, the system ones too.
        let all = ws.open("all", &head, "spelaion-root").await;
        let schemas = all.session.connection().list_schemas().await.unwrap();
        assert!(
            schemas.iter().any(|s| s.name == "information_schema"),
            "{engine}: {schemas:?}"
        );
        all.close().await;

        // `MySQL` in another case still names `mysql`; the connection's own is always shown.
        let shown = ws
            .open(
                "shown",
                &format!("{head}databases = [\"MySQL\", \"no_such_database\"]\n"),
                "spelaion-root",
            )
            .await;
        let connection = shown.session.connection();
        let schemas = connection.list_schemas().await.unwrap();
        assert_eq!(
            lower(schemas.into_iter().map(|s| s.name)),
            ["mysql", "spelaion"],
            "{engine}"
        );
        if let Ok(databases) = connection.list_databases().await {
            assert_eq!(lower(databases), ["mysql", "spelaion"], "{engine}");
        }
        // What is shown, never a permission: another database can still be read.
        let tables = spelaion_driver_api::NameFilter::default();
        let matching = connection.list_schemas_matching(&tables).await.unwrap();
        assert_eq!(matching.len(), 2, "{engine}: {matching:?}");
        let mut events = shown
            .session
            .run_script("select count(*) from information_schema.schemata");
        let mut failed = None;
        while let Some(event) = futures_util::StreamExt::next(&mut events).await {
            if let spelaion_core::CoreEvent::StatementFailed { error, .. } = event {
                failed = Some(error.message);
            }
        }
        assert_eq!(failed, None, "{engine}");
        shown.close().await;
    }
}

#[tokio::test]
async fn sql_server_shows_only_the_named_databases_and_all_of_their_schemas() {
    let Some((host, port)) = addr("SPELAION_TEST_MSSQL") else {
        eprintln!("SPELAION_TEST_MSSQL not set: test skipped");
        return;
    };
    let ws = Workspace::new("sqlserver");
    let head = format!(
        "schema_version = 1\nengine = \"sqlserver\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"sa\"\npassword = {{ secret = \"prompt\" }}\ndatabases = [\"master\"]\n\n[tls]\nmode = \"require\"\n"
    );
    let shown = ws.open("shown", &head, "Spelaion-test-1").await;
    let connection = shown.session.connection();
    assert_eq!(
        lower(connection.list_databases().await.unwrap()),
        ["master", "spelaion"]
    );
    // Its schemas are not databases: every one is listed.
    let schemas = connection.list_schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "dbo"), "{schemas:?}");
    shown.close().await;
}
