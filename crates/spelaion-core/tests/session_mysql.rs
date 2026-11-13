//! Sessions against a real MySQL. Needs `SPELAION_TEST_MYSQL=host:port`
//! (see `crates/spelaion-driver-mysql/tests/mysql.rs`).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{ColumnFilter, CoreEvent, FilterOp, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, Secret};
use spelaion_driver_mysql::MysqlDriver;

async fn session() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELAION_TEST_MYSQL") else {
        eprintln!("SPELAION_TEST_MYSQL not set: test skipped");
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
    let driver = MysqlDriver::mysql();
    let conn = driver.connect(&params).await.expect("connecting");
    Some(Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    ))
}

#[tokio::test]
async fn processlist_filtered_by_command_without_a_where() {
    // A second, idle connection shows up as "Sleep".
    let (Some(session), Some(_idle)) = (session().await, session().await) else {
        return;
    };
    // Other tests list processes at the same time: ours is told apart by its connection id.
    let own_id = {
        let events: Vec<CoreEvent> = session.run_script("select connection_id()").collect().await;
        let id = events
            .iter()
            .find_map(|e| match e {
                CoreEvent::ResultStarted { result, .. } => Some(*result),
                _ => None,
            })
            .expect("a result");
        let stored = session.results().get(id).unwrap();
        let stored = stored.read().unwrap();
        let batch = &stored.window(0, 1)[0];
        spelaion_driver_api::Value::from_array(
            batch.column(0).as_ref(),
            0,
            stored.schema().columns[0].logical,
        )
        .unwrap()
        .to_string()
    };
    let events: Vec<CoreEvent> = session.run_script("show full processlist").collect().await;
    let id = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result");
    let stored = session.results().get(id).unwrap();
    let mut stored = stored.write().unwrap();
    let command = stored
        .schema()
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("command"))
        .expect("a Command column");
    let info = stored
        .schema()
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("info"))
        .expect("an Info column");
    let total = stored.total_rows();
    let sleeping = stored
        .filter_columns(vec![ColumnFilter {
            column: command,
            op: FilterOp::Eq,
            value: "sleep".into(),
        }])
        .unwrap();
    assert!(sleeping >= 1, "the idle connection sleeps");
    let busy = stored
        .filter_columns(vec![ColumnFilter {
            column: command,
            op: FilterOp::NotEq,
            value: "Sleep".into(),
        }])
        .unwrap();
    assert!(busy >= 1, "our own query is running");
    assert_eq!(sleeping + busy, total);
    // Our session runs this statement; NULL Info rows never match LIKE.
    let process_id = stored
        .schema()
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("id"))
        .expect("an Id column");
    let ours = stored
        .filter_columns(vec![
            ColumnFilter {
                column: process_id,
                op: FilterOp::Eq,
                value: own_id,
            },
            ColumnFilter {
                column: command,
                op: FilterOp::NotEq,
                value: "Sleep".into(),
            },
            ColumnFilter {
                column: info,
                op: FilterOp::Like,
                value: "%processlist%".into(),
            },
        ])
        .unwrap();
    assert_eq!(ours, 1);
}

#[tokio::test]
async fn process_monitor_lists_sessions_and_checks_targets_before_closing() {
    let (Some(monitor), Some(target)) = (session().await, session().await) else {
        return;
    };
    let target_id = spelaion_core::list_processes(&target).await.unwrap().own_id;
    let snapshot = spelaion_core::list_processes(&monitor).await.unwrap();
    let process = snapshot
        .processes
        .iter()
        .find(|p| p.id == target_id)
        .expect("target is visible");
    assert_eq!(process.command.to_lowercase(), "sleep");
    let mut target_info = spelaion_core::ProcessTarget {
        id: target_id.clone(),
        user: process.user.clone(),
        host: "incorrect host".into(),
        command: process.command.clone(),
        sql: process.sql.clone(),
        identity: process.identity.clone(),
    };
    assert!(
        spelaion_core::control_process(&monitor, &target_info, true, false, true)
            .await
            .is_err()
    );
    target_info.host = process.host.clone();
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
    target_info.command = "unexpected".into();
    assert!(
        spelaion_core::control_process(&monitor, &target_info, true, false, true)
            .await
            .is_err()
    );
    target_info.command = process.command.clone();
    let own = snapshot
        .processes
        .iter()
        .find(|p| p.id == snapshot.own_id)
        .expect("monitor is visible");
    let own_info = spelaion_core::ProcessTarget {
        id: own.id.clone(),
        user: own.user.clone(),
        host: own.host.clone(),
        command: own.command.clone(),
        sql: own.sql.clone(),
        identity: own.identity.clone(),
    };
    assert!(
        spelaion_core::control_process(&monitor, &own_info, true, false, true)
            .await
            .is_err()
    );
    spelaion_core::control_process(&monitor, &target_info, true, false, true)
        .await
        .unwrap();
    let after = spelaion_core::list_processes(&monitor).await.unwrap();
    assert!(!after.processes.iter().any(|p| p.id == target_id));
}

