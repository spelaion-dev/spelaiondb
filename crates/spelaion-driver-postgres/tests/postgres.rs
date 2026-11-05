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
    ConnectParams, Connection, ConstraintKind, Driver, DriverError, ExecEvent, ExecOptions,
    LogicalType, ObjectKind, SchemaObjectKind, Secret, ServerObjectKind, TlsMode, Value,
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
            (
                "arr_six_dimensions",
                "array_fill(42, array[1, 1, 1, 1, 1, 1])",
            ),
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
         alter table {schema}.orders add constraint orders_total_positive check (total >= 0);
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
        assert_eq!(
            constraints(&orders),
            [
                (
                    Some("orders_pkey"),
                    ConstraintKind::PrimaryKey,
                    vec!["id"],
                    "PRIMARY KEY (id)"
                ),
                (
                    Some("orders_total_positive"),
                    ConstraintKind::Check,
                    vec![],
                    "CHECK (total >= 0::numeric)"
                ),
            ]
        );
        let customer = conn.describe_table(&schema, "customer").await.unwrap();
        assert_eq!(
            constraints(&customer),
            [
                (
                    Some("customer_pkey"),
                    ConstraintKind::PrimaryKey,
                    vec!["id"],
                    "PRIMARY KEY (id)"
                ),
                (
                    Some("customer_email_key"),
                    ConstraintKind::Unique,
                    vec!["email"],
                    "UNIQUE (email)"
                ),
            ]
        );
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
async fn explorer_lists_the_other_objects_of_a_schema() {
    let Some(conn) = connect().await else { return };
    let schema = format!("spelaion_objects_{}", std::process::id());
    let ddl = [
        format!("create schema {schema}"),
        format!("create table {schema}.t (id int generated always as identity, s serial)"),
        format!("create table {schema}.p (id int) partition by range (id)"),
        format!("create materialized view {schema}.m as select 1 as x"),
        format!(
            "create function {schema}.add1(a integer) returns integer language sql as 'select a + 1'"
        ),
        format!(
            "create function {schema}.add1(a integer, b text) returns integer language sql as 'select a + 1'"
        ),
        format!("comment on function {schema}.add1(integer) is 'adds one'"),
        format!("create procedure {schema}.noop() language sql as 'select 1'"),
        format!(
            "create aggregate {schema}.total(integer) (sfunc = int4pl, stype = integer, initcond = '0')"
        ),
        format!("create sequence {schema}.seq1 start 5"),
        format!(
            "create function {schema}.trg() returns trigger language plpgsql as 'begin return new; end'"
        ),
        format!(
            "create trigger t_ins before insert on {schema}.t for each row execute function {schema}.trg()"
        ),
        format!("create type {schema}.mood as enum ('sad', 'it''s ok')"),
        format!("create domain {schema}.posint as integer not null default 1 check (value > 0)"),
        format!("create type {schema}.pair as (a int, b text)"),
        format!("create type {schema}.floatrange as range (subtype = float8)"),
    ];
    for stmt in &ddl {
        run(&*conn, stmt)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }

    let result = async {
        let kinds: Vec<_> = conn
            .list_objects(&schema)
            .await
            .unwrap()
            .into_iter()
            .map(|o| (o.name, o.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("m".to_string(), ObjectKind::MaterializedView),
                ("p".to_string(), ObjectKind::PartitionedTable),
                ("t".to_string(), ObjectKind::Table),
            ]
        );

        let objects = conn.list_schema_objects(&schema).await.unwrap();
        let listed: Vec<_> = objects
            .iter()
            .map(|o| (o.name.as_str(), o.kind, o.detail.as_deref()))
            .collect();
        // Neither the identity column's sequence nor the range type's constructors.
        assert_eq!(
            listed,
            [
                ("add1", SchemaObjectKind::Function, Some("a integer")),
                (
                    "add1",
                    SchemaObjectKind::Function,
                    Some("a integer, b text")
                ),
                ("floatrange", SchemaObjectKind::Type, Some("RANGE")),
                ("mood", SchemaObjectKind::Enum, None),
                ("noop", SchemaObjectKind::Procedure, Some("")),
                ("pair", SchemaObjectKind::Type, None),
                ("posint", SchemaObjectKind::Domain, Some("integer")),
                ("seq1", SchemaObjectKind::Sequence, None),
                ("t_ins", SchemaObjectKind::Trigger, Some("t")),
                ("t_s_seq", SchemaObjectKind::Sequence, None),
                ("total", SchemaObjectKind::Aggregate, Some("integer")),
                ("trg", SchemaObjectKind::Function, Some("")),
            ]
        );
        assert_eq!(objects[0].comment.as_deref(), Some("adds one"));

        let find = |name: &str, kind: SchemaObjectKind| {
            objects
                .iter()
                .find(|o| o.name == name && o.kind == kind)
                .unwrap_or_else(|| panic!("{name} is not listed"))
                .clone()
        };
        let text = conn.object_definition(&objects[1]).await.unwrap();
        assert!(
            text.starts_with(&format!(
                "CREATE OR REPLACE FUNCTION {schema}.add1(a integer, b text)"
            )),
            "{text}"
        );
        assert!(text.ends_with(';'), "{text}");
        let expected = [
            (
                "total",
                SchemaObjectKind::Aggregate,
                format!(
                    "CREATE AGGREGATE {schema}.total(integer) (\n    SFUNC = int4pl,\n    STYPE = integer,\n    INITCOND = '0'\n);"
                ),
            ),
            (
                "seq1",
                SchemaObjectKind::Sequence,
                format!(
                    "CREATE SEQUENCE {schema}.seq1 AS bigint INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 START WITH 5 CACHE 1 NO CYCLE;"
                ),
            ),
            (
                "t_ins",
                SchemaObjectKind::Trigger,
                format!(
                    "CREATE TRIGGER t_ins BEFORE INSERT ON {schema}.t FOR EACH ROW EXECUTE FUNCTION {schema}.trg();"
                ),
            ),
            (
                "mood",
                SchemaObjectKind::Enum,
                format!("CREATE TYPE {schema}.mood AS ENUM ('sad', 'it''s ok');"),
            ),
            (
                "posint",
                SchemaObjectKind::Domain,
                format!(
                    "CREATE DOMAIN {schema}.posint AS integer DEFAULT 1 NOT NULL CONSTRAINT posint_check CHECK (VALUE > 0);"
                ),
            ),
            (
                "pair",
                SchemaObjectKind::Type,
                format!("CREATE TYPE {schema}.pair AS (\n    a integer,\n    b text\n);"),
            ),
            (
                "floatrange",
                SchemaObjectKind::Type,
                format!("CREATE TYPE {schema}.floatrange AS RANGE (SUBTYPE = double precision);"),
            ),
        ];
        for (name, kind, sql) in expected {
            assert_eq!(
                conn.object_definition(&find(name, kind)).await.unwrap(),
                sql
            );
        }
        let mut missing = find("seq1", SchemaObjectKind::Sequence);
        missing.name = "nope".into();
        assert!(conn.object_definition(&missing).await.is_err());

        // What belongs to an extension is left out: plpgsql (always there) puts its handlers in
        // pg_catalog, next to the built-in functions, which are listed.
        let catalog = conn.list_schema_objects("pg_catalog").await.unwrap();
        assert!(catalog.iter().any(|o| o.name == "now"));
        assert!(
            !catalog.iter().any(|o| o.name == "plpgsql_call_handler"),
            "{catalog:?}"
        );

        let server = conn.list_server_objects().await.unwrap();
        let has = |name: &str, kind| server.iter().any(|o| o.name == name && o.kind == kind);
        assert!(has("spelaion", ServerObjectKind::Role), "{server:?}");
        assert!(
            has("pg_default", ServerObjectKind::Tablespace),
            "{server:?}"
        );
        assert!(has("plpgsql", ServerObjectKind::Extension), "{server:?}");
        // The predefined roles are there, marked as system objects; ours are not.
        assert!(has("pg_monitor", ServerObjectKind::Role), "{server:?}");
        assert!(
            server.iter().all(
                |o| o.system == (o.kind == ServerObjectKind::Role && o.name.starts_with("pg_"))
            )
        );
        let role = server.iter().find(|o| o.name == "spelaion").unwrap();
        assert_eq!(
            role.detail.as_deref(),
            Some("SUPERUSER, LOGIN, CREATEDB, CREATEROLE, REPLICATION")
        );
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
    let Some(conn) = connect().await else { return };
    let schema = format!("spelaion_like_{}", std::process::id());
    run(&*conn, &format!("create schema {schema}"))
        .await
        .unwrap();
    let result = async {
        for name in TRICKY_NAMES {
            let quoted = name.replace('"', "\"\"");
            run(
                &*conn,
                &format!("create table {schema}.\"{quoted}\" (id int)"),
            )
            .await
            .unwrap();
            run(
                &*conn,
                &format!("create sequence {schema}.\"{quoted}_seq\""),
            )
            .await
            .unwrap();
        }
        server_side_keeps_what_rust_keeps(&*conn, &schema).await;
        // Schemas too.
        let mine = spelaion_driver_api::NameFilter {
            include: vec![format!("{schema}*"), "PUBLIC".into()],
            exclude: vec![],
        };
        let mut got: Vec<String> = conn
            .list_schemas_matching(&mine)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        got.sort();
        assert_eq!(got, ["public".to_string(), schema.clone()]);
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    run(&*conn, &format!("drop schema {schema} cascade"))
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
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
    let Some(conn) = connect().await else { return };
    let schema = format!("spelaion_ddl_{}", std::process::id());
    let ddl = [
        format!("create schema {schema}"),
        format!("create table {schema}.customer (id int primary key, email text not null unique)"),
        format!(
            "create table {schema}.\"Order Lines\" (
                 id bigint generated always as identity primary key,
                 customer_id int not null references {schema}.customer (id) on delete cascade,
                 qty int not null default 1 check (qty > 0),
                 price numeric(12, 2),
                 total numeric(14, 2) generated always as (qty * price) stored,
                 note text collate \"C\",
                 seen timestamptz default now(),
                 unique (customer_id, note)
             )"
        ),
        format!(
            "create index order_lines_seen on {schema}.\"Order Lines\" (seen desc) where qty > 1"
        ),
        format!("comment on table {schema}.\"Order Lines\" is 'it''s the lines'"),
        format!("comment on column {schema}.\"Order Lines\".qty is 'how many'"),
        format!("create table {schema}.events (id int, at date not null) partition by range (at)"),
        format!(
            "create view {schema}.big as select id, qty from {schema}.\"Order Lines\" where qty > 10"
        ),
        format!(
            "create materialized view {schema}.counts as select customer_id, count(*) as n from {schema}.\"Order Lines\" group by customer_id"
        ),
    ];
    for stmt in &ddl {
        run(&*conn, stmt)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
    let result = async {
        let dialect = spelaion_driver_postgres::PostgresDialect;
        // The view and the materialized view depend on the table: dropped with it, and made
        // again from their own DDL, read before.
        let big = conn.relation_definition(&schema, "big").await.unwrap();
        assert!(
            big.starts_with(&format!("CREATE OR REPLACE VIEW {schema}.big AS\n")),
            "{big}"
        );
        let counts = conn.relation_definition(&schema, "counts").await.unwrap();
        assert!(
            counts.starts_with(&format!("CREATE MATERIALIZED VIEW {schema}.counts AS\n")),
            "{counts}"
        );
        assert!(counts.ends_with("WITH DATA;"), "{counts}");
        let lines = ddl_round_trip(
            &*conn,
            &dialect,
            &schema,
            "Order Lines",
            &format!("drop table {schema}.\"Order Lines\" cascade"),
        )
        .await;
        for expected in [
            "GENERATED ALWAYS AS IDENTITY NOT NULL",
            "GENERATED ALWAYS AS (((qty)::numeric * price)) STORED",
            "COLLATE \"C\"",
            "DEFAULT now()",
            "ON DELETE CASCADE",
            "CREATE INDEX order_lines_seen ON",
            "IS 'it''s the lines';",
            "IS 'how many';",
        ] {
            assert!(lines.contains(expected), "{expected}\n{lines}");
        }
        for view in [&big, &counts] {
            for span in spelaion_driver_api::Dialect::split_statements(&dialect, view) {
                run(&*conn, span.text(view)).await.unwrap();
            }
        }
        let customer = ddl_round_trip(
            &*conn,
            &dialect,
            &schema,
            "customer",
            &format!("drop table {schema}.customer cascade"),
        )
        .await;
        assert!(customer.contains("UNIQUE (email)"), "{customer}");
        let events = ddl_round_trip(
            &*conn,
            &dialect,
            &schema,
            "events",
            &format!("drop table {schema}.events"),
        )
        .await;
        assert!(events.contains("PARTITION BY RANGE (at)"), "{events}");
        assert!(conn.relation_definition(&schema, "nope").await.is_err());
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    run(&*conn, &format!("drop schema {schema} cascade"))
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
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
    let Some(conn) = connect().await else { return };
    let schema = format!("spelaion_call_{}", std::process::id());
    for stmt in [
        format!("create schema {schema}"),
        format!(
            "create procedure {schema}.calc(a integer, inout b integer, out c text) \
             language plpgsql as $$ begin b := b + a; c := 'x' || a; end $$"
        ),
        format!("create procedure {schema}.nothing() language sql as 'select 1'"),
    ] {
        run(&*conn, &stmt)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
    let result = async {
        let objects = conn.list_schema_objects(&schema).await.unwrap();
        let find = |name: &str| objects.iter().find(|o| o.name == name).unwrap().clone();
        let call = conn.procedure_call(&find("calc")).await.unwrap();
        assert_eq!(
            call,
            format!("CALL {schema}.calc(a => :a, b => :b, c => NULL);")
        );
        let row = first_row(&*conn, &call.replace(":a", "1").replace(":b", "2")).await;
        let values: Vec<&str> = row.iter().map(|c| c.3.as_str()).collect();
        assert_eq!(values, ["3", "x1"]);
        assert_eq!(
            conn.procedure_call(&find("nothing")).await.unwrap(),
            format!("CALL {schema}.nothing();")
        );
        // A function is not called with CALL.
        let mut function = find("calc");
        function.kind = spelaion_driver_api::SchemaObjectKind::Function;
        assert!(conn.procedure_call(&function).await.is_err());
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    run(&*conn, &format!("drop schema {schema} cascade"))
        .await
        .unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}
