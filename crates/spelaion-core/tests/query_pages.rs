//! Query result pages (ADR-054) on every engine: a query is paged only when each page can be
//! fetched exactly, and then every row that exists throughout appears once, in order, while
//! another session deletes and inserts between pages and the ORDER BY has ties everywhere. A
//! query that is not paged runs as written. SQLite always runs; the servers need
//! `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`, `SPELAION_TEST_MARIADB` and `SPELAION_TEST_MSSQL`
//! (skipped without them, which does not count as tested).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultId, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};
use spelaion_driver_mysql::MysqlDriver;
use spelaion_driver_postgres::PostgresDriver;
use spelaion_driver_sqlite::SqliteDriver;
use spelaion_driver_sqlserver::SqlServerDriver;

mod support;
use support::{cleaning_up, run_cleanup};

const ROWS: i64 = 3000;
const PAGE: u64 = 250;

/// Runs `sql`; panics on an error.
async fn run(label: &str, session: &Session, sql: &str) {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    for event in &events {
        if let CoreEvent::StatementFailed { error, .. } = event {
            panic!("{label}: {sql}: {}", error.message);
        }
    }
}

/// Runs `sql` and returns its result, which stays in the session's store.
async fn result(label: &str, session: &Session, sql: &str) -> ResultId {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let mut found = None;
    for event in events {
        match event {
            CoreEvent::ResultStarted { result, .. } => found = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{label}: {sql}: {}", error.message),
            _ => {}
        }
    }
    found.unwrap_or_else(|| panic!("{label}: {sql}: no result"))
}

/// Every value of a result's column, as text.
fn column(session: &Session, id: ResultId, column: usize) -> Vec<String> {
    let stored = session.results().get(id).unwrap();
    let stored = stored.read().unwrap();
    (0..stored.rows())
        .map(|row| match stored.value_at(row, column).unwrap() {
            Value::Text(text) => text,
            other => other.to_string(),
        })
        .collect()
}

/// The first value of `sql`, as text.
async fn first(label: &str, session: &Session, sql: &str) -> String {
    let id = result(label, session, sql).await;
    let value = column(session, id, 0)
        .into_iter()
        .next()
        .unwrap_or_default();
    session.results().remove(id);
    value
}

/// How to ask the server what a session last ran: the statement that gives the session's id
/// on the server, and the statement (from another session) that reads what it ran.
type Received = (&'static str, fn(&str) -> String);

/// An engine to test on: two sessions (one reads pages, the other writes between them), how
/// to fill the table, and how to read the last statement the reading session sent.
struct Engine {
    label: &'static str,
    reader: Session,
    writer: Session,
    fill: String,
    /// The text of the statements the server last received from the reader, if this engine
    /// can tell (the argument is the reader's session id on the server).
    received: Option<Received>,
}

fn session(driver: &dyn Driver, connection: Box<dyn spelaion_driver_api::Connection>) -> Session {
    Session::new(connection, driver.dialect(), Arc::new(ResultStore::new()))
}

async fn connect(driver: &dyn Driver, params: &ConnectParams) -> Session {
    session(driver, driver.connect(params).await.unwrap())
}

fn server(var: &str, user: &str, password: &str) -> Option<ConnectParams> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: that engine is skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    Some(ConnectParams {
        host: host.into(),
        port: port.parse().unwrap(),
        user: user.into(),
        password: Some(Secret::new(password)),
        database: "spelaion".into(),
        ..Default::default()
    })
}

