//! Backup and restore through the tunnel (ADR-049): the vendors' tools dump a PostgreSQL, a
//! MySQL and a MariaDB database through the two SSH hops of `testing/docker`, the data is
//! dropped and restored from the file, and it must come back as it was. Each run's log and its
//! command line must never hold the password.
//!
//! Needs `SPELAION_TEST_SSH` and each server's variable (`SPELAION_TEST_PG`,
//! `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB`). The tools are those of the official images
//! the test services use, started with `docker run` (so this machine needs none of them), or the
//! ones in `SPELAION_TEST_DB_TOOLS` (a folder) when it is set.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, ResolvedConnection, Resolver, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::backup::{
    BackupCancel, BackupError, BackupEvent, DumpOptions, DumpRequest, MysqlDumpOptions,
    MysqlRestoreOptions, PgDumpOptions, PgFormat, PgRestoreOptions, RestoreFormat, RestoreOptions,
    RestoreRequest, ToolSource, run_dump, run_restore,
};
use spelaion_core::{
    CoreEvent, Engines, OpenConnection, OpenOptions, ResultStore, RouteOptions, TunnelPool,
    open_connection,
};
use spelaion_driver_api::{Secret, Value};
use spelaion_ssh::{HostInfo, Interaction, KnownHosts, SecretRequest};

mod support;
use support::{cleaning_up, run_cleanup};

/// The password of the account each test creates: distinctive, so finding any piece of it in a
/// log means it leaked.
fn password() -> String {
    format!("Bk-{}-s3cret'pw", std::process::id())
}

struct Workspace {
    base: PathBuf,
    tunnels: Arc<TunnelPool>,
}

impl Workspace {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        std::fs::create_dir_all(base.join("out")).unwrap();
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

    fn known_hosts(&self) -> KnownHosts {
        KnownHosts {
            team_file: None,
            user_file: None,
            app_file: self.base.join("user/known_hosts"),
        }
    }

    fn resolve(&self, id: &str) -> ResolvedConnection {
        let config = load(Some(&self.base.join("repo")), &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let id = ConnectionId {
            source: Source::Workspace,
            path: id.into(),
        };
        Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&id)
            .expect("resolving")
    }

    fn route(&self) -> RouteOptions {
        RouteOptions {
            tunnels: self.tunnels.clone(),
            known_hosts: self.known_hosts(),
            interaction: Arc::new(AcceptHosts),
        }
    }

    async fn open(&self, id: &str, password: &str) -> OpenConnection {
        let options = OpenOptions {
            tunnels: self.tunnels.clone(),
            known_hosts: self.known_hosts(),
            interaction: Arc::new(AcceptHosts),
            password: Some(Secret::new(password)),
            read_only_session: false,
        };
        open_connection(
            self.resolve(id),
            &Engines::builtin(),
            options,
            Arc::new(ResultStore::new()),
        )
        .await
        .expect("opening")
    }

