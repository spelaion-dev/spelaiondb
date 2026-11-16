//! Sessions over a real SQL Server (testing/docker). Without
//! `SPELAION_TEST_MSSQL=127.0.0.1:11433` they are skipped, which does not count as tested.

mod engines;

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CellInput, CoreEvent, EditError, ResultStore, RowChange, Session, TablePage};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

async fn session(database: &str) -> Option<Session> {
    let Ok(addr) = std::env::var("SPELAION_TEST_MSSQL") else {
        eprintln!("SPELAION_TEST_MSSQL not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let driver = engines::sqlserver();
    let conn = driver
        .connect(&ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "sa".into(),
            password: Some(Secret::new("Spelaion-test-1")),
            database: database.into(),
            tls: TlsMode::Require,
            ..Default::default()
        })
        .await
        .expect("connecting to SQL Server");
    Some(Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    ))
}

#[tokio::test]
async fn process_monitor_lists_and_ends_sqlserver_sessions() {
    let (Some(monitor), Some(target)) = (session("master").await, session("master").await) else {
        return;
    };
    let target_id = spelaion_core::list_processes(&target).await.unwrap().own_id;
    let snapshot = spelaion_core::list_processes(&monitor).await.unwrap();
    assert!(!snapshot.limited);
    let process = snapshot
        .processes
        .iter()
        .find(|p| p.id == target_id)
        .expect("target session is visible");
    assert_eq!(process.activity, "idle");
    assert!(process.identity.is_some());
    let mut target_info = spelaion_core::ProcessTarget {
        id: process.id.clone(),
        user: process.user.clone(),
        host: process.host.clone(),
        command: process.command.clone(),
        sql: process.sql.clone(),
        identity: Some("outdated session".into()),
    };
    assert!(
        spelaion_core::control_process(&monitor, &target_info, true, false, true)
            .await
            .is_err()
    );
    target_info.identity = process.identity.clone();
    let valid_identity = target_info.identity.take();
    assert!(
        spelaion_core::control_process(&monitor, &target_info, true, false, true)
            .await
            .is_err()
    );
    target_info.identity = valid_identity;
    assert!(
        spelaion_core::control_process(&monitor, &target_info, false, false, true)
            .await
            .is_err()
    );
    assert!(
        spelaion_core::control_process(&monitor, &target_info, true, true, true)
            .await
            .is_err()
    );
    assert!(
        spelaion_core::control_process(&monitor, &target_info, true, false, false)
            .await
            .is_err()
    );
    spelaion_core::control_process(&monitor, &target_info, true, false, true)
        .await
        .unwrap();
    // KILL returns before SQL Server has removed the session: it may still be listed for a
    // moment, so the test waits (up to 10 s) for it to go.
    let mut gone = false;
    for _ in 0..50 {
        gone = !spelaion_core::list_processes(&monitor)
            .await
            .unwrap()
            .processes
            .iter()
            .any(|p| p.id == target_id);
        if gone {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(gone, "session {target_id} is still listed after KILL");
}

#[tokio::test]
async fn process_monitor_shows_the_current_sqlserver_session_without_admin_rights() {
    let Some(admin) = session("master").await else {
        return;
    };
    let addr = std::env::var("SPELAION_TEST_MSSQL").unwrap();
    let (host, port) = addr.rsplit_once(':').unwrap();
    let login = format!("spelaion_monitor_{}", std::process::id());
    let _: Vec<_> = admin
        .run_script(&format!(
            "CREATE LOGIN [{login}] WITH PASSWORD = 'Spelaion-test-1', CHECK_POLICY = OFF"
        ))
        .collect()
        .await;
    let driver = engines::sqlserver();
    let connected = driver
        .connect(&ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: login.clone(),
            password: Some(Secret::new("Spelaion-test-1")),
            database: "master".into(),
            tls: TlsMode::Require,
            ..Default::default()
        })
        .await;
    let result = match connected {
        Ok(conn) => {
            let low = Session::new(
                conn,
                spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
                Arc::new(ResultStore::new()),
            );
            let result = spelaion_core::list_processes(&low).await;
            drop(low);
            result
        }
        Err(error) => Err(error.to_string()),
    };
    // The low session closes in the background, and a login still connected cannot be
    // dropped: try again until it goes, so no login is left behind on the shared server.
    let mut dropped = false;
    for _ in 0..50 {
        let events: Vec<CoreEvent> = admin
            .run_script(&format!("DROP LOGIN [{login}]"))
            .collect()
            .await;
        if !events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. }))
        {
            dropped = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(dropped, "the login {login} could not be dropped");
    let snapshot =
        result.expect("a login can inspect its own process without server-wide permissions");
    assert!(snapshot.limited);
    assert!(snapshot.processes.iter().any(|p| p.id == snapshot.own_id));
}

