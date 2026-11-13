//! DuckDB driver tests. DuckDB is compiled in: they need nothing external and always run.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use spelaion_driver_api::{
    ConnectParams, Connection, ConstraintKind, Driver, DriverError, ExecEvent, ExecOptions,
    LogicalType, ObjectKind, SchemaObjectKind, Value,
};
use spelaion_driver_duckdb::DuckdbDriver;

/// Temporary file deleted when the test ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("spelaion-{name}-{}.duckdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }

    fn params(&self) -> ConnectParams {
        ConnectParams {
            database: self.0.display().to_string(),
            ..Default::default()
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", ".wal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

async fn open(db: &TempDb) -> Box<dyn Connection> {
    DuckdbDriver {
        create_if_missing: true,
    }
    .connect(&db.params())
    .await
    .expect("opening DuckDB")
}

async fn memory() -> Box<dyn Connection> {
    DuckdbDriver::default()
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .expect("opening :memory:")
}

async fn run(conn: &dyn Connection, sql: &str) -> Result<Vec<ExecEvent>, DriverError> {
    run_with(conn, sql, 10_000).await
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

async fn exec_all(conn: &dyn Connection, script: &str) {
    for stmt in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        run(conn, stmt)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

/// Header (name, logical type, native type) and the rows as text.
async fn table(
    conn: &dyn Connection,
    sql: &str,
) -> (Vec<(String, LogicalType, String)>, Vec<Vec<String>>) {
    let events = run(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let mut header = Vec::new();
    let mut rows = Vec::new();
    for event in events {
        match event {
            ExecEvent::Columns(schema) => {
                header = schema
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), c.logical, c.native_type.clone()))
                    .collect();
            }
            ExecEvent::Batch(batch) => {
                for r in 0..batch.num_rows() {
                    rows.push(
                        header
                            .iter()
                            .enumerate()
                            .map(|(i, (_, logical, _))| {
                                Value::from_array(batch.column(i).as_ref(), r, *logical)
                                    .unwrap()
                                    .to_string()
                            })
                            .collect(),
                    );
                }
            }
            ExecEvent::Notice(_) | ExecEvent::Done { .. } => {}
        }
    }
    (header, rows)
}

/// Every type DuckDB has, through the driver: the logical type each one gets, and its text
/// next to DuckDB's own `CAST(… AS VARCHAR)`. They must be equal except for the deliberate
/// differences listed at the end.
#[tokio::test]
async fn type_zoo() {
    let conn = memory().await;
    let zoo: &[(&str, LogicalType, &str)] = &[
        ("true", LogicalType::Bool, "BOOLEAN"),
        ("(-5)::tinyint", LogicalType::Int, "TINYINT"),
        ("300::smallint", LogicalType::Int, "SMALLINT"),
        ("2147483647::integer", LogicalType::Int, "INTEGER"),
        ("(-9223372036854775808)::bigint", LogicalType::Int, "BIGINT"),
        ("255::utinyint", LogicalType::Int, "UTINYINT"),
        ("65535::usmallint", LogicalType::Int, "USMALLINT"),
        ("4294967295::uinteger", LogicalType::Int, "UINTEGER"),
        (
            "18446744073709551615::ubigint",
            LogicalType::Decimal,
            "UBIGINT",
        ),
        (
            "170141183460469231731687303715884105727::hugeint",
            LogicalType::Decimal,
            "HUGEINT",
        ),
        (
            "12.50::decimal(10,2)",
            LogicalType::Decimal,
            "DECIMAL(10,2)",
        ),
        (
            "(-1.5)::decimal(38,10)",
            LogicalType::Decimal,
            "DECIMAL(38,10)",
        ),
        ("0.1::float", LogicalType::Float, "FLOAT"),
        ("1.25::double", LogicalType::Float, "DOUBLE"),
        ("'hola ñandú'", LogicalType::Text, "VARCHAR"),
        ("''", LogicalType::Text, "VARCHAR"),
        ("'a'::enum('a', 'b')", LogicalType::Text, "ENUM"),
        ("'{\"a\": [1, 2]}'::json", LogicalType::Text, "VARCHAR"),
        (
            "'8d5c0a3e-6f2b-4c1d-9e7a-1b2c3d4e5f60'::uuid",
            LogicalType::Text,
            "VARCHAR",
        ),
        ("date '2024-02-29'", LogicalType::Date, "DATE"),
        ("'infinity'::date", LogicalType::Date, "DATE"),
        ("time '23:59:59.123456'", LogicalType::Time, "TIME"),
        ("time '10:11:12.5'", LogicalType::Time, "TIME"),
        (
            "timestamp '2024-01-01 10:00:00.5'",
            LogicalType::Timestamp,
            "TIMESTAMP",
        ),
        (
            "'-infinity'::timestamp",
            LogicalType::Timestamp,
            "TIMESTAMP",
        ),
        (
            "timestamp_s '2024-01-01 10:00:00'",
            LogicalType::Timestamp,
            "TIMESTAMP_S",
        ),
        (
            "timestamp_ms '2024-01-01 10:00:00.123'",
            LogicalType::Timestamp,
            "TIMESTAMP_MS",
        ),
        (
            "timestamptz '2024-01-01 10:00:00+02'",
            LogicalType::TimestampTz,
            "TIMESTAMP WITH TIME ZONE",
        ),
        (
            "interval 1 year + interval 3 days + interval 4 hour",
            LogicalType::Interval,
            "INTERVAL",
        ),
        (
            "interval '-1 day -00:00:01.5'",
            LogicalType::Interval,
            "INTERVAL",
        ),
        ("interval 0 second", LogicalType::Interval, "INTERVAL"),
        ("interval 29 month", LogicalType::Interval, "INTERVAL"),
        ("[1, 2, null]", LogicalType::Array, "INTEGER[]"),
        (
            "['a b', null, '', 'it''s', 'NULL', 'x:y', ' lead', 'a\\b']",
            LogicalType::Array,
            "VARCHAR[]",
        ),
        ("[[1, 2], [3]]", LogicalType::Array, "INTEGER[][]"),
        ("[1, 2]::integer[2]", LogicalType::Array, "INTEGER[2]"),
        ("[1.5, 2.25]", LogicalType::Array, "DECIMAL(3,2)[]"),
        (
            "[1e20, 0.1, 1e-5, 1e15]::double[]",
            LogicalType::Array,
            "DOUBLE[]",
        ),
        ("[date '2024-01-01', null]", LogicalType::Array, "DATE[]"),
        ("[time '10:00:00']", LogicalType::Array, "TIME[]"),
        (
            "[timestamp '2024-01-01 10:00:00.25']",
            LogicalType::Array,
            "TIMESTAMP[]",
        ),
        (
            "[timestamptz '2024-01-01 00:00:00+00']",
            LogicalType::Array,
            "TIMESTAMP WITH TIME ZONE[]",
        ),
        ("[interval 2 day]", LogicalType::Array, "INTERVAL[]"),
        ("[true, false]", LogicalType::Array, "BOOLEAN[]"),
        ("['\\x01\\xAA'::blob]", LogicalType::Array, "BLOB[]"),
        (
            "{'a': 1, 'b': 'x y', 'c': [1, 2], 'd': {'e': null::integer}}",
            LogicalType::Native,
            "STRUCT(a INTEGER, b VARCHAR, c INTEGER[], d STRUCT(e INTEGER))",
        ),
        (
            "map {'k': 1, 'a b': 2}",
            LogicalType::Native,
            "MAP(VARCHAR, INTEGER)",
        ),
        (
            "[map {'k': 'v'}]",
            LogicalType::Array,
            "MAP(VARCHAR, VARCHAR)[]",
        ),
        (
            "union_value(n := 1)::union(n integer, s varchar)",
            LogicalType::Native,
            "UNION(n INTEGER, s VARCHAR)",
        ),
    ];
    for (expr, logical, native) in zoo {
        let sql = format!("select {expr} as v, cast({expr} as varchar) as duckdb");
        let (header, rows) = table(&*conn, &sql).await;
        assert_eq!(
            (header[0].1, header[0].2.as_str()),
            (*logical, *native),
            "{expr}"
        );
        assert_eq!(rows[0][0], rows[0][1], "{expr}: ours vs DuckDB's text");
    }

    // Deliberate differences, as in the other drivers: bytes in PostgreSQL's hex form,
    // dates before year 1 with ` BC`, and top-level doubles as Rust writes them.
    let (header, rows) = table(
        &*conn,
        "select 'ab\\x00'::blob, date '0001-01-01' - 1, 1e20::double",
    )
    .await;
    assert_eq!(header[0].1, LogicalType::Bytes);
    assert_eq!(
        rows[0],
        ["\\x616200", "0001-12-31 BC", "100000000000000000000"]
    );
    // NULLs of any type.
    let (_, rows) = table(
        &*conn,
        "select null::integer, null::varchar, null::integer[], null::struct(a int)",
    )
    .await;
    assert_eq!(rows[0], ["NULL", "NULL", "NULL", "NULL"]);
}

#[tokio::test]
async fn rows_affected_only_where_it_makes_sense() {
    let conn = memory().await;
    let done = |events: Vec<ExecEvent>| match events.last() {
        Some(ExecEvent::Done { rows_affected }) => *rows_affected,
        other => panic!("{other:?}"),
    };
    let columns = |events: &[ExecEvent]| events.iter().any(|e| matches!(e, ExecEvent::Columns(_)));
    let created = run(&*conn, "create table t (a int primary key, b varchar)")
        .await
        .unwrap();
    assert!(!columns(&created), "DDL has no result set");
    assert_eq!(done(created), None);
    let inserted = run(&*conn, "insert into t values (1, 'x'), (2, 'y'), (3, 'z')")
        .await
        .unwrap();
    assert!(!columns(&inserted));
    assert_eq!(done(inserted), Some(3));
    assert_eq!(
        done(
            run(&*conn, "update t set b = 'w' where a > 1")
                .await
                .unwrap()
        ),
        Some(2)
    );
    assert_eq!(done(run(&*conn, "set threads = 2").await.unwrap()), None);
    assert_eq!(done(run(&*conn, "begin").await.unwrap()), None);
    assert_eq!(
        done(run(&*conn, "delete from t where a = 3").await.unwrap()),
        Some(1)
    );
    assert_eq!(done(run(&*conn, "rollback").await.unwrap()), None);
    assert_eq!(done(run(&*conn, "select * from t").await.unwrap()), Some(3));
    // RETURNING and a column called Count in a query are rows, not a count.
    let (header, rows) = table(&*conn, "insert into t values (4, 'v') returning a").await;
    assert_eq!(
        (header[0].0.as_str(), rows),
        ("a", vec![vec!["4".to_string()]])
    );
    let (header, _) = table(&*conn, "select 7 as \"Count\"").await;
    assert_eq!(header[0].0, "Count");
}

#[tokio::test]
async fn batches_respect_batch_size() {
    let conn = memory().await;
    let events = run_with(&*conn, "select range as n from range(25000)", 10_000)
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
}

#[tokio::test]
async fn errors_are_reported_and_connection_survives() {
    let conn = memory().await;
    match run(&*conn, "select * from missing_table").await {
        Err(DriverError::Database {
            message, detail, ..
        }) => {
            assert!(message.contains("missing_table"), "{message}");
            assert!(detail.is_some_and(|d| d.starts_with("LINE 1:")));
        }
        other => panic!("{other:?}"),
    }
    let (_, rows) = table(&*conn, "select 1").await;
    assert_eq!(rows[0][0], "1");
}

/// Never ends on its own: only a cancel stops it.
const ENDLESS: &str = "select count(*) from range(100000000) a, range(100000000) b";

/// How long a cancelled statement may take to stop. A cancel that is lost fails the test
/// here instead of hanging it (and CI) forever.
const CANCEL_LIMIT: Duration = Duration::from_secs(10);

/// Collects a statement's events, failing if it does not end within [`CANCEL_LIMIT`].
async fn finish(
    events: spelaion_driver_api::EventStream,
    what: &str,
) -> Result<Vec<ExecEvent>, DriverError> {
    tokio::time::timeout(CANCEL_LIMIT, events.try_collect())
        .await
        .unwrap_or_else(|_| panic!("{what}: the cancel was lost, the statement still runs"))
}

/// The connection still answers after a cancel, and no cancel is left over for it.
async fn still_answers(conn: &dyn Connection) {
    let answer = tokio::time::timeout(CANCEL_LIMIT, table(conn, "select 42"))
        .await
        .expect("the connection should answer after a cancel");
    assert_eq!(answer.1, vec![vec!["42".to_string()]]);
}

#[tokio::test]
async fn cancel_interrupts_a_long_query() {
    let conn: Arc<dyn Connection> = Arc::from(memory().await);
    let cancel = conn.cancel_handle();
    let started = Instant::now();
    let events = conn
        .execute(ENDLESS, &ExecOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    cancel.cancel().await.unwrap();
    let result = finish(events, "during").await;
    assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
    assert!(started.elapsed() < CANCEL_LIMIT);
    still_answers(&*conn).await;
}

/// A cancel that arrives while the statement is still queued on the connection's thread, or is
/// being prepared, used to be wiped by DuckDB when the statement started: it must stop it.
#[tokio::test]
async fn cancel_before_the_statement_starts_stops_it() {
    let conn = memory().await;
    let cancel = conn.cancel_handle();
    for round in 0..20 {
        let events = conn
            .execute(ENDLESS, &ExecOptions::default())
            .await
            .unwrap();
        cancel.cancel().await.unwrap();
        let result = finish(events, &format!("before, round {round}")).await;
        assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
    }
    still_answers(&*conn).await;
}

/// Cancels at every moment of a statement's start: queued, being prepared, starting to run.
#[tokio::test]
async fn cancel_while_the_statement_starts_stops_it() {
    let conn = memory().await;
    let cancel = conn.cancel_handle();
    for micros in [
        0, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000,
    ] {
        let events = conn
            .execute(ENDLESS, &ExecOptions::default())
            .await
            .unwrap();
        let started = std::time::Instant::now();
        // A busy wait: tokio's timer is too coarse for these delays.
        while started.elapsed() < Duration::from_micros(micros) {
            std::hint::spin_loop();
        }
        cancel.cancel().await.unwrap();
        let result = finish(events, &format!("{micros} µs after it was sent")).await;
        assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
    }
    still_answers(&*conn).await;
}

/// A cancel stops every statement sent before it, also those waiting behind the running one.
#[tokio::test]
async fn cancel_stops_queued_statements_and_appends() {
    let conn = memory().await;
    exec_all(&*conn, "create table t (a int)").await;
    let first = conn
        .execute(ENDLESS, &ExecOptions::default())
        .await
        .unwrap();
    let second = conn
        .execute(ENDLESS, &ExecOptions::default())
        .await
        .unwrap();
    let target = spelaion_driver_api::InsertTarget {
        schema: String::new(),
        table: "t".into(),
        columns: vec!["a".into()],
    };
    let rows = spelaion_driver_api::TextRows::from_rows(1, &[vec![Some("1".to_string())]]).unwrap();
    let mut append = Box::pin(conn.insert_rows(&target, rows));
    // Polled once: the append is queued behind the statements before the cancel.
    assert!(futures_util::poll!(append.as_mut()).is_pending());
    conn.cancel_handle().cancel().await.unwrap();
    for (events, what) in [(first, "running"), (second, "queued")] {
        let result = finish(events, what).await;
        assert!(
            matches!(result, Err(DriverError::Cancelled)),
            "{what}: {result:?}"
        );
    }
    let appended = tokio::time::timeout(CANCEL_LIMIT, append)
        .await
        .expect("the queued append should stop");
    assert!(
        matches!(appended, Err(DriverError::Cancelled)),
        "{appended:?}"
    );
    assert_eq!(table(&*conn, "select count(*) from t").await.1[0][0], "0");
}

/// A cancel reaches only what was sent before it: never a statement sent afterwards.
#[tokio::test]
async fn a_cancel_never_reaches_a_later_statement() {
    let conn = memory().await;
    let cancel = conn.cancel_handle();
    // With nothing running.
    cancel.cancel().await.unwrap();
    still_answers(&*conn).await;
    // After a statement that ended.
    table(&*conn, "select 1").await;
    cancel.cancel().await.unwrap();
    still_answers(&*conn).await;
    // After a cancelled one, many times, with the next statement sent at once.
    for _ in 0..20 {
        let events = conn
            .execute(ENDLESS, &ExecOptions::default())
            .await
            .unwrap();
        cancel.cancel().await.unwrap();
        let result = finish(events, "cancelled").await;
        assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
        let next = run(&*conn, "select count(*) from range(3000000)").await;
        assert!(next.is_ok(), "the next statement was cancelled: {next:?}");
    }
}

/// A cancel while the rows of a large result are being read stops it between batches.
#[tokio::test]
async fn cancel_stops_a_result_being_read() {
    let conn = memory().await;
    let mut events = conn
        .execute(
            "select * from range(1000000000)",
            &ExecOptions {
                batch_size: 100,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    use futures_util::StreamExt;
    let first = tokio::time::timeout(CANCEL_LIMIT, events.next())
        .await
        .unwrap();
    assert!(
        matches!(first, Some(Ok(ExecEvent::Columns(_)))),
        "{first:?}"
    );
    conn.cancel_handle().cancel().await.unwrap();
    let result = finish(events, "reading").await;
    assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
    still_answers(&*conn).await;
}

#[tokio::test]
async fn missing_file_is_not_created_by_default() {
    let db = TempDb::new("duck-missing");
    let result = DuckdbDriver::default().connect(&db.params()).await;
    assert!(result.is_err(), "should not open a nonexistent database");
    assert!(!db.0.exists(), "should not create the file");
}

/// The app opens several connections to one file (tabs, explorer): DuckDB allows one open
/// per process, so they share it, and the file is free again once they all close.
#[tokio::test]
async fn connections_to_one_file_share_it() {
    let db = TempDb::new("duck-shared");
    let first = open(&db).await;
    let second = DuckdbDriver::default()
        .connect(&db.params())
        .await
        .expect("a second connection");
    exec_all(&*first, "create table t (a int); insert into t values (1)").await;
    assert_eq!(table(&*second, "select count(*) from t").await.1[0][0], "1");
    // Another mode while it is open is refused with our own words.
    let read_only = ConnectParams {
        read_only: true,
        ..db.params()
    };
    match DuckdbDriver::default().connect(&read_only).await {
        Err(DriverError::Connection(message)) => {
            assert!(message.contains("already open"), "{message}")
        }
        other => panic!("{:?}", other.err()),
    }
    drop(first);
    match DuckdbDriver::default().connect(&read_only).await {
        Err(DriverError::Connection(message)) => {
            assert!(message.contains("already open"), "{message}")
        }
        other => panic!(
            "the remaining connection should keep the file open: {:?}",
            other.err()
        ),
    }
    drop(second);
    // Closed: it opens read-only now, and writes fail.
    let conn = DuckdbDriver::default()
        .connect(&read_only)
        .await
        .expect("read-only after closing");
    assert_eq!(table(&*conn, "select a from t").await.1[0][0], "1");
    match run(&*conn, "insert into t values (2)").await {
        Err(DriverError::Database { message, .. }) => {
            assert!(message.to_lowercase().contains("read-only"), "{message}")
        }
        other => panic!("the write should fail: {other:?}"),
    }
    drop(conn);
    // Closing an actor is asynchronous: changing mode immediately must still be reliable.
    for _ in 0..10 {
        let writable = DuckdbDriver::default()
            .connect(&db.params())
            .await
            .expect("writable after closing read-only");
        drop(writable);
        let readonly = DuckdbDriver::default()
            .connect(&read_only)
            .await
            .expect("read-only after closing writable");
        assert_eq!(table(&*readonly, "select a from t").await.1[0][0], "1");
        drop(readonly);
    }
}

#[tokio::test]
async fn introspection_describes_tables() {
    let conn = memory().await;
    exec_all(
        &*conn,
        "create table customer (id integer primary key, name varchar not null default 'x',
                                 email varchar unique, check (id > 0));
         create table orders (id integer, line integer, customer_id integer references customer (id),
                              total decimal(12, 2), primary key (id, line));
         create index orders_total on orders (total);
         create view big_orders as select * from orders where total > 100;
         create schema sales;
         create table sales.region (code varchar)",
    )
    .await;
    let schemas = conn.list_schemas().await.unwrap();
    let names: Vec<_> = schemas
        .iter()
        .filter(|s| !s.system)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(names, ["main", "sales"]);

    let objects = conn.list_objects("main").await.unwrap();
    let listed: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(
        listed,
        [
            ("big_orders", ObjectKind::View),
            ("customer", ObjectKind::Table),
            ("orders", ObjectKind::Table)
        ]
    );
    assert_eq!(conn.list_objects("sales").await.unwrap()[0].name, "region");

    let orders = conn.describe_table("main", "orders").await.unwrap();
    assert_eq!(orders.primary_key, ["id", "line"]);
    let columns: Vec<_> = orders
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.native_type.as_str(), c.nullable))
        .collect();
    assert_eq!(
        columns,
        [
            ("id", "INTEGER", false),
            ("line", "INTEGER", false),
            ("customer_id", "INTEGER", true),
            ("total", "DECIMAL(12,2)", true)
        ]
    );
    assert_eq!(orders.foreign_keys.len(), 1);
    let fk = &orders.foreign_keys[0];
    assert_eq!(
        (
            fk.columns.as_slice(),
            fk.referenced_table.as_str(),
            fk.referenced_columns.as_slice()
        ),
        (
            &["customer_id".to_string()][..],
            "customer",
            &["id".to_string()][..]
        )
    );
    assert!(
        orders
            .indexes
            .iter()
            .any(|i| i.name == "orders_total" && !i.unique)
    );
    assert!(orders.indexes.iter().any(|i| i.primary));
    let customer = conn.describe_table("main", "customer").await.unwrap();
    assert_eq!(customer.columns[1].default.as_deref(), Some("'x'"));
    assert_eq!(
        constraints(&customer),
        [
            (
                Some("customer_id_pkey"),
                ConstraintKind::PrimaryKey,
                vec!["id"],
                "PRIMARY KEY(id)"
            ),
            (
                Some("customer_email_key"),
                ConstraintKind::Unique,
                vec!["email"],
                "UNIQUE(email)"
            ),
            (
                Some("customer_id_check"),
                ConstraintKind::Check,
                vec![],
                "CHECK((id > 0))"
            ),
        ]
    );
    assert!(conn.describe_table("main", "missing").await.is_err());
}

#[tokio::test]
async fn attached_databases_are_browsable() {
    let other = TempDb::new("duck-attached");
    let conn = memory().await;
    // Create the attached database through this connection so no background writer can
    // still be closing the same file while ATTACH or DETACH touches its WAL.
    let path = other.0.to_string_lossy().replace('\'', "''");
    run(&*conn, &format!("attach '{path}' as other"))
        .await
        .expect("attaching the file");
    exec_all(&*conn, "create table other.main.things (a int)").await;
    let schemas = conn.list_schemas().await.unwrap();
    assert!(
        schemas.iter().any(|s| s.name == "other.main"),
        "{schemas:?}"
    );
    let objects = conn.list_objects("other.main").await.unwrap();
    assert_eq!(
        (objects[0].schema.as_str(), objects[0].name.as_str()),
        ("other.main", "things")
    );
    exec_all(&*conn, "detach other").await;
}

#[tokio::test]
async fn server_version_is_reported() {
    let conn = memory().await;
    let version = conn.server_version().await.unwrap();
    assert!(version.starts_with('v'), "{version}");
}

#[tokio::test]
async fn new_databases_are_created_once_and_never_overwritten() {
    let db = TempDb::new("duck-new");
    spelaion_driver_duckdb::create_database(&db.0).unwrap();
    let conn = DuckdbDriver::default().connect(&db.params()).await.unwrap();
    exec_all(&*conn, "create table kept (a int)").await;
    drop(conn);
    assert!(spelaion_driver_duckdb::create_database(&db.0).is_err());
    let conn = DuckdbDriver::default().connect(&db.params()).await.unwrap();
    assert_eq!(conn.list_objects("main").await.unwrap()[0].name, "kept");
}

#[tokio::test]
async fn explorer_lists_macros_sequences_and_types() {
    let conn = memory().await;
    exec_all(
        &*conn,
        "create schema s;
         create macro s.both(a) as a, (a, b) as a + b;
         create macro s.series(n) as table select * from range(n);
         create sequence s.seq1 start 5 increment by 2;
         create type s.mood as enum ('sad', 'it''s ok');
         create type s.money2 as decimal(12, 2);
         create type s.pt as struct(x int, y int)",
    )
    .await;
    let objects = conn.list_schema_objects("s").await.unwrap();
    let listed: Vec<_> = objects
        .iter()
        .map(|o| (o.name.as_str(), o.kind, o.detail.as_deref()))
        .collect();
    assert_eq!(
        listed,
        [
            ("both", SchemaObjectKind::Function, Some("a")),
            ("both", SchemaObjectKind::Function, Some("a, b")),
            ("money2", SchemaObjectKind::Type, Some("DECIMAL")),
            ("mood", SchemaObjectKind::Enum, None),
            ("pt", SchemaObjectKind::Type, Some("STRUCT")),
            ("seq1", SchemaObjectKind::Sequence, None),
            ("series", SchemaObjectKind::Function, Some("n")),
        ]
    );
    let definitions = [
        "CREATE MACRO s.\"both\"(a) AS a;",
        "CREATE MACRO s.\"both\"(a, b) AS (a + b);",
        "CREATE TYPE s.money2 AS DECIMAL(12,2);",
        "CREATE TYPE s.mood AS ENUM ('sad', 'it''s ok');",
        "CREATE TYPE s.pt AS STRUCT(x INTEGER, y INTEGER);",
        "CREATE SEQUENCE s.seq1 INCREMENT BY 2 MINVALUE 1 MAXVALUE 9223372036854775807 START 5 NO CYCLE;",
        "CREATE MACRO s.series(n) AS TABLE SELECT * FROM \"range\"(n);",
    ];
    for (i, (object, expected)) in objects.iter().zip(definitions).enumerate() {
        let definition = conn.object_definition(object).await.unwrap();
        assert_eq!(definition, expected);
        // What the explorer opens can be run: in another schema it creates the same object
        // (one overload of a macro per definition: the second would replace the first).
        if i != 1 {
            let elsewhere = definition.replacen("s.", "main.", 1);
            exec_all(&*conn, elsewhere.trim_end_matches(';')).await;
        }
    }
    assert_eq!(
        conn.list_schema_objects("main").await.unwrap().len(),
        objects.len() - 1
    );
    let mut missing = objects[0].clone();
    missing.detail = Some("x, y, z".into());
    assert!(conn.object_definition(&missing).await.is_err());
    assert_eq!(conn.list_server_objects().await.unwrap(), []);
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
    let conn = memory().await;
    run(&*conn, "create schema like_probe").await.unwrap();
    for name in TRICKY_NAMES {
        let quoted = name.replace('"', "\"\"");
        run(
            &*conn,
            &format!("create table like_probe.\"{quoted}\" (id int)"),
        )
        .await
        .unwrap();
        run(
            &*conn,
            &format!("create sequence like_probe.\"{quoted}_seq\""),
        )
        .await
        .unwrap();
    }
    server_side_keeps_what_rust_keeps(&*conn, "like_probe").await;

    // An attached database's schemas are shown, and filtered, as `database.schema`.
    // A new file, created by ATTACH itself.
    let other = TempDb::new("duck-like-attached");
    let path = other.0.to_string_lossy().replace('\'', "''");
    run(&*conn, &format!("attach '{path}' as other"))
        .await
        .unwrap();
    run(&*conn, "create table other.main.things (a int)")
        .await
        .unwrap();
    let names = |filter: spelaion_driver_api::NameFilter| {
        let conn = &conn;
        async move {
            let mut got: Vec<String> = conn
                .list_schemas_matching(&filter)
                .await
                .unwrap()
                .into_iter()
                .map(|s| s.name)
                .collect();
            got.sort();
            got
        }
    };
    let only = |pattern: &str| spelaion_driver_api::NameFilter {
        include: vec![pattern.into()],
        exclude: vec![],
    };
    assert_eq!(names(only("OTHER.*")).await, ["other.main"]);
    assert_eq!(names(only("like_*")).await, ["like_probe"]);
    assert_eq!(
        conn.list_objects_matching("other.main", &only("thing?"))
            .await
            .unwrap()
            .into_iter()
            .map(|o| o.name)
            .collect::<Vec<_>>(),
        ["things"]
    );
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
    let conn = memory().await;
    exec_all(
        &*conn,
        "create schema sales;
         create table sales.customer (id integer primary key, email varchar not null unique);
         create table sales.lines (id integer primary key,
                                   customer_id integer not null references sales.customer (id),
                                   qty integer not null default 1 check (qty > 0),
                                   note varchar);
         create index lines_qty on sales.lines (qty);
         create view sales.big as select id, qty from sales.lines where qty > 10",
    )
    .await;
    let dialect = spelaion_driver_duckdb::DuckdbDialect;
    let big = conn.relation_definition("sales", "big").await.unwrap();
    assert!(big.starts_with("CREATE VIEW sales.big AS"), "{big}");
    let lines = ddl_round_trip(&*conn, &dialect, "sales", "lines", "drop table sales.lines").await;
    assert!(
        lines.contains("CREATE INDEX lines_qty ON sales.lines"),
        "{lines}"
    );
    assert!(conn.relation_definition("sales", "nope").await.is_err());
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
