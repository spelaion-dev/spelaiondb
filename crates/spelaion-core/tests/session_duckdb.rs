//! Sessions over in-memory DuckDB: they always run, with nothing external.

mod engines;

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CellInput, CoreEvent, EditError, ResultStore, RowChange, Session, TablePage};
use spelaion_driver_api::{ConnectParams, Driver};

async fn session() -> Session {
    let driver = engines::duckdb();
    let conn = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .expect("opening :memory:");
    Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

async fn run(session: &Session, script: &str) -> Vec<CoreEvent> {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    assert!(
        matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished { failed: false, .. })
        ),
        "{events:#?}"
    );
    events
}

async fn first_result(session: &Session, sql: &str) -> spelaion_core::ResultId {
    run(session, sql)
        .await
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result")
}

async fn texts(session: &Session, sql: &str) -> Vec<String> {
    let id = first_result(session, sql).await;
    let stored = session.results().get(id).unwrap();
    let stored = stored.read().unwrap();
    let mut out = Vec::new();
    for batch in stored.window(0, 1000) {
        for row in 0..batch.num_rows() {
            let cells: Vec<String> = (0..batch.num_columns())
                .map(|c| {
                    spelaion_driver_api::Value::from_array(
                        batch.column(c).as_ref(),
                        row,
                        stored.schema().columns[c].logical,
                    )
                    .map_or("?".into(), |v| v.to_string())
                })
                .collect();
            out.push(cells.join("|"));
        }
    }
    out
}

/// Macro bodies in `$$` and casts with `::` go through the splitter and the parameter
/// scanner untouched.
#[tokio::test]
async fn scripts_split_like_duckdb_reads_them() {
    let session = session().await;
    let events = run(
        &session,
        "create macro twice(x) as x * 2; -- a comment; with a semicolon\n\
         select twice(21)::varchar as v, $$a;b$$ as s",
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished { executed: 2, .. })
    ));
    assert_eq!(
        texts(&session, "select twice(21)::varchar, $$a;b$$").await,
        ["42|a;b"]
    );
    assert!(
        spelaion_core::script_params(session.dialect().as_ref(), "select 1::int, $$:no$$")
            .is_empty()
    );
}

#[tokio::test]
async fn tables_are_counted_and_paged() {
    let session = session().await;
    run(
        &session,
        "create table \"order lines\" as select range as x from range(25); \
         create view big as select * from \"order lines\" where x > 20",
    )
    .await;
    assert_eq!(
        spelaion_core::count_rows(&session, "main", "order lines").await,
        Ok(25)
    );
    assert_eq!(
        spelaion_core::count_rows(&session, "main", "big").await,
        Ok(4)
    );
    let page = TablePage {
        schema: "main".into(),
        name: "order lines".into(),
        filter: "x % 2 = 0".into(),
        order_by: "x desc".into(),
        limit: 3,
        offset: 1,
    };
    let sql = spelaion_core::table_page_sql(session.dialect().as_ref(), &page).unwrap();
    assert_eq!(texts(&session, &sql).await, ["22", "20", "18"]);
}

