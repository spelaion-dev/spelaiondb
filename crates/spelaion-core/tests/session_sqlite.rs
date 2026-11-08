//! Sessions over in-memory SQLite: they always run, with nothing external.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver};
use spelaion_driver_sqlite::SqliteDriver;

async fn session() -> Session {
    let driver = SqliteDriver::default();
    let conn = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .expect("opening :memory:");
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

#[tokio::test]
async fn trigger_bodies_run_as_one_statement() {
    let session = session().await;
    let script = "create table t (x int);
                  create table log (msg text);
                  create trigger t_ins after insert on t begin
                      insert into log values (case when new.x > 0 then 'pos' else 'neg' end);
                  end;
                  insert into t values (1), (-1);
                  select msg from log order by msg";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    assert!(
        matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished {
                executed: 5,
                failed: false,
                ..
            })
        ),
        "{events:#?}"
    );
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        session
            .results()
            .get(result)
            .unwrap()
            .read()
            .unwrap()
            .rows(),
        2
    );
}

#[tokio::test]
async fn syntax_errors_point_into_the_script() {
    let session = session().await;
    let script = "select 1;\nselect * form t";
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    let error = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::StatementFailed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("should fail");
    let offset = error.script_offset.expect("position");
    assert!(
        script[offset..].starts_with("form"),
        "points at {:?}",
        &script[offset..]
    );
}

#[tokio::test]
async fn tables_and_views_are_counted_exactly_and_the_result_is_freed() {
    let session = session().await;
    let setup: Vec<CoreEvent> = session
        .run_script(
            "create table \"order lines\" (x int); \
             insert into \"order lines\" values (1), (2), (3); \
             create view big as select * from \"order lines\" where x > 1",
        )
        .collect()
        .await;
    assert!(matches!(
        setup.last(),
        Some(CoreEvent::ScriptFinished { failed: false, .. })
    ));
    let before = session.results().len();
    assert_eq!(
        spelaion_core::count_rows(&session, "main", "order lines").await,
        Ok(3)
    );
    assert_eq!(
        spelaion_core::count_rows(&session, "main", "big").await,
        Ok(2)
    );
    assert_eq!(session.results().len(), before);
    let err = spelaion_core::count_rows(&session, "main", "missing")
        .await
        .unwrap_err();
    assert!(err.contains("no such table"), "{err}");
}

/// The first result of a script, for the edit tests.
async fn first_result(session: &Session, sql: &str) -> spelaion_core::ResultId {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result")
}

