//! Sessions against a real PostgreSQL. Needs `SPELAION_TEST_PG=host:port`
//! (see `crates/spelaion-driver-postgres/tests/postgres.rs`).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, Secret};
use spelaion_driver_postgres::PostgresDriver;

async fn session() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    let params = ConnectParams {
        host: host.into(),
        port: port.parse().expect("port"),
        user: "spelaion".into(),
        password: Some(Secret::new("spelaion")),
        database: "spelaion".into(),
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
async fn process_monitor_cancels_and_ends_postgres_sessions() {
    let (Some(monitor), Some(target)) = (session().await, session().await) else {
        return;
    };
    let target = Arc::new(target);
    let target_id = spelaion_core::list_processes(&target).await.unwrap().own_id;
    let initial = spelaion_core::list_processes(&monitor).await.unwrap();
    let idle = initial
        .processes
        .iter()
        .find(|p| p.id == target_id)
        .expect("target session is visible");
    assert_eq!(idle.activity, "idle");
    assert!(
        idle.sql.is_none(),
        "an idle session's last statement is not a running query"
    );
    assert!(idle.identity.is_some());

    let task = tokio::spawn({
        let target = target.clone();
        async move {
            target
                .run_script("SELECT pg_sleep(30)")
                .collect::<Vec<_>>()
                .await
        }
    });
    let mut running = None;
    for _ in 0..100 {
        let snapshot = spelaion_core::list_processes(&monitor).await.unwrap();
        if let Some(p) = snapshot.processes.into_iter().find(|p| {
            p.id == target_id
                && p.sql
                    .as_deref()
                    .is_some_and(|sql| sql.contains("pg_sleep(30)"))
        }) {
            running = Some(p);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let process = running.expect("running query is visible");
    assert!(matches!(process.activity.as_str(), "active" | "waiting"));
    let mut target_info = spelaion_core::ProcessTarget {
        id: process.id,
        user: process.user,
        host: process.host,
        command: process.command,
        sql: process.sql,
        identity: Some("outdated session".into()),
    };
    assert!(
        spelaion_core::control_process(&monitor, &target_info, false, false, true)
            .await
            .is_err()
    );
    target_info.identity = process.identity;
    let valid_identity = target_info.identity.take();
    assert!(
        spelaion_core::control_process(&monitor, &target_info, false, false, true)
            .await
            .is_err()
    );
    target_info.identity = valid_identity;
    assert!(
        spelaion_core::control_process(&monitor, &target_info, false, true, true)
            .await
            .is_err()
    );
    assert!(
        spelaion_core::control_process(&monitor, &target_info, false, false, false)
            .await
            .is_err()
    );
    spelaion_core::control_process(&monitor, &target_info, false, false, true)
        .await
        .unwrap();
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    );
    let after: Vec<_> = target.run_script("SELECT 1").collect().await;
    assert!(matches!(
        after.last(),
        Some(CoreEvent::ScriptFinished { failed: false, .. })
    ));
    let snapshot = spelaion_core::list_processes(&monitor).await.unwrap();
    let process = snapshot
        .processes
        .into_iter()
        .find(|p| p.id == target_id)
        .unwrap();
    let target_info = spelaion_core::ProcessTarget {
        id: process.id,
        user: process.user,
        host: process.host,
        command: process.command,
        sql: process.sql,
        identity: process.identity,
    };
    spelaion_core::control_process(&monitor, &target_info, true, false, true)
        .await
        .unwrap();
    assert!(
        !spelaion_core::list_processes(&monitor)
            .await
            .unwrap()
            .processes
            .iter()
            .any(|p| p.id == target_id)
    );
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
            failed: false,
            ..
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
            failed: true,
            ..
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
    let tsv = |r: &spelaion_core::StoredResult, rows: std::ops::Range<usize>, column: usize| {
        let mut out = Vec::new();
        spelaion_core::write_result(
            r,
            &spelaion_core::ExportOptions {
                format: spelaion_core::ExportFormat::Tsv,
                rows,
                columns: vec![column],
                header: false,
                csv: spelaion_core::CsvOptions::default(),
            },
            &spelaion_core::GenericDialect,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let key = |column, descending| Some(spelaion_core::SortKey { column, descending });

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
    use spelaion_core::{CellInput, RowChange, TablePage};
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
    let sql = spelaion_core::table_page_sql(session.dialect().as_ref(), &page).unwrap();
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
        spelaion_core::edit_statements(
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
        spelaion_core::apply_edits(&session, &statements).await,
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
            spelaion_driver_api::Value::from_array(
                batch.column(0).as_ref(),
                0,
                stored.schema().columns[0].logical
            ),
            Some(spelaion_driver_api::Value::Int(1))
        );
    }
    let _: Vec<CoreEvent> = session.run_script("drop table core_edits").collect().await;
}

async fn editable(session: &Session, sql: &str) -> Option<(String, String)> {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let id = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{sql}: {events:#?}"));
    spelaion_core::editable_query_table(session, id, sql, false)
        .await
        .unwrap()
        .map(|t| (t.object.schema, t.object.name))
}

#[tokio::test]
async fn query_results_resolve_their_table_as_the_server_does() {
    let Some(session) = session().await else {
        return;
    };
    let _: Vec<CoreEvent> = session
        .run_script(
            "drop schema if exists core_eq_s cascade; \
             drop table if exists core_eq_orders; \
             create table core_eq_orders (id int primary key, item text); \
             insert into core_eq_orders values (1, 'pen'); \
             create schema core_eq_s; \
             create table core_eq_s.core_eq_only (id int primary key); \
             create table core_eq_s.core_eq_orders (id int primary key, item text)",
        )
        .collect()
        .await;
    let public = Some(("public".to_string(), "core_eq_orders".to_string()));
    // `core_eq_orders` exists in two schemas: the search path picks one.
    assert_eq!(
        editable(&session, "select * from core_eq_orders").await,
        public
    );
    let _: Vec<CoreEvent> = session
        .run_script("set search_path = core_eq_s, public")
        .collect()
        .await;
    assert_eq!(
        editable(&session, "select * from core_eq_orders").await,
        Some(("core_eq_s".to_string(), "core_eq_orders".to_string()))
    );
    assert_eq!(
        editable(&session, "select * from core_eq_only").await,
        Some(("core_eq_s".to_string(), "core_eq_only".to_string()))
    );
    let _: Vec<CoreEvent> = session
        .run_script("set search_path = public")
        .collect()
        .await;

    // A temporary table shadows `public.core_eq_orders`: never written through the name.
    let _: Vec<CoreEvent> = session
        .run_script(
            "create temp table core_eq_orders (id int primary key, item text); \
             insert into core_eq_orders values (1, 'shadow')",
        )
        .collect()
        .await;
    assert_eq!(
        editable(&session, "select * from core_eq_orders").await,
        None
    );
    assert_eq!(
        editable(&session, "select * from pg_temp.core_eq_orders").await,
        None
    );
    assert_eq!(
        editable(&session, "select * from public.core_eq_orders").await,
        public
    );
}

/// The first cell of every result `script` returned, in order; panics on a failed statement.
async fn first_cells(session: &Session, script: &str) -> Vec<String> {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    let mut cells = Vec::new();
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => panic!("{script}: {error:?}"),
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(*result).unwrap();
                let stored = stored.read().unwrap();
                let batch = &stored.window(0, 1)[0];
                cells.push(
                    spelaion_driver_api::Value::from_array(
                        batch.column(0).as_ref(),
                        0,
                        stored.schema().columns[0].logical,
                    )
                    .unwrap()
                    .to_string(),
                );
            }
            _ => {}
        }
    }
    cells
}

/// SQL-standard routine bodies (`BEGIN ATOMIC … END`, as `pg_dump` writes them) hold
/// semicolons: the editor runs each routine as one statement, so it is created and works, and
/// its `END` never reaches the server alone (where it would mean `COMMIT`).
#[tokio::test]
async fn begin_atomic_routines_run_as_one_statement() {
    let Some(session) = session().await else {
        return;
    };
    let clean = "drop function if exists sdb114_add(int, int); \
                 drop procedure if exists sdb114_log(); \
                 drop table if exists sdb114_t";
    first_cells(&session, clean).await;
    let cells = first_cells(
        &session,
        "CREATE TABLE sdb114_t (n int);\n\
         CREATE OR REPLACE FUNCTION sdb114_add(a int, b int) RETURNS int\n\
         LANGUAGE sql\n\
         BEGIN ATOMIC\n\
           SELECT 1;\n\
           SELECT CASE WHEN a + b > 0 THEN a + b ELSE 0 END;\n\
         END;\n\
         CREATE PROCEDURE sdb114_log() LANGUAGE sql\n\
         BEGIN ATOMIC\n\
           INSERT INTO sdb114_t VALUES (1);\n\
           INSERT INTO sdb114_t VALUES (2);\n\
         END;\n\
         CALL sdb114_log();\n\
         SELECT sdb114_add(2, 3);\n\
         SELECT sdb114_add(-2, 1);\n\
         SELECT count(*) FROM sdb114_t;",
    )
    .await;
    assert_eq!(cells, ["5", "0", "2"]);

    // Inside a transaction: the routine's END is not a COMMIT, so a rollback undoes it all.
    first_cells(
        &session,
        "begin;\n\
         create function sdb114_gone() returns int language sql\n\
         begin atomic\n  select 1;\nend;\n\
         rollback;",
    )
    .await;
    assert_eq!(
        first_cells(
            &session,
            "select count(*) from pg_proc where proname = 'sdb114_gone'"
        )
        .await,
        ["0"]
    );
    first_cells(&session, clean).await;
}
