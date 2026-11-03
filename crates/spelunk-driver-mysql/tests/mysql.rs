//! Tests against real MySQL and MariaDB servers (ADR-012).
//!
//! They need `SPELUNK_TEST_MYSQL=host:port` and/or `SPELUNK_TEST_MARIADB=host:port` (user,
//! password and database: `spelunk`). Locally: `docker compose -f testing/docker/compose.yaml
//! up -d --wait` and `SPELUNK_TEST_MYSQL=127.0.0.1:13306 SPELUNK_TEST_MARIADB=127.0.0.1:13307
//! cargo test`. Without the variables they are skipped with a notice.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use mysql_async::prelude::Queryable;
use spelunk_driver_api::{
    ColumnMeta, ConnectParams, Connection, Driver, DriverError, ExecEvent, ExecOptions,
    LogicalType, ObjectKind, Secret, TlsMode, Value,
};
use spelunk_driver_mysql::MysqlDriver;

struct Server {
    label: &'static str,
    driver: MysqlDriver,
    host: String,
    port: u16,
}

/// The servers configured in the environment; the test is skipped if there is none.
fn servers() -> Vec<Server> {
    let mut list = Vec::new();
    for (label, var, driver) in [
        ("mysql", "SPELUNK_TEST_MYSQL", MysqlDriver::mysql()),
        ("mariadb", "SPELUNK_TEST_MARIADB", MysqlDriver::mariadb()),
    ] {
        match std::env::var(var) {
            Ok(addr) => {
                let (host, port) = addr.rsplit_once(':').expect("host:port");
                list.push(Server {
                    label,
                    driver,
                    host: host.into(),
                    port: port.parse().expect("port"),
                });
            }
            Err(_) => eprintln!("{var} not set: {label} skipped"),
        }
    }
    list
}

impl Server {
    fn params(&self) -> ConnectParams {
        ConnectParams {
            host: self.host.clone(),
            port: self.port,
            user: "spelunk".into(),
            password: Some(Secret::new("spelunk")),
            database: "spelunk".into(),
            application_name: Some("spelunk-tests".into()),
            ..Default::default()
        }
    }

    async fn connect(&self) -> Box<dyn Connection> {
        self.driver
            .connect(&self.params())
            .await
            .unwrap_or_else(|e| panic!("connecting to {}: {e}", self.label))
    }

    /// Reference connection with the client library alone: values as the server sends them.
    async fn reference(&self) -> mysql_async::Conn {
        let opts = mysql_async::OptsBuilder::default()
            .ip_or_hostname(self.host.clone())
            .tcp_port(self.port)
            .user(Some("spelunk"))
            .pass(Some("spelunk"))
            .db_name(Some("spelunk"))
            .prefer_socket(false);
        mysql_async::Conn::new(opts)
            .await
            .expect("reference connection")
    }
}

async fn run(conn: &dyn Connection, sql: &str) -> Result<Vec<ExecEvent>, DriverError> {
    run_with(conn, sql, 10_000).await
}

#[tokio::test]
async fn database_selection_is_per_session_and_tracks_use() {
    for server in servers() {
        let first = server.connect().await;
        let second = server.connect().await;
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("spelunk")
        );
        first.select_database("information_schema").await.unwrap();
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("information_schema")
        );
        assert_eq!(
            second.current_database().await.unwrap().as_deref(),
            Some("spelunk")
        );
        run(first.as_ref(), "USE `spelunk`").await.unwrap();
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("spelunk")
        );
        assert!(
            first
                .select_database("missing`; USE information_schema; --")
                .await
                .is_err()
        );
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("spelunk")
        );
    }
}

async fn run_with(
    conn: &dyn Connection,
    sql: &str,
    batch_size: usize,
) -> Result<Vec<ExecEvent>, DriverError> {
    conn.execute(sql, &ExecOptions { batch_size })
        .await?
        .try_collect()
        .await
}

