//! Agents' reads and writes (ADR-020) on MySQL and MariaDB (`SPELAION_TEST_MYSQL`,
//! `SPELAION_TEST_MARIADB`), SQL Server (`SPELAION_TEST_MSSQL`) and DuckDB (always). Each test
//! tries to get around the barriers: what the parser lets through must still be stopped by the
//! engine, and nothing may be left open after an error or a timeout.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, ReadError, ReadKind, ReadLimits, ResultStore, Session, WriteError, WriteLimits,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};
use spelaion_driver_duckdb::DuckdbDriver;
use spelaion_driver_mysql::MysqlDriver;
use spelaion_driver_sqlserver::SqlServerDriver;

fn limits(max_rows: usize) -> ReadLimits {
    ReadLimits {
        max_rows,
        timeout: Duration::from_secs(10),
    }
}

fn short() -> ReadLimits {
    ReadLimits {
        max_rows: 1,
        timeout: Duration::from_secs(1),
    }
}

fn write_limits() -> WriteLimits {
    WriteLimits {
        timeout: Duration::from_secs(2),
    }
}

/// A normal statement (not through the agents' paths) that must succeed.
async fn run(session: &Session, script: &str) {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
        "{script}: {events:?}"
    );
}

/// Straight to the driver, without splitting (a routine's body has semicolons).
async fn raw(session: &Session, sql: &str) {
    let mut events = session
        .connection()
        .execute(sql, &spelaion_driver_api::ExecOptions::default())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    while let Some(event) = events.next().await {
        event.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// The first value of a normal query, as text.
async fn first(session: &Session, sql: &str) -> String {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{sql}: {events:?}"));
    let stored = session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    Value::from_array(
        batch.column(0).as_ref(),
        0,
        stored.schema().columns[0].logical,
    )
    .unwrap()
    .to_string()
}

fn rejected<T: std::fmt::Debug>(result: Result<T, ReadError>, sql: &str) {
    assert!(
        matches!(result, Err(ReadError::Rejected(_))),
        "{sql}: {result:?}"
    );
}

fn timed_out<T: std::fmt::Debug>(result: Result<T, ReadError>, started: Instant) {
    assert!(
        matches!(
            result,
            Err(ReadError::Timeout(_))
                | Err(ReadError::Database(
                    spelaion_driver_api::DriverError::Cancelled
                ))
        ),
        "{result:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
}

async fn mysql_sessions() -> Vec<(&'static str, Session)> {
    let mut sessions = Vec::new();
    for (var, driver) in [
        ("SPELAION_TEST_MYSQL", MysqlDriver::mysql()),
        ("SPELAION_TEST_MARIADB", MysqlDriver::mariadb()),
    ] {
        let Ok(addr) = std::env::var(var) else {
            eprintln!("{var} not set: test skipped");
            continue;
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
        let session = Session::new(
            driver.connect(&params).await.unwrap(),
            driver.dialect(),
            Arc::new(ResultStore::new()),
        );
        sessions.push((var, session));
    }
    sessions
}

#[tokio::test]
async fn mysql_executable_comments_do_not_bypass_agent_barriers() {
    for (server, session) in mysql_sessions().await {
        let t = format!("agent_comments_{}", std::process::id());
        run(&session, &format!("create table {t} (id int primary key, n int); insert into {t} values (1, 1), (2, 2)")).await;
        for sql in [
            format!("delete from {t} /*!99999 where id = 1 */"),
            format!("update {t} set n = 0 /*!99999 where id = 1 */"),
            format!("delete from {t} /*M!999999 where id = 1 */"),
        ] {
            assert!(
                matches!(
                    session.run_write(&sql, &write_limits()).await,
                    Err(WriteError::Rejected(_))
                ),
                "{server}: {sql}"
            );
        }
        for sql in [
            format!("select * from {t} /*!99999 where id = 1 */"),
            format!("explain select * from {t} /*M!100000 where id = 1 */"),
            "select 1 /*M! , get_lock('agent_comment_test', 0) */".to_string(),
        ] {
            rejected(session.run_read_only(&sql, &limits(5)).await, &sql);
        }
        assert_eq!(
            first(&session, &format!("select count(*) from {t} where n = id")).await,
            "2",
            "{server}"
        );
        session
            .run_write(
                &format!("update {t} set n = 3 /* ordinary comment */ where id = 1"),
                &write_limits(),
            )
            .await
            .unwrap();
        assert_eq!(
            first(&session, &format!("select n from {t} where id = 1")).await,
            "3",
            "{server}"
        );
        run(&session, &format!("drop table {t}")).await;
    }
}

#[tokio::test]
async fn mysql_reads_are_read_only_capped_and_leave_nothing_open() {
    for (server, session) in mysql_sessions().await {
        let t = format!("agent_read_{}", std::process::id());
        run(
            &session,
            &format!(
                "drop table if exists {t}; create table {t} (id int primary key); \
                 insert into {t} values (0), (1), (2), (3), (4), (5), (6), (7), (8), (9)"
            ),
        )
        .await;

        // 100,000 rows from a cross join: only the first ones are read.
        let started = Instant::now();
        let r = session
            .run_read_only(
                &format!("select a.id from {t} a, {t} b, {t} c, {t} d, {t} e"),
                &limits(5),
            )
            .await
            .unwrap();
        assert_eq!((r.rows.len(), r.truncated), (5, true), "{server}");
        assert!(started.elapsed() < Duration::from_secs(5), "{server}");
        let r = session
            .run_read_only("show tables", &limits(500))
            .await
            .unwrap();
        assert_eq!(r.kind, ReadKind::Show, "{server}");
        let r = session
            .run_read_only(
                &format!("explain select * from {t} where id = 1"),
                &limits(5),
            )
            .await
            .unwrap();
        assert_eq!(r.kind, ReadKind::Explain, "{server}");

        for sql in [
            format!("delete from {t}"),
            format!("select * from {t} for update"),
            format!("select * into outfile '/tmp/x' from {t}"),
            "select load_file('/etc/passwd')".into(),
            "select get_lock('agents', 1)".into(),
            format!("explain analyze select * from {t}"),
        ] {
            rejected(session.run_read_only(&sql, &limits(5)).await, &sql);
        }

        // A function that writes looks like a read: the READ ONLY transaction stops it.
        let f = format!("agent_writes_{}", std::process::id());
        run(&session, &format!("drop function if exists {f}")).await;
        raw(
            &session,
            &format!(
                "create function {f}() returns int modifies sql data \
                 begin insert into {t} values (100); return 1; end"
            ),
        )
        .await;
        let result = session
            .run_read_only(&format!("select {f}()"), &limits(1))
            .await;
        assert!(
            matches!(result, Err(ReadError::Database(_))),
            "{server}: {result:?}"
        );
        assert_eq!(
            first(&session, &format!("select count(*) from {t}")).await,
            "10",
            "{server}"
        );

        // A timeout cancels the query and the session is still usable.
        let started = Instant::now();
        timed_out(
            session.run_read_only("select sleep(10)", &short()).await,
            started,
        );
        let r = session
            .run_read_only("select 1 as one", &limits(1))
            .await
            .unwrap();
        assert_eq!(r.rows, [[Value::Int(1)]], "{server}");

        run(&session, &format!("drop function {f}; drop table {t}")).await;
    }
}

#[tokio::test]
async fn mysql_writes_commit_roll_back_and_time_out() {
    for (server, session) in mysql_sessions().await {
        let t = format!("agent_write_{}", std::process::id());
        run(
            &session,
            &format!(
                "drop table if exists {t}; \
                 create table {t} (id int primary key, body varchar(20) not null)"
            ),
        )
        .await;
        let done = session
            .run_write(
                &format!("insert into {t} values (1, 'a'), (2, 'b')"),
                &write_limits(),
            )
            .await
            .unwrap();
        assert_eq!(done.rows_affected, Some(2), "{server}");
        let err = session
            .run_write(
                &format!("insert into {t} values (3, 'c'), (1, 'duplicate')"),
                &write_limits(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::Database(_)), "{server}: {err:?}");
        for sql in [
            format!("drop table {t}"),
            format!("delete from {t}"),
            format!("lock tables {t} write"),
            format!("insert into {t} select 9, load_file('/etc/passwd')"),
        ] {
            assert!(
                matches!(
                    session.run_write(&sql, &write_limits()).await,
                    Err(WriteError::Rejected(_))
                ),
                "{server}: {sql}"
            );
        }
        // Too slow: cancelled and rolled back.
        let started = Instant::now();
        let err = session
            .run_write(
                &format!("update {t} set body = concat(body, sleep(10)) where id = 1"),
                &write_limits(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::Timeout(_)), "{server}: {err:?}");
        assert!(started.elapsed() < Duration::from_secs(6), "{server}");
        assert_eq!(
            first(
                &session,
                &format!("select group_concat(body order by id) from {t}")
            )
            .await,
            "a,b",
            "{server}"
        );
        run(&session, &format!("drop table {t}")).await;
    }
}

async fn sqlserver() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELAION_TEST_MSSQL") else {
        eprintln!("SPELAION_TEST_MSSQL not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let connect = |database: &str| ConnectParams {
        host: host.into(),
        port: port.parse().unwrap(),
        user: "sa".into(),
        password: Some(Secret::new("Spelaion-test-1")),
        database: database.into(),
        tls: TlsMode::Require,
        ..Default::default()
    };
    let master = SqlServerDriver.connect(&connect("master")).await.unwrap();
    let master = Session::new(
        master,
        SqlServerDriver.dialect(),
        Arc::new(ResultStore::new()),
    );
    // Two tests may create it at once on a new server: whichever loses finds it there.
    let _: Vec<CoreEvent> = master
        .run_script("if db_id('spelaion') is null create database spelaion")
        .collect()
        .await;
    Some(Session::new(
        SqlServerDriver.connect(&connect("spelaion")).await.unwrap(),
        SqlServerDriver.dialect(),
        Arc::new(ResultStore::new()),
    ))
}

/// Ten rows built in the statement itself, so SQL Server reads no table and takes no lock.
macro_rules! mssql_digits {
    () => {
        "(values (0), (1), (2), (3), (4), (5), (6), (7), (8), (9))"
    };
}

/// Minutes of work for SQL Server: a cross join of ten billion rows, every one computed.
/// It reads no table: a scan of `sys.all_objects` locks catalog rows that other tests'
/// DDL locks too, and SQL Server then picks it as a deadlock victim (error 1205) before
/// the timeout it is there to provoke.
const MSSQL_SLOW: &str = concat!(
    "select count_big(*) from ",
    mssql_digits!(),
    " a(x) cross join ",
    mssql_digits!(),
    " b(x) cross join ",
    mssql_digits!(),
    " c(x) cross join ",
    mssql_digits!(),
    " d(x) cross join ",
    mssql_digits!(),
    " e(x) cross join ",
    mssql_digits!(),
    " f(x) cross join ",
    mssql_digits!(),
    " g(x) cross join ",
    mssql_digits!(),
    " h(x) cross join ",
    mssql_digits!(),
    " i(x) cross join ",
    mssql_digits!(),
    " j(x) where checksum(a.x, b.x, c.x, d.x, e.x, f.x, g.x, h.x, i.x, j.x) = 7"
);

#[tokio::test]
async fn sqlserver_reads_roll_back_explain_without_running_and_time_out() {
    let Some(session) = sqlserver().await else {
        return;
    };
    let t = format!("agent_read_{}", std::process::id());
    run(
        &session,
        &format!(
            "if object_id('{t}') is not null drop table {t}; \
             create table {t} (id int primary key); \
             insert into {t} values (1), (2), (3)"
        ),
    )
    .await;

    let started = Instant::now();
    let r = session
        .run_read_only(
            concat!(
                "select a.x from ",
                mssql_digits!(),
                " a(x) cross join ",
                mssql_digits!(),
                " b(x)"
            ),
            &limits(5),
        )
        .await
        .unwrap();
    assert_eq!((r.rows.len(), r.truncated), (5, true));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(first(&session, "select @@trancount").await, "0");

    // The plan, compiled under SHOWPLAN_XML: nothing runs.
    let r = session
        .run_read_only(
            &format!("explain select * from {t} where id = 1"),
            &limits(5),
        )
        .await
        .unwrap();
    assert_eq!(r.kind, ReadKind::Explain);
    assert!(
        matches!(&r.rows[0][0], Value::Text(plan) if plan.contains("ShowPlanXML")),
        "{:?}",
        r.rows
    );
    assert_eq!(
        first(
            &session,
            "select cast(sessionproperty('ansi_nulls') as int)"
        )
        .await,
        "1",
        "SHOWPLAN is off again"
    );
    let r = session
        .run_read_only(&format!("select count(*) from {t}"), &limits(5))
        .await
        .unwrap();
    assert_eq!(r.rows, [[Value::Int(3)]]);

    for sql in [
        format!("delete from {t}"),
        format!("select * into {t}_copy from {t}"),
        "exec sp_who".into(),
        "select * from openrowset('SQLNCLI', 'Server=x;Trusted_Connection=yes', 'select 1')".into(),
        format!("select * from other.spelaion.dbo.{t}"),
    ] {
        rejected(session.run_read_only(&sql, &limits(5)).await, &sql);
    }

    // A failed read and a timeout leave no transaction open.
    assert!(matches!(
        session.run_read_only("select 1 / 0", &limits(1)).await,
        Err(ReadError::Database(_))
    ));
    assert_eq!(first(&session, "select @@trancount").await, "0");
    let started = Instant::now();
    timed_out(session.run_read_only(MSSQL_SLOW, &short()).await, started);
    assert_eq!(first(&session, "select @@trancount").await, "0");
    run(&session, &format!("drop table {t}")).await;
}

#[tokio::test]
async fn sqlserver_writes_commit_roll_back_and_time_out() {
    let Some(session) = sqlserver().await else {
        return;
    };
    let t = format!("agent_write_{}", std::process::id());
    run(
        &session,
        &format!(
            "if object_id('{t}') is not null drop table {t}; \
             create table {t} (id int primary key, n bigint not null)"
        ),
    )
    .await;
    let done = session
        .run_write(
            &format!("insert into {t} values (1, 10), (2, 20)"),
            &write_limits(),
        )
        .await
        .unwrap();
    assert_eq!(done.rows_affected, Some(2));
    let err = session
        .run_write(
            &format!("insert into {t} values (3, 30), (1, 0)"),
            &write_limits(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, WriteError::Database(_)), "{err:?}");
    assert_eq!(first(&session, "select @@trancount").await, "0");
    for sql in [
        format!("drop table {t}"),
        format!("truncate table {t}"),
        "exec sp_configure 'show advanced options', 1".into(),
        format!("insert into {t} select * from other.spelaion.dbo.{t}"),
    ] {
        assert!(
            matches!(
                session.run_write(&sql, &write_limits()).await,
                Err(WriteError::Rejected(_))
            ),
            "{sql}"
        );
    }
    let started = Instant::now();
    let err = session
        .run_write(
            &format!("update {t} set n = ({MSSQL_SLOW}) where id = 1"),
            &write_limits(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, WriteError::Timeout(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(6));
    assert_eq!(first(&session, "select @@trancount").await, "0");
    assert_eq!(
        first(&session, &format!("select sum(n) from {t}")).await,
        "30"
    );
    run(&session, &format!("drop table {t}")).await;
}

async fn duckdb(database: &str, read_only: bool) -> Session {
    let driver = DuckdbDriver {
        create_if_missing: true,
    };
    let conn = driver
        .connect(&ConnectParams {
            database: database.into(),
            read_only,
            ..Default::default()
        })
        .await
        .unwrap();
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

#[tokio::test]
async fn destructive_table_ddl_preserves_existing_data() {
    let mut sessions = mysql_sessions().await;
    sessions.push(("duckdb", duckdb(":memory:", false).await));
    for (server, session) in sessions {
        let table = format!("agent_ddl_{}", std::process::id());
        run(&session, &format!("create table {table} (id int, body varchar(20)); insert into {table} values (1, 'kept'), (2, 'safe')")).await;
        for sql in [
            format!("create or replace table {table} (id int)"),
            format!("alter table {table} drop column body"),
        ] {
            assert!(
                matches!(
                    session.run_write(&sql, &write_limits()).await,
                    Err(WriteError::Rejected(_))
                ),
                "{server}: {sql}"
            );
        }
        assert_eq!(
            first(
                &session,
                &format!("select count(*) from {table} where body in ('kept', 'safe')")
            )
            .await,
            "2",
            "{server}"
        );
        session
            .run_write(
                &format!("alter table {table} add column extra int"),
                &write_limits(),
            )
            .await
            .unwrap();
        assert_eq!(
            first(
                &session,
                &format!("select count(*) from {table} where extra is null")
            )
            .await,
            "2",
            "{server}"
        );
        run(&session, &format!("drop table {table}")).await;
    }
}

#[tokio::test]
async fn mariadb_schema_replacement_preserves_tables() {
    let Ok(addr) = std::env::var("SPELAION_TEST_MARIADB") else {
        eprintln!("SPELAION_TEST_MARIADB not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let driver = MysqlDriver::mariadb();
    let conn = driver
        .connect(&ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "root".into(),
            password: Some(Secret::new("spelaion-root")),
            database: "spelaion".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()));
    let schema = format!("agent_ddl_schema_{}", std::process::id());
    run(&session, &format!("create database {schema}; create table {schema}.t (id int); insert into {schema}.t values (1)")).await;
    for sql in [
        format!("create or replace schema {schema}"),
        format!("create or replace database {schema}"),
    ] {
        assert!(
            matches!(
                session.run_write(&sql, &write_limits()).await,
                Err(WriteError::Rejected(_))
            ),
            "{sql}"
        );
    }
    assert_eq!(
        first(&session, &format!("select count(*) from {schema}.t")).await,
        "1"
    );
    run(&session, &format!("drop database {schema}")).await;
}

#[tokio::test]
async fn duckdb_reads_reach_no_files_and_roll_back() {
    let session = duckdb(":memory:", false).await;
    let csv = std::env::temp_dir().join(format!("spelaion-agents-{}.csv", std::process::id()));
    std::fs::write(&csv, "secret\nhunter2\n").unwrap();
    let csv_path = csv.display().to_string().replace('\\', "/");
    // The user can read the file; once an agent has been here, nobody in this database can.
    assert_eq!(
        first(&session, &format!("select count(*) from '{csv_path}'")).await,
        "1"
    );
    run(&session, "create table t as select * from range(10) r(i)").await;

    let started = Instant::now();
    let r = session
        .run_read_only("select * from range(10000000)", &limits(5))
        .await
        .unwrap();
    assert_eq!((r.rows.len(), r.truncated), (5, true));
    assert!(started.elapsed() < Duration::from_secs(5));
    let r = session
        .run_read_only("explain select * from t", &limits(50))
        .await
        .unwrap();
    assert_eq!(r.kind, ReadKind::Explain);

    for sql in [
        format!("select * from read_csv('{csv_path}')"),
        "select getenv('PATH')".to_string(),
        "attach 'other.duckdb' as other".into(),
        "copy t to 'out.csv'".into(),
        "install httpfs".into(),
        "set enable_external_access = true".into(),
        "explain analyze select * from t".into(),
    ] {
        rejected(session.run_read_only(&sql, &limits(5)).await, &sql);
    }
    // A file named as a table passes the parser: the engine refuses it.
    let result = session
        .run_read_only(&format!("select * from '{csv_path}'"), &limits(5))
        .await;
    assert!(matches!(result, Err(ReadError::Database(_))), "{result:?}");
    // And it cannot be turned back on, not even by the user's own statements.
    let events: Vec<CoreEvent> = session
        .run_script("set enable_external_access = true")
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
        "{events:?}"
    );

    // nextval() writes, and looks like a read: the READ ONLY transaction stops it.
    run(&session, "create sequence agent_seq").await;
    let result = session
        .run_read_only("select nextval('agent_seq')", &limits(1))
        .await;
    assert!(matches!(result, Err(ReadError::Database(_))), "{result:?}");
    assert_eq!(first(&session, "select nextval('agent_seq')").await, "1");

    // A timeout interrupts the query and leaves no transaction open.
    let started = Instant::now();
    timed_out(
        session
            .run_read_only(
                "select sum(a.i * b.i) from range(1000000) a(i), range(1000000) b(i)",
                &short(),
            )
            .await,
        started,
    );
    run(&session, "begin transaction; rollback").await;
    let _ = std::fs::remove_file(&csv);
}

#[tokio::test]
async fn duckdb_writes_commit_only_where_the_file_allows_them() {
    let path = std::env::temp_dir().join(format!("spelaion-agents-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let database = path.display().to_string();
    {
        let session = duckdb(&database, false).await;
        run(
            &session,
            "create table notes (id integer primary key, body varchar not null)",
        )
        .await;
        let done = session
            .run_write(
                "insert into notes values (1, 'a'), (2, 'b')",
                &write_limits(),
            )
            .await
            .unwrap();
        assert_eq!(done.rows_affected, Some(2));
        let err = session
            .run_write(
                "insert into notes values (3, 'c'), (1, 'dup')",
                &write_limits(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::Database(_)), "{err:?}");
        for sql in [
            "drop table notes",
            "delete from notes",
            "create table stolen as select * from read_csv('x.csv')",
            "create secret s (type s3, key_id 'x', secret 'y')",
        ] {
            assert!(
                matches!(
                    session.run_write(sql, &write_limits()).await,
                    Err(WriteError::Rejected(_))
                ),
                "{sql}"
            );
        }
        assert_eq!(first(&session, "select count(*) from notes").await, "2");
    }
    // Opened read-only (the connection does not let agents write): DuckDB refuses.
    {
        let session = duckdb(&database, true).await;
        let err = session
            .run_write("insert into notes values (5, 'e')", &write_limits())
            .await
            .unwrap_err();
        assert!(matches!(err, WriteError::Database(_)), "{err:?}");
        let r = session
            .run_read_only("select count(*) from notes", &limits(1))
            .await
            .unwrap();
        assert_eq!(r.rows, [[Value::Int(2)]]);
    }
    let _ = std::fs::remove_file(&path);
}
