//! Sessions against a real MySQL. Needs `SPELUNK_TEST_MYSQL=host:port`
//! (see `crates/spelunk-driver-mysql/tests/mysql.rs`).

use std::sync::Arc;

use futures_util::StreamExt;
use spelunk_core::{ColumnFilter, CoreEvent, FilterOp, ResultStore, Session};
use spelunk_driver_api::{ConnectParams, Driver, Secret};
use spelunk_driver_mysql::MysqlDriver;

async fn session() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELUNK_TEST_MYSQL") else {
        eprintln!("SPELUNK_TEST_MYSQL not set: test skipped");
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
    let driver = MysqlDriver::mysql();
    let conn = driver.connect(&params).await.expect("connecting");
    Some(Session::new(
        conn,
        driver.dialect(),
        Arc::new(ResultStore::new()),
    ))
}

#[tokio::test]
async fn processlist_filtered_by_command_without_a_where() {
    // A second, idle connection shows up as "Sleep".
    let (Some(session), Some(_idle)) = (session().await, session().await) else {
        return;
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
    // Only our session runs a statement; NULL Info rows never match LIKE.
    let ours = stored
        .filter_columns(vec![
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
    let page = spelunk_core::TablePage {
        schema: "spelunk".into(),
        name: "core pages".into(),
        filter: "kind = 'odd'".into(),
        order_by: "id desc".into(),
        limit: 5,
        offset: 5,
    };
    let sql = spelunk_core::table_page_sql(session.dialect().as_ref(), &page).unwrap();
    assert!(sql.contains("`spelunk`.`core pages`"), "{sql}");
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
        let first = spelunk_driver_api::Value::from_array(
            first.column(0).as_ref(),
            0,
            stored.schema().columns[0].logical,
        );
        assert_eq!(first, Some(spelunk_driver_api::Value::Int(15)));
    }
    assert_eq!(
        spelunk_core::count_table_rows(&session, "spelunk", "core pages", "kind = 'odd'").await,
        Ok(13)
    );
    let _: Vec<CoreEvent> = session
        .run_script("drop table `core pages`")
        .collect()
        .await;
}
