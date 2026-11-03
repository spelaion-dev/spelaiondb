//! SQL Server driver tests against a real server (testing/docker). Without
//! `SPELUNK_TEST_MSSQL=127.0.0.1:11433` they are skipped, which does not count as tested.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use spelunk_driver_api::{
    ConnectParams, Connection, Driver, DriverError, ExecEvent, ExecOptions, LogicalType,
    ObjectKind, Secret, TlsMode, Value,
};
use spelunk_driver_sqlserver::SqlServerDriver;

const PASSWORD: &str = "Spelunk-test-1";

fn params(database: &str) -> Option<ConnectParams> {
    let Ok(addr) = std::env::var("SPELUNK_TEST_MSSQL") else {
        eprintln!("SPELUNK_TEST_MSSQL not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().unwrap(),
        user: "sa".into(),
        password: Some(Secret::new(PASSWORD)),
        database: database.into(),
        tls: TlsMode::Require,
        ..Default::default()
    })
}

/// A connection to the `spelunk` database, created the first time.
async fn connect() -> Option<Box<dyn Connection>> {
    let master = SqlServerDriver
        .connect(&params("master")?)
        .await
        .expect("connecting to master");
    // Tests run in parallel: a database another test just created is fine (error 1801).
    match run(
        &*master,
        "if db_id('spelunk') is null create database spelunk",
    )
    .await
    {
        Ok(_) => {}
        Err(DriverError::Database { code, .. }) if code.as_deref() == Some("1801") => {}
        Err(e) => panic!("{e:?}"),
    }
    Some(
        SqlServerDriver
            .connect(&params("spelunk")?)
            .await
            .expect("connecting to spelunk"),
    )
}

async fn run(conn: &dyn Connection, sql: &str) -> Result<Vec<ExecEvent>, DriverError> {
    conn.execute(sql, &ExecOptions { batch_size: 10_000 })
        .await?
        .try_collect()
        .await
}

/// A result set: its header (name, logical type, native type) and its rows as text.
type Table = (Vec<(String, LogicalType, String)>, Vec<Vec<String>>);

/// Every result set of a statement: its header and rows as text.
async fn tables(conn: &dyn Connection, sql: &str) -> Vec<Table> {
    let events = run(conn, sql)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let mut out: Vec<Table> = Vec::new();
    for event in events {
        match event {
            ExecEvent::Columns(schema) => out.push((
                schema
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), c.logical, c.native_type.clone()))
                    .collect(),
                Vec::new(),
            )),
            ExecEvent::Batch(batch) => {
                let (header, rows) = out.last_mut().unwrap();
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
    out
}

async fn exec_all(conn: &dyn Connection, script: &str) {
    for stmt in script.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
        run(conn, stmt)
            .await
            .unwrap_or_else(|e| panic!("{stmt}: {e:?}"));
    }
}