#[tokio::test]
async fn query_pages_append_atomically_and_count_all_matching_rows() {
    let session = session().await;
    let _: Vec<CoreEvent> = session.run_script("create table t (id integer primary key, name text); insert into t values (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')").collect().await;
    let original = "select id, name from t where id > 1 order by id";
    let first = spelaion_core::query_page_sql(session.dialect().as_ref(), original, 2, 0).unwrap();
    let id = first_result(&session, &first).await;
    assert_eq!(session.results().get(id).unwrap().read().unwrap().rows(), 2);
    assert_eq!(
        spelaion_core::count_query_rows(&session, original)
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        session.results().len(),
        1,
        "count releases its temporary result"
    );
    assert_eq!(
        spelaion_core::append_query_page(&session, id, original, 2, 2)
            .await
            .unwrap(),
        2
    );
    assert_eq!(session.results().get(id).unwrap().read().unwrap().rows(), 4);
    assert_eq!(
        session.results().len(),
        1,
        "fetch releases its temporary result"
    );
    assert!(
        spelaion_core::append_query_page(&session, id, original, 2, 2)
            .await
            .is_err(),
        "a stale offset cannot append twice"
    );
    let result = session.results().get(id).unwrap();
    assert_eq!(result.read().unwrap().rows(), 4);
    result.write().unwrap().filter(Some("e")).unwrap();
    assert!(
        spelaion_core::append_query_page(&session, id, original, 2, 4)
            .await
            .is_err(),
        "a filtered view cannot be extended"
    );
    assert_eq!(result.read().unwrap().total_rows(), 4);
    assert_eq!(session.results().len(), 1);
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

#[tokio::test]
async fn table_edits_apply_in_one_transaction_by_primary_key() {
    use spelaion_core::{CellInput, EditError, RowChange, TablePage};
    let session = session().await;
    let _: Vec<CoreEvent> = session
        .run_script(
            "create table \"line item\" (id integer primary key, name text, qty integer default 1); \
             insert into \"line item\" values (1, 'one', 1), (2, 'two', 2), (3, 'three', 3)",
        )
        .collect()
        .await;
    let table = session
        .connection()
        .describe_table("main", "line item")
        .await
        .unwrap();
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
    let statements = {
        let stored = stored.read().unwrap();
        // Rows shown: 3, 2, 1 (id desc).
        spelaion_core::edit_statements(
            session.dialect().as_ref(),
            &table,
            &stored,
            &[
                RowChange::Update {
                    row: 0,
                    values: vec![
                        (1, CellInput::Text("it's three".into())),
                        (2, CellInput::Null),
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
            "UPDATE \"line item\" SET name = 'it''s three', qty = NULL WHERE id = 3",
            "DELETE FROM \"line item\" WHERE id = 2",
            "INSERT INTO \"line item\" (id, name) VALUES (10, 'ten')",
        ]
    );
    assert_eq!(
        spelaion_core::apply_edits(&session, &statements).await,
        Ok(3)
    );
    assert_eq!(
        texts(&session, "select * from \"line item\" order by id").await,
        ["1|one|1", "3|it's three|NULL", "10|ten|1"]
    );

    // A row that vanished meanwhile: nothing is applied, not even the valid change.
    let stale = {
        let stored = stored.read().unwrap();
        spelaion_core::edit_statements(
            session.dialect().as_ref(),
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
    // The session is usable again (the transaction was rolled back).
    assert_eq!(
        texts(&session, "select count(*) from \"line item\"").await,
        ["3"]
    );

    // Bad input is refused before anything runs; so are keyless tables.
    let stored_guard = stored.read().unwrap();
    assert!(matches!(
        spelaion_core::edit_statements(
            session.dialect().as_ref(),
            &table,
            &stored_guard,
            &[RowChange::Update {
                row: 0,
                values: vec![(2, CellInput::Text("lots".into()))],
            }],
        ),
        Err(EditError::BadValue { .. })
    ));
    let mut keyless = table.clone();
    keyless.primary_key.clear();
    assert!(matches!(
        spelaion_core::edit_statements(
            session.dialect().as_ref(),
            &keyless,
            &stored_guard,
            &[RowChange::Delete { row: 0 }],
        ),
        Err(EditError::NoPrimaryKey(_))
    ));
}

/// The table a query's result would be edited as, as `(schema, table)`.
async fn editable(session: &Session, sql: &str, read_only: bool) -> Option<(String, String)> {
    let id = first_result(session, sql).await;
    spelaion_core::editable_query_table(session, id, sql, read_only)
        .await
        .unwrap()
        .map(|t| (t.object.schema, t.object.name))
}

#[tokio::test]
async fn query_results_are_editable_only_as_the_table_they_came_from() {
    let session = session().await;
    let _: Vec<CoreEvent> = session
        .run_script(
            "create table orders (id integer primary key, item text, qty int);
             create table nokey (a int);
             create view v_orders as select * from orders;
             insert into orders values (1, 'pen', 2);
             insert into nokey values (1)",
        )
        .collect()
        .await;
    let main = Some(("main".to_string(), "orders".to_string()));
    assert_eq!(
        editable(&session, "select * from orders", false).await,
        main
    );
    assert_eq!(
        editable(&session, "select id, qty from orders where qty > 0", false).await,
        main
    );
    assert_eq!(
        editable(&session, "select * from main.orders", false).await,
        main
    );
    for sql in [
        // No primary key in the result, or no primary key at all.
        "select item, qty from orders",
        "select * from nokey",
        // A view, a column twice, a join.
        "select * from v_orders",
        "select id, id from orders",
        "select * from orders join nokey on 1 = 1",
        // SQLite turns an unknown double-quoted name into a string: not a column.
        "select id, \"nope\" from orders",
    ] {
        assert_eq!(editable(&session, sql, false).await, None, "{sql}");
    }
    assert_eq!(
        editable(&session, "select * from orders", true).await,
        None,
        "a read-only connection never edits"
    );

    // A temporary table shadows `orders`: its rows must never be written to `main.orders`.
    let _: Vec<CoreEvent> = session
        .run_script(
            "create temp table orders (id integer primary key, item text, qty int);
             insert into temp.orders values (1, 'shadow', 9)",
        )
        .collect()
        .await;
    assert_eq!(
        editable(&session, "select * from orders", false).await,
        None
    );
    assert_eq!(
        editable(&session, "select * from main.orders", false).await,
        main
    );
    assert_eq!(
        editable(&session, "select * from temp.orders", false).await,
        Some(("temp".to_string(), "orders".to_string()))
    );
}