/// A session in the `spelaion` database (created if needed, as the driver tests do).
async fn spelaion() -> Option<Session> {
    let master = session("master").await?;
    let _: Vec<CoreEvent> = master
        .run_script("if db_id('spelaion') is null create database spelaion")
        .collect()
        .await;
    session("spelaion").await
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

fn results(events: &[CoreEvent]) -> Vec<spelaion_core::ResultId> {
    events
        .iter()
        .filter_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .collect()
}

async fn texts(session: &Session, sql: &str) -> Vec<String> {
    let id = results(&run(session, sql).await)[0];
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

/// A procedure (or any batch) with several result sets: each one is a result, all complete.
#[tokio::test]
async fn batches_with_several_result_sets_give_several_results() {
    let Some(session) = spelaion().await else {
        return;
    };
    let events = run(
        &session,
        "create or alter procedure two_sets as\nselect 1 as a\nselect 'x' as b, 2 as c\ngo\nexec two_sets",
    )
    .await;
    let ids = results(&events);
    assert_eq!(ids.len(), 2, "{events:#?}");
    for id in ids {
        assert!(
            session
                .results()
                .get(id)
                .unwrap()
                .read()
                .unwrap()
                .is_complete()
        );
    }
}

#[tokio::test]
async fn table_data_pages_and_edits_in_one_transaction() {
    let Some(session) = spelaion().await else {
        return;
    };
    run(
        &session,
        "drop table if exists [line item];\n\
         create table [line item] (id int primary key, name nvarchar(40), qty int default 1, \
                                   due date, data varbinary(8), done bit);\n\
         insert into [line item] values (1, N'one', 1, '2024-01-01', 0x61, 0), \
                                        (2, N'two', 2, null, null, 0), (3, N'three', 3, null, null, 0)",
    )
    .await;
    let table = session
        .connection()
        .describe_table("dbo", "line item")
        .await
        .unwrap();
    assert_eq!(table.primary_key, ["id"]);
    let page = |order_by: &str, offset| TablePage {
        schema: "dbo".into(),
        name: "line item".into(),
        filter: String::new(),
        order_by: order_by.into(),
        limit: 100,
        offset,
    };
    // Without an order, the server's; a second page skips rows.
    let sql = spelaion_core::table_page_sql(session.dialect().as_ref(), &page("", 0)).unwrap();
    assert_eq!(texts(&session, &sql).await.len(), 3);
    let sql = spelaion_core::table_page_sql(session.dialect().as_ref(), &page("id", 2)).unwrap();
    assert_eq!(texts(&session, &sql).await, ["3|three|3|NULL|NULL|false"]);
    assert_eq!(
        spelaion_core::count_rows(&session, "dbo", "line item").await,
        Ok(3)
    );

    let sql =
        spelaion_core::table_page_sql(session.dialect().as_ref(), &page("id desc", 0)).unwrap();
    let id = results(&run(&session, &sql).await)[0];
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
                        (1, CellInput::Text("it's ñandú".into())),
                        (2, CellInput::Null),
                        (3, CellInput::Text("2024-12-31".into())),
                        (4, CellInput::Text("\\x00ff".into())),
                        (5, CellInput::Text("true".into())),
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
            "UPDATE dbo.[line item] SET name = N'it''s ñandú', qty = NULL, due = N'2024-12-31', data = 0x00ff, done = 1 WHERE id = 3",
            "DELETE FROM dbo.[line item] WHERE id = 2",
            "INSERT INTO dbo.[line item] (id, name) VALUES (10, N'ten')",
        ]
    );
    assert_eq!(
        spelaion_core::apply_edits(&session, &statements).await,
        Ok(3)
    );
    assert_eq!(
        texts(
            &session,
            "select id, name, qty, due, data, done from [line item] order by id"
        )
        .await,
        [
            "1|one|1|2024-01-01|\\x61|false",
            "3|it's ñandú|NULL|2024-12-31|\\x00ff|true",
            "10|ten|1|NULL|NULL|NULL"
        ]
    );

    // A row that vanished meanwhile: nothing is applied, and the transaction is undone.
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
        texts(&session, "select name from [line item] where id = 1").await,
        ["one"]
    );
    assert_eq!(texts(&session, "select @@trancount").await, ["0"]);
    run(&session, "drop table [line item]").await;
}

async fn editable(session: &Session, sql: &str) -> Option<(String, String)> {
    let id = results(&run(session, sql).await)[0];
    spelaion_core::editable_query_table(session, id, sql, false)
        .await
        .unwrap()
        .map(|t| (t.object.schema, t.object.name))
}

#[tokio::test]
async fn query_results_resolve_their_table_as_the_server_does() {
    let Some(session) = spelaion().await else {
        return;
    };
    run(
        &session,
        "if schema_id('core_eq') is null exec('create schema core_eq')
         drop synonym if exists dbo.core_eq_syn
         drop table if exists dbo.core_eq_orders, core_eq.core_eq_syn
         create table dbo.core_eq_orders (id int primary key, item nvarchar(20))
         insert into dbo.core_eq_orders values (1, N'pen')
         create table core_eq.core_eq_syn (id int primary key, item nvarchar(20))
         create synonym dbo.core_eq_syn for dbo.core_eq_orders",
    )
    .await;
    // After an EXEC the session cannot tell its database, and rows read then are not editable
    // (SDB-137); the app reads it again after every script, as here.
    session.connection().current_database().await.unwrap();
    assert_eq!(
        editable(&session, "select * from core_eq_orders").await,
        Some(("dbo".to_string(), "core_eq_orders".to_string()))
    );
    // The name reaches the synonym in `dbo` (the default schema), not the table with the
    // same name in another schema: editing it there would change the wrong rows.
    assert_eq!(editable(&session, "select * from core_eq_syn").await, None);
    assert_eq!(
        editable(&session, "select * from core_eq.core_eq_syn").await,
        Some(("core_eq".to_string(), "core_eq_syn".to_string()))
    );
}
