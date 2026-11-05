//! SQLite driver tests. They need nothing external: they always run.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use spelaion_driver_api::{
    ConnectParams, Connection, ConstraintKind, Driver, DriverError, ExecEvent, ExecOptions,
    LogicalType, ObjectKind, SchemaObjectKind, Value,
};
use spelaion_driver_sqlite::SqliteDriver;

/// Temporary file deleted when the test ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("spelaion-{name}-{}.db", std::process::id()));
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
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

async fn open(db: &TempDb) -> Box<dyn Connection> {
    SqliteDriver {
        create_if_missing: true,
    }
    .connect(&db.params())
    .await
    .expect("opening SQLite")
}

async fn memory() -> Box<dyn Connection> {
    SqliteDriver::default()
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
    conn.execute(sql, &ExecOptions { batch_size })
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

/// All rows as text, with the logical type of each column.
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

#[tokio::test]
async fn type_zoo() {
    let conn = memory().await;
    exec_all(
        &*conn,
        "create table zoo (
            i integer, big bigint, r real, d double precision, n numeric, dec decimal(10, 2),
            t text, v varchar(10), b blob, flag boolean, day date, untyped
         );
         insert into zoo values (
            1, 9223372036854775807, 1.5, 2.0, 7, 12.5,
            'text ñ €', 'v', x'deadbeef', 1, '2026-01-31', null
         );
         insert into zoo values (null, null, null, null, null, null, null, null, null, null, null, 'x')",
    )
    .await;
    let (header, rows) = table(&*conn, "select * from zoo").await;
    let mut out = String::new();
    for (i, (name, logical, native)) in header.iter().enumerate() {
        out.push_str(&format!(
            "{name:<8} {:<8} {native:<18} {:<26} {}\n",
            logical.as_str(),
            rows[0][i],
            rows[1][i]
        ));
    }
    insta::assert_snapshot!(out);
}

#[tokio::test]
async fn expressions_without_declared_type() {
    let conn = memory().await;
    let (header, rows) = table(
        &*conn,
        "select 1 + 1 as n, 1.0 / 4 as r, 'a' || 'b' as t, x'01' as b, null as z",
    )
    .await;
    let types: Vec<_> = header
        .iter()
        .map(|(n, l, native)| (n.as_str(), *l, native.as_str()))
        .collect();
    assert_eq!(
        types,
        [
            ("n", LogicalType::Int, "INTEGER"),
            ("r", LogicalType::Float, "REAL"),
            ("t", LogicalType::Text, "TEXT"),
            ("b", LogicalType::Bytes, "BLOB"),
            ("z", LogicalType::Bytes, "NULL"),
        ]
    );
    assert_eq!(rows[0], ["2", "0.25", "ab", "\\x01", "NULL"]);
}

#[tokio::test]
async fn mixed_types_in_first_batch_become_text() {
    let conn = memory().await;
    exec_all(
        &*conn,
        "create table m (x integer); insert into m values (1), ('dos'), (3.5), (x'ff')",
    )
    .await;
    let (header, rows) = table(&*conn, "select x from m").await;
    assert_eq!(header[0].1, LogicalType::Text);
    let values: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
    assert_eq!(values, ["1", "dos", "3.5", "\\xff"]);
}

#[tokio::test]
async fn mixed_types_after_first_batch_fail_loudly() {
    let conn = memory().await;
    exec_all(
        &*conn,
        "create table m (x integer); insert into m values (1), (2), (3), ('four')",
    )
    .await;
    // Batch of 2: the column is decided as integer and 'four' arrives later.
    match run_with(&*conn, "select x from m", 2).await {
        Err(DriverError::Decode { message, .. }) => {
            assert!(
                message.contains("row 4") && message.contains("CAST"),
                "{message}"
            );
        }
        other => panic!("expected a mixed-types error: {other:?}"),
    }
    // Lossless conversions are accepted: 4.0 fits in an integer column.
    exec_all(
        &*conn,
        "delete from m; insert into m values (1), (2), (4.0)",
    )
    .await;
    assert!(run_with(&*conn, "select x from m", 2).await.is_ok());
}

#[tokio::test]
async fn invalid_utf8_is_kept_as_bytes() {
    let conn = memory().await;
    let (header, rows) = table(&*conn, "select cast(x'41ff42' as text) as t").await;
    assert_eq!(header[0].1, LogicalType::Bytes);
    assert_eq!(rows[0][0], "\\x41ff42");
}