/// Every common type through the driver: its logical type, SQL Server's name and its text.
#[tokio::test]
async fn type_zoo() {
    let Some(conn) = connect().await else { return };
    let zoo: &[(&str, LogicalType, &str, &str)] = &[
        ("cast(1 as bit)", LogicalType::Bool, "bit", "true"),
        ("cast(255 as tinyint)", LogicalType::Int, "tinyint", "255"),
        (
            "cast(-32768 as smallint)",
            LogicalType::Int,
            "smallint",
            "-32768",
        ),
        (
            "cast(2147483647 as int)",
            LogicalType::Int,
            "int",
            "2147483647",
        ),
        (
            "cast(-9223372036854775808 as bigint)",
            LogicalType::Int,
            "bigint",
            "-9223372036854775808",
        ),
        ("cast(0.1 as real)", LogicalType::Float, "real", "0.1"),
        ("cast(1.25 as float)", LogicalType::Float, "float", "1.25"),
        (
            "cast(12.50 as decimal(10,2))",
            LogicalType::Decimal,
            "decimal",
            "12.50",
        ),
        (
            "cast(-0.5 as numeric(5,1))",
            LogicalType::Decimal,
            "numeric",
            "-0.5",
        ),
        (
            "cast(12 as decimal(10,0))",
            LogicalType::Decimal,
            "decimal",
            "12",
        ),
        (
            "cast(12345678901234567890.123456789 as decimal(38,9))",
            LogicalType::Decimal,
            "decimal",
            "12345678901234567890.123456789",
        ),
        (
            "cast(1234.56 as money)",
            LogicalType::Decimal,
            "money",
            "1234.5600",
        ),
        // A nullable smallmoney travels as the same TDS type as money.
        (
            "cast(-12.3456 as smallmoney)",
            LogicalType::Decimal,
            "money",
            "-12.3456",
        ),
        (
            "cast('6F9619FF-8B86-D011-B42D-00C04FC964FF' as uniqueidentifier)",
            LogicalType::Uuid,
            "uniqueidentifier",
            "6F9619FF-8B86-D011-B42D-00C04FC964FF",
        ),
        ("N'hola ñandú'", LogicalType::Text, "nvarchar", "hola ñandú"),
        (
            "cast('abc' as varchar(10))",
            LogicalType::Text,
            "varchar",
            "abc",
        ),
        ("cast('a' as char(3))", LogicalType::Text, "char", "a  "),
        (
            "cast(N'x' as nvarchar(max))",
            LogicalType::Text,
            "nvarchar",
            "x",
        ),
        (
            "cast('<a>1</a>' as xml)",
            LogicalType::Text,
            "xml",
            "<a>1</a>",
        ),
        (
            "cast(0x00ff as varbinary(4))",
            LogicalType::Bytes,
            "varbinary",
            "\\x00ff",
        ),
        (
            "cast(0xab as binary(2))",
            LogicalType::Bytes,
            "binary",
            "\\xab00",
        ),
        (
            "cast('2024-02-29' as date)",
            LogicalType::Date,
            "date",
            "2024-02-29",
        ),
        (
            "cast('0001-01-01' as date)",
            LogicalType::Date,
            "date",
            "0001-01-01",
        ),
        (
            "cast('23:59:59.1234567' as time(7))",
            LogicalType::Time,
            "time",
            "23:59:59.123456",
        ),
        (
            "cast('10:11:12' as time(0))",
            LogicalType::Time,
            "time",
            "10:11:12",
        ),
        (
            "cast('2024-01-01 10:00:00.003' as datetime)",
            LogicalType::Timestamp,
            "datetime",
            "2024-01-01 10:00:00.003",
        ),
        (
            "cast('2024-01-01 10:00:00.007' as datetime)",
            LogicalType::Timestamp,
            "datetime",
            "2024-01-01 10:00:00.007",
        ),
        // A nullable smalldatetime travels as the same TDS type as datetime.
        (
            "cast('2024-01-01 10:00:29' as smalldatetime)",
            LogicalType::Timestamp,
            "datetime",
            "2024-01-01 10:00:00",
        ),
        (
            "cast('2024-01-01 10:00:00.5' as datetime2(3))",
            LogicalType::Timestamp,
            "datetime2",
            "2024-01-01 10:00:00.5",
        ),
        (
            "cast('9999-12-31 23:59:59.9999999' as datetime2(7))",
            LogicalType::Timestamp,
            "datetime2",
            "9999-12-31 23:59:59.999999",
        ),
        (
            "cast('2024-01-01 10:00:00 +02:00' as datetimeoffset)",
            LogicalType::TimestampTz,
            "datetimeoffset",
            "2024-01-01 08:00:00+00",
        ),
        (
            "cast(42 as sql_variant)",
            LogicalType::Native,
            "sql_variant",
            "42",
        ),
        ("cast(null as int)", LogicalType::Int, "int", "NULL"),
        (
            "cast(null as nvarchar(10))",
            LogicalType::Text,
            "nvarchar",
            "NULL",
        ),
    ];
    for (expr, logical, native, text) in zoo {
        let result = tables(&*conn, &format!("select {expr} as v")).await;
        let (header, rows) = &result[0];
        assert_eq!(
            (header[0].1, header[0].2.as_str()),
            (*logical, *native),
            "{expr}"
        );
        assert_eq!(rows[0][0], *text, "{expr}");
    }
}

#[tokio::test]
async fn a_batch_returns_every_result_set() {
    let Some(conn) = connect().await else { return };
    let results = tables(
        &*conn,
        "select 1 as a; select 'x' as b, 2 as c union all select 'y', 3",
    )
    .await;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].1, [["1"]]);
    assert_eq!(
        results[1]
            .0
            .iter()
            .map(|c| c.0.as_str())
            .collect::<Vec<_>>(),
        ["b", "c"]
    );
    assert_eq!(results[1].1, [["x", "2"], ["y", "3"]]);
}

