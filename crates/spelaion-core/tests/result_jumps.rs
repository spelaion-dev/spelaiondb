//! Moving through big results (SDB-386) on every engine: SQLite and DuckDB always, PostgreSQL,
//! MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without them they are
//! skipped, which does not count as tested).
//!
//! - A table data tab jumps to page N, to its last page (counted) and loads every row, each in
//!   the order of its pages: the user's order, then the primary key (ADR-054).
//! - A paged query reaches a row past the ones loaded, and loads every row, by keyset pages in
//!   order. A load stopped between pages, or cancelled before it starts, keeps exactly the
//!   pages loaded before; the same result then loads the rest, every row once.

use crate::engines;
use crate::support;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, LoadCancel, LoadOutcome, ResultId, ResultStore, Session, TablePage,
    first_query_page_sql, last_table_page, load_query_rows, table_page_sql_by_key,
    table_rows_sql_by_key,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};
use support::{cleaning_up, run_cleanup};

const ROWS: u64 = 1000;
const PAGE: u64 = 100;
/// A load is bounded: a lost cancel or a load that never ends fails in seconds.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// Every engine available here, with the schema its table goes in.
async fn engines() -> Vec<(&'static str, Session, &'static str)> {
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        (
            "sqlite",
            session(&*engines::sqlite(), memory()).await,
            "main",
        ),
        (
            "duckdb",
            session(&*engines::duckdb(), memory()).await,
            "main",
        ),
    ];
    let login =
        |host: String, port: u16, user: &str, password: &str, database: &str| ConnectParams {
            host,
            port,
            user: user.into(),
            password: Some(Secret::new(password)),
            database: database.into(),
            ..Default::default()
        };
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        let params = login(host, port, "spelaion", "spelaion", "spelaion");
        out.push((
            "postgres",
            session(&*engines::postgres(), params).await,
            "public",
        ));
    }
    for (label, var, driver) in [
        ("mysql", "SPELAION_TEST_MYSQL", engines::mysql()),
        ("mariadb", "SPELAION_TEST_MARIADB", engines::mariadb()),
    ] {
        if let Some((host, port)) = server(var) {
            let params = login(host, port, "spelaion", "spelaion", "spelaion");
            out.push((label, session(&driver, params).await, "spelaion"));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push((
            "sqlserver",
            session(&*engines::sqlserver(), params).await,
            "dbo",
        ));
    }
    out
}

/// Runs `sql`; returns the result it produced, if any. Panics on an error.
async fn run(label: &str, session: &Session, sql: &str) -> Option<ResultId> {
    let mut result = None;
    let mut events = session.run_script(sql);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => {
                panic!("{label}: {sql}\n→ {}", error.message)
            }
            CoreEvent::ResultStarted { result: id, .. } => result = Some(id),
            _ => {}
        }
    }
    result
}

/// The ids (the first column) of a result, in order.
fn ids(session: &Session, id: ResultId) -> Vec<u64> {
    let stored = session.results().get(id).unwrap();
    let stored = stored.read().unwrap();
    (0..stored.rows())
        .map(|row| match stored.value_at(row, 0).unwrap() {
            Value::Int(n) => u64::try_from(n).unwrap(),
            other => other.to_string().parse().unwrap(),
        })
        .collect()
}

/// The rows a statement reads, by id.
async fn read_ids(label: &str, session: &Session, sql: &str) -> Vec<u64> {
    let id = run(label, session, sql)
        .await
        .unwrap_or_else(|| panic!("{label}: {sql}: no result"));
    let out = ids(session, id);
    session.results().remove(id);
    out
}

/// The table: ids 1 to `ROWS`, `grp` in three groups (ties everywhere), inserted in shuffled
/// order so the server's own order is not the key's.
async fn fill(label: &str, session: &Session, table: &str) {
    run(label, session, &format!("DROP TABLE IF EXISTS {table}")).await;
    run(
        label,
        session,
        &format!(
            "CREATE TABLE {table} (id INT NOT NULL PRIMARY KEY, grp INT NOT NULL, name VARCHAR(20) NOT NULL)"
        ),
    )
    .await;
    let mut order: Vec<u64> = (1..=ROWS).collect();
    // A fixed shuffle: every 7th id, wrapping (7 and 1000 share no factor).
    order.sort_by_key(|id| (id * 7) % ROWS);
    for chunk in order.chunks(250) {
        let values = chunk
            .iter()
            .map(|id| format!("({id}, {}, 'name {id}')", id % 3))
            .collect::<Vec<_>>()
            .join(", ");
        run(
            label,
            session,
            &format!("INSERT INTO {table} VALUES {values}"),
        )
        .await;
    }
}