/// Every row as (logical type, native type, text) per column.
fn table(events: &[ExecEvent]) -> (Vec<ColumnMeta>, Vec<Vec<Option<String>>>) {
    let columns = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Columns(schema) => Some(schema.columns.clone()),
            _ => None,
        })
        .expect("the query returned no columns");
    let mut rows = Vec::new();
    for event in events {
        if let ExecEvent::Batch(batch) = event {
            for row in 0..batch.num_rows() {
                rows.push(
                    (0..batch.num_columns())
                        .map(|c| {
                            let value = Value::from_array(
                                batch.column(c).as_ref(),
                                row,
                                columns[c].logical,
                            )
                            .expect("value");
                            (!matches!(value, Value::Null)).then(|| value.to_string())
                        })
                        .collect(),
                );
            }
        }
    }
    (columns, rows)
}

const ZOO_TABLE: &str = "create table zoo (
    ti tinyint, si smallint, mi mediumint, i int, bi bigint, ubi bigint unsigned,
    ui int unsigned, flag tinyint(1), yr year, f float, d double, dec_ decimal(12,4),
    dt date, dtm datetime, dtm6 datetime(6), ts3 timestamp(3) null, tm time, tm_big time,
    ch char(5), vc varchar(20), tx text, bn binary(4), vb varbinary(8), bl blob,
    en enum('a','b'), st set('x','y'), bits bit(8), js json, nul int
)";

const ZOO_ROW: &str = "insert into zoo values (
    -5, 300, 70000, -123456, 9007199254740993, 18446744073709551615,
    4000000000, 1, 2026, 1.1, 0.000025, 12345678.1234,
    '2026-01-31', '2026-01-31 10:20:30', '2026-01-31 10:20:30.123456',
    '2026-01-31 10:20:30.120', '-12:30:15', '838:59:59',
    'ab', 'text ñ €', 'long text', x'deadbeef', x'0102', x'00ff',
    'b', 'x,y', b'10100101', '{\"a\": [1, 2]}', null
)";

/// Deliberate differences from the text the server sends (see ADR-005).
fn expected_difference(column: &str) -> bool {
    // Binary values are shown as `\x…` hex (the server sends raw bytes), and timestamps
    // drop trailing zeros of the fraction (`.120` is shown as `.12`).
    matches!(column, "bn" | "vb" | "bl" | "bits" | "ts3")
}