#[tokio::test]
async fn rows_affected_only_where_it_makes_sense() {
    let Some(conn) = connect().await else { return };
    let done = |events: Vec<ExecEvent>| match events.last() {
        Some(ExecEvent::Done { rows_affected }) => *rows_affected,
        other => panic!("{other:?}"),
    };
    exec_all(&*conn, "drop table if exists smoke_counts").await;
    assert_eq!(
        done(
            run(
                &*conn,
                "create table smoke_counts (a int primary key, b nvarchar(10))"
            )
            .await
            .unwrap()
        ),
        None
    );
    assert_eq!(
        done(
            run(
                &*conn,
                "insert into smoke_counts values (1, 'x'), (2, 'y'), (3, 'z')"
            )
            .await
            .unwrap()
        ),
        Some(3)
    );
    assert_eq!(
        done(
            run(&*conn, "update smoke_counts set b = 'w' where a > 1")
                .await
                .unwrap()
        ),
        Some(2)
    );
    assert_eq!(
        done(
            run(&*conn, "delete from smoke_counts where a = 99")
                .await
                .unwrap()
        ),
        Some(0)
    );
    assert_eq!(
        done(run(&*conn, "select * from smoke_counts").await.unwrap()),
        Some(3)
    );
    // A transaction spans statements: the session keeps it.
    exec_all(&*conn, "begin tran;\ndelete from smoke_counts;\nrollback").await;
    assert_eq!(
        tables(&*conn, "select count(*) as n from smoke_counts").await[0].1,
        [["3"]]
    );
    exec_all(&*conn, "drop table smoke_counts").await;
}