#[tokio::test]
async fn rows_affected_only_where_it_makes_sense() {
    let conn = memory().await;
    let done = |events: Vec<ExecEvent>| match events.last() {
        Some(ExecEvent::Done { rows_affected }) => *rows_affected,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        done(run(&*conn, "create table t (a int)").await.unwrap()),
        None
    );
    assert_eq!(
        done(
            run(&*conn, "insert into t values (1), (2), (3)")
                .await
                .unwrap()
        ),
        Some(3)
    );
    assert_eq!(
        done(run(&*conn, "create index ti on t (a)").await.unwrap()),
        None
    );
    assert_eq!(
        done(
            run(&*conn, "update t set a = a + 1 where a > 1")
                .await
                .unwrap()
        ),
        Some(2)
    );
    assert_eq!(done(run(&*conn, "select * from t").await.unwrap()), Some(3));
}

#[tokio::test]
async fn batches_respect_batch_size() {
    let conn = memory().await;
    let events = run_with(
        &*conn,
        "with recursive c(n) as (select 1 union all select n + 1 from c where n < 25000) select n from c",
        10_000,
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
}

#[tokio::test]
async fn errors_are_reported_and_connection_survives() {
    let conn = memory().await;
    match run(&*conn, "select * from missing_table").await {
        Err(DriverError::Database { message, .. }) => {
            assert!(message.contains("missing_table"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    // Syntax errors point to where they are (in characters, 1-based).
    match run(&*conn, "select ñ form t").await {
        Err(DriverError::Database {
            position: Some(p), ..
        }) => assert_eq!(p, 15, "should point at the t"),
        other => panic!("expected a syntax error with a position: {other:?}"),
    }
    let (_, rows) = table(&*conn, "select 1").await;
    assert_eq!(rows[0][0], "1");
}

#[tokio::test]
async fn cancel_interrupts_a_long_query() {
    let conn: Arc<dyn Connection> = Arc::from(memory().await);
    let cancel = conn.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel().await
    });
    let started = Instant::now();
    let result = run(
        &*conn,
        "with recursive c(n) as (select 1 union all select n + 1 from c) select count(*) from c",
    )
    .await;
    canceller.await.unwrap().unwrap();
    assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn missing_file_is_not_created_by_default() {
    let db = TempDb::new("missing");
    let result = SqliteDriver::default().connect(&db.params()).await;
    assert!(result.is_err(), "should not open a nonexistent database");
    assert!(!db.0.exists(), "should not create the file");
}

#[tokio::test]
async fn read_only_connections_reject_writes() {
    let db = TempDb::new("readonly");
    {
        let conn = open(&db).await;
        exec_all(&*conn, "create table t (a int); insert into t values (1)").await;
    }
    let params = ConnectParams {
        read_only: true,
        ..db.params()
    };
    let conn = SqliteDriver::default().connect(&params).await.unwrap();
    assert_eq!(table(&*conn, "select a from t").await.1[0][0], "1");
    match run(&*conn, "insert into t values (2)").await {
        Err(DriverError::Database { message, .. }) => {
            assert!(message.contains("readonly"), "{message}")
        }
        other => panic!("the write should fail: {other:?}"),
    }
}

#[tokio::test]
async fn introspection_describes_tables() {
    let db = TempDb::new("introspect");
    let conn = open(&db).await;
    exec_all(
        &*conn,
        "create table customer (id integer primary key, email text not null unique);
         create table orders (
            id integer primary key autoincrement,
            customer_id integer not null references customer,
            total numeric default 0,
            created text
         );
         create index orders_created on orders (created);
         create table line (order_id integer, n integer, sku text, primary key (order_id, n),
                            foreign key (order_id) references orders (id));
         create view big_orders as select * from orders where total > 100;
         attach ':memory:' as aux",
    )
    .await;
    run(
        &*conn,
        "create trigger orders_total after update of total on orders begin select 1; end",
    )
    .await
    .unwrap();

    let schemas: Vec<String> = conn
        .list_schemas()
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(schemas, ["main", "aux"]);

    let objects = conn.list_objects("main").await.unwrap();
    let kinds: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("big_orders", ObjectKind::View),
            ("customer", ObjectKind::Table),
            ("line", ObjectKind::Table),
            ("orders", ObjectKind::Table),
        ]
    );

    // Triggers are what the explorer lists besides tables and views (ADR-024).
    let others = conn.list_schema_objects("main").await.unwrap();
    assert_eq!(others.len(), 1);
    assert_eq!(
        (
            others[0].name.as_str(),
            others[0].kind,
            others[0].detail.as_deref()
        ),
        ("orders_total", SchemaObjectKind::Trigger, Some("orders"))
    );
    assert_eq!(
        conn.object_definition(&others[0]).await.unwrap(),
        "CREATE TRIGGER orders_total after update of total on orders begin select 1; end;"
    );
    let mut sequence = others[0].clone();
    sequence.kind = SchemaObjectKind::Sequence;
    assert!(conn.object_definition(&sequence).await.is_err());
    assert_eq!(conn.list_schema_objects("aux").await.unwrap(), []);
    assert_eq!(conn.list_server_objects().await.unwrap(), []);

    let orders = conn.describe_table("main", "orders").await.unwrap();
    let cols: Vec<_> = orders
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.native_type.as_str(), c.nullable))
        .collect();
    assert_eq!(
        cols,
        [
            ("id", "INTEGER", true),
            ("customer_id", "INTEGER", false),
            ("total", "numeric", true),
            ("created", "TEXT", true)
        ]
    );
    assert_eq!(orders.columns[2].default.as_deref(), Some("0"));
    assert_eq!(orders.primary_key, ["id"]);
    assert_eq!(
        orders
            .indexes
            .iter()
            .map(|i| i.name.as_str())
            .collect::<Vec<_>>(),
        ["orders_created"]
    );
    assert!(orders.indexes[0].definition.starts_with("CREATE INDEX"));
    // `references customer` without a column: points to the primary key.
    assert_eq!(orders.foreign_keys.len(), 1);
    assert_eq!(orders.foreign_keys[0].referenced_table, "customer");
    assert_eq!(orders.foreign_keys[0].referenced_columns, ["id"]);

    let line = conn.describe_table("main", "line").await.unwrap();
    assert_eq!(line.primary_key, ["order_id", "n"]);
    assert!(
        line.indexes
            .iter()
            .any(|i| i.primary && i.definition.contains("automatic"))
    );

    let customer = conn.describe_table("main", "customer").await.unwrap();
    assert!(customer.indexes.iter().any(|i| i.unique && !i.primary));
    // SQLite keeps no name for the primary key, and check constraints only in the CREATE text.
    assert_eq!(
        constraints(&customer),
        [
            (
                None,
                ConstraintKind::PrimaryKey,
                vec!["id"],
                "PRIMARY KEY (id)"
            ),
            (
                Some("sqlite_autoindex_customer_1"),
                ConstraintKind::Unique,
                vec!["email"],
                "UNIQUE (email)"
            ),
        ]
    );
    assert_eq!(constraints(&line)[0].3, "PRIMARY KEY (order_id, n)");
}

