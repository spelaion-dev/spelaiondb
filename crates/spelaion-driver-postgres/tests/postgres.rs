//! Tests against a real PostgreSQL server (ADR-012).
//!
//! They need `SPELAION_TEST_PG=host:port` (user, password and database: `spelaion`).
//! Locally: `docker compose -f testing/docker/compose.yaml up -d` and
//! `SPELAION_TEST_PG=127.0.0.1:15432 cargo test`. Without the variable they are skipped
//! with a notice.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use spelaion_driver_api::{
    ConnectParams, Connection, Driver, DriverError, ExecEvent, ExecOptions, LogicalType,
    ObjectKind, Secret, TlsMode, Value,
};
use spelaion_driver_postgres::PostgresDriver;

fn test_addr() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return None;
    };
    let (host, port) = addr
        .rsplit_once(':')
        .expect("SPELAION_TEST_PG must be host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

async fn connect() -> Option<Box<dyn Connection>> {
    let (host, port) = test_addr()?;
    let params = ConnectParams {
        host,
        port,
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: "spelaion".into(),
        application_name: Some("spelaion-tests".into()),
        ..Default::default()
    };
    let conn = PostgresDriver
        .connect(&params)
        .await
        .expect("connecting to the test PostgreSQL");
    // Fixed time zone: the text of timestamptz depends on it.
    run(&*conn, "set timezone to 'UTC'")
        .await
        .expect("set timezone");
    Some(conn)
}

/// Runs a statement and returns all its events.
async fn run(conn: &dyn Connection, sql: &str) -> Result<Vec<ExecEvent>, DriverError> {
    run_with(conn, sql, &ExecOptions::default()).await
}

async fn run_with(
    conn: &dyn Connection,
    sql: &str,
    opts: &ExecOptions,
) -> Result<Vec<ExecEvent>, DriverError> {
    conn.execute(sql, opts).await?.try_collect().await
}

/// First row of the result, as (column, logical type, native type, text).
async fn first_row(conn: &dyn Connection, sql: &str) -> Vec<(String, LogicalType, String, String)> {
    let events = run(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n→ {e:?}"));
    let schema = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Columns(s) => Some(s.clone()),
            _ => None,
        })
        .expect("the query returned no columns");
    let batch = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Batch(b) => Some(b.clone()),
            _ => None,
        })
        .expect("the query returned no rows");
    schema
        .columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let value = Value::from_array(batch.column(i).as_ref(), 0, col.logical)
                .unwrap_or_else(|| panic!("column {} has an unexpected physical type", col.name));
            (
                col.name.clone(),
                col.logical,
                col.native_type.clone(),
                value.to_string(),
            )
        })
        .collect()
}

/// Text of the first row according to the server itself: the simple protocol returns the
/// output of each type's output function, exactly what `psql` shows.
async fn server_text(sql: &str) -> Vec<String> {
    let (host, port) = test_addr().expect("SPELAION_TEST_PG");
    let (client, connection) = tokio_postgres::Config::new()
        .host(&host)
        .port(port)
        .user("spelaion")
        .password("spelaion")
        .dbname("spelaion")
        .connect(tokio_postgres::NoTls)
        .await
        .expect("reference connection");
    tokio::spawn(connection);
    client.batch_execute("set timezone to 'UTC'").await.unwrap();
    let messages = client.simple_query(sql).await.unwrap_or_else(|e| {
        panic!(
            "{sql}
→ {e}"
        )
    });
    let row = messages
        .iter()
        .find_map(|m| match m {
            tokio_postgres::SimpleQueryMessage::Row(r) => Some(r),
            _ => None,
        })
        .expect("no rows");
    (0..row.len())
        .map(|i| row.get(i).unwrap_or("NULL").to_string())
        .collect()
}