#[tokio::test]
async fn process_monitor_cancels_a_query_without_closing_its_session() {
    let (Some(monitor), Some(target)) = (session().await, session().await) else {
        return;
    };
    let target = Arc::new(target);
    let target_id = spelaion_core::list_processes(&target).await.unwrap().own_id;
    let task = tokio::spawn({
        let target = target.clone();
        async move {
            target
                .run_script("SELECT SLEEP(30)")
                .collect::<Vec<_>>()
                .await
        }
    });
    let mut found = None;
    for _ in 0..100 {
        let snapshot = spelaion_core::list_processes(&monitor).await.unwrap();
        if let Some(p) = snapshot.processes.into_iter().find(|p| {
            p.id == target_id
                && p.sql
                    .as_deref()
                    .is_some_and(|sql| sql.contains("SLEEP(30)"))
        }) {
            found = Some(p);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let process = found.expect("running query is visible");
    let mut target_info = spelaion_core::ProcessTarget {
        id: process.id,
        user: process.user,
        host: process.host,
        command: process.command,
        sql: Some("a different query".into()),
        identity: process.identity,
    };
    assert!(
        spelaion_core::control_process(&monitor, &target_info, false, false, true)
            .await
            .is_err()
    );
    target_info.sql = process.sql;
    spelaion_core::control_process(&monitor, &target_info, false, false, true)
        .await
        .unwrap();
    let events = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    // MySQL may return SLEEP's interrupted result (1) rather than an error.
    if !events
        .iter()
        .any(|event| matches!(event, CoreEvent::StatementFailed { .. }))
    {
        let id = events
            .iter()
            .find_map(|event| match event {
                CoreEvent::ResultStarted { result, .. } => Some(*result),
                _ => None,
            })
            .expect("SLEEP returned a result");
        let result = target.results().get(id).unwrap();
        let result = result.read().unwrap();
        let batch = result.window(0, 1).remove(0);
        let value = spelaion_driver_api::Value::from_array(
            batch.column(0).as_ref(),
            0,
            result.schema().columns[0].logical,
        )
        .unwrap();
        assert_eq!(value.to_string(), "1");
    }
    let after: Vec<_> = target.run_script("SELECT 1").collect().await;
    assert!(matches!(
        after.last(),
        Some(CoreEvent::ScriptFinished { failed: false, .. })
    ));
}

#[tokio::test]
async fn table_pages_and_counts_run_on_mysql() {
    let Some(session) = session().await else {
        return;
    };
    let setup: Vec<CoreEvent> = session
        .run_script(
            "drop table if exists `core pages`; \
             create table `core pages` (id int primary key, kind varchar(10)); \
             insert into `core pages` select n, if(n % 2, 'odd', 'even') from \
             (with recursive s(n) as (select 1 union all select n + 1 from s where n < 25) select n from s) x",
        )
        .collect()
        .await;
    assert!(matches!(
        setup.last(),
        Some(CoreEvent::ScriptFinished { failed: false, .. })
    ));
    let page = spelaion_core::TablePage {
        schema: "spelaion".into(),
        name: "core pages".into(),
        filter: "kind = 'odd'".into(),
        order_by: "id desc".into(),
        limit: 5,
        offset: 5,
    };
    let sql = spelaion_core::table_page_sql(session.dialect().as_ref(), &page).unwrap();
    assert!(sql.contains("`spelaion`.`core pages`"), "{sql}");
    let events: Vec<CoreEvent> = session.run_script(&sql).collect().await;
    let id = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result");
    {
        let stored = session.results().get(id).unwrap();
        let stored = stored.read().unwrap();
        assert_eq!(stored.rows(), 5);
        // Odd ids descending: 25, 23, 21, 19, 17 | 15, 13, …
        let first = stored.window(0, 1).remove(0);
        let first = spelaion_driver_api::Value::from_array(
            first.column(0).as_ref(),
            0,
            stored.schema().columns[0].logical,
        );
        assert_eq!(first, Some(spelaion_driver_api::Value::Int(15)));
    }
    assert_eq!(
        spelaion_core::count_table_rows(&session, "spelaion", "core pages", "kind = 'odd'").await,
        Ok(13)
    );
    let _: Vec<CoreEvent> = session
        .run_script("drop table `core pages`")
        .collect()
        .await;
}