/// The ids in the order `grp DESC`, then the key.
fn by_group_desc() -> Vec<u64> {
    let mut all: Vec<u64> = (1..=ROWS).collect();
    all.sort_by_key(|id| (std::cmp::Reverse(id % 3), *id));
    all
}

#[tokio::test]
async fn table_data_tabs_jump_to_page_n_and_to_the_last_page_exactly_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    let table = "result_jumps_table";
    let mut tested = Vec::new();
    for (label, session, schema) in engines().await {
        let body = async {
            fill(label, &session, table).await;
            let page = |order_by: &str, filter: &str, page: u64| TablePage {
                schema: schema.into(),
                name: table.into(),
                filter: filter.into(),
                order_by: order_by.into(),
                limit: PAGE,
                offset: page * PAGE,
            };
            // Page 5 (from 0: rows 501 to 600) with no order of the user's: the key's order.
            let sql = table_page_sql_by_key(&session, &page("", "", 5))
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(
                read_ids(label, &session, &sql).await,
                (501..=600).collect::<Vec<_>>(),
                "{label}: page 6"
            );
            // Page 3 ordered by a column full of ties: the key breaks them.
            let sql = table_page_sql_by_key(&session, &page("grp DESC", "", 3))
                .await
                .unwrap();
            assert_eq!(
                read_ids(label, &session, &sql).await,
                by_group_desc()[300..400].to_vec(),
                "{label}: page 4 by grp"
            );
            // The last page, counted.
            let last = last_table_page(&session, schema, table, "", PAGE)
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!((last.total, last.page), (ROWS, 9), "{label}");
            let sql = table_page_sql_by_key(&session, &page("", "", last.page))
                .await
                .unwrap();
            assert_eq!(
                read_ids(label, &session, &sql).await,
                (901..=ROWS).collect::<Vec<_>>(),
                "{label}: last page"
            );
            // With a filter the last page is the partial one that holds its last row.
            let last = last_table_page(&session, schema, table, "id <= 950", PAGE)
                .await
                .unwrap();
            assert_eq!((last.total, last.page), (950, 9), "{label}");
            let sql = table_page_sql_by_key(&session, &page("grp DESC", "id <= 950", last.page))
                .await
                .unwrap();
            let expected: Vec<u64> = by_group_desc()
                .into_iter()
                .filter(|id| *id <= 950)
                .skip(900)
                .collect();
            assert_eq!(expected.len(), 50);
            assert_eq!(
                read_ids(label, &session, &sql).await,
                expected,
                "{label}: last filtered page"
            );
            // A page past the last one is empty, never an earlier page again.
            let sql = table_page_sql_by_key(&session, &page("", "", 10))
                .await
                .unwrap();
            assert!(read_ids(label, &session, &sql).await.is_empty(), "{label}");
            // A filter that keeps nothing: one empty page, page 0.
            let last = last_table_page(&session, schema, table, "id < 0", PAGE)
                .await
                .unwrap();
            assert_eq!((last.total, last.page), (0, 0), "{label}");
            // A page size out of bounds is refused before anything runs.
            assert!(
                last_table_page(&session, schema, table, "", 0)
                    .await
                    .is_err()
            );

            // Every row, in the pages' order: what "Load every row" reads.
            let sql = table_rows_sql_by_key(&session, schema, table, "", "grp DESC")
                .await
                .unwrap();
            assert_eq!(
                read_ids(label, &session, &sql).await,
                by_group_desc(),
                "{label}: every row"
            );
            // The filter still cannot add a limit or another statement.
            for filter in ["1=1 LIMIT 5", "1=1; DROP TABLE result_jumps_table"] {
                assert!(
                    table_rows_sql_by_key(&session, schema, table, filter, "")
                        .await
                        .is_err(),
                    "{label}: {filter}"
                );
            }
        };
        let drop = vec![format!("DROP TABLE IF EXISTS {table}")];
        let session_ref = &session;
        cleaning_up(body, |panicked| async move {
            run_cleanup(label, session_ref, &drop, panicked).await
        })
        .await;
        tested.push(label);
    }
    eprintln!("tested on {tested:?}");
}

