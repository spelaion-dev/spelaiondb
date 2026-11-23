//! Query result pages (ADR-054) on every engine: a query is paged only when each page can be
//! fetched exactly, and then every row that exists throughout appears once, in order, while
//! another session deletes and inserts between pages and the ORDER BY has ties everywhere. A
//! query that is not paged runs as written. The grid's sort of a paged query orders the pages
//! by columns that repeat and hold NULLs the same way. SQLite always runs, and DuckDB through
//! its plugin; the servers need `SPELAION_TEST_PG`, `SPELAION_TEST_MYSQL`,
//! `SPELAION_TEST_MARIADB` and `SPELAION_TEST_MSSQL` (skipped without them, which does not
//! count as tested).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultId, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

use crate::engines;
use crate::support;
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
    Session::new(
        connection,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
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
            reader: connect(&*engines::postgres(), &params).await,
            writer: connect(&*engines::postgres(), &params).await,
            fill: format!(
                "insert into {table} select g, g % 3, 'v' || g from generate_series(1, {ROWS}) g"
            ),
            received: Some(("select pg_backend_pid()", |pid| {
                format!("select query from pg_stat_activity where pid = {pid}")
            })),
        });
    }
    for (var, engine, driver) in [
        ("SPELAION_TEST_MYSQL", "mysql", engines::mysql()),
        ("SPELAION_TEST_MARIADB", "mariadb", engines::mariadb()),
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
        let master = connect(&*engines::sqlserver(), &params).await;
        run(
            "sqlserver",
            &master,
            "if db_id('spelaion') is null create database spelaion",
        )
        .await;
        params.database = "spelaion".into();
        out.push(Engine {
            label: "sqlserver",
            reader: connect(&*engines::sqlserver(), &params).await,
            writer: connect(&*engines::sqlserver(), &params).await,
            fill: format!(
                "insert into {table} select n, n % 3, concat('v', n) from (select top ({ROWS}) row_number() over (order by (select null)) as n from sys.all_objects a cross join sys.all_objects b) s"
            ),
            received: None,
        });
    }
    // DuckDB: both sessions on one file, which its plugin's one process shares.
    let duck_file = sqlite_file.with_extension("duckdb");
    let _ = std::fs::remove_file(&duck_file);
    engines::ensure_database(spelaion_config::Engine::Duckdb, &duck_file).await;
    let duck = ConnectParams {
        database: duck_file.display().to_string(),
        ..Default::default()
    };
    let driver = engines::duckdb();
    out.push(Engine {
        label: "duckdb",
        reader: connect(&*driver, &duck).await,
        writer: connect(&*driver, &duck).await,
        fill: format!(
            "insert into {table} select g, g % 3, 'v' || g from range(1, {ROWS} + 1) t(g)"
        ),
        received: None,
    });
    let lite = ConnectParams {
        database: sqlite_file.display().to_string(),
        ..Default::default()
    };
    let driver = engines::sqlite();
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
    let _turn = crate::serial::file_turn(module_path!());
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
                spelaion_core::query_pages_exactly(&engine.reader, &sql, &[]).await,
                "{label}"
            );
            let page = spelaion_core::first_query_page_sql(&engine.reader, &sql, PAGE, &[])
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
                let added =
                    spelaion_core::append_query_page(&engine.reader, id, &sql, PAGE, shown, &[])
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
                !spelaion_core::query_pages_exactly(&engine.reader, &whole, &[]).await,
                "{label}"
            );
            assert!(
                spelaion_core::first_query_page_sql(&engine.reader, &whole, PAGE, &[])
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

/// A cell of a sorted column as the test compares it: a value, or NULL after every value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Cell {
    Number(i64),
    Text(String),
    Null,
}

/// One row of the sorted table: its id, `grp` and `v`.
type Row = (i64, Cell, Cell);