async fn engines(table: &str, sqlite_file: &std::path::Path) -> Vec<Engine> {
    let mut out = Vec::new();
    if let Some(params) = server("SPELAION_TEST_PG", "spelaion", "spelaion") {
        out.push(Engine {
            label: "postgres",
            reader: connect(&PostgresDriver, &params).await,
            writer: connect(&PostgresDriver, &params).await,
            fill: format!(
                "insert into {table} select g, g % 3, 'v' || g from generate_series(1, {ROWS}) g"
            ),
            received: Some(("select pg_backend_pid()", |pid| {
                format!("select query from pg_stat_activity where pid = {pid}")
            })),
        });
    }
    for (var, engine, driver) in [
        ("SPELAION_TEST_MYSQL", "mysql", MysqlDriver::mysql()),
        ("SPELAION_TEST_MARIADB", "mariadb", MysqlDriver::mariadb()),
    ] {
        if let Some(params) = server(var, "root", "spelaion-root") {
            out.push(Engine {
                label: engine,
                reader: connect(&driver, &params).await,
                writer: connect(&driver, &params).await,
                fill: format!(
                    // Both stop a recursive CTE at 1000 rows here, each with its own setting.
                    "{}insert into {table} with recursive s(n) as (select 1 union all select n + 1 from s where n < {ROWS}) select n, n % 3, concat('v', n) from s",
                    if engine == "mysql" {
                        "set session cte_max_recursion_depth = 10000; "
                    } else {
                        "set session max_recursive_iterations = 10000; "
                    }
                ),
                // MariaDB's performance schema is off by default.
                received: (engine == "mysql").then_some((
                    "select connection_id()",
                    (|id: &str| {
                        format!(
                            "select group_concat(sql_text separator '\n') from performance_schema.events_statements_history h join performance_schema.threads t on t.thread_id = h.thread_id where t.processlist_id = {id}"
                        )
                    }) as fn(&str) -> String,
                )),
            });
        }
    }
    if let Some(mut params) = server("SPELAION_TEST_MSSQL", "sa", "Spelaion-test-1") {
        params.tls = TlsMode::Require;
        params.database = "master".into();
        let master = connect(&SqlServerDriver, &params).await;
        run(
            "sqlserver",
            &master,
            "if db_id('spelaion') is null create database spelaion",
        )
        .await;
        params.database = "spelaion".into();
        out.push(Engine {
            label: "sqlserver",
            reader: connect(&SqlServerDriver, &params).await,
            writer: connect(&SqlServerDriver, &params).await,
            fill: format!(
                "insert into {table} select n, n % 3, concat('v', n) from (select top ({ROWS}) row_number() over (order by (select null)) as n from sys.all_objects a cross join sys.all_objects b) s"
            ),
            received: None,
        });
    }
    let lite = ConnectParams {
        database: sqlite_file.display().to_string(),
        ..Default::default()
    };
    let driver = SqliteDriver::default();
    out.push(Engine {
        label: "sqlite",
        reader: connect(&driver, &lite).await,
        writer: connect(&driver, &lite).await,
        fill: format!(
            "insert into {table} with recursive s(n) as (select 1 union all select n + 1 from s where n < {ROWS}) select n, n % 3, 'v' || n from s"
        ),
        received: None,
    });
    out
}