#[tokio::test]
async fn type_zoo_matches_the_server_text() {
    for server in servers() {
        let conn = server.connect().await;
        run(&*conn, "drop table if exists zoo").await.unwrap();
        run(&*conn, ZOO_TABLE).await.unwrap();
        run(&*conn, ZOO_ROW).await.unwrap();
        let (columns, rows) = table(&run(&*conn, "select * from zoo").await.unwrap());

        let mut reference = server.reference().await;
        let raw: Vec<mysql_async::Row> = reference.query("select * from zoo").await.unwrap();
        let raw: Vec<Option<String>> = raw[0]
            .clone()
            .unwrap()
            .into_iter()
            .map(|v| match v {
                mysql_async::Value::NULL => None,
                mysql_async::Value::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                other => Some(other.as_sql(true)),
            })
            .collect();

        let mut mismatches = Vec::new();
        let mut snapshot = String::new();
        for (i, column) in columns.iter().enumerate() {
            let ours = rows[0][i].clone();
            snapshot.push_str(&format!(
                "{:<7} {:<10} {:<20} {}\n",
                column.name,
                column.logical.as_str(),
                column.native_type,
                ours.as_deref().unwrap_or("NULL")
            ));
            if ours != raw[i] && !expected_difference(&column.name) {
                mismatches.push(format!(
                    "{}: ours {:?} ≠ server {:?}",
                    column.name, ours, raw[i]
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{}: {}",
            server.label,
            mismatches.join("\n")
        );
        insta::assert_snapshot!(format!("type_zoo_{}", server.label), snapshot);
        run(&*conn, "drop table zoo").await.unwrap();
    }
}

#[tokio::test]
async fn zero_dates_are_kept_as_text_instead_of_lost() {
    for server in servers() {
        let conn = server.connect().await;
        // Legacy data: allowed only without the strict modes of current versions.
        run(&*conn, "set session sql_mode = ''").await.unwrap();
        run(&*conn, "drop table if exists legacy").await.unwrap();
        run(&*conn, "create table legacy (d date, t datetime)")
            .await
            .unwrap();
        run(
            &*conn,
            "insert into legacy values ('2026-01-31', '2026-01-31 10:00:00'), ('0000-00-00', '0000-00-00 00:00:00')",
        )
        .await
        .unwrap();
        let (columns, rows) = table(&run(&*conn, "select * from legacy").await.unwrap());
        assert_eq!(columns[0].logical, LogicalType::Text, "{}", server.label);
        assert_eq!(columns[1].logical, LogicalType::Text, "{}", server.label);
        assert_eq!(rows[1][0].as_deref(), Some("0000-00-00"));
        assert_eq!(rows[1][1].as_deref(), Some("0000-00-00 00:00:00"));

        // A zero date after the first batch cannot change the column any more: explicit error.
        match run_with(&*conn, "select d from legacy order by d desc", 1).await {
            Err(DriverError::Decode { message, .. }) => {
                assert!(message.contains("CAST"), "{message}")
            }
            other => panic!("{}: expected a decode error, got {other:?}", server.label),
        }
        run(&*conn, "drop table legacy").await.unwrap();
    }
}

#[tokio::test]
async fn errors_are_reported_and_the_connection_survives() {
    for server in servers() {
        let conn = server.connect().await;
        match run(&*conn, "select id\nfrom missing_table").await {
            Err(DriverError::Database { code, message, .. }) => {
                assert_eq!(
                    code.as_deref(),
                    Some("42S02"),
                    "{}: {message}",
                    server.label
                );
            }
            other => panic!("{}: expected a database error, got {other:?}", server.label),
        }
        match run(
            &*conn,
            "select * from
where x = 1",
        )
        .await
        {
            Err(DriverError::Database { position, .. }) => {
                assert_eq!(position, Some(15), "{}", server.label)
            }
            other => panic!("{}: expected a syntax error, got {other:?}", server.label),
        }
        let (_, rows) = table(&run(&*conn, "select 1 + 1 as two").await.unwrap());
        assert_eq!(rows[0][0].as_deref(), Some("2"));
        assert!(!conn.server_version().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn wrong_passwords_report_sqlstate_28000() {
    for server in servers() {
        let mut params = server.params();
        params.password = Some(Secret::new("wrong"));
        match server.driver.connect(&params).await {
            Err(DriverError::Database { code, .. }) => {
                assert_eq!(code.as_deref(), Some("28000"), "{}", server.label)
            }
            Err(other) => panic!("{}: expected access denied, got {other:?}", server.label),
            Ok(_) => panic!("{}: a wrong password must not connect", server.label),
        }
    }
}

fn ca() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys/ca.crt")
}

#[tokio::test]
async fn tls_modes_encrypt_and_verify() {
    for server in servers() {
        let encrypted = |conn: Box<dyn Connection>| async move {
            let (_, rows) = table(
                &run(&*conn, "show session status like 'Ssl_cipher'")
                    .await
                    .unwrap(),
            );
            !rows[0][1].clone().unwrap_or_default().is_empty()
        };
        for mode in [TlsMode::Prefer, TlsMode::Require] {
            let mut params = server.params();
            params.tls = mode;
            let conn = server
                .driver
                .connect(&params)
                .await
                .unwrap_or_else(|e| panic!("{} {mode:?}: {e}", server.label));
            assert!(encrypted(conn).await, "{} {mode:?}", server.label);
        }
        let mut plain = server.params();
        plain.tls = TlsMode::Disable;
        assert!(!encrypted(server.driver.connect(&plain).await.unwrap()).await);

        // verify-full against the certificate's name ("localhost") while connecting to the
        // address, as through a tunnel; a name the certificate does not cover is rejected.
        let addr: SocketAddr = format!("{}:{}", server.host, server.port).parse().unwrap();
        let mut verified = server.params();
        verified.tls = TlsMode::VerifyFull;
        verified.ca_certificate = Some(ca());
        verified.host = "localhost".into();
        verified.connect_addr = Some(addr);
        let conn = server
            .driver
            .connect(&verified)
            .await
            .unwrap_or_else(|e| panic!("{} verify-full: {e}", server.label));
        assert!(encrypted(conn).await);
        verified.host = "not-in-the-certificate".into();
        assert!(
            server.driver.connect(&verified).await.is_err(),
            "{}: verify-full must reject a name the certificate does not cover",
            server.label
        );
    }
}

#[tokio::test]
async fn running_queries_can_be_cancelled() {
    for server in servers() {
        let conn = server.connect().await;
        let cancel = conn.cancel_handle();
        let started = Instant::now();
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel.cancel().await
        });
        let result = run(&*conn, "select sleep(30)").await;
        canceller.await.unwrap().unwrap();
        // MySQL ends SLEEP() early with 1 instead of an error; MariaDB returns an error.
        match result {
            Err(DriverError::Cancelled) => {}
            Ok(events) => {
                let (_, rows) = table(&events);
                assert_eq!(rows[0][0].as_deref(), Some("1"), "{}", server.label);
            }
            Err(other) => panic!("{}: {other:?}", server.label),
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{}",
            server.label
        );
        let (_, rows) = table(&run(&*conn, "select 'still usable'").await.unwrap());
        assert_eq!(rows[0][0].as_deref(), Some("still usable"));
    }
}

#[tokio::test]
async fn large_results_arrive_in_batches_with_counts() {
    for server in servers() {
        let conn = server.connect().await;
        // Both stop recursive CTEs after 1000 iterations by default (MariaDB silently).
        let raise = match server.label {
            "mysql" => "set session cte_max_recursion_depth = 100000",
            _ => "set session max_recursive_iterations = 100000",
        };
        run(&*conn, raise).await.unwrap();
        let events = run_with(
            &*conn,
            "with recursive n(x) as (select 1 union all select x + 1 from n where x < 25000) select x from n",
            10_000,
        )
        .await
        .unwrap();
        let batches = events
            .iter()
            .filter(|e| matches!(e, ExecEvent::Batch(_)))
            .count();
        assert_eq!(batches, 3, "{}", server.label);
        let (columns, rows) = table(&events);
        assert_eq!(columns[0].logical, LogicalType::Int);
        assert_eq!(rows.len(), 25_000);
        assert_eq!(rows[24_999][0].as_deref(), Some("25000"));

        run(&*conn, "drop table if exists counted").await.unwrap();
        let done = |events: &[ExecEvent]| match events.last() {
            Some(ExecEvent::Done { rows_affected }) => *rows_affected,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            done(&run(&*conn, "create table counted (a int)").await.unwrap()),
            None
        );
        assert_eq!(
            done(
                &run(&*conn, "insert into counted values (1), (2), (3)")
                    .await
                    .unwrap()
            ),
            Some(3)
        );
        // Matched rows, not changed rows: setting a value to itself still counts its row.
        assert_eq!(
            done(
                &run(&*conn, "update counted set a = 1 where a = 1")
                    .await
                    .unwrap()
            ),
            Some(1)
        );
        run(&*conn, "drop table counted").await.unwrap();
    }
}

#[tokio::test]
async fn introspection_lists_tables_columns_keys_and_indexes() {
    for server in servers() {
        let conn = server.connect().await;
        for sql in [
            "drop table if exists line_item",
            "drop table if exists purchase",
            "drop view if exists big_purchase",
            "create table purchase (id int primary key, customer varchar(40) not null default 'anon' comment 'who', total decimal(10,2), unique key purchase_customer (customer, id)) comment 'purchases'",
            "create table line_item (purchase_id int, n int, primary key (purchase_id, n), constraint line_purchase foreign key (purchase_id) references purchase (id))",
            "create view big_purchase as select * from purchase where total > 100",
        ] {
            run(&*conn, sql).await.unwrap();
        }
        let schemas = conn.list_schemas().await.unwrap();
        assert!(schemas.iter().any(|s| s.name == "spelunk" && !s.system));
        assert!(
            schemas
                .iter()
                .any(|s| s.name == "information_schema" && s.system)
        );

        let objects = conn.list_objects("spelunk").await.unwrap();
        let kind = |name: &str| objects.iter().find(|o| o.name == name).map(|o| o.kind);
        assert_eq!(kind("purchase"), Some(ObjectKind::Table));
        assert_eq!(kind("big_purchase"), Some(ObjectKind::View));

        let purchase = conn.describe_table("spelunk", "purchase").await.unwrap();
        assert_eq!(purchase.object.comment.as_deref(), Some("purchases"));
        assert_eq!(purchase.primary_key, ["id"]);
        let customer = &purchase.columns[1];
        assert_eq!(customer.native_type, "varchar(40)");
        assert!(!customer.nullable);
        assert_eq!(customer.comment.as_deref(), Some("who"));
        assert!(
            customer
                .default
                .as_deref()
                .is_some_and(|d| d.contains("anon")),
            "{}: {:?}",
            server.label,
            customer.default
        );
        assert!(purchase.indexes.iter().any(|i| i.name == "purchase_customer" && i.unique
            && i.definition == "CREATE UNIQUE INDEX `purchase_customer` ON `purchase` (`customer`, `id`)"));

        let line = conn.describe_table("spelunk", "line_item").await.unwrap();
        assert_eq!(line.primary_key, ["purchase_id", "n"]);
        assert_eq!(line.foreign_keys.len(), 1);
        assert_eq!(line.foreign_keys[0].referenced_table, "purchase");
        assert_eq!(line.foreign_keys[0].referenced_columns, ["id"]);

        for sql in [
            "drop view big_purchase",
            "drop table line_item",
            "drop table purchase",
        ] {
            run(&*conn, sql).await.unwrap();
        }
    }
}

/// ADR-022: a user that requires a client certificate from the test authority logs in with
/// it and no password.
#[tokio::test]
async fn client_certificates_log_in_without_a_password() {
    let key = |name: &str| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testing/docker/keys")
            .join(name)
    };
    for server in servers() {
        // Only an administrator can create the user.
        let root = ConnectParams {
            user: "root".into(),
            password: Some(Secret::new("spelunk-root")),
            ..server.params()
        };
        let admin = server
            .driver
            .connect(&root)
            .await
            .unwrap_or_else(|e| panic!("{} as root: {e}", server.label));
        for sql in [
            "create user if not exists 'spelunk_cert'@'%' require subject '/CN=spelunk_cert'",
            "grant select on spelunk.* to 'spelunk_cert'@'%'",
        ] {
            run(&*admin, sql)
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", server.label));
        }
        // verify-full against the certificate's name, connecting to the address.
        let addr: SocketAddr = format!("{}:{}", server.host, server.port).parse().unwrap();
        let params = ConnectParams {
            user: "spelunk_cert".into(),
            password: None,
            host: "localhost".into(),
            connect_addr: Some(addr),
            tls: TlsMode::VerifyFull,
            ca_certificate: Some(ca()),
            client_certificate: Some(key("client.crt")),
            client_key: Some(key("client.key")),
            ..server.params()
        };
        let conn = server
            .driver
            .connect(&params)
            .await
            .unwrap_or_else(|e| panic!("{} with the certificate: {e}", server.label));
        let (_, rows) = table(&run(&*conn, "select current_user()").await.unwrap());
        assert_eq!(
            rows[0][0].as_deref(),
            Some("spelunk_cert@%"),
            "{}",
            server.label
        );
        // Without it, the same user is refused.
        let without = ConnectParams {
            client_certificate: None,
            client_key: None,
            ..params.clone()
        };
        assert!(
            server.driver.connect(&without).await.is_err(),
            "{}",
            server.label
        );
    }
}