#[tokio::test]
async fn errors_point_at_their_line_and_the_session_survives() {
    let Some(conn) = connect().await else { return };
    let sql = "select 1 as a\nselect * from missing_table";
    match run(&*conn, sql).await {
        Err(DriverError::Database {
            code,
            message,
            position,
            ..
        }) => {
            assert_eq!(code.as_deref(), Some("208"), "{message}");
            assert!(message.contains("missing_table"), "{message}");
            assert_eq!(position, Some(15), "the start of line 2");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(tables(&*conn, "select 1 as one").await[0].1, [["1"]]);
}

#[tokio::test]
async fn cancel_stops_the_batch_and_keeps_the_session() {
    let Some(conn) = connect().await else { return };
    let conn: Arc<dyn Connection> = Arc::from(conn);
    exec_all(&*conn, "set lock_timeout 1000").await;
    let cancel = conn.cancel_handle();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel.cancel().await
    });
    let started = Instant::now();
    let result = run(&*conn, "waitfor delay '00:00:30'").await;
    canceller.await.unwrap().unwrap();
    assert!(matches!(result, Err(DriverError::Cancelled)), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    // Same session: its settings are still there.
    assert_eq!(
        tables(&*conn, "select @@lock_timeout as t").await[0].1,
        [["1000"]]
    );
    // A cancel with nothing running does not touch the next statement.
    conn.cancel_handle().cancel().await.unwrap();
    assert_eq!(tables(&*conn, "select 2 as two").await[0].1, [["2"]]);
}

/// A cancel while rows stream (the response is dropped in the middle of a packet) also
/// leaves the session in step: the next statements get their own results.
#[tokio::test]
async fn cancel_while_rows_stream() {
    let Some(conn) = connect().await else { return };
    let mut events = conn
        .execute(
            "select a.object_id, a.name, b.name as other from sys.all_objects a cross join sys.all_objects b",
            &ExecOptions { batch_size: 500 },
        )
        .await
        .unwrap();
    // Two batches in, while the server is still sending.
    let mut batches = 0;
    while let Some(event) = events.try_next().await.unwrap() {
        if matches!(event, ExecEvent::Batch(_)) {
            batches += 1;
            if batches == 2 {
                break;
            }
        }
    }
    conn.cancel_handle().cancel().await.unwrap();
    let rest: Result<Vec<ExecEvent>, DriverError> = events.try_collect().await;
    assert!(
        matches!(rest, Err(DriverError::Cancelled)),
        "{:?}",
        rest.map(|e| e.len())
    );
    for n in 1..=3 {
        assert_eq!(
            tables(&*conn, &format!("select {n} as n")).await[0].1,
            [[n.to_string()]]
        );
    }
}

#[tokio::test]
async fn tls_modes_and_login_errors() {
    let Some(base) = params("master") else { return };
    for tls in [TlsMode::Disable, TlsMode::Prefer, TlsMode::Require] {
        let conn = SqlServerDriver
            .connect(&ConnectParams {
                tls,
                ..base.clone()
            })
            .await
            .unwrap_or_else(|e| panic!("{tls:?}: {e:?}"));
        let encrypted = tables(
            &*conn,
            "select encrypt_option from sys.dm_exec_connections where session_id = @@spid",
        )
        .await;
        let expected = if tls == TlsMode::Disable {
            "FALSE"
        } else {
            "TRUE"
        };
        assert_eq!(encrypted[0].1[0][0], expected, "{tls:?}");
    }
    // The server's certificate is self-signed: verifying it fails, with the reason.
    match SqlServerDriver
        .connect(&ConnectParams {
            tls: TlsMode::VerifyFull,
            ..base.clone()
        })
        .await
    {
        Err(DriverError::Connection(message)) => {
            assert!(message.to_lowercase().contains("certificate"), "{message}")
        }
        other => panic!("{:?}", other.err()),
    }
    match SqlServerDriver
        .connect(&ConnectParams {
            password: Some(Secret::new("wrong")),
            ..base
        })
        .await
    {
        Err(DriverError::Database { code, .. }) => assert_eq!(code.as_deref(), Some("18456")),
        other => panic!("{:?}", other.err()),
    }
}

/// The database is per session, a script's `USE` moves it too, and a name never runs as SQL.
#[tokio::test]
async fn database_selection_is_per_session_and_tracks_use() {
    let Some(first) = connect().await else { return };
    let second = connect().await.unwrap();
    let databases = first.list_databases().await.unwrap();
    assert!(
        databases.contains(&"spelunk".to_string()) && databases.contains(&"master".to_string()),
        "{databases:?}"
    );
    assert_eq!(
        first.current_database().await.unwrap().as_deref(),
        Some("spelunk")
    );
    first.select_database("master").await.unwrap();
    assert_eq!(
        first.current_database().await.unwrap().as_deref(),
        Some("master")
    );
    assert_eq!(
        second.current_database().await.unwrap().as_deref(),
        Some("spelunk")
    );
    // Statements now run there.
    assert_eq!(
        tables(&*first, "select db_name() as d").await[0].1,
        [["master"]]
    );
    run(&*first, "use spelunk").await.unwrap();
    assert_eq!(
        first.current_database().await.unwrap().as_deref(),
        Some("spelunk")
    );
    assert!(
        first
            .select_database("missing]; use master; --")
            .await
            .is_err()
    );
    assert_eq!(
        first.current_database().await.unwrap().as_deref(),
        Some("spelunk")
    );
}

#[tokio::test]
async fn introspection_describes_tables() {
    let Some(conn) = connect().await else { return };
    exec_all(
        &*conn,
        "drop view if exists zoo.big_orders;
         drop table if exists zoo.orders;
         drop table if exists zoo.customer;
         if schema_id('zoo') is null exec('create schema zoo');
         create table zoo.customer (id int primary key, name nvarchar(80) not null default N'x');
         create table zoo.orders (id int, line int, customer_id int references zoo.customer (id),
                                  total decimal(12, 2), note varchar(max), primary key (id, line));
         create index orders_total on zoo.orders (total desc);
         exec('create view zoo.big_orders as select * from zoo.orders where total > 100');
         exec sp_addextendedproperty 'MS_Description', N'Every order', 'schema', 'zoo', 'table', 'orders'",
    )
    .await;
    let schemas = conn.list_schemas().await.unwrap();
    let user: Vec<_> = schemas
        .iter()
        .filter(|s| !s.system)
        .map(|s| s.name.as_str())
        .collect();
    assert!(user.contains(&"dbo") && user.contains(&"zoo"), "{user:?}");
    assert!(schemas.iter().any(|s| s.name == "sys" && s.system));

    let objects = conn.list_objects("zoo").await.unwrap();
    let listed: Vec<_> = objects.iter().map(|o| (o.name.as_str(), o.kind)).collect();
    assert_eq!(
        listed,
        [
            ("big_orders", ObjectKind::View),
            ("customer", ObjectKind::Table),
            ("orders", ObjectKind::Table)
        ]
    );
    let orders = conn.describe_table("zoo", "orders").await.unwrap();
    assert_eq!(orders.object.comment.as_deref(), Some("Every order"));
    assert_eq!(orders.primary_key, ["id", "line"]);
    let columns: Vec<_> = orders
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.native_type.as_str(), c.nullable))
        .collect();
    assert_eq!(
        columns,
        [
            ("id", "int", false),
            ("line", "int", false),
            ("customer_id", "int", true),
            ("total", "decimal(12,2)", true),
            ("note", "varchar(max)", true)
        ]
    );
    let fk = &orders.foreign_keys[0];
    assert_eq!(
        (
            fk.columns.as_slice(),
            fk.referenced_schema.as_str(),
            fk.referenced_table.as_str()
        ),
        (&["customer_id".to_string()][..], "zoo", "customer")
    );
    let index = orders
        .indexes
        .iter()
        .find(|i| i.name == "orders_total")
        .unwrap();
    assert_eq!(
        index.definition,
        "CREATE NONCLUSTERED INDEX [orders_total] ON [zoo].[orders] ([total] DESC)"
    );
    assert!(orders.indexes.iter().any(|i| i.primary));
    let customer = conn.describe_table("zoo", "customer").await.unwrap();
    assert_eq!(customer.columns[1].default.as_deref(), Some("(N'x')"));
    assert_eq!(customer.columns[1].native_type, "nvarchar(80)");
    assert!(conn.describe_table("zoo", "missing").await.is_err());
    let version = conn.server_version().await.unwrap();
    assert!(version.starts_with("16."), "{version}");
}
