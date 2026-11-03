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
