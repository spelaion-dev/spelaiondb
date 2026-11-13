//! Read-only mode (ADR-008) against a real PostgreSQL (`SPELAION_TEST_PG`) and SQLite. The
//! other engines are in `agents_engines.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_core::{ReadError, ReadKind, ReadLimits, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, DriverError, Secret, Value};
use spelaion_driver_postgres::PostgresDriver;
use spelaion_driver_sqlite::SqliteDriver;

async fn pg() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let params = ConnectParams {
        host: host.into(),
        port: port.parse().unwrap(),
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: "spelaion".into(),
        ..Default::default()
    };
    let conn = PostgresDriver.connect(&params).await.unwrap();
    Some(Session::new(
        conn,
        spelaion_dialect::for_engine(PostgresDriver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    ))
}

fn limits(max_rows: usize) -> ReadLimits {
    ReadLimits {
        max_rows,
        timeout: Duration::from_secs(10),
    }
}

/// A normal (not read-only) session to prepare data.
async fn setup(session: &Session, script: &str) {
    let events: Vec<_> = session.run_script(script).collect().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, spelaion_core::CoreEvent::StatementFailed { .. })),
        "{events:?}"
    );
}

/// The session has not been left inside a transaction.
async fn not_in_transaction(session: &Session) {
    let r = session
        .run_read_only("select now() = statement_timestamp()", &limits(1))
        .await
        .unwrap();
    assert_eq!(
        r.rows[0][0],
        Value::Bool(false),
        "fixed now() = a transaction is still open"
    );
}

#[tokio::test]
async fn reads_are_capped_with_a_server_cursor() {
    let Some(session) = pg().await else { return };
    let started = Instant::now();
    let r = session
        .run_read_only("select * from big order by id;", &limits(5))
        .await
        .unwrap();
    assert_eq!(r.kind, ReadKind::Query);
    assert_eq!(r.rows.len(), 5);
    assert!(r.truncated);
    assert_eq!(r.columns[0].name, "id");
    assert_eq!(r.rows[4][0], Value::Int(5));
    // With a cursor the million rows are not transferred.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    let r = session
        .run_read_only("select 1 as uno", &limits(5))
        .await
        .unwrap();
    assert!(!r.truncated);
    not_in_transaction(&session).await;
}

#[tokio::test]
async fn explain_and_show_are_allowed() {
    let Some(session) = pg().await else { return };
    let r = session
        .run_read_only("explain select * from big where id = 1", &limits(50))
        .await
        .unwrap();
    assert_eq!(r.kind, ReadKind::Explain);
    assert!(
        matches!(&r.rows[0][0], Value::Text(plan) if plan.contains("big")),
        "{:?}",
        r.rows
    );
    let r = session
        .run_read_only("show server_version", &limits(5))
        .await
        .unwrap();
    assert_eq!(r.kind, ReadKind::Show);
}

#[tokio::test]
async fn writes_never_reach_the_server() {
    let Some(session) = pg().await else { return };
    for sql in [
        "delete from big",
        "explain analyze delete from big",
        "with d as (delete from big returning *) select count(*) from d",
        "select 1; drop table big",
    ] {
        assert!(
            matches!(
                session.run_read_only(sql, &limits(5)).await,
                Err(ReadError::Rejected(_))
            ),
            "{sql}"
        );
    }
    let r = session
        .run_read_only("select count(*) from big", &limits(1))
        .await
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1_000_000));
}

#[tokio::test]
async fn the_read_only_transaction_stops_what_the_parser_cannot_see() {
    let Some(session) = pg().await else { return };
    let seq = format!("spelaion_seq_{}", std::process::id());
    setup(&session, &format!("create sequence {seq}")).await;
    // nextval() looks like a read, but it writes: the READ ONLY transaction prevents it.
    match session
        .run_read_only(&format!("select nextval('{seq}')"), &limits(1))
        .await
    {
        Err(ReadError::Database(DriverError::Database { code, .. })) => {
            assert_eq!(code.as_deref(), Some("25006"))
        }
        other => panic!("nextval should fail in a read-only transaction: {other:?}"),
    }
    not_in_transaction(&session).await;
    setup(&session, &format!("drop sequence {seq}")).await;
}

#[tokio::test]
async fn errors_and_timeouts_leave_the_session_clean() {
    let Some(session) = pg().await else { return };
    assert!(matches!(
        session.run_read_only("select 1 / 0", &limits(1)).await,
        Err(ReadError::Database(_))
    ));
    not_in_transaction(&session).await;

    let short = ReadLimits {
        max_rows: 1,
        timeout: Duration::from_secs(1),
    };
    let started = Instant::now();
    let result = session.run_read_only("select pg_sleep(10)", &short).await;
    assert!(
        matches!(
            result,
            Err(ReadError::Timeout(_)) | Err(ReadError::Database(DriverError::Cancelled))
        ),
        "{result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    not_in_transaction(&session).await;
}

#[tokio::test]
async fn sqlite_read_only_mode() {
    let path = std::env::temp_dir().join(format!("spelaion-readonly-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let writer = SqliteDriver {
            create_if_missing: true,
        }
        .connect(&ConnectParams {
            database: path.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
        let session = Session::new(
            writer,
            spelaion_dialect::for_engine(SqliteDriver::default().engine()).expect("a dialect"),
            Arc::new(ResultStore::new()),
        );
        setup(
            &session,
            "create table t (a int); insert into t values (1), (2), (3)",
        )
        .await;
    }
    let params = ConnectParams {
        database: path.display().to_string(),
        read_only: true,
        ..Default::default()
    };
    let conn = SqliteDriver::default().connect(&params).await.unwrap();
    let session = Session::new(
        conn,
        spelaion_dialect::for_engine(SqliteDriver::default().engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );

    let r = session
        .run_read_only("select a from t order by a", &limits(2))
        .await
        .unwrap();
    assert_eq!(r.rows, [[Value::Int(1)], [Value::Int(2)]]);
    assert!(r.truncated);
    assert!(matches!(
        session
            .run_read_only("insert into t values (4)", &limits(2))
            .await,
        Err(ReadError::Rejected(_))
    ));
    assert!(matches!(
        session
            .run_read_only("attach 'x.db' as x", &limits(2))
            .await,
        Err(ReadError::Rejected(_))
    ));
    drop(session);
    let _ = std::fs::remove_file(&path);
}