/// Every row of a result of `id, grp, v`.
fn rows_of(session: &Session, id: ResultId) -> Vec<Row> {
    let stored = session.results().get(id).unwrap();
    let stored = stored.read().unwrap();
    (0..stored.rows())
        .map(|row| {
            let key = match stored.value_at(row, 0).unwrap() {
                Value::Int(n) => n,
                other => other.to_string().parse().unwrap(),
            };
            let grp = match stored.value_at(row, 1).unwrap() {
                Value::Null => Cell::Null,
                Value::Int(n) => Cell::Number(n),
                other => Cell::Number(other.to_string().parse().unwrap()),
            };
            let v = match stored.value_at(row, 2).unwrap() {
                Value::Null => Cell::Null,
                Value::Text(text) => Cell::Text(text),
                other => Cell::Text(other.to_string()),
            };
            (key, grp, v)
        })
        .collect()
}

/// The grid's sort of a paged query (SDB-380) on every engine: the pages are read again from
/// the server ordered by columns that repeat and hold NULLs, NULLs last in each key whatever
/// the engine and the direction, then the primary key. While another session deletes and
/// inserts between pages, every row that exists throughout appears once, in that order. Then
/// a table data tab's header sort: its numbered pages, ordered by columns that repeat and
/// hold NULLs and then the key, show every row once.
#[tokio::test]
async fn pages_sorted_by_columns_with_nulls_and_ties_show_every_row_once_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    use spelaion_core::{ColumnSort, SortKey, TablePage};
    let table = format!("sorted_pages_{}", std::process::id());
    let dir = std::env::temp_dir().join(format!("spelaion-sorted-pages-{}", std::process::id()));
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
                &format!("drop table if exists {t}; create table {t} (id int not null primary key, grp int, v varchar(20))"),
            )
            .await;
            run(label, &engine.writer, &engine.fill).await;
            // Three values of grp and a NULL every fifth row; half the rows share one v, and
            // every seventh has none.
            for change in [
                format!("update {t} set v = 'same' where id % 2 = 0"),
                format!("update {t} set v = null where id % 7 = 0"),
                format!("update {t} set grp = null where id % 5 = 0"),
            ] {
                run(label, &engine.writer, &change).await;
            }
            let sql = format!("select id, grp, v from {t}");
            let key = |column, descending| SortKey { column, descending };
            let mut round = 0;
            for sort in [
                vec![key(1, true), key(2, false)],
                vec![key(2, true)],
                vec![key(1, false), key(2, true), key(0, true)],
            ] {
                round += 1;
                assert!(
                    spelaion_core::query_pages_exactly(&engine.reader, &sql, &sort).await,
                    "{label}: {sort:?}"
                );
                let existing: BTreeSet<i64> = {
                    let all = result(label, &engine.writer, &format!("select id from {t}")).await;
                    let ids = column(&engine.writer, all, 0)
                        .iter()
                        .map(|v| v.parse().unwrap())
                        .collect();
                    engine.writer.results().remove(all);
                    ids
                };
                let page = spelaion_core::first_query_page_sql(&engine.reader, &sql, PAGE, &sort)
                    .await
                    .unwrap_or_else(|| panic!("{label}: not paged"));
                let id = result(label, &engine.reader, &page).await;
                let mut deleted = BTreeSet::new();
                let mut pages = 1;
                loop {
                    let shown = engine
                        .reader
                        .results()
                        .get(id)
                        .unwrap()
                        .read()
                        .unwrap()
                        .rows() as u64;
                    // Rows shown and not yet shown go, with and without NULLs.
                    let gone: Vec<i64> = (0..5)
                        .map(|j| (round * 1009 + pages * 389 + j * 601) % ROWS + 1)
                        .collect();
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
                    // New rows in every group, the NULL one too, with and without a v.
                    for j in 0..5 {
                        let new = 100_000 + round * 10_000 + pages * 10 + j;
                        let grp = if j == 3 {
                            "null".to_string()
                        } else {
                            (j % 3).to_string()
                        };
                        let v = if j == 4 { "null" } else { "'new'" };
                        run(
                            label,
                            &engine.writer,
                            &format!("insert into {t} values ({new}, {grp}, {v})"),
                        )
                        .await;
                    }
                    let added = spelaion_core::append_query_page(
                        &engine.reader,
                        id,
                        &sql,
                        PAGE,
                        shown,
                        &sort,
                    )
                    .await
                    .unwrap_or_else(|e| panic!("{label}: {sort:?}: page {pages}: {e}"));
                    pages += 1;
                    if (added as u64) < PAGE {
                        break;
                    }
                    assert!(pages < 100, "{label}: pages never end");
                }
                let rows = rows_of(&engine.reader, id);
                let mut seen: HashMap<i64, usize> = HashMap::new();
                for (key, _, _) in &rows {
                    *seen.entry(*key).or_default() += 1;
                }
                let twice: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
                assert!(
                    twice.is_empty(),
                    "{label}: {sort:?}: rows shown twice: {twice:?}"
                );
                let missing: Vec<i64> = existing
                    .iter()
                    .filter(|key| !deleted.contains(key) && !seen.contains_key(key))
                    .copied()
                    .collect();
                assert!(
                    missing.is_empty(),
                    "{label}: {sort:?}: rows never shown: {missing:?}"
                );
                // In the sort's order, NULLs last in each key in both directions, then the key.
                let cell = |row: &Row, column: usize| match column {
                    0 => Cell::Number(row.0),
                    1 => row.1.clone(),
                    _ => row.2.clone(),
                };
                let before = |a: &Row, b: &Row| {
                    for k in &sort {
                        let (x, y) = (cell(a, k.column), cell(b, k.column));
                        if x == y {
                            continue;
                        }
                        return match (&x, &y) {
                            (Cell::Null, _) => false,
                            (_, Cell::Null) => true,
                            _ if k.descending => x > y,
                            _ => x < y,
                        };
                    }
                    a.0 < b.0
                };
                for pair in rows.windows(2) {
                    assert!(
                        before(&pair[0], &pair[1]),
                        "{label}: {sort:?}: out of order: {:?} before {:?}",
                        pair[0],
                        pair[1]
                    );
                }
                assert!(pages > 10, "{label}: only {pages} pages");
                let nulls = rows.iter().filter(|r| r.1 == Cell::Null).count();
                let no_v = rows.iter().filter(|r| r.2 == Cell::Null).count();
                assert!(
                    nulls > 400 && no_v > 300,
                    "{label}: {nulls} and {no_v} NULLs"
                );
                engine.reader.results().remove(id);
            }

            // A sort the pages cannot follow is not paged: the app then sorts loaded rows.
            assert!(
                !spelaion_core::query_pages_exactly(&engine.reader, &sql, &[key(7, false)]).await,
                "{label}"
            );
            assert!(
                spelaion_core::first_query_page_sql(&engine.reader, &sql, PAGE, &[key(7, false)])
                    .await
                    .is_none(),
                "{label}"
            );

            // The table data tab: the core writes the ORDER BY from the table's columns, the
            // key follows it, and the numbered pages hold every row once.
            let schema = spelaion_core::active_schema(&engine.reader)
                .await
                .unwrap()
                .unwrap_or_default();
            let by = |column: &str, descending| ColumnSort {
                column: column.into(),
                descending,
            };
            let order_by = spelaion_core::table_order_by(
                &engine.reader,
                &schema,
                t,
                &[by("grp", true), by("v", false)],
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert!(
                spelaion_core::table_order_by(
                    &engine.reader,
                    &schema,
                    t,
                    &[by("grp desc, (select 1)", false)],
                )
                .await
                .is_err(),
                "{label}"
            );
            let count: usize = first(label, &engine.writer, &format!("select count(*) from {t}"))
                .await
                .parse()
                .unwrap();
            let mut shown: Vec<Row> = Vec::new();
            let mut offset = 0;
            loop {
                let page = spelaion_core::table_page_sql_by_key(
                    &engine.reader,
                    &TablePage {
                        schema: schema.clone(),
                        name: t.into(),
                        filter: String::new(),
                        order_by: order_by.clone(),
                        limit: PAGE,
                        offset,
                    },
                )
                .await
                .unwrap_or_else(|e| panic!("{label}: {e}"));
                let id = result(label, &engine.reader, &page).await;
                let rows = rows_of(&engine.reader, id);
                engine.reader.results().remove(id);
                let last = (rows.len() as u64) < PAGE;
                shown.extend(rows);
                if last {
                    break;
                }
                offset += PAGE;
                assert!(offset < 100_000, "{label}: pages never end");
            }
            let distinct: BTreeSet<i64> = shown.iter().map(|r| r.0).collect();
            assert_eq!(shown.len(), count, "{label}: rows in the table's pages");
            assert_eq!(
                distinct.len(),
                count,
                "{label}: a row twice in the table's pages"
            );
            // Within equal grp and v the key orders the rows, wherever the engine puts NULLs.
            for pair in shown.windows(2) {
                if pair[0].1 == pair[1].1 && pair[0].2 == pair[1].2 {
                    assert!(
                        pair[0].0 < pair[1].0,
                        "{label}: {:?} before {:?}",
                        pair[0],
                        pair[1]
                    );
                }
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

/// SQL Server: a `varchar` key of a SQL collation sorts with the collation's non-Unicode rules,
/// where a hyphen or an apostrophe is a character like any other. Compared with an `N'…'`
/// literal it would be converted to `nvarchar` and compared with Unicode rules, which weigh
/// them otherwise, and a page boundary between such values would show a row twice or hide one
/// (SDB-404). Pages one row long must show every row once, in the server's own `ORDER BY`
/// order.
#[tokio::test]
async fn sqlserver_pages_by_a_varchar_key_follow_its_collation() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(mut params) = server("SPELAION_TEST_MSSQL", "sa", "Spelaion-test-1") else {
        return;
    };
    params.tls = TlsMode::Require;
    params.database = "master".into();
    let master = connect(&*engines::sqlserver(), &params).await;
    run(
        "sqlserver",
        &master,
        "if db_id('spelaion') is null create database spelaion",
    )
    .await;
    params.database = "spelaion".into();
    let session = connect(&*engines::sqlserver(), &params).await;
    let cleanup = connect(&*engines::sqlserver(), &params).await;
    let t = format!("collation_pages_{}", std::process::id());
    let values = [
        "co-op",
        "coop",
        "co'op",
        "co op",
        "cop",
        "co-operate",
        "cooperate",
        "coo",
        "co--op",
        "c-o-o-p",
        "'coop",
        "-coop",
        "coo-p",
        "COOP",
        "Co-Op",
        "co_op",
        "co.op",
        // Outside ASCII, in the database's code page (1252).
        "coöp",
        "cöop",
        "coop-é",
        "co'-op",
    ];
    let body = async {
        run(
            "sqlserver",
            &session,
            &format!(
                "drop table if exists {t}; create table {t} (code varchar(20) collate SQL_Latin1_General_CP1_CS_AS not null primary key, n int not null)"
            ),
        )
        .await;
        for (n, value) in values.iter().enumerate() {
            let literal = value.replace("'", "''");
            run(
                "sqlserver",
                &session,
                &format!("insert into {t} values ('{literal}', {n})"),
            )
            .await;
        }
        let ordered = result(
            "sqlserver",
            &session,
            &format!("select code from {t} order by code"),
        )
        .await;
        let expected = column(&session, ordered, 0);
        session.results().remove(ordered);
        assert_eq!(expected.len(), values.len());

        let sql = format!("select code, n from {t}");
        if !spelaion_core::query_pages_exactly(&session, &sql, &[]).await {
            // Not paged: it runs as written, which is exact too.
            eprintln!("not paged by a varchar key of a SQL collation");
            return;
        }
        let page = spelaion_core::first_query_page_sql(&session, &sql, 1, &[])
            .await
            .expect("paged");
        let id = result("sqlserver", &session, &page).await;
        for _ in 0..values.len() + 2 {
            let shown = session.results().get(id).unwrap().read().unwrap().rows() as u64;
            let added = spelaion_core::append_query_page(&session, id, &sql, 1, shown, &[])
                .await
                .expect("a page");
            if added == 0 {
                break;
            }
        }
        let paged = column(&session, id, 0);
        session.results().remove(id);
        assert_eq!(paged, expected, "pages differ from the server's ORDER BY");
    };
    let drop = vec![format!("drop table if exists {t}")];
    cleaning_up(body, |panicked| async move {
        run_cleanup("sqlserver", &cleanup, &drop, panicked).await
    })
    .await;
}