#[tokio::test]
async fn pages_show_every_row_that_existed_throughout_exactly_once_while_others_write() {
    let table = format!("query_pages_{}", std::process::id());
    let dir = std::env::temp_dir().join(format!("spelaion-query-pages-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sqlite_file = dir.join("pages.sqlite");
    std::fs::write(&sqlite_file, b"").unwrap();
    let mut tested = Vec::new();
    for engine in engines(&table, &sqlite_file).await {
        let label = engine.label;
        let t = table.as_str();
        let body = async {
            run(
                label,
                &engine.writer,
                &format!("drop table if exists {t}; create table {t} (id int not null primary key, grp int not null, v varchar(20) not null)"),
            )
            .await;
            run(label, &engine.writer, &engine.fill).await;

            // Ties everywhere: three values of grp for 3000 rows.
            let sql = format!("select id, grp, v from {t} order by grp desc");
            assert!(
                spelaion_core::query_pages_exactly(&engine.reader, &sql).await,
                "{label}"
            );
            let page = spelaion_core::first_query_page_sql(&engine.reader, &sql, PAGE)
                .await
                .unwrap_or_else(|| panic!("{label}: not paged"));
            let id = result(label, &engine.reader, &page).await;
            let mut deleted = BTreeSet::new();
            let mut pages = 1;
            loop {
                // Between pages another session deletes rows (some shown, some not yet) and
                // inserts new ones into every group.
                let shown = engine
                    .reader
                    .results()
                    .get(id)
                    .unwrap()
                    .read()
                    .unwrap()
                    .rows() as u64;
                let gone: Vec<i64> = (0..5).map(|j| (pages * 389 + j * 601) % ROWS + 1).collect();
                deleted.extend(gone.iter().copied());
                let list = gone
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                run(
                    label,
                    &engine.writer,
                    &format!("delete from {t} where id in ({list})"),
                )
                .await;
                for j in 0..5 {
                    let new = 100_000 + pages * 10 + j;
                    run(
                        label,
                        &engine.writer,
                        &format!("insert into {t} values ({new}, {}, 'new')", j % 3),
                    )
                    .await;
                }
                let added = spelaion_core::append_query_page(&engine.reader, id, &sql, PAGE, shown)
                    .await
                    .unwrap_or_else(|e| panic!("{label}: page {pages}: {e}"));
                pages += 1;
                if (added as u64) < PAGE {
                    break;
                }
                assert!(pages < 100, "{label}: pages never end");
            }
            let ids: Vec<i64> = column(&engine.reader, id, 0)
                .iter()
                .map(|v| v.parse().unwrap())
                .collect();
            let groups: Vec<i64> = column(&engine.reader, id, 1)
                .iter()
                .map(|v| v.parse().unwrap())
                .collect();
            // No row twice.
            let mut seen: HashMap<i64, usize> = HashMap::new();
            for id in &ids {
                *seen.entry(*id).or_default() += 1;
            }
            let twice: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
            assert!(twice.is_empty(), "{label}: rows shown twice: {twice:?}");
            // Every row that existed throughout, once.
            let missing: Vec<i64> = (1..=ROWS)
                .filter(|id| !deleted.contains(id) && !seen.contains_key(id))
                .collect();
            assert!(missing.is_empty(), "{label}: rows never shown: {missing:?}");
            // In the order asked for, then the key.
            for pair in ids.iter().zip(&groups).collect::<Vec<_>>().windows(2) {
                let ((a_id, a_grp), (b_id, b_grp)) = (pair[0], pair[1]);
                assert!(
                    a_grp > b_grp || (a_grp == b_grp && a_id < b_id),
                    "{label}: out of order: ({a_id}, {a_grp}) before ({b_id}, {b_grp})"
                );
            }
            assert!(pages > 10, "{label}: only {pages} pages");
            engine.reader.results().remove(id);

            // Not paged (the key is not in the result): it runs as written, every row at once.
            let whole = format!("select grp, v from {t} order by grp");
            assert!(
                !spelaion_core::query_pages_exactly(&engine.reader, &whole).await,
                "{label}"
            );
            assert!(
                spelaion_core::first_query_page_sql(&engine.reader, &whole, PAGE)
                    .await
                    .is_none(),
                "{label}"
            );
            let marker = format!("{whole} -- spelaion run as written");
            let rows = result(label, &engine.reader, &marker).await;
            let count: usize = first(label, &engine.writer, &format!("select count(*) from {t}"))
                .await
                .parse()
                .unwrap();
            assert_eq!(
                engine
                    .reader
                    .results()
                    .get(rows)
                    .unwrap()
                    .read()
                    .unwrap()
                    .rows(),
                count,
                "{label}"
            );
            engine.reader.results().remove(rows);
            if let Some((own_id, statement)) = engine.received {
                // Asked of the reading session itself, the question would be its last
                // statement: its id first, then the other session asks.
                let reader_id = first(label, &engine.reader, own_id).await;
                let marker_again = result(label, &engine.reader, &marker).await;
                engine.reader.results().remove(marker_again);
                let received = first(label, &engine.writer, &statement(&reader_id)).await;
                assert!(
                    received.contains("spelaion run as written"),
                    "{label}: {received}"
                );
                assert!(
                    !received.to_ascii_lowercase().contains("limit")
                        && !received.to_ascii_lowercase().contains("fetch next"),
                    "{label}: the server received a limit: {received}"
                );
            }
        };
        let writer = &engine.writer;
        let drop = vec![format!("drop table if exists {t}")];
        cleaning_up(body, |panicked| async move {
            run_cleanup(label, writer, &drop, panicked).await
        })
        .await;
        tested.push(label);
    }
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("tested on {tested:?}");
}
