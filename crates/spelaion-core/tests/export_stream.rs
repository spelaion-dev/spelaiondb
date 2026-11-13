//! Streaming exports (ADR-034) on every engine: PostgreSQL (`SPELAION_TEST_PG`), MySQL and
//! MariaDB (`SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB`), SQL Server (`SPELAION_TEST_MSSQL`),
//! and DuckDB and SQLite (always). Each engine streams a query and a table data tab to files,
//! is cancelled half way (no file is left), and is tried with writes that must never reach the
//! server through the export path, nor through the engine's own read barrier.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    CoreEvent, CsvOptions, DEFAULT_BATCH_ROWS, ExportCancel, ExportError, ExportFormat,
    ExportProgress, ResultStore, Session, StreamExport, TableSource, TextEncoding, export_query,
    export_table, export_tables, rerun_check,
};
use spelaion_driver_api::{ConnectParams, Driver, ExecOptions, Secret, TlsMode, Value};
use spelaion_driver_duckdb::DuckdbDriver;
use spelaion_driver_mysql::MysqlDriver;
use spelaion_driver_postgres::PostgresDriver;
use spelaion_driver_sqlite::SqliteDriver;
use spelaion_driver_sqlserver::SqlServerDriver;

mod support;
use support::{cleaning_up, run_cleanup};

/// Rows of the test table: more than two driver batches (10,000 rows each).
const ROWS: usize = 25_000;

struct Engine {
    label: &'static str,
    session: Session,
    schema: &'static str,
    table: String,
    /// Fills `table` (`id`, `name`, `note`) with `ROWS` rows.
    fill: Vec<String>,
    /// A query that streams rows for a long time, without a table.
    endless: String,
    /// A query of exactly a million rows, without a table.
    million: String,
    /// Writes that must be refused before reaching the server.
    writes: Vec<String>,
    /// Reads that write behind the classifier's back; the engine's barrier must stop them.
    hidden_writes: Vec<(Vec<String>, String)>,
    /// On a shared server: a session of its own (the last export cancels `session`) and the
    /// statements that drop what `fill` and `hidden_writes` made, run even when the test fails.
    cleanup: Option<(Session, Vec<String>)>,
}

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
        .execute(sql, &ExecOptions::default())
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    while let Some(event) = events.next().await {
        event.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

async fn count(session: &Session, table: &str) -> i64 {
    let events: Vec<CoreEvent> = session
        .run_script(&format!("select count(*) from {table}"))
        .collect()
        .await;
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("count {table}: {events:?}"));
    let stored = session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    match Value::from_array(
        batch.column(0).as_ref(),
        0,
        stored.schema().columns[0].logical,
    )
    .unwrap()
    {
        Value::Int(n) => n,
        other => other.to_string().parse().unwrap(),
    }
}

fn csv(escape_formulas: bool) -> StreamExport {
    StreamExport {
        format: ExportFormat::Csv,
        header: true,
        csv: CsvOptions {
            delimiter: ';',
            null_text: "NULL".into(),
            escape_formulas,
        },
        encoding: TextEncoding::Utf8,
        columns: None,
        batch_size: DEFAULT_BATCH_ROWS,
    }
}