#[tokio::test]
async fn paged_queries_reach_a_row_and_load_every_row_with_cancel_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    let table = "result_jumps_query";
    let mut tested = Vec::new();
    for (label, session, _schema) in engines().await {
        let body = async {
            fill(label, &session, table).await;
            let sql = format!("SELECT id, grp, name FROM {table} ORDER BY grp DESC");
            let first = first_query_page_sql(&session, &sql, PAGE, &[])
                .await
                .unwrap_or_else(|| panic!("{label}: not paged"));
            let result = run(label, &session, &first).await.unwrap();
            assert_eq!(ids(&session, result).len() as u64, PAGE, "{label}");
            let load =
                |offset: u64, until: Option<u64>, page_rows: u64, cancel: Arc<LoadCancel>| {
                    let session = &session;
                    let sql = sql.clone();
                    let heard = Arc::new(AtomicU64::new(0));
                    let seen = heard.clone();
                    async move {
                        let progress = move |rows: u64| seen.store(rows, Ordering::SeqCst);
                        let loaded = tokio::time::timeout(
                            LOAD_TIMEOUT,
                            load_query_rows(
                                session,
                                result,
                                &sql,
                                offset,
                                &[],
                                until,
                                page_rows,
                                &cancel,
                                &progress,
                            ),
                        )
                        .await
                        .unwrap_or_else(|_| panic!("{label}: the load never ended"))
                        .unwrap_or_else(|e| panic!("{label}: {e}"));
                        (loaded, heard.load(Ordering::SeqCst))
                    }
                };

            // Row 777 (page 8 of 100): loaded by keyset pages up to it, never by an offset.
            let (loaded, heard) = load(PAGE, Some(777), 150, Arc::new(LoadCancel::new())).await;
            assert_eq!(loaded.rows, 777, "{label}");
            assert_eq!(loaded.outcome, LoadOutcome::Reached, "{label}");
            assert_eq!(heard, 777, "{label}");
            assert_eq!(
                ids(&session, result),
                by_group_desc()[..777].to_vec(),
                "{label}: up to row 777"
            );

            // Cancelled before it starts: nothing more is loaded.
            let cancel = Arc::new(LoadCancel::new());
            cancel.cancel(&session).await;
            let (loaded, _) = load(777, None, 150, cancel).await;
            assert_eq!(loaded.outcome, LoadOutcome::Cancelled, "{label}");
            assert_eq!(loaded.rows, 777, "{label}");
            assert_eq!(ids(&session, result).len(), 777, "{label}");

            // Stopped after its first page: that page stays, whole, and nothing after it.
            let cancel = Arc::new(LoadCancel::new());
            let stopper = cancel.clone();
            let session_ref = &session;
            let sql_ref = sql.as_str();
            let progress = move |rows: u64| {
                if rows >= 777 + 50 {
                    stopper.stop();
                }
            };
            let loaded = tokio::time::timeout(
                LOAD_TIMEOUT,
                load_query_rows(
                    session_ref,
                    result,
                    sql_ref,
                    777,
                    &[],
                    None,
                    50,
                    &cancel,
                    &progress,
                ),
            )
            .await
            .unwrap_or_else(|_| panic!("{label}: the load never ended"))
            .unwrap();
            assert_eq!(loaded.outcome, LoadOutcome::Cancelled, "{label}");
            assert_eq!(loaded.rows, 827, "{label}");
            assert_eq!(
                ids(&session, result),
                by_group_desc()[..827].to_vec(),
                "{label}: stopped after one page"
            );

            // The session is still usable, and the same result loads the rest: every row once,
            // in order, ending because the query has no more.
            let (loaded, heard) = load(827, None, 40, Arc::new(LoadCancel::new())).await;
            assert_eq!(loaded.outcome, LoadOutcome::Complete, "{label}");
            assert_eq!(loaded.rows, ROWS, "{label}");
            assert_eq!(heard, ROWS, "{label}");
            let all = ids(&session, result);
            assert_eq!(all, by_group_desc(), "{label}: every row");
            assert_eq!(
                all.iter().collect::<HashSet<_>>().len() as u64,
                ROWS,
                "{label}: a row twice"
            );
            // A load that is told the wrong number of rows held is refused, rows untouched.
            let (loaded, _) = load(500, None, 40, Arc::new(LoadCancel::new())).await;
            assert!(
                matches!(loaded.outcome, LoadOutcome::Failed(_)),
                "{label}: {loaded:?}"
            );
            assert_eq!(ids(&session, result).len() as u64, ROWS, "{label}");
            // Out of bounds before anything runs.
            assert!(
                load_query_rows(
                    &session,
                    result,
                    &sql,
                    ROWS,
                    &[],
                    None,
                    0,
                    &LoadCancel::new(),
                    &|_| {}
                )
                .await
                .is_err()
            );
            session.results().remove(result);
        };
        let drop = vec![format!("DROP TABLE IF EXISTS {table}")];
        let session_ref = &session;
        cleaning_up(body, |panicked| async move {
            run_cleanup(label, session_ref, &drop, panicked).await
        })
        .await;
        tested.push(label);
    }
    eprintln!("tested on {tested:?}");
}
