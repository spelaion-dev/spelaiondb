//! Sessions against a real PostgreSQL. Needs `SPELUNK_TEST_PG=host:port`
//! (see `crates/spelunk-driver-postgres/tests/postgres.rs`).

use std::sync::Arc;

use futures_util::StreamExt;
use spelunk_core::{CoreEvent, ResultStore, Session};
use spelunk_driver_api::{ConnectParams, Driver, Secret};
use spelunk_driver_postgres::PostgresDriver;

async fn session() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELUNK_TEST_PG") else {
        eprintln!("SPELUNK_TEST_PG not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let params = ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: "spelunk".into(),
        password: Some(Secret::new("spelunk")),
        database: "spelunk".into(),
        ..Default::default()
    };
    let driver = PostgresDriver;
    let conn = driver.connect(&params).await.expect("connecting");
    Some(Session::new(
        conn,
        driver.dialect(),
        Arc::new(ResultStore::new()),
    ))
}

#[tokio::test]
async fn runs_a_script_statement_by_statement() {
    let Some(session) = session().await else {
        return;
    };
    let script = "create temp table t (id int, name text);
                  insert into t values (1, 'a'), (2, 'b'), (3, 'c');
                  -- stray comment;
                  select * from t order by id;
                  select count(*) from t";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;

    let finished: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::StatementFinished {
                index,
                rows_affected,
                ..
            } => Some((*index, *rows_affected)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        [(0, None), (1, Some(3)), (2, Some(3)), (3, Some(1))]
    );
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished {
            executed: 4,
            failed: false
        })
    ));

    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    let first = session.results().get(results[0]).unwrap();
    let first = first.read().unwrap();
    assert_eq!(first.rows(), 3);
    assert!(first.is_complete());
    assert_eq!(
        first
            .schema()
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "name"]
    );
}

#[tokio::test]
async fn stops_at_the_first_error_and_points_to_it() {
    let Some(session) = session().await else {
        return;
    };
    let script = "select 1;\nselect * from\n  missing_table;\nselect 3";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;

    let error = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::StatementFailed { index: 1, error } => Some(error.clone()),
            _ => None,
        })
        .expect("the second statement should fail");
    assert_eq!(error.code.as_deref(), Some("42P01"));
    let offset = error.script_offset.expect("error position");
    assert!(
        script[offset..].starts_with("missing_table"),
        "points at {:?}",
        &script[offset..]
    );
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished {
            executed: 2,
            failed: true
        })
    ));
}

#[tokio::test]
async fn large_results_arrive_in_batches() {
    let Some(session) = session().await else {
        return;
    };
    let events: Vec<CoreEvent> = session
        .run_script("select g, md5(g::text) from generate_series(1, 100000) g")
        .collect()
        .await;
    let progress: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::Rows { total_rows, .. } => Some(*total_rows),
            _ => None,
        })
        .collect();
    assert_eq!(progress.len(), 10);
    assert_eq!(progress.last(), Some(&100_000));
}

#[tokio::test]
async fn results_sort_by_typed_values_across_batches() {
    let Some(session) = session().await else {
        return;
    };
    // Ten batches of 10,000 rows; `n` as numeric sorts by value, not as text.
    let script = "select g, (g % 7 - 3)::numeric * 1.5 as n,
                         case when g % 1000 = 0 then null else g % 3 = 0 end as b,
                         timestamptz '2024-01-01 00:00+00' + g * interval '1 minute' as ts
                  from generate_series(1, 100000) g";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    let id = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap();
    let stored = session.results().get(id).unwrap();
    let mut stored = stored.write().unwrap();
    let tsv = |r: &spelunk_core::StoredResult, rows: std::ops::Range<usize>, column: usize| {
        let mut out = Vec::new();
        spelunk_core::write_result(
            r,
            &spelunk_core::ExportOptions {
                format: spelunk_core::ExportFormat::Tsv,
                rows,
                columns: column..column + 1,
                header: false,
            },
            &spelunk_core::GenericDialect,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let key = |column, descending| Some(spelunk_core::SortKey { column, descending });

    stored.sort(key(1, false)).unwrap();
    assert_eq!(tsv(&stored, 0..2, 1), ["-4.5", "-4.5"]);
    assert_eq!(tsv(&stored, 99_998..100_000, 1), ["4.5", "4.5"]);
    // Stable: equal values keep the server order.
    assert_eq!(tsv(&stored, 0..3, 0), ["7", "14", "21"]);

    stored.sort(key(3, true)).unwrap();
    assert_eq!(tsv(&stored, 0..1, 0), ["100000"]);

    // NULLs last whichever the direction.
    stored.sort(key(2, true)).unwrap();
    assert_eq!(tsv(&stored, 0..1, 2), ["true"]);
    assert_eq!(tsv(&stored, 99_900..100_000, 2), vec![String::new(); 100]);
    stored.sort(key(2, false)).unwrap();
    assert_eq!(tsv(&stored, 0..1, 2), ["false"]);
    assert_eq!(tsv(&stored, 99_999..100_000, 0), ["100000"]);

    stored.sort(None).unwrap();
    assert_eq!(tsv(&stored, 0..2, 0), ["1", "2"]);
}

#[tokio::test]
async fn table_edits_write_typed_literals_on_postgres() {
    use spelunk_core::{CellInput, RowChange, TablePage};
    let Some(session) = session().await else {
        return;
    };
    let _: Vec<CoreEvent> = session
        .run_script(
            "drop table if exists core_edits; \
             create table core_edits (id uuid primary key, price numeric(10,2), day date, ok boolean, raw bytea); \
             insert into core_edits values ('6f1c1f7e-0000-4000-8000-000000000001', 1.50, '2026-01-01', false, '\\x00')",
        )
        .collect()
        .await;
    let table = session
        .connection()
        .describe_table("public", "core_edits")
        .await
        .unwrap();
    let page = TablePage {
        schema: "public".into(),
        name: "core_edits".into(),
        filter: String::new(),
        order_by: String::new(),
        limit: 10,
        offset: 0,
    };
    let sql = spelunk_core::table_page_sql(session.dialect().as_ref(), &page).unwrap();
    let events: Vec<CoreEvent> = session.run_script(&sql).collect().await;
    let id = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap();
    let statements = {
        let stored = session.results().get(id).unwrap();
        let stored = stored.read().unwrap();
        spelunk_core::edit_statements(
            session.dialect().as_ref(),
            &table,
            &stored,
            &[RowChange::Update {
                row: 0,
                values: vec![
                    (1, CellInput::Text("19.99".into())),
                    (2, CellInput::Text("2026-11-03".into())),
                    (3, CellInput::Text("true".into())),
                    (4, CellInput::Text("\\xcafe".into())),
                ],
            }],
        )
        .unwrap()
    };
    assert_eq!(
        spelunk_core::apply_edits(&session, &statements).await,
        Ok(1)
    );
    let check: Vec<CoreEvent> = session
        .run_script(
            "select count(*) from core_edits where price = 19.99 and day = '2026-11-03' \
             and ok and raw = '\\xcafe'::bytea",
        )
        .collect()
        .await;
    let id = check
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap();
    {
        let stored = session.results().get(id).unwrap();
        let stored = stored.read().unwrap();
        let batch = stored.window(0, 1).remove(0);
        assert_eq!(
            spelunk_driver_api::Value::from_array(
                batch.column(0).as_ref(),
                0,
                stored.schema().columns[0].logical
            ),
            Some(spelunk_driver_api::Value::Int(1))
        );
    }
    let _: Vec<CoreEvent> = session.run_script("drop table core_edits").collect().await;
}