#[tokio::test]
async fn server_version_is_reported() {
    let conn = memory().await;
    let version = conn.server_version().await.unwrap();
    assert!(version.starts_with('3'), "{version}");
}

#[tokio::test]
async fn new_databases_are_created_once_and_never_overwritten() {
    let db = TempDb::new("create");
    spelaion_driver_sqlite::create_database(&db.0).unwrap();
    let header = std::fs::read(&db.0).unwrap();
    assert!(header.starts_with(b"SQLite format 3\0"));
    // The default driver (which never creates files) opens it.
    let conn = SqliteDriver::default().connect(&db.params()).await.unwrap();
    run(conn.as_ref(), "create table t (x int)").await.unwrap();
    run(conn.as_ref(), "insert into t values (1)")
        .await
        .unwrap();
    drop(conn);
    let before = std::fs::read(&db.0).unwrap();
    let err = spelaion_driver_sqlite::create_database(&db.0).unwrap_err();
    assert!(
        err.to_string()
            .ends_with("already exists: a new database never replaces a file"),
        "{err}"
    );
    assert_eq!(std::fs::read(&db.0).unwrap(), before);
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
    let db = TempDb::new("like");
    let conn = open(&db).await;
    for name in TRICKY_NAMES {
        let quoted = name.replace('"', "\"\"");
        run(&*conn, &format!("create table \"{quoted}\" (id int)"))
            .await
            .unwrap();
        run(
            &*conn,
            &format!(
                "create trigger \"{quoted}_trigger\" after insert on \"{quoted}\" begin select 1; end"
            ),
        )
        .await
        .unwrap();
    }
    server_side_keeps_what_rust_keeps(&*conn, "main").await;
    run(&*conn, "attach ':memory:' as Aux_1").await.unwrap();
    let got: Vec<String> = conn
        .list_schemas_matching(&spelaion_driver_api::NameFilter {
            include: vec!["aux_?".into()],
            exclude: vec![],
        })
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(got, ["Aux_1"]);
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