/// Checks that every expression decodes to the same text `psql` shows, and stores the
/// logical type chosen for each one in a snapshot.
async fn check_zoo(conn: &dyn Connection, name: &str, zoo: &[(&str, &str)]) {
    let sql = format!(
        "select {}",
        zoo.iter()
            .map(|(alias, expr)| format!("{expr} as {alias}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let ours = first_row(conn, &sql).await;
    let theirs = server_text(&sql).await;

    let mut mismatches = Vec::new();
    let mut table = String::new();
    for ((col, logical, native, text), expected) in ours.iter().zip(&theirs) {
        if text != expected && !KNOWN_DIFFERENCES.contains(&col.as_str()) {
            mismatches.push(format!(
                "{col} ({native}): ours {text:?} ≠ PostgreSQL {expected:?}"
            ));
        }
        table.push_str(&format!(
            "{col:<16} {:<12} {native:<14} {text}\n",
            logical.as_str()
        ));
    }
    assert!(
        mismatches.is_empty(),
        "differences from the PostgreSQL text:\n{}",
        mismatches.join("\n")
    );
    insta::assert_snapshot!(name, table);
}

/// Deliberate differences from the PostgreSQL text.
const KNOWN_DIFFERENCES: &[&str] = &[
    // A bool cell is shown as true/false (PostgreSQL: t/f). Inside arrays, t/f.
    "bool",
    // money depends on lc_monetary; we return the number without symbol or separators.
    "money",
];

#[tokio::test]
async fn type_zoo_builtin() {
    let Some(conn) = connect().await else { return };
    check_zoo(
        &*conn,
        "type_zoo_builtin",
        &[
            ("bool", "true"),
            ("int2", "(-32768)::int2"),
            ("int4", "2147483647::int4"),
            ("int8", "(-9223372036854775808)::int8"),
            ("oid", "26::oid"),
            ("float4", "0.1::float4"),
            ("float8", "0.1::float8"),
            ("float_nan", "'NaN'::float8"),
            ("float_inf", "'-Infinity'::float8"),
            ("numeric", "12345678901234567890.123456789::numeric"),
            ("numeric_small", "(-0.0005)::numeric"),
            ("numeric_scale", "1.10::numeric(10,4)"),
            ("numeric_nan", "'NaN'::numeric"),
            ("money", "1234.56::money"),
            ("text", "'text ñ €'::text"),
            ("bpchar", "'ab'::char(4)"),
            ("varchar", "'v'::varchar(5)"),
            ("name", "'a_name'::name"),
            ("qchar", "'x'::\"char\""),
            ("xml", "'<a/>'::xml"),
            ("bytea", r"'\xdeadbeef'::bytea"),
            ("date", "'2026-01-31'::date"),
            ("date_inf", "'infinity'::date"),
            ("date_bc", "'0044-03-15 BC'::date"),
            ("time", "'12:34:56.789'::time"),
            ("timetz", "'12:34:56+05:30'::timetz"),
            ("timestamp", "'2026-01-31 12:00:00.5'::timestamp"),
            ("timestamp_inf", "'-infinity'::timestamp"),
            ("timestamptz", "'2026-01-31 12:00:00+02'::timestamptz"),
            ("interval", "'1 year 2 mons 3 days 04:05:06.5'::interval"),
            ("interval_neg", "'-1 day +02:00'::interval"),
            ("uuid", "'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid"),
            ("json", r#"'{"a": [1, 2]}'::json"#),
            ("jsonb", r#"'{"a": [1, 2], "b": null}'::jsonb"#),
            ("inet", "'192.168.0.1'::inet"),
            ("inet_net", "'192.168.0.0/24'::inet"),
            ("cidr", "'10.0.0.0/8'::cidr"),
            ("inet6", "'::1'::inet"),
            ("macaddr", "'08:00:2b:01:02:03'::macaddr"),
            ("bit", "B'1011'::bit(4)"),
            ("varbit", "B'10110'::varbit"),
            ("point", "point(1.5, -2)"),
            ("box", "box(point(0, 0), point(2, 3))"),
            ("path", "'[(0,0),(1,1)]'::path"),
            ("arr_int", "array[1, 2, null]::int4[]"),
            ("arr_text", "array[['a', 'b c'], ['{', 'null']]::text[]"),
            ("arr_bounds", "'[2:3]={7,8}'::int[]"),
            ("arr_empty", "array[]::int[]"),
            ("arr_bool", "array[true, false, null]"),
            ("arr_ts", "array['2026-01-01 10:00'::timestamp]"),
            ("int4range", "int4range(1, 10)"),
            ("daterange", "'[2026-01-01,)'::daterange"),
            ("numrange_empty", "'empty'::numrange"),
            ("tsrange", "tsrange('2026-01-01', '2026-01-02 12:00')"),
            ("multirange", "'{[1,3),[5,7)}'::int4multirange"),
            ("null_text", "null::text"),
            ("null_int", "null::int"),
        ],
    )
    .await;
}

#[tokio::test]
async fn type_zoo_user_defined() {
    let Some(conn) = connect().await else { return };
    // A real schema (not pg_temp) so the reference connection can see the types.
    let schema = format!("spelaion_zoo_{}", std::process::id());
    for ddl in [
        format!("create schema {schema}"),
        format!("create type {schema}.mood as enum ('happy', 'sad')"),
        format!("create domain {schema}.posint as int check (value > 0)"),
        format!("create type {schema}.pair as (a int, b text, c date)"),
    ] {
        run(&*conn, &ddl)
            .await
            .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    let zoo = [
        ("enum", format!("'happy'::{schema}.mood")),
        (
            "enum_arr",
            format!("array['sad', 'happy']::{schema}.mood[]"),
        ),
        ("domain", format!("42::{schema}.posint")),
        ("composite", format!("row(1, 'x y', null)::{schema}.pair")),
        (
            "composite_arr",
            format!("array[row(2, 'z', '2026-01-01')::{schema}.pair]"),
        ),
    ];
    let zoo: Vec<(&str, &str)> = zoo.iter().map(|(a, e)| (*a, e.as_str())).collect();
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(check_zoo(
        &*conn,
        "type_zoo_user_defined",
        &zoo,
    )))
    .await;
    run(&*conn, &format!("drop schema {schema} cascade"))
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn type_zoo_extensions() {
    let Some(conn) = connect().await else { return };
    for ext in ["hstore", "citext", "ltree"] {
        if let Err(e) = run(&*conn, &format!("create extension if not exists {ext}")).await {
            eprintln!("extension {ext} not available ({e}): test skipped");
            return;
        }
    }
    check_zoo(
        &*conn,
        "type_zoo_extensions",
        &[
            ("hstore", r#"'a=>1, "b c"=>NULL'::hstore"#),
            ("citext", "'MiXeD'::citext"),
            ("ltree", "'top.science.astronomy'::ltree"),
        ],
    )
    .await;
}

#[tokio::test]
async fn batches_and_rows_affected() {
    let Some(conn) = connect().await else { return };
    let events = run_with(
        &*conn,
        "select g from generate_series(1, 25000) g",
        &ExecOptions { batch_size: 10_000 },
    )
    .await
    .unwrap();
    let sizes: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Batch(b) => Some(b.num_rows()),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, [10_000, 10_000, 5_000]);
    assert!(matches!(
        events.last(),
        Some(ExecEvent::Done {
            rows_affected: Some(25_000)
        })
    ));

    run(&*conn, "create temp table t (id int)").await.unwrap();
    let events = run(&*conn, "insert into t select generate_series(1, 7)")
        .await
        .unwrap();
    assert!(matches!(
        events.as_slice(),
        [ExecEvent::Done {
            rows_affected: Some(7)
        }]
    ));
}

#[tokio::test]
async fn notices_are_delivered() {
    let Some(conn) = connect().await else { return };
    let events = run(&*conn, "do $$ begin raise notice 'hola %', 42; end $$")
        .await
        .unwrap();
    let notices: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Notice(n) => Some((n.severity.as_str(), n.message.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(notices, [("NOTICE", "hola 42")]);
}

#[tokio::test]
async fn errors_carry_code_and_position() {
    let Some(conn) = connect().await else { return };
    match run(&*conn, "select * from missing_table").await {
        Err(DriverError::Database { code, position, .. }) => {
            assert_eq!(code.as_deref(), Some("42P01"));
            assert_eq!(position, Some(15));
        }
        other => panic!("expected a database error, got {other:?}"),
    }
    // The connection is still usable after an error.
    assert_eq!(first_row(&*conn, "select 1 as uno").await[0].3, "1");
}

#[tokio::test]
async fn cancel_stops_a_running_query() {
    let Some(conn) = connect().await else { return };
    let conn: Arc<dyn Connection> = Arc::from(conn);
    let cancel = conn.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    let started = Instant::now();
    let result = run(&*conn, "select pg_sleep(30)").await;
    canceller.await.unwrap().unwrap();
    assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn introspection_describes_tables() {
    let Some(conn) = connect().await else { return };
    let schema = format!("spelaion_test_{}", std::process::id());
    let ddl = format!(
        "create schema {schema};
         create table {schema}.customer (id int primary key, email text not null unique);
         comment on table {schema}.customer is 'customers';
         create table {schema}.orders (
             id bigint generated always as identity primary key,
             customer_id int not null references {schema}.customer (id),
             total numeric(12, 2) default 0,
             created timestamptz
         );
         create index orders_created on {schema}.orders (created);
         create view {schema}.big_orders as select * from {schema}.orders where total > 100"
    );
    for stmt in ddl.split(';') {
        run(&*conn, stmt)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }

    let result = async {
        let schemas = conn.list_schemas().await.unwrap();
        assert!(schemas.iter().any(|s| s.name == schema && !s.system));
        assert!(schemas.iter().any(|s| s.name == "pg_catalog" && s.system));

        let objects = conn.list_objects(&schema).await.unwrap();
        let kinds: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("big_orders", ObjectKind::View),
                ("customer", ObjectKind::Table),
                ("orders", ObjectKind::Table)
            ]
        );
        assert_eq!(objects[1].comment.as_deref(), Some("customers"));

        let orders = conn.describe_table(&schema, "orders").await.unwrap();
        let cols: Vec<_> = orders
            .columns
            .iter()
            .map(|c| (c.name.as_str(), c.native_type.as_str(), c.nullable))
            .collect();
        assert_eq!(
            cols,
            [
                ("id", "bigint", false),
                ("customer_id", "integer", false),
                ("total", "numeric(12,2)", true),
                ("created", "timestamp with time zone", true),
            ]
        );
        assert_eq!(orders.columns[2].default.as_deref(), Some("0"));
        assert_eq!(orders.primary_key, ["id"]);
        assert_eq!(orders.indexes.len(), 2);
        assert!(
            orders
                .indexes
                .iter()
                .any(|i| i.name == "orders_created" && !i.unique)
        );
        assert_eq!(orders.foreign_keys.len(), 1);
        assert_eq!(orders.foreign_keys[0].columns, ["customer_id"]);
        assert_eq!(orders.foreign_keys[0].referenced_table, "customer");
        assert_eq!(orders.foreign_keys[0].referenced_columns, ["id"]);
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    run(&*conn, &format!("drop schema {schema} cascade"))
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
async fn server_version_is_reported() {
    let Some(conn) = connect().await else { return };
    let version = conn.server_version().await.unwrap();
    assert!(version.starts_with(char::is_numeric), "{version}");
}

/// Certificate of the test authority (`testing/docker/gen-keys.sh`).
fn test_ca() -> Option<std::path::PathBuf> {
    let ca =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys/ca.crt");
    if ca.exists() {
        Some(ca)
    } else {
        eprintln!(
            "{} does not exist (run testing/docker/gen-keys.sh): test skipped",
            ca.display()
        );
        None
    }
}

async fn connect_tls(
    host: &str,
    tls: TlsMode,
    ca: Option<&std::path::Path>,
) -> Result<Box<dyn Connection>, DriverError> {
    let (_, port) = test_addr().expect("SPELAION_TEST_PG");
    let params = ConnectParams {
        host: host.into(),
        port,
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: "spelaion".into(),
        tls,
        ca_certificate: ca.map(Into::into),
        ..Default::default()
    };
    PostgresDriver.connect(&params).await
}

async fn is_encrypted(conn: &dyn Connection) -> bool {
    first_row(
        conn,
        "select ssl from pg_stat_ssl where pid = pg_backend_pid()",
    )
    .await[0]
        .3
        == "true"
}

#[tokio::test]
async fn tls_modes_behave_like_libpq() {
    if test_addr().is_none() {
        return;
    }
    let Some(ca) = test_ca() else { return };
    let ca = ca.as_path();

    // Without verification: still encrypted (except disable).
    assert!(
        !is_encrypted(
            &*connect_tls("127.0.0.1", TlsMode::Disable, None)
                .await
                .unwrap()
        )
        .await
    );
    assert!(
        is_encrypted(
            &*connect_tls("127.0.0.1", TlsMode::Prefer, None)
                .await
                .unwrap()
        )
        .await
    );
    assert!(
        is_encrypted(
            &*connect_tls("127.0.0.1", TlsMode::Require, None)
                .await
                .unwrap()
        )
        .await
    );

    // The server certificate is only valid for "localhost" and "postgres".
    let full = connect_tls("localhost", TlsMode::VerifyFull, Some(ca))
        .await
        .unwrap();
    assert!(is_encrypted(&*full).await);
    let wrong_name = connect_tls("127.0.0.1", TlsMode::VerifyFull, Some(ca)).await;
    assert!(
        matches!(&wrong_name, Err(DriverError::Connection(m)) if m.contains("not valid for")),
        "verify-full should reject a non-matching name: {:?}",
        wrong_name.err()
    );
    // verify-ca checks the chain but not the name.
    assert!(
        is_encrypted(
            &*connect_tls("127.0.0.1", TlsMode::VerifyCa, Some(ca))
                .await
                .unwrap()
        )
        .await
    );

    // Without the test authority, the system store does not trust the server.
    let untrusted = connect_tls("localhost", TlsMode::VerifyFull, None).await;
    assert!(
        matches!(untrusted, Err(DriverError::Connection(_))),
        "{:?}",
        untrusted.err()
    );
}

#[tokio::test]
async fn cancel_works_over_tls() {
    if test_addr().is_none() {
        return;
    }
    let conn: Arc<dyn Connection> = Arc::from(
        connect_tls("127.0.0.1", TlsMode::Require, None)
            .await
            .expect("TLS connection"),
    );
    let cancel = conn.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    let result = run(&*conn, "select pg_sleep(30)").await;
    canceller.await.unwrap().unwrap();
    assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
}

fn test_key(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testing/docker/keys")
        .join(name)
}

/// ADR-022: the user "spelaion_cert" logs in with a client certificate and no password
/// (testing/docker/postgres/pg_hba.conf).
#[tokio::test]
async fn client_certificates_log_in_without_a_password() {
    let Some(admin) = connect().await else { return };
    run(
        &*admin,
        "do $$ begin create role spelaion_cert login; exception when duplicate_object then null; end $$",
    )
    .await
    .unwrap();
    let (host, port) = test_addr().unwrap();
    let params = ConnectParams {
        host,
        port,
        user: "spelaion_cert".into(),
        password: None,
        database: "spelaion".into(),
        tls: TlsMode::VerifyCa,
        ca_certificate: Some(test_key("ca.crt")),
        client_certificate: Some(test_key("client.crt")),
        client_key: Some(test_key("client.key")),
        ..Default::default()
    };
    let conn = PostgresDriver
        .connect(&params)
        .await
        .expect("logging in with the client certificate");
    let events = run(
        &*conn,
        "select current_user::text, client_dn from pg_stat_ssl where pid = pg_backend_pid()",
    )
    .await
    .unwrap();
    let batch = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Batch(batch) => Some(batch.clone()),
            _ => None,
        })
        .expect("a row");
    let text = |c: usize| {
        spelaion_driver_api::Value::from_array(batch.column(c).as_ref(), 0, LogicalType::Text)
            .unwrap()
            .to_string()
    };
    assert_eq!(
        (text(0), text(1)),
        ("spelaion_cert".into(), "/CN=spelaion_cert".into())
    );

    // Without the certificate the same user is refused, with or without TLS.
    for tls in [TlsMode::VerifyCa, TlsMode::Disable] {
        let without = ConnectParams {
            tls,
            client_certificate: None,
            client_key: None,
            ..params.clone()
        };
        assert!(PostgresDriver.connect(&without).await.is_err(), "{tls:?}");
    }
    // A key that cannot be read says which file, and nothing of any key.
    let missing = ConnectParams {
        client_key: Some(test_key("no-such.key")),
        ..params.clone()
    };
    let message = PostgresDriver
        .connect(&missing)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(message.contains("no-such.key"), "{message}");
    assert!(!message.contains("PRIVATE KEY"), "{message}");
}
