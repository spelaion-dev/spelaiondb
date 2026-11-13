//! Tests against real MySQL and MariaDB servers (ADR-012).
//!
//! They need `SPELAION_TEST_MYSQL=host:port` and/or `SPELAION_TEST_MARIADB=host:port` (user,
//! password and database: `spelaion`). Locally: `docker compose -f testing/docker/compose.yaml
//! up -d --wait` and `SPELAION_TEST_MYSQL=127.0.0.1:13306 SPELAION_TEST_MARIADB=127.0.0.1:13307
//! cargo test`. Without the variables they are skipped with a notice.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use mysql_async::prelude::Queryable;
use spelaion_driver_api::{
    ColumnMeta, ConnectParams, Connection, ConstraintKind, Driver, DriverError, ExecEvent,
    ExecOptions, LogicalType, ObjectKind, SchemaObjectKind, Secret, ServerObjectKind, TlsMode,
    Value,
};
use spelaion_driver_mysql::MysqlDriver;

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
        ("mysql", "SPELAION_TEST_MYSQL", MysqlDriver::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", MysqlDriver::mariadb()),
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
            user: "spelaion".into(),
            password: Some(Secret::new("spelaion")),
            database: "spelaion".into(),
            application_name: Some("spelaion-tests".into()),
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
            .user(Some("spelaion"))
            .pass(Some("spelaion"))
            .db_name(Some("spelaion"))
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
async fn mysql5_describes_keys_without_a_check_constraint_catalog() {
    let Ok(addr) = std::env::var("SPELAION_TEST_MYSQL5") else {
        eprintln!("SPELAION_TEST_MYSQL5 not set: MySQL 5 skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let server = Server {
        label: "mysql5",
        driver: MysqlDriver::mysql(),
        host: host.into(),
        port: port.parse().expect("port"),
    };
    let mut params = server.params();
    // The stock MySQL 5.7 image does not negotiate TLS with modern rustls defaults.
    params.tls = TlsMode::Disable;
    let conn = server.driver.connect(&params).await.unwrap();
    assert!(conn.server_version().await.unwrap().starts_with("5."));
    run(&*conn, "drop table if exists old_mysql_catalog_test")
        .await
        .unwrap();
    run(&*conn, "create table old_mysql_catalog_test (id int primary key, name varchar(40), unique key unique_name (name))")
        .await
        .unwrap();
    let table = conn
        .describe_table("spelaion", "old_mysql_catalog_test")
        .await
        .unwrap();
    assert_eq!(table.primary_key, ["id"]);
    assert!(table.constraints.iter().any(|constraint| {
        constraint.kind == ConstraintKind::Unique && constraint.columns == ["name"]
    }));
    assert_eq!(table.columns.len(), 2);
    run(&*conn, "drop table old_mysql_catalog_test")
        .await
        .unwrap();
}

#[tokio::test]
async fn database_selection_is_per_session_and_tracks_use() {
    for server in servers() {
        let first = server.connect().await;
        let second = server.connect().await;
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("spelaion")
        );
        first.select_database("information_schema").await.unwrap();
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("information_schema")
        );
        assert_eq!(
            second.current_database().await.unwrap().as_deref(),
            Some("spelaion")
        );
        run(first.as_ref(), "USE `spelaion`").await.unwrap();
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("spelaion")
        );
        assert!(
            first
                .select_database("missing`; USE information_schema; --")
                .await
                .is_err()
        );
        assert_eq!(
            first.current_database().await.unwrap().as_deref(),
            Some("spelaion")
        );
    }
}

async fn run_with(
    conn: &dyn Connection,
    sql: &str,
    batch_size: usize,
) -> Result<Vec<ExecEvent>, DriverError> {
    conn.execute(
        sql,
        &ExecOptions {
            batch_size,
            ..Default::default()
        },
    )
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
    // Binary values are shown as `\x…` hex (the server sends raw bytes). Dates and timestamps
    // are the server's text, fraction digits included (SDB-122).
    matches!(column, "bn" | "vb" | "bl" | "bits")
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

        // Wherever it comes: dates are text with their native type, never decided by values.
        let (columns, rows) = table(
            &run_with(&*conn, "select d, t from legacy order by d desc", 1)
                .await
                .unwrap(),
        );
        assert_eq!(columns[0].native_type, "date", "{}", server.label);
        assert_eq!(columns[1].native_type, "datetime", "{}", server.label);
        assert_eq!(
            rows[1][0].as_deref(),
            Some("0000-00-00"),
            "{}",
            server.label
        );
        run(&*conn, "drop table legacy").await.unwrap();
    }
}