#[tokio::test]
async fn table_edits_apply_in_one_transaction_by_primary_key() {
    let session = session().await;
    run(
        &session,
        "create table \"line item\" (id integer primary key, name varchar, qty integer default 1, \
                                    due date, data blob); \
         insert into \"line item\" values (1, 'one', 1, date '2024-01-01', 'a'::blob), \
                                         (2, 'two', 2, null, null), (3, 'three', 3, null, null)",
    )
    .await;
    let table = session
        .connection()
        .describe_table("main", "line item")
        .await
        .unwrap();
    assert_eq!(table.primary_key, ["id"]);
    let page = TablePage {
        schema: "main".into(),
        name: "line item".into(),
        filter: String::new(),
        order_by: "id desc".into(),
        limit: 100,
        offset: 0,
    };
    let sql = spelaion_core::table_page_sql(session.dialect().as_ref(), &page).unwrap();
    let id = first_result(&session, &sql).await;
    let stored = session.results().get(id).unwrap();
    let edit_target = session.edit_target().await.unwrap();
    let statements = {
        let stored = stored.read().unwrap();
        // Rows shown: 3, 2, 1 (id desc).
        spelaion_core::edit_statements(
            &edit_target,
            &table,
            &stored,
            &[
                RowChange::Update {
                    row: 0,
                    values: vec![
                        (1, CellInput::Text("it's three".into())),
                        (2, CellInput::Null),
                        (3, CellInput::Text("2024-12-31".into())),
                        (4, CellInput::Text("\\x00ff".into())),
                    ],
                },
                RowChange::Delete { row: 1 },
                RowChange::Insert {
                    values: vec![
                        (0, CellInput::Text("10".into())),
                        (1, CellInput::Text("ten".into())),
                    ],
                },
            ],
        )
        .unwrap()
    };
    assert_eq!(
        statements,
        [
            "UPDATE \"line item\" SET name = 'it''s three', qty = NULL, due = '2024-12-31', data = from_hex('00ff') WHERE id = 3",
            "DELETE FROM \"line item\" WHERE id = 2",
            "INSERT INTO \"line item\" (id, name) VALUES (10, 'ten')",
        ]
    );
    assert_eq!(
        spelaion_core::apply_edits(&session, &statements).await,
        Ok(3)
    );
    assert_eq!(
        texts(
            &session,
            "select id, name, qty, due, data from \"line item\" order by id"
        )
        .await,
        [
            "1|one|1|2024-01-01|\\x61",
            "3|it's three|NULL|2024-12-31|\\x00ff",
            "10|ten|1|NULL|NULL"
        ]
    );

    // A row that vanished meanwhile: nothing is applied, not even the valid change.
    let stale = {
        let stored = stored.read().unwrap();
        spelaion_core::edit_statements(
            &edit_target,
            &table,
            &stored,
            &[
                RowChange::Update {
                    row: 2,
                    values: vec![(1, CellInput::Text("uno".into()))],
                },
                RowChange::Update {
                    row: 1,
                    values: vec![(1, CellInput::Text("gone".into()))],
                },
            ],
        )
        .unwrap()
    };
    assert!(matches!(
        spelaion_core::apply_edits(&session, &stale).await,
        Err(EditError::NotOneRow { index: 2, .. })
    ));
    assert_eq!(
        texts(&session, "select name from \"line item\" where id = 1").await,
        ["one"]
    );
    assert_eq!(
        texts(&session, "select count(*) from \"line item\"").await,
        ["3"]
    );
}

#[tokio::test]
async fn risky_statements_are_recognized() {
    let session = session().await;
    let risks = spelaion_core::script_risks(
        session.dialect().as_ref(),
        "delete from t; update t set a = 1; drop table t; select 1",
        &Default::default(),
    );
    assert_eq!(risks.len(), 3, "{risks:?}");
}

/// The MCP's read-only mode serves DuckDB without its files (ADR-020): `read_text` is refused
/// before anything is sent (`agents_engines.rs` tries the ways around it).
#[tokio::test]
async fn read_only_mode_never_reads_local_files() {
    let session = session().await;
    let result = session
        .run_read_only(
            "select * from read_text('Cargo.toml')",
            &spelaion_core::ReadLimits::default(),
        )
        .await;
    assert!(
        matches!(result, Err(spelaion_core::ReadError::Rejected(ref reason)) if reason.contains("outside the database")),
        "{result:?}"
    );
}

async fn editable(session: &Session, sql: &str) -> Option<(String, String)> {
    let id = first_result(session, sql).await;
    spelaion_core::editable_query_table(session, id, sql, false)
        .await
        .unwrap()
        .map(|t| (t.object.schema, t.object.name))
}

#[tokio::test]
async fn query_results_never_edit_a_table_shadowed_by_a_temporary_one() {
    let session = session().await;
    run(
        &session,
        "create table orders (id integer primary key, item varchar);
         insert into orders values (1, 'pen')",
    )
    .await;
    assert_eq!(
        editable(&session, "select * from orders").await,
        Some(("main".to_string(), "orders".to_string()))
    );
    // `a.b` may name a database or a schema: it stays read-only.
    assert_eq!(editable(&session, "select * from main.orders").await, None);
    run(
        &session,
        "create temp table orders (id integer primary key, item varchar);
         insert into orders values (1, 'shadow')",
    )
    .await;
    assert_eq!(editable(&session, "select * from orders").await, None);
}