fn folder(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spelaion-export-{}-{}",
        label.to_lowercase().replace(' ', "-"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn part(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".part");
    PathBuf::from(name)
}

fn nothing(_: ExportProgress) {}

async fn exercise(engine: Engine) {
    let Engine {
        label,
        session,
        schema,
        table,
        fill,
        endless,
        million,
        writes,
        hidden_writes,
        cleanup,
    } = engine;
    // `session` moves into the body, so it is closed before the cleanup runs on its own session.
    let body = async move {
        let dir = folder(label);
        for statement in &fill {
            raw(&session, statement).await;
        }
        run(
            &session,
            &format!("update {table} set name = '=1+2' where id = 1"),
        )
        .await;
        run(
            &session,
            &format!("update {table} set name = 'a;b \"q\"' where id = 2"),
        )
        .await;
        let safe = Safeguards::default();

        // 1. A query, every row, with the CSV options and formula escaping.
        let path = dir.join("query.csv");
        let reported = AtomicU64::new(0);
        let progress = |p: ExportProgress| reported.store(p.rows, Ordering::Relaxed);
        let summary = export_query(
            &session,
            &format!("select id, name, note from {table} order by id;"),
            &safe,
            false,
            &path,
            &csv(true),
            &ExportCancel::new(),
            &progress,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(summary.rows, ROWS as u64, "{label}");
        assert_eq!(reported.load(Ordering::Relaxed), ROWS as u64, "{label}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(summary.bytes, text.len() as u64, "{label}");
        assert!(
            text.starts_with("id;name;note\r\n1;'=1+2;x\r\n2;\"a;b \"\"q\"\"\";NULL\r\n3;n3;x\r\n"),
            "{label}: {}",
            &text[..80.min(text.len())]
        );
        assert!(
            text.ends_with(&format!("{ROWS};n{ROWS};NULL\r\n")),
            "{label}"
        );
        assert_eq!(text.matches("\r\n").count(), ROWS + 1, "{label}");
        assert!(!part(&path).exists(), "{label}");

        // 1b. A million rows in batches of 1,000: the export never holds more than one batch.
        let path = dir.join("million.csv");
        let small = StreamExport {
            batch_size: 1_000,
            ..csv(true)
        };
        let last = AtomicU64::new(0);
        let reports = AtomicU64::new(0);
        let progress = |p: ExportProgress| {
            last.store(p.rows, Ordering::Relaxed);
            reports.fetch_add(1, Ordering::Relaxed);
        };
        let summary = export_query(
            &session,
            &million,
            &safe,
            false,
            &path,
            &small,
            &ExportCancel::new(),
            &progress,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(summary.rows, 1_000_000, "{label}");
        assert_eq!(summary.peak_rows, 1_000, "{label}: rows held at once");
        assert_eq!(last.load(Ordering::Relaxed), 1_000_000, "{label}");
        assert!(reports.load(Ordering::Relaxed) >= 1, "{label}");
        let lines =
            std::io::BufRead::lines(std::io::BufReader::new(std::fs::File::open(&path).unwrap()))
                .count();
        assert_eq!(lines, 1_000_001, "{label}");
        assert_eq!(
            summary.bytes,
            std::fs::metadata(&path).unwrap().len(),
            "{label}"
        );
        std::fs::remove_file(&path).unwrap();

        // 2. A table data tab: its filter and order, every page, chosen columns, TSV.
        let tab = TableSource {
            schema: schema.into(),
            name: table.clone(),
            filter: "id <= 100".into(),
            order_by: "id desc".into(),
        };
        let tsv = StreamExport {
            format: ExportFormat::Tsv,
            header: true,
            csv: CsvOptions::default(),
            encoding: TextEncoding::Utf8,
            columns: Some(vec![1, 0, 99]),
            batch_size: DEFAULT_BATCH_ROWS,
        };
        let path = dir.join("tab.tsv");
        let summary = export_table(&session, &tab, &path, &tsv, &ExportCancel::new(), &nothing)
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(summary.rows, 100, "{label}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("name\tid\nn100\t100\nn99\t99\n"),
            "{label}: {text:.60}"
        );
        assert!(text.ends_with("=1+2\t1\n"), "{label}: unescaped when asked");
        // A filter that tries to change the query is refused, and nothing is written.
        let sneaky = TableSource {
            filter: "1=1) union select * from secrets where (1".into(),
            ..tab.clone()
        };
        let path = dir.join("sneaky.tsv");
        assert!(matches!(
            export_table(
                &session,
                &sneaky,
                &path,
                &tsv,
                &ExportCancel::new(),
                &nothing
            )
            .await,
            Err(ExportError::Table(_))
        ));
        assert!(!path.exists());
        // SQL INSERT names the exported table.
        let path = dir.join("tab.sql");
        let inserts = StreamExport {
            format: ExportFormat::SqlInsert {
                table: "ignored".into(),
            },
            ..csv(false)
        };
        export_table(
            &session,
            &tab,
            &path,
            &inserts,
            &ExportCancel::new(),
            &nothing,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        let sql = std::fs::read_to_string(&path).unwrap();
        assert_eq!(sql.lines().count(), 100, "{label}");
        assert!(sql.starts_with("INSERT INTO "), "{label}");
        assert!(sql.lines().next().unwrap().contains(&table), "{label}");
        assert!(!sql.contains("ignored"), "{label}");

        // 3. Explorer tables into a folder: never over an existing file.
        let tables_dir = dir.join("tables");
        std::fs::create_dir_all(&tables_dir).unwrap();
        std::fs::write(tables_dir.join(format!("{table}.csv")), "keep").unwrap();
        let files = export_tables(
            &session,
            &[(schema.to_string(), table.clone())],
            &tables_dir,
            "csv",
            &csv(true),
            &ExportCancel::new(),
            &|_, _| {},
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, tables_dir.join(format!("{table} (2).csv")));
        assert_eq!(files[0].summary.rows, ROWS as u64);
        assert_eq!(
            std::fs::read_to_string(tables_dir.join(format!("{table}.csv"))).unwrap(),
            "keep"
        );

        // 4. A connection type that confirms execution asks before running the query again.
        let confirming = Safeguards {
            confirm_execution: true,
            ..Safeguards::default()
        };
        let path = dir.join("confirm.csv");
        let sql = format!("select id from {table}");
        assert!(
            matches!(
                export_query(
                    &session,
                    &sql,
                    &confirming,
                    false,
                    &path,
                    &csv(true),
                    &ExportCancel::new(),
                    &nothing
                )
                .await,
                Err(ExportError::NeedsConfirmation)
            ),
            "{label}"
        );
        assert!(!path.exists());
        export_query(
            &session,
            &sql,
            &confirming,
            true,
            &path,
            &csv(true),
            &ExportCancel::new(),
            &nothing,
        )
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));

        // 5. Writes never get through the export path, and nothing is written to disk.
        assert_eq!(
            rerun_check(&session, Some(&format!("select * from {table}"))).refused,
            None,
            "{label}"
        );
        for sql in &writes {
            // The dialog says why before offering it: only the loaded rows can be exported.
            assert!(
                rerun_check(&session, Some(sql)).refused.is_some(),
                "{label}: {sql}"
            );
            let path = dir.join("write.csv");
            let result = export_query(
                &session,
                sql,
                &safe,
                true,
                &path,
                &csv(true),
                &ExportCancel::new(),
                &nothing,
            )
            .await;
            assert!(
                matches!(result, Err(ExportError::NotARead(_))),
                "{label}: {sql}: {result:?}"
            );
            assert!(!path.exists() && !part(&path).exists(), "{label}: {sql}");
        }
        // What the classifier cannot see (a function that writes) is stopped by the engine.
        for (setup, sql) in &hidden_writes {
            for statement in setup {
                raw(&session, statement).await;
            }
            let path = dir.join("hidden.csv");
            let result = export_query(
                &session,
                sql,
                &safe,
                true,
                &path,
                &csv(true),
                &ExportCancel::new(),
                &nothing,
            )
            .await;
            assert!(
                matches!(result, Err(ExportError::Database(_))),
                "{label}: {sql}: {result:?}"
            );
            assert!(!path.exists() && !part(&path).exists(), "{label}: {sql}");
        }
        assert_eq!(count(&session, &table).await, ROWS as i64, "{label}");

        // 6. Cancel half way: no file and no temporary file are left, and it stops quickly.
        let path = dir.join("endless.csv");
        let cancel = ExportCancel::new();
        cancel.attach(&session);
        let seen = AtomicU64::new(0);
        let progress = |p: ExportProgress| seen.store(p.rows, Ordering::Relaxed);
        let canceller = async {
            let waiting = Instant::now();
            while seen.load(Ordering::Relaxed) == 0 && waiting.elapsed() < Duration::from_secs(30) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancel.cancel().await;
            Instant::now()
        };
        let export = csv(true);
        let exporting = export_query(
            &session, &endless, &safe, false, &path, &export, &cancel, &progress,
        );
        let (result, cancelled_at) = tokio::time::timeout(Duration::from_secs(90), async {
            tokio::join!(exporting, canceller)
        })
        .await
        .unwrap_or_else(|_| panic!("{label}: the cancel did not stop the export"));
        assert!(
            matches!(result, Err(ExportError::Cancelled)),
            "{label}: {result:?}"
        );
        assert!(
            cancelled_at.elapsed() < Duration::from_secs(1),
            "{label}: stopped {:?} after the cancel",
            cancelled_at.elapsed()
        );
        assert!(
            seen.load(Ordering::Relaxed) > 0,
            "{label}: rows were written"
        );
        assert!(!path.exists() && !part(&path).exists(), "{label}");
        // A cancel before the export starts stops it before anything is sent.
        let early = ExportCancel::new();
        early.cancel().await;
        let export = csv(true);
        let result = export_query(
            &session, "select 1", &safe, false, &path, &export, &early, &nothing,
        )
        .await;
        assert!(
            matches!(result, Err(ExportError::Cancelled)),
            "{label}: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    };
    cleaning_up(body, move |panicked| async move {
        if let Some((admin, statements)) = &cleanup {
            run_cleanup(label, admin, statements, panicked).await;
        }
    })
    .await;
}

#[tokio::test]
async fn postgres_streams_exports() {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return;
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
        PostgresDriver.connect(&params).await.unwrap(),
        PostgresDriver.dialect(),
        Arc::new(ResultStore::new()),
    );
    let t = format!("export_stream_{}", std::process::id());
    exercise(Engine {
        label: "PostgreSQL",
        session,
        schema: "public",
        table: t.clone(),
        fill: vec![
            format!("drop table if exists {t}"),
            format!("create table {t} (id int primary key, name text, note text)"),
            format!(
                "insert into {t} select g, 'n' || g, case when g % 2 = 0 then null else 'x' end \
                 from generate_series(1, {ROWS}) g"
            ),
        ],
        endless: "select generate_series(1, 2000000000) as n".into(),
        million: "select g, 'row ' || g from generate_series(1, 1000000) g".into(),
        writes: vec![
            format!("delete from {t}"),
            format!("update {t} set name = 'x'"),
            format!("insert into {t} values (-1, 'a', 'b') returning *"),
            format!("delete from {t} where id = 1 returning *"),
            "call export_missing_procedure()".into(),
            format!("select 1; delete from {t}"),
            format!("select * from {t}; drop table {t}"),
            format!("with gone as (delete from {t} returning *) select * from gone"),
            format!("with x as (update {t} set name = 'y' returning *) select * from x"),
            format!(
                "with x as (insert into {t} values (-1, 'a', 'b') returning *) select * from x"
            ),
            format!("select * into export_copy from {t}"),
            format!("select * from {t} for update"),
            format!("create table export_copy as select * from {t}"),
            format!("explain analyze delete from {t}"),
            format!("copy {t} to stdout"),
            format!("truncate {t}"),
            "select pg_terminate_backend(pg_backend_pid())".into(),
            "select set_config('search_path', 'x', false)".into(),
            format!("do $$ begin delete from {t}; end $$"),
        ],
        hidden_writes: vec![(
            vec![format!(
                "create or replace function export_wipe_{t}() returns int language sql as \
                 'delete from {t}; select 1'"
            )],
            format!("select export_wipe_{t}()"),
        )],
        cleanup: Some((
            Session::new(
                PostgresDriver.connect(&params).await.unwrap(),
                PostgresDriver.dialect(),
                Arc::new(ResultStore::new()),
            ),
            vec![
                format!("drop function if exists export_wipe_{t}()"),
                format!("drop table if exists {t}"),
            ],
        )),
    })
    .await;
}

/// Digits 0–9 as a derived table, to make many rows on MySQL without a recursion limit.
fn digits(alias: &str) -> String {
    format!(
        "(select 0 d union all select 1 union all select 2 union all select 3 union all \
         select 4 union all select 5 union all select 6 union all select 7 union all \
         select 8 union all select 9) {alias}"
    )
}

#[tokio::test]
async fn mysql_and_mariadb_stream_exports() {
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
        let t = format!("export_stream_{}", std::process::id());
        let numbers = format!(
            "select a.d + b.d * 10 + c.d * 100 + d.d * 1000 + e.d * 10000 + 1 as n from {}, {}, {}, {}, {}",
            digits("a"),
            digits("b"),
            digits("c"),
            digits("d"),
            digits("e")
        );
        exercise(Engine {
            label: if var == "SPELAION_TEST_MYSQL" { "MySQL" } else { "MariaDB" },
            session,
            schema: "spelaion",
            table: t.clone(),
            fill: vec![
                format!("drop table if exists {t}"),
                format!("create table {t} (id int primary key, name varchar(100), note varchar(10))"),
                format!(
                    "insert into {t} select n, concat('n', n), case when n % 2 = 0 then null else 'x' end \
                     from ({numbers}) s where n <= {ROWS}"
                ),
                format!("drop function if exists export_wipe_{t}"),
            ],
            million: format!(
                "select a.d + b.d * 10 + c.d * 100 + d.d * 1000 + e.d * 10000 + f.d * 100000 as n \
                 from {}, {}, {}, {}, {}, {}",
                digits("a"),
                digits("b"),
                digits("c"),
                digits("d"),
                digits("e"),
                digits("f")
            ),
            // Streams at once (derived tables of digits are materialized first on MySQL 8).
            endless: format!(
                "select a.table_name, b.table_name, c.d from information_schema.columns a                  cross join information_schema.columns b cross join {}",
                digits("c")
            ),
            writes: vec![
                format!("delete from {t}"),
                format!("select 1; delete from {t}"),
                format!("select * from {t} into outfile '/tmp/export_{t}'"),
                format!("select * from {t} /*!50000 into outfile '/tmp/export2_{t}' */"),
                format!("select * from {t} /*M! into outfile '/tmp/export3_{t}' */"),
                format!("select * from {t} for update"),
                format!("select * from {t} lock in share mode"),
                "select load_file('/etc/passwd')".into(),
                "select get_lock('export', 1)".into(),
                format!("create table export_copy select * from {t}"),
                format!("replace into {t} values (1, 'a', 'b')"),
                format!("call export_proc_{t}()"),
            ],
            hidden_writes: vec![(
                vec![format!(
                    "create function export_wipe_{t}() returns int modifies sql data \
                     begin delete from {t}; return 1; end"
                )],
                format!("select export_wipe_{t}()"),
            )],
            cleanup: Some((
                Session::new(
                    driver.connect(&params).await.unwrap(),
                    driver.dialect(),
                    Arc::new(ResultStore::new()),
                ),
                vec![
                    format!("drop function if exists export_wipe_{t}"),
                    format!("drop table if exists {t}"),
                ],
            )),
        })
        .await;
    }
}

#[tokio::test]
async fn sqlserver_streams_exports() {
    let Ok(addr) = std::env::var("SPELAION_TEST_MSSQL") else {
        eprintln!("SPELAION_TEST_MSSQL not set: test skipped");
        return;
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
    let master = Session::new(
        SqlServerDriver.connect(&connect("master")).await.unwrap(),
        SqlServerDriver.dialect(),
        Arc::new(ResultStore::new()),
    );
    let _: Vec<CoreEvent> = master
        .run_script("if db_id('spelaion') is null create database spelaion")
        .collect()
        .await;
    let session = Session::new(
        SqlServerDriver.connect(&connect("spelaion")).await.unwrap(),
        SqlServerDriver.dialect(),
        Arc::new(ResultStore::new()),
    );
    let t = format!("export_stream_{}", std::process::id());
    exercise(Engine {
        label: "SQL Server",
        session,
        schema: "dbo",
        table: t.clone(),
        fill: vec![
            format!("if object_id('{t}') is not null drop table {t}"),
            format!("create table {t} (id int primary key, name nvarchar(100), note nvarchar(10))"),
            format!(
                "insert into {t} select n, concat('n', n), case when n % 2 = 0 then null else 'x' end \
                 from (select top {ROWS} row_number() over (order by (select null)) as n \
                 from sys.all_objects a cross join sys.all_objects b) s"
            ),
        ],
        endless: "select a.object_id, b.object_id, c.object_id from sys.all_objects a \
                  cross join sys.all_objects b cross join sys.all_objects c"
            .into(),
        million: "select top 1000000 a.object_id, b.name from sys.all_objects a \
                  cross join sys.all_objects b"
            .into(),
        writes: vec![
            format!("delete from {t}"),
            format!("select 1; delete from {t}"),
            format!("select 1\ngo\ndelete from {t}"),
            format!("select * into #export_copy from {t}"),
            format!("select * into export_copy from {t}"),
            format!("with x as (select 1 as a) delete from {t}"),
            format!("begin delete from {t} end"),
            format!("exec sp_executesql N'delete from {t}'"),
            format!("truncate table {t}"),
            "select * from openrowset('SQLNCLI', 'x', 'select 1')".into(),
            format!("select * from otherserver.spelaion.dbo.{t}"),
            format!("merge {t} as d using (select 1 as id) s on d.id = s.id when matched then delete;"),
        ],
        // T-SQL functions cannot write: the rolled-back transaction is checked by the agents' tests.
        hidden_writes: Vec::new(),
        cleanup: Some((
            Session::new(
                SqlServerDriver.connect(&connect("spelaion")).await.unwrap(),
                SqlServerDriver.dialect(),
                Arc::new(ResultStore::new()),
            ),
            vec![format!("drop table if exists {t}")],
        )),
    })
    .await;
}

#[tokio::test]
async fn duckdb_streams_exports() {
    let dir = folder("duckdb-file");
    let path = dir.join("export.duckdb");
    let driver = DuckdbDriver {
        create_if_missing: true,
    };
    let conn = driver
        .connect(&ConnectParams {
            database: path.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()));
    let t = "export_stream".to_string();
    exercise(Engine {
        label: "DuckDB",
        session,
        schema: "main",
        table: t.clone(),
        fill: vec![
            format!("create table {t} (id integer primary key, name varchar, note varchar)"),
            format!(
                "insert into {t} select i, 'n' || i, case when i % 2 = 0 then null else 'x' end \
                 from range(1, {}) r(i)",
                ROWS + 1
            ),
        ],
        endless: "select * from range(4000000000) r(i)".into(),
        million: "select i, 'row ' || i from range(1000000) r(i)".into(),
        writes: vec![
            format!("delete from {t}"),
            format!("select 1; delete from {t}"),
            format!("copy {t} to 'out.csv'"),
            "select * from read_csv('x.csv')".into(),
            "attach 'other.duckdb'".into(),
            "install httpfs".into(),
            format!("create table export_copy as select * from {t}"),
            format!("with x as (select 1) delete from {t}"),
        ],
        // A macro cannot write: the read-only transaction is checked by the agents' tests.
        hidden_writes: Vec::new(),
        // The file goes with its folder.
        cleanup: None,
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn sqlite_streams_exports() {
    let dir = folder("sqlite-file");
    let path = dir.join("export.db");
    std::fs::File::create(&path).unwrap();
    let conn = SqliteDriver::default()
        .connect(&ConnectParams {
            database: path.display().to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(
        conn,
        SqliteDriver::default().dialect(),
        Arc::new(ResultStore::new()),
    );
    let t = "export_stream".to_string();
    exercise(Engine {
        label: "SQLite",
        session,
        schema: "main",
        table: t.clone(),
        fill: vec![
            format!("create table {t} (id integer primary key, name text, note text)"),
            format!(
                "insert into {t} with recursive s(n) as (select 1 union all select n + 1 from s where n < {ROWS}) \
                 select n, 'n' || n, case when n % 2 = 0 then null else 'x' end from s"
            ),
        ],
        endless: "with recursive s(n) as (select 1 union all select n + 1 from s) select n from s"
            .into(),
        million: "with recursive s(n) as (select 1 union all select n + 1 from s where n < 1000000) \
                  select n, 'row ' || n from s"
            .into(),
        writes: vec![
            format!("delete from {t}"),
            format!("select 1; delete from {t}"),
            format!("with x as (select 1) delete from {t}"),
            "select writefile('x', 'y')".into(),
            "select load_extension('x')".into(),
            "attach 'other.db' as other".into(),
            "pragma writable_schema = on".into(),
            "vacuum into 'copy.db'".into(),
            format!("insert into {t} select * from {t}"),
        ],
        hidden_writes: Vec::new(),
        // The file goes with its folder.
        cleanup: None,
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A tab with an open transaction: the export dialog warns that its uncommitted changes will
/// not be in the file, and indeed the export's own session does not see them.
#[tokio::test]
async fn postgres_warns_that_an_open_transaction_is_not_exported() {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return;
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
    let open = || async {
        Session::new(
            PostgresDriver.connect(&params).await.unwrap(),
            PostgresDriver.dialect(),
            Arc::new(ResultStore::new()),
        )
    };
    let tab = open().await;
    tab.follow_transactions(&Safeguards::default()).await;
    let t = format!("export_tx_{}", std::process::id());
    let cleanup = vec![
        // A failed test may leave the tab's transaction open, holding the table.
        "rollback".to_string(),
        format!("drop table if exists {t}"),
    ];
    let body = async {
        run(
            &tab,
            &format!(
                "drop table if exists {t}; create table {t} (id int); insert into {t} values (1)"
            ),
        )
        .await;
        let sql = format!("select id from {t} order by id");
        assert_eq!(
            rerun_check(&tab, Some(&sql)),
            spelaion_core::RerunCheck {
                refused: None,
                open_transaction: false
            }
        );
        run(&tab, &format!("begin; insert into {t} values (2)")).await;
        let check = rerun_check(&tab, Some(&sql));
        assert!(check.open_transaction, "{check:?}");
        assert_eq!(check.refused, None);
        // The export's session sees committed rows only.
        let export_session = open().await;
        let dir = folder("pg-transaction");
        let path = dir.join("tx.csv");
        export_query(
            &export_session,
            &sql,
            &Safeguards::default(),
            false,
            &path,
            &csv(false),
            &ExportCancel::new(),
            &nothing,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "id\r\n1\r\n");
        run(&tab, "rollback").await;
        assert!(!rerun_check(&tab, Some(&sql)).open_transaction);
        let _ = std::fs::remove_dir_all(&dir);
    };
    cleaning_up(body, |panicked| {
        run_cleanup("PostgreSQL", &tab, &cleanup, panicked)
    })
    .await;
}

/// The export's session finds unqualified names where the tab does: the tab's schema on
/// PostgreSQL and DuckDB, its database on MySQL and MariaDB.
#[tokio::test]
async fn the_export_session_follows_the_tabs_schema() {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return;
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
    let open = || async {
        Session::new(
            PostgresDriver.connect(&params).await.unwrap(),
            PostgresDriver.dialect(),
            Arc::new(ResultStore::new()),
        )
    };
    let tab = open().await;
    let schema = format!("export_ctx_{}", std::process::id());
    let cleanup = vec![format!("drop schema if exists {schema} cascade")];
    let body = async {
        run(
            &tab,
            &format!(
                "drop schema if exists {schema} cascade; create schema {schema}; \
                 create table {schema}.only_here (id int); insert into {schema}.only_here values (7)"
            ),
        )
        .await;
        spelaion_core::set_active_schema(&tab, &schema)
            .await
            .unwrap();
        let export = open().await;
        spelaion_core::follow_session_context(&tab, &export)
            .await
            .unwrap();
        let dir = folder("pg-context");
        let path = dir.join("ctx.csv");
        export_query(
            &export,
            "select id from only_here",
            &Safeguards::default(),
            false,
            &path,
            &csv(false),
            &ExportCancel::new(),
            &nothing,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "id\r\n7\r\n");
        // A tab still running a statement is not waited for: the export says why at once.
        let busy = tab.run_script("select pg_sleep(8)");
        let sleeping = tokio::spawn(async move { busy.collect::<Vec<_>>().await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let asked = Instant::now();
        let refused = spelaion_core::follow_session_context(&tab, &export).await;
        assert!(
            matches!(&refused, Err(message) if message.contains("still running")),
            "{refused:?}"
        );
        assert!(
            asked.elapsed() < Duration::from_secs(6),
            "{:?}",
            asked.elapsed()
        );
        sleeping.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    };
    cleaning_up(body, |panicked| {
        run_cleanup("PostgreSQL", &tab, &cleanup, panicked)
    })
    .await;
}