/// A zero date after the first 10,000 rows (SDB-122): every row comes back with the server's
/// text; nothing fails, nothing is dropped or turned into NULL.
#[tokio::test]
async fn zero_dates_after_the_first_ten_thousand_rows_keep_their_text() {
    for server in servers() {
        let label = server.label;
        let conn = server.connect().await;
        run(&*conn, "set session sql_mode = ''").await.unwrap();
        run(&*conn, "drop table if exists zero_far").await.unwrap();
        run(
            &*conn,
            "create table zero_far (id int primary key, d date, dt datetime, ts timestamp null)",
        )
        .await
        .unwrap();
        // 10,000 dated rows (a cross join of digits), then the zero dates.
        run(
            &*conn,
            "insert into zero_far (id, d, dt, ts) \
             select a.n + b.n * 10 + c.n * 100 + e.n * 1000 + 1, \
                    '2024-01-15', '2024-01-15 10:20:30', '2024-01-15 10:20:30' \
             from (select 0 n union all select 1 union all select 2 union all select 3 union all \
                   select 4 union all select 5 union all select 6 union all select 7 union all \
                   select 8 union all select 9) a \
             cross join (select 0 n union all select 1 union all select 2 union all select 3 union all \
                   select 4 union all select 5 union all select 6 union all select 7 union all \
                   select 8 union all select 9) b \
             cross join (select 0 n union all select 1 union all select 2 union all select 3 union all \
                   select 4 union all select 5 union all select 6 union all select 7 union all \
                   select 8 union all select 9) c \
             cross join (select 0 n union all select 1 union all select 2 union all select 3 union all \
                   select 4 union all select 5 union all select 6 union all select 7 union all \
                   select 8 union all select 9) e",
        )
        .await
        .unwrap();
        run(
            &*conn,
            "insert into zero_far values \
             (10001, '0000-00-00', '0000-00-00 00:00:00', '0000-00-00 00:00:00')",
        )
        .await
        .unwrap();
        for batch in [10_000, 1] {
            let (columns, rows) = table(
                &run_with(
                    &*conn,
                    "select id, d, dt, ts from zero_far order by id",
                    batch,
                )
                .await
                .unwrap_or_else(|e| panic!("{label} (batches of {batch}): {e}")),
            );
            let natives: Vec<&str> = columns.iter().map(|c| c.native_type.as_str()).collect();
            assert_eq!(natives[1..], ["date", "datetime", "timestamp"], "{label}");
            assert!(
                columns[1..].iter().all(|c| c.logical == LogicalType::Text),
                "{label}: {columns:?}"
            );
            assert_eq!(rows.len(), 10_001, "{label} (batches of {batch})");
            for row in &rows[..10_000] {
                assert_eq!(
                    row[1..],
                    [
                        Some("2024-01-15".to_string()),
                        Some("2024-01-15 10:20:30".to_string()),
                        Some("2024-01-15 10:20:30".to_string())
                    ],
                    "{label}"
                );
            }
            assert_eq!(
                rows[10_000],
                [
                    Some("10001".to_string()),
                    Some("0000-00-00".to_string()),
                    Some("0000-00-00 00:00:00".to_string()),
                    Some("0000-00-00 00:00:00".to_string())
                ],
                "{label} (batches of {batch})"
            );
        }
        run(&*conn, "drop table zero_far").await.unwrap();
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

        // verify-ca: a trusted authority, whatever the name; here the IP address, which the
        // certificate does not list (vendor/mysql_async/PATCHED.md).
        let mut by_authority = server.params();
        by_authority.tls = TlsMode::VerifyCa;
        by_authority.ca_certificate = Some(ca());
        let conn = server
            .driver
            .connect(&by_authority)
            .await
            .unwrap_or_else(|e| panic!("{} verify-ca by IP: {e}", server.label));
        assert!(encrypted(conn).await);
        // A certificate from another authority is still refused.
        by_authority.ca_certificate = Some(ca().with_file_name("client.crt"));
        assert!(
            server.driver.connect(&by_authority).await.is_err(),
            "{}: verify-ca must reject another authority",
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
            "alter table purchase add constraint purchase_total check (total >= 0)",
        ] {
            run(&*conn, sql).await.unwrap();
        }
        let schemas = conn.list_schemas().await.unwrap();
        assert!(schemas.iter().any(|s| s.name == "spelaion" && !s.system));
        assert!(
            schemas
                .iter()
                .any(|s| s.name == "information_schema" && s.system)
        );

        let objects = conn.list_objects("spelaion").await.unwrap();
        let kind = |name: &str| objects.iter().find(|o| o.name == name).map(|o| o.kind);
        assert_eq!(kind("purchase"), Some(ObjectKind::Table));
        assert_eq!(kind("big_purchase"), Some(ObjectKind::View));

        let purchase = conn.describe_table("spelaion", "purchase").await.unwrap();
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

        assert_eq!(
            constraints(&purchase),
            [
                (
                    Some("PRIMARY"),
                    ConstraintKind::PrimaryKey,
                    vec!["id"],
                    "PRIMARY KEY (`id`)"
                ),
                (
                    Some("purchase_customer"),
                    ConstraintKind::Unique,
                    vec!["customer", "id"],
                    "UNIQUE (`customer`, `id`)"
                ),
                (
                    Some("purchase_total"),
                    ConstraintKind::Check,
                    vec![],
                    "CHECK (`total` >= 0)"
                ),
            ],
            "{}",
            server.label
        );

        let line = conn.describe_table("spelaion", "line_item").await.unwrap();
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

#[tokio::test]
async fn explorer_lists_routines_triggers_events_and_accounts() {
    for server in servers() {
        let conn = server.connect().await;
        let mariadb = server.label == "mariadb";
        let mut ddl = vec![
            "drop function if exists explorer_add1",
            "drop procedure if exists explorer_noop",
            "drop event if exists explorer_daily",
            "drop table if exists explorer_t",
            "drop sequence if exists explorer_seq",
            "create function explorer_add1(a int) returns int deterministic no sql comment 'adds one' return a + 1",
            "create procedure explorer_noop() select 1",
            "create table explorer_t (id int)",
            "create trigger explorer_t_ins before insert on explorer_t for each row set new.id = new.id",
            "create event explorer_daily on schedule every 1 day do select 1",
        ];
        if !mariadb {
            ddl.retain(|sql| !sql.contains("sequence"));
        } else {
            ddl.push("create sequence explorer_seq start with 5");
        }
        for sql in ddl {
            run(&*conn, sql)
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", server.label));
        }

        let objects: Vec<_> = conn
            .list_schema_objects("spelaion")
            .await
            .unwrap()
            .into_iter()
            .filter(|o| o.name.starts_with("explorer_"))
            .collect();
        let listed: Vec<_> = objects
            .iter()
            .map(|o| (o.name.as_str(), o.kind, o.detail.as_deref()))
            .collect();
        let mut expected = vec![
            ("explorer_add1", SchemaObjectKind::Function, None),
            ("explorer_daily", SchemaObjectKind::Event, None),
            ("explorer_noop", SchemaObjectKind::Procedure, None),
        ];
        if mariadb {
            expected.push(("explorer_seq", SchemaObjectKind::Sequence, None));
        }
        expected.push((
            "explorer_t_ins",
            SchemaObjectKind::Trigger,
            Some("explorer_t"),
        ));
        assert_eq!(listed, expected, "{}", server.label);
        assert_eq!(objects[0].comment.as_deref(), Some("adds one"));
        // A sequence is not a table.
        let relations = conn.list_objects("spelaion").await.unwrap();
        assert!(!relations.iter().any(|o| o.name == "explorer_seq"));

        for object in &objects {
            let definition = conn
                .object_definition(object)
                .await
                .unwrap_or_else(|e| panic!("{}: {}: {e}", server.label, object.name));
            let keyword = match object.kind {
                SchemaObjectKind::Sequence => "CREATE SEQUENCE",
                _ => "CREATE DEFINER=",
            };
            assert!(
                definition.starts_with(keyword),
                "{}: {definition}",
                server.label
            );
            assert!(
                definition.contains(&object.name),
                "{}: {definition}",
                server.label
            );
            assert!(definition.ends_with(';'), "{}: {definition}", server.label);
        }
        let mut missing = objects[0].clone();
        missing.name = "explorer_nope".into();
        assert!(conn.object_definition(&missing).await.is_err());

        // The test user cannot read mysql.user: no accounts, and no error.
        assert_eq!(conn.list_server_objects().await.unwrap(), []);
        let root = ConnectParams {
            user: "root".into(),
            password: Some(Secret::new("spelaion-root")),
            ..server.params()
        };
        let admin = server.driver.connect(&root).await.unwrap();
        let accounts = admin.list_server_objects().await.unwrap();
        assert!(
            accounts.iter().any(|a| a.name == "'spelaion'@'%'"
                && a.kind == ServerObjectKind::User
                && !a.system),
            "{}: {accounts:?}",
            server.label
        );
        let internal = if mariadb {
            "'mariadb.sys'@'localhost'"
        } else {
            "'mysql.sys'@'localhost'"
        };
        assert!(
            accounts.iter().any(|a| a.name == internal && a.system),
            "{}: {accounts:?}",
            server.label
        );

        for sql in [
            "drop event explorer_daily",
            "drop table explorer_t",
            "drop procedure explorer_noop",
            "drop function explorer_add1",
            "drop sequence if exists explorer_seq",
        ] {
            if mariadb || !sql.contains("sequence") {
                run(&*conn, sql).await.unwrap();
            }
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
            password: Some(Secret::new("spelaion-root")),
            ..server.params()
        };
        let admin = server
            .driver
            .connect(&root)
            .await
            .unwrap_or_else(|e| panic!("{} as root: {e}", server.label));
        for sql in [
            "create user if not exists 'spelaion_cert'@'%' require subject '/CN=spelaion_cert'",
            "grant select on spelaion.* to 'spelaion_cert'@'%'",
        ] {
            run(&*admin, sql)
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", server.label));
        }
        // verify-full against the certificate's name, connecting to the address.
        let addr: SocketAddr = format!("{}:{}", server.host, server.port).parse().unwrap();
        let params = ConnectParams {
            user: "spelaion_cert".into(),
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
            Some("spelaion_cert@%"),
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

/// (name, kind, columns, definition) of each constraint, to compare in one go.
fn constraints(
    table: &spelaion_driver_api::TableInfo,
) -> Vec<(Option<&str>, ConstraintKind, Vec<&str>, &str)> {
    table
        .constraints
        .iter()
        .map(|c| {
            (
                c.name.as_deref(),
                c.kind,
                c.columns.iter().map(String::as_str).collect(),
                c.definition.as_str(),
            )
        })
        .collect()
}

#[tokio::test]
async fn server_side_filters_keep_what_the_filter_keeps() {
    for server in servers() {
        let conn = server.connect().await;
        let quote = |name: &str| format!("`{}`", name.replace('`', "``"));
        let schema = "spelaion_like";
        // A schema of its own, created by root: the test user may only create in spelaion.
        let root = ConnectParams {
            user: "root".into(),
            password: Some(Secret::new("spelaion-root")),
            ..server.params()
        };
        let admin = server.driver.connect(&root).await.unwrap();
        for sql in [
            format!("drop database if exists {schema}"),
            format!("create database {schema} character set utf8mb4"),
            format!("grant all on {schema}.* to 'spelaion'@'%'"),
        ] {
            run(&*admin, &sql).await.unwrap();
        }
        for name in TRICKY_NAMES {
            run(
                &*admin,
                &format!("create table {schema}.{} (id int)", quote(name)),
            )
            .await
            .unwrap_or_else(|e| panic!("{}: {name}: {e}", server.label));
            run(
                &*admin,
                &format!(
                    "create procedure {schema}.{}() select 1",
                    quote(&format!("{name}_proc"))
                ),
            )
            .await
            .unwrap();
        }
        server_side_keeps_what_rust_keeps(&*conn, schema).await;
        let mut got: Vec<String> = conn
            .list_schemas_matching(&spelaion_driver_api::NameFilter {
                include: vec!["SPELAION*".into()],
                exclude: vec!["*_like".into()],
            })
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        got.sort();
        assert_eq!(got, ["spelaion"], "{}", server.label);
        run(&*admin, &format!("drop database {schema}"))
            .await
            .unwrap();
    }
}

/// Filters whose patterns are hard to send to a server (ADR-025): a literal `_` and `%`, the
/// escape character, a non-ASCII letter whose case differs, `?` and an exclude-only filter.
fn tricky_filters() -> Vec<spelaion_driver_api::NameFilter> {
    let filter = |include: &[&str], exclude: &[&str]| spelaion_driver_api::NameFilter {
        include: include.iter().map(|p| p.to_string()).collect(),
        exclude: exclude.iter().map(|p| p.to_string()).collect(),
    };
    vec![
        filter(&["tenant_*"], &[]),
        filter(&["tenant\\%*"], &[]),
        filter(&["ñame*"], &[]),
        filter(&["t?nant*"], &[]),
        filter(&["wow!*"], &[]),
        filter(&[], &["*_a", "wow!*"]),
        filter(&["*"], &["ñ*"]),
        filter(&["nothing_matches_this"], &[]),
    ]
}

/// The names created by each test for [`tricky_filters`].
const TRICKY_NAMES: [&str; 6] = [
    "Tenant_A", "tenantXb", "tenant%c", "ÑAME_1", "plain", "wow!x",
];

/// Narrowed on the server or only in Rust, the same names come back.
async fn server_side_keeps_what_rust_keeps(conn: &dyn Connection, schema: &str) {
    let all_objects = conn.list_objects(schema).await.unwrap();
    let all_others = conn.list_schema_objects(schema).await.unwrap();
    for filter in tricky_filters() {
        let names = |items: Vec<String>| {
            let mut items = items;
            items.sort();
            items
        };
        let expected = names(
            all_objects
                .iter()
                .filter(|o| filter.matches(&o.name))
                .map(|o| o.name.clone())
                .collect(),
        );
        let got = names(
            conn.list_objects_matching(schema, &filter)
                .await
                .unwrap_or_else(|e| panic!("{filter:?}: {e}"))
                .into_iter()
                .map(|o| o.name)
                .collect(),
        );
        assert_eq!(got, expected, "objects, {filter:?}");
        let expected = names(
            all_others
                .iter()
                .filter(|o| filter.matches(&o.name))
                .map(|o| o.name.clone())
                .collect(),
        );
        let got = names(
            conn.list_schema_objects_matching(schema, &filter)
                .await
                .unwrap_or_else(|e| panic!("{filter:?}: {e}"))
                .into_iter()
                .map(|o| o.name)
                .collect(),
        );
        assert_eq!(got, expected, "schema objects, {filter:?}");
    }
    // The filters do something: each name is found by the one meant for it.
    let only = |pattern: &str| spelaion_driver_api::NameFilter {
        include: vec![pattern.into()],
        exclude: vec![],
    };
    for (pattern, name) in [
        ("tenant_*", "Tenant_A"),
        ("tenant\\%*", "tenant%c"),
        ("ñame*", "ÑAME_1"),
        ("wow!*", "wow!x"),
    ] {
        let got: Vec<String> = conn
            .list_objects_matching(schema, &only(pattern))
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect();
        assert_eq!(got, [name], "{pattern}");
    }
}

#[tokio::test]
async fn generated_ddl_recreates_tables_and_views() {
    for server in servers() {
        let conn = server.connect().await;
        for sql in [
            "drop view if exists ddl_big",
            "drop table if exists ddl_lines",
            "drop table if exists ddl_customer",
            "create table ddl_customer (id int primary key, email varchar(80) not null unique)",
            "create table ddl_lines (
                 id bigint auto_increment primary key,
                 customer_id int not null,
                 qty int not null default 1 comment 'how many',
                 note varchar(40) character set latin1,
                 key ddl_lines_qty (qty desc),
                 constraint ddl_lines_customer foreign key (customer_id) references ddl_customer (id)
                     on delete cascade,
                 constraint ddl_lines_qty_positive check (qty > 0)
             ) comment 'the lines'",
            "create view ddl_big as select id, qty from ddl_lines where qty > 10",
        ] {
            run(&*conn, sql)
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", server.label));
        }
        let dialect = spelaion_driver_mysql::MysqlDialect;
        let big = conn
            .relation_definition("spelaion", "ddl_big")
            .await
            .unwrap();
        assert!(big.contains("VIEW `ddl_big` AS"), "{}: {big}", server.label);
        let lines = ddl_round_trip(
            &*conn,
            &dialect,
            "spelaion",
            "ddl_lines",
            "drop table ddl_lines",
        )
        .await;
        assert!(
            lines.contains("ON DELETE CASCADE"),
            "{}: {lines}",
            server.label
        );
        run(&*conn, "drop view ddl_big").await.unwrap();
        run(&*conn, &big).await.unwrap();
        run(&*conn, "drop view ddl_big").await.unwrap();
        run(&*conn, "drop table ddl_lines").await.unwrap();
        run(&*conn, "drop table ddl_customer").await.unwrap();
    }
}

/// "Generate DDL" round trip: the DDL of `table`, run after `drop`, creates the same table
/// (columns, keys, constraints, foreign keys, indexes and comment). Returns the DDL.
async fn ddl_round_trip(
    conn: &dyn Connection,
    dialect: &dyn spelaion_driver_api::Dialect,
    schema: &str,
    table: &str,
    drop: &str,
) -> String {
    let before = conn.describe_table(schema, table).await.unwrap();
    let ddl = conn
        .relation_definition(schema, table)
        .await
        .unwrap_or_else(|e| panic!("{table}: {e}"));
    assert!(ddl.ends_with(';'), "{ddl}");
    run(conn, drop)
        .await
        .unwrap_or_else(|e| panic!("{drop}: {e:?}"));
    for span in dialect.split_statements(&ddl) {
        let statement = span.text(&ddl);
        run(conn, statement)
            .await
            .unwrap_or_else(|e| panic!("{statement}\n→ {e:?}\n\n{ddl}"));
    }
    let after = conn.describe_table(schema, table).await.unwrap();
    assert_eq!(after.columns, before.columns, "{ddl}");
    assert_eq!(after.primary_key, before.primary_key, "{ddl}");
    assert_eq!(after.constraints, before.constraints, "{ddl}");
    assert_eq!(after.foreign_keys, before.foreign_keys, "{ddl}");
    assert_eq!(after.indexes, before.indexes, "{ddl}");
    assert_eq!(after.object.comment, before.object.comment, "{ddl}");
    assert_eq!(after.object.kind, before.object.kind, "{ddl}");
    ddl
}

#[tokio::test]
async fn procedure_calls_pass_inputs_and_collect_outputs() {
    for server in servers() {
        let conn = server.connect().await;
        for sql in [
            "drop procedure if exists call_calc",
            "create procedure call_calc(in a int, inout b int, out c varchar(10)) \
             begin set b = b + a; set c = concat('x', a); end",
        ] {
            run(&*conn, sql)
                .await
                .unwrap_or_else(|e| panic!("{}: {sql}: {e}", server.label));
        }
        let objects = conn.list_schema_objects("spelaion").await.unwrap();
        let calc = objects.iter().find(|o| o.name == "call_calc").unwrap();
        let call = conn.procedure_call(calc).await.unwrap();
        assert_eq!(
            call,
            "SET @b = :b;\nCALL `spelaion`.`call_calc`(:a, @b, @c);\nSELECT @b AS `b`, @c AS `c`;",
            "{}",
            server.label
        );
        let filled = call.replace(":a", "1").replace(":b", "2");
        let dialect = spelaion_driver_mysql::MysqlDialect;
        let mut last = Vec::new();
        for span in spelaion_driver_api::Dialect::split_statements(&dialect, &filled) {
            last = run(&*conn, span.text(&filled)).await.unwrap();
        }
        let (_, rows) = table(&last);
        assert_eq!(
            rows,
            [[Some("3".to_string()), Some("x1".to_string())]],
            "{}",
            server.label
        );
        run(&*conn, "drop procedure call_calc").await.unwrap();
    }
}

#[tokio::test]
async fn keep_alives_answer_and_say_when_a_transaction_is_open() {
    use spelaion_driver_api::KeepAlive;
    for server in servers() {
        let conn = server.connect().await;
        assert_eq!(
            conn.keep_alive().await.unwrap(),
            KeepAlive::Alive,
            "{}",
            server.label
        );
        run(&*conn, "start transaction").await.unwrap();
        run(&*conn, "select 1").await.unwrap();
        assert_eq!(
            conn.keep_alive().await.unwrap(),
            KeepAlive::InTransaction,
            "{}",
            server.label
        );
        run(&*conn, "commit").await.unwrap();
        assert_eq!(
            conn.keep_alive().await.unwrap(),
            KeepAlive::Alive,
            "{}",
            server.label
        );
        // With autocommit off, the first statement on a transactional table opens one.
        run(
            &*conn,
            "create temporary table keepalive_probe (id int) engine = innodb",
        )
        .await
        .unwrap();
        run(&*conn, "set autocommit = 0").await.unwrap();
        run(&*conn, "insert into keepalive_probe values (1)")
            .await
            .unwrap();
        assert_eq!(
            conn.keep_alive().await.unwrap(),
            KeepAlive::InTransaction,
            "{}",
            server.label
        );
    }
}

/// What happens overnight: the server closes a session idle for longer than `wait_timeout`.
#[tokio::test]
async fn a_session_past_wait_timeout_is_a_connection_error() {
    for server in servers() {
        let conn = server.connect().await;
        run(&*conn, "set session wait_timeout = 1").await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        match conn.keep_alive().await {
            Err(DriverError::Connection(_)) => {}
            other => panic!("{}: expected a connection error: {other:?}", server.label),
        }
    }
}