    fn out(&self) -> PathBuf {
        self.base.join("out")
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

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

fn toml_path(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// The two hops of `testing/docker`.
fn write_tunnel(ws: &Workspace, ssh: &str) {
    let (host, port) = ssh.rsplit_once(':').unwrap();
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
}

/// Runs a script, failing the test on any error.
async fn exec(conn: &OpenConnection, script: &str) {
    let events: Vec<CoreEvent> = conn.session.run_script(script).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{script}: {error:?}");
    }
}

/// Every cell of the script's last result, as text, row by row.
async fn rows(conn: &OpenConnection, script: &str) -> Vec<Vec<String>> {
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
    let logicals: Vec<_> = stored.schema().columns.iter().map(|c| c.logical).collect();
    let mut out = Vec::new();
    for batch in stored.window(0, 100_000) {
        for row in 0..batch.num_rows() {
            out.push(
                (0..batch.num_columns())
                    .map(|c| {
                        Value::from_array(batch.column(c).as_ref(), row, logicals[c])
                            .unwrap()
                            .to_string()
                    })
                    .collect(),
            );
        }
    }
    out
}

/// The tools: a local folder, or the image's own run with `docker run`. On Linux the container
/// shares the host's network, so the tunnel's `127.0.0.1` is its own; elsewhere Docker Desktop
/// reaches the host's loopback as `host.docker.internal`.
fn tools(image: &str) -> ToolSource {
    if let Ok(folder) = std::env::var("SPELAION_TEST_DB_TOOLS") {
        return ToolSource::Folder(PathBuf::from(folder));
    }
    let mut leading: Vec<String> = ["run", "--rm", "-i", "-e", "PGPASSWORD", "-e", "MYSQL_PWD"]
        .map(String::from)
        .to_vec();
    let reach_host = if cfg!(target_os = "linux") {
        leading.extend(["--network".into(), "host".into()]);
        None
    } else {
        Some("host.docker.internal".to_string())
    };
    leading.push(image.to_string());
    ToolSource::Launcher {
        program: PathBuf::from("docker"),
        leading_args: leading,
        reach_host,
    }
}

/// Collects a run's events.
#[derive(Default)]
struct Log(Mutex<Vec<BackupEvent>>);

impl Log {
    fn sink(&self) -> impl Fn(BackupEvent) + Send + Sync + '_ {
        move |e| self.0.lock().unwrap().push(e)
    }

    /// Every text the run showed: the command line and each log line.
    fn texts(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                BackupEvent::Started { command_line } => Some(command_line.clone()),
                BackupEvent::Log { line } => Some(line.clone()),
                BackupEvent::Progress { .. } | BackupEvent::Statements { .. } => None,
            })
            .collect()
    }

    fn assert_no_password(&self, password: &str) {
        let texts = self.texts();
        assert!(
            texts.iter().any(|t| t.contains("--")),
            "the command line was shown: {texts:?}"
        );
        for text in texts {
            assert!(!text.contains(password), "the password leaked: {text}");
            assert!(
                !text.contains("s3cret"),
                "part of the password leaked: {text}"
            );
        }
    }
}

fn no_part_files(folder: &Path) {
    for entry in std::fs::read_dir(folder).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        assert!(!name.ends_with(".part"), "{name} was left behind");
    }
}

const PG_IMAGE: &str = "postgis/postgis:17-3.5-alpine";

#[tokio::test]
async fn postgres_dumps_and_restores_through_two_hops() {
    let (Ok(ssh), Ok(pg)) = (
        std::env::var("SPELAION_TEST_SSH"),
        std::env::var("SPELAION_TEST_PG"),
    ) else {
        eprintln!("SPELAION_TEST_SSH or SPELAION_TEST_PG not set: test skipped");
        return;
    };
    let pid = std::process::id();
    let role = format!("backup_role_{pid}");
    let schema = format!("backup_{pid}");
    let password = password();
    let ws = Workspace::new("pg");
    write_tunnel(&ws, &ssh);
    let (pg_host, pg_port) = pg.rsplit_once(':').unwrap();
    ws.write(
        ".spelaion/connections/admin.toml",
        &format!(
            "schema_version = 1\nengine = \"postgres\"\nhost = \"{pg_host}\"\nport = {pg_port}\ndatabase = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\n"
        ),
    );
    for (id, extra) in [("remote", ""), ("remote_ro", "read_only = true\n")] {
        ws.write(
            &format!(".spelaion/connections/{id}.toml"),
            &format!(
                "schema_version = 1\nengine = \"postgres\"\nhost = \"postgres\"\ndatabase = \"spelaion\"\nuser = \"{role}\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n{extra}"
            ),
        );
    }
    let admin = ws.open("admin", "spelaion").await;
    let cleanup = vec![
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("REVOKE CREATE ON DATABASE spelaion FROM {role}"),
        format!("DROP ROLE IF EXISTS {role}"),
    ];
    let body = async {
        let quoted = password.replace('\'', "''");
        exec(
            &admin,
            &format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE; DROP ROLE IF EXISTS {role};
                 CREATE ROLE {role} LOGIN PASSWORD '{quoted}';
                 GRANT CREATE ON DATABASE spelaion TO {role};
                 CREATE SCHEMA {schema} AUTHORIZATION {role};
                 SET ROLE {role};
                 CREATE TABLE {schema}.items (id serial PRIMARY KEY, name text, price numeric(10,2), data bytea, at timestamptz);
                 INSERT INTO {schema}.items (name, price, data, at) VALUES
                   ('it''s', 1.50, '\\x00ff', '2026-11-11 10:00:00+00'),
                   ('ünïcødé ✓', NULL, NULL, NULL),
                   (E'tab\\there', 99999999.99, '\\xdeadbeef', '1999-12-31 23:59:59+00');
                 CREATE VIEW {schema}.cheap AS SELECT name FROM {schema}.items WHERE price < 10;
                 RESET ROLE;"
            ),
        )
        .await;
        let snapshot = format!(
            "SELECT id, name, price, encode(data, 'hex'), at, (SELECT last_value FROM {schema}.items_id_seq) FROM {schema}.items ORDER BY id"
        );
        let before = rows(&admin, &snapshot).await;
        assert_eq!(before.len(), 3);

        let resolved = ws.resolve("remote");
        for format in [PgFormat::Custom, PgFormat::Plain, PgFormat::Tar] {
            let log = Log::default();
            let request = DumpRequest {
                tools: tools(PG_IMAGE),
                folder: ws.out(),
                name_pattern: format!("{{database}}_{format:?}_{{timestamp}}"),
                extra_args: String::new(),
                options: DumpOptions::Postgres(PgDumpOptions {
                    schemas: vec![schema.clone()],
                    format,
                    ..PgDumpOptions::default()
                }),
            };
            let summary = run_dump(
                &resolved,
                Some(Secret::new(password.clone())),
                ws.route(),
                &request,
                None,
                &BackupCancel::new(),
                &log.sink(),
            )
            .await
            .unwrap_or_else(|e| panic!("{format:?} dump: {e}\n{:#?}", log.texts()));
            assert!(summary.bytes > 0);
            assert!(summary.path.is_file(), "{}", summary.path.display());
            no_part_files(&ws.out());
            log.assert_no_password(&password);

            exec(&admin, &format!("DROP SCHEMA {schema} CASCADE")).await;

            // A read-only connection and an unconfirmed restore are refused before anything runs.
            let restore = RestoreRequest {
                tools: tools(PG_IMAGE),
                input: summary.path.clone(),
                format: RestoreFormat::Auto,
                extra_args: String::new(),
                options: RestoreOptions::Postgres(PgRestoreOptions::default()),
            };
            let read_only = ws.resolve("remote_ro");
            let refused = run_restore(
                &read_only,
                Some(Secret::new(password.clone())),
                ws.route(),
                &restore,
                true,
                &BackupCancel::new(),
                &|_| panic!("nothing runs on a read-only connection"),
            )
            .await;
            assert!(matches!(refused, Err(BackupError::ReadOnly)), "{refused:?}");
            let unconfirmed = run_restore(
                &resolved,
                Some(Secret::new(password.clone())),
                ws.route(),
                &restore,
                false,
                &BackupCancel::new(),
                &|_| panic!("nothing runs unconfirmed"),
            )
            .await;
            assert!(matches!(unconfirmed, Err(BackupError::NeedsConfirmation)));

            let log = Log::default();
            run_restore(
                &resolved,
                Some(Secret::new(password.clone())),
                ws.route(),
                &restore,
                true,
                &BackupCancel::new(),
                &log.sink(),
            )
            .await
            .unwrap_or_else(|e| panic!("{format:?} restore: {e}\n{:#?}", log.texts()));
            log.assert_no_password(&password);
            assert_eq!(rows(&admin, &snapshot).await, before, "{format:?}");
            assert_eq!(
                rows(&admin, &format!("SELECT name FROM {schema}.cheap")).await,
                [["it's".to_string()]]
            );
        }

        // A cancelled dump leaves nothing behind.
        let cancel = BackupCancel::new();
        cancel.cancel().await;
        let cancelled = run_dump(
            &resolved,
            Some(Secret::new(password.clone())),
            ws.route(),
            &DumpRequest {
                tools: tools(PG_IMAGE),
                folder: ws.out(),
                name_pattern: "cancelled".into(),
                extra_args: String::new(),
                options: DumpOptions::Postgres(PgDumpOptions::default()),
            },
            None,
            &cancel,
            &|_| {},
        )
        .await;
        assert!(
            matches!(cancelled, Err(BackupError::Cancelled)),
            "{cancelled:?}"
        );
        assert!(!ws.out().join("cancelled.dump").exists());
        no_part_files(&ws.out());
    };
    cleaning_up(body, |panicked| {
        run_cleanup("postgres", &admin.session, &cleanup, panicked)
    })
    .await;
    admin.close().await;
}

/// MySQL and MariaDB: a database with a view and a procedure whose definer is `root`, dumped
/// with "remove DEFINER" by an account that is not root, so the restore only works if every
/// definer is gone.
async fn mysql_round_trip(engine: &str, server_var: &str, host_in_network: &str, image: &str) {
    let (Ok(ssh), Ok(direct)) = (
        std::env::var("SPELAION_TEST_SSH"),
        std::env::var(server_var),
    ) else {
        eprintln!("SPELAION_TEST_SSH or {server_var} not set: test skipped");
        return;
    };
    let pid = std::process::id();
    let db = format!("backup_{pid}");
    let user = format!("bk_{pid}");
    let password = password();
    let ws = Workspace::new(engine);
    write_tunnel(&ws, &ssh);
    let (host, port) = direct.rsplit_once(':').unwrap();
    ws.write(
        ".spelaion/connections/admin.toml",
        &format!(
            "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"spelaion\"\nuser = \"root\"\npassword = {{ secret = \"prompt\" }}\n"
        ),
    );
    ws.write(
        ".spelaion/connections/remote.toml",
        &format!(
            "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host_in_network}\"\ndatabase = \"{db}\"\nuser = \"{user}\"\npassword = {{ secret = \"prompt\" }}\ntunnel = \"bastions\"\n"
        ),
    );
    let admin = ws.open("admin", "spelaion-root").await;
    let cleanup = vec![
        format!("DROP DATABASE IF EXISTS {db}"),
        format!("DROP USER IF EXISTS '{user}'@'%'"),
    ];
    let body = async {
        let quoted = password.replace('\'', "''");
        exec(
            &admin,
            &format!(
                "DROP DATABASE IF EXISTS {db}; DROP USER IF EXISTS '{user}'@'%';
                 CREATE DATABASE {db} CHARACTER SET utf8mb4;
                 CREATE USER '{user}'@'%' IDENTIFIED BY '{quoted}';
                 GRANT ALL ON {db}.* TO '{user}'@'%';
                 CREATE TABLE {db}.items (id int AUTO_INCREMENT PRIMARY KEY, name varchar(100), price decimal(10,2), data varbinary(16), at datetime(3)) ENGINE=InnoDB;
                 INSERT INTO {db}.items (name, price, data, at) VALUES
                   ('it''s', 1.50, 0x00ff, '2026-11-11 10:00:00.123'),
                   ('ünïcødé ✓', NULL, NULL, NULL),
                   ('back\\\\slash', 99999999.99, 0xdeadbeef, '1999-12-31 23:59:59');
                 CREATE DEFINER=root@'%' SQL SECURITY DEFINER VIEW {db}.cheap AS SELECT name FROM {db}.items WHERE price < 10;
                 CREATE DEFINER=root@'%' PROCEDURE {db}.item_count() SELECT COUNT(*) FROM {db}.items;"
            ),
        )
        .await;
        if engine == "mysql" {
            // MySQL 8 shows another account's routine only with this privilege.
            exec(
                &admin,
                &format!("GRANT SHOW_ROUTINE ON *.* TO '{user}'@'%'"),
            )
            .await;
        }
        let snapshot = format!("SELECT id, name, price, HEX(data), at FROM {db}.items ORDER BY id");
        let before = rows(&admin, &snapshot).await;
        assert_eq!(before.len(), 3);

        let resolved = ws.resolve("remote");
        let log = Log::default();
        let request = DumpRequest {
            tools: tools(image),
            folder: ws.out(),
            name_pattern: "{connection}_{database}_{date}".into(),
            extra_args: "--comments".into(),
            options: DumpOptions::Mysql(MysqlDumpOptions {
                strip_definer: true,
                hex_blob: true,
                ..MysqlDumpOptions::default()
            }),
        };
        let summary = run_dump(
            &resolved,
            Some(Secret::new(password.clone())),
            ws.route(),
            &request,
            None,
            &BackupCancel::new(),
            &log.sink(),
        )
        .await
        .unwrap_or_else(|e| panic!("{engine} dump: {e}\n{:#?}", log.texts()));
        no_part_files(&ws.out());
        log.assert_no_password(&password);
        let text = std::fs::read_to_string(&summary.path).unwrap();
        assert!(text.contains("CREATE TABLE `items`"), "{text}");
        assert!(text.contains("item_count"), "the procedure was dumped");
        for line in text.lines().filter(|l| !l.starts_with("INSERT INTO")) {
            assert!(!line.contains("DEFINER=`"), "a definer is left: {line}");
        }

        exec(
            &admin,
            &format!("DROP VIEW {db}.cheap; DROP PROCEDURE {db}.item_count; DROP TABLE {db}.items"),
        )
        .await;

        let log = Log::default();
        run_restore(
            &resolved,
            Some(Secret::new(password.clone())),
            ws.route(),
            &RestoreRequest {
                tools: tools(image),
                input: summary.path.clone(),
                format: RestoreFormat::Auto,
                extra_args: String::new(),
                options: RestoreOptions::Mysql(MysqlRestoreOptions {
                    database: Some(db.clone()),
                    disable_foreign_keys: true,
                }),
            },
            true,
            &BackupCancel::new(),
            &log.sink(),
        )
        .await
        .unwrap_or_else(|e| panic!("{engine} restore: {e}\n{:#?}", log.texts()));
        log.assert_no_password(&password);
        // These clients can keep the file from running shell commands, and the restore says so
        // (Windows' `mysql.exe` once had its version misread from its install folder).
        let sandbox = if engine == "mariadb" {
            "--sandbox"
        } else {
            "--skip-system-command"
        };
        assert!(
            log.texts().iter().any(|t| t.contains(sandbox)),
            "{engine} restore ran without {sandbox}: {:#?}",
            log.texts()
        );
        assert_eq!(rows(&admin, &snapshot).await, before);
        assert_eq!(
            rows(&admin, &format!("SELECT name FROM {db}.cheap")).await,
            [["it's".to_string()]]
        );
    };
    cleaning_up(body, |panicked| {
        run_cleanup(engine, &admin.session, &cleanup, panicked)
    })
    .await;
    admin.close().await;
}

#[tokio::test]
async fn mysql_dumps_and_restores_through_two_hops() {
    mysql_round_trip("mysql", "SPELAION_TEST_MYSQL", "mysql", "mysql:8.4").await;
}

#[tokio::test]
async fn mariadb_dumps_and_restores_through_two_hops() {
    mysql_round_trip(
        "mariadb",
        "SPELAION_TEST_MARIADB",
        "mariadb",
        "mariadb:11.4",
    )
    .await;
}
