//! Dumps written by `mysqldump` and `mariadb-dump`, run from the editor
//! (`Session::run_script`). Both write views, triggers and the settings around them as
//! executable comments (`/*!50001 … */`, MariaDB's `/*M!100616 … */`), whose body the server
//! runs: they are statements, not comments. Needs `SPELAION_TEST_MYSQL` and
//! `SPELAION_TEST_MARIADB` (see `testing/docker`).
//!
//! The fixtures in `tests/fixtures/mysqldump` were written by the test servers' own tools
//! (`mysqldump` 8.4.13 and `mariadb-dump` 11.4.13, `--no-tablespaces --skip-dump-date`) from a
//! table `dump_t` with a trigger `dump_t_bi` and a view `dump_v`.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{CoreEvent, ResultStore, Risk, Session, check_script};
use spelaion_driver_api::{ConnectParams, Driver, Secret, Value};
use spelaion_driver_mysql::MysqlDriver;

const MYSQL_DUMP: &str = include_str!("fixtures/mysqldump/mysql.sql");
const MARIADB_DUMP: &str = include_str!("fixtures/mysqldump/mariadb.sql");

async fn session(var: &str, driver: MysqlDriver) -> Option<Session> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: test skipped");
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
    let conn = driver.connect(&params).await.expect("connecting");
    Some(Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    ))
}

/// Runs a script; panics on a failed statement. Returns how many statements finished.
async fn run(session: &Session, script: &str) -> usize {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{error:?}");
    }
    events
        .iter()
        .filter(|e| matches!(e, CoreEvent::StatementFinished { .. }))
        .count()
}

/// The first cell of a query, as text.
async fn value(session: &Session, sql: &str) -> String {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {error:?}"),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{sql}: no result"));
    let stored = session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    let batch = &stored.window(0, 1)[0];
    Value::from_array(
        batch.column(0).as_ref(),
        0,
        stored.schema().columns[0].logical,
    )
    .unwrap()
    .to_string()
}

const CLEAN: &str = "drop view if exists dump_v; drop table if exists dump_t";

#[tokio::test]
async fn dumps_restore_their_views_and_triggers() {
    for (var, driver, dump) in [
        ("SPELAION_TEST_MYSQL", MysqlDriver::mysql(), MYSQL_DUMP),
        (
            "SPELAION_TEST_MARIADB",
            MysqlDriver::mariadb(),
            MARIADB_DUMP,
        ),
    ] {
        let Some(session) = session(var, driver).await else {
            continue;
        };
        run(&session, CLEAN).await;
        let statements = session.dialect().split_statements(dump).len();
        assert_eq!(run(&session, dump).await, statements, "{var}");
        // The settings around the view and the trigger ran too: these alone are dozens.
        assert!(statements > 30, "{var}: {statements} statements");

        let catalog = |kind: &str, name: &str| {
            format!(
                "select count(*) from information_schema.{kind} \
                 where {}_schema = database() and {}_name = '{name}'",
                if kind == "views" { "table" } else { "trigger" },
                if kind == "views" { "table" } else { "trigger" },
            )
        };
        assert_eq!(
            value(&session, &catalog("views", "dump_v")).await,
            "1",
            "{var}"
        );
        assert_eq!(
            value(&session, &catalog("triggers", "dump_t_bi")).await,
            "1",
            "{var}"
        );
        assert_eq!(
            value(&session, "select doubled from dump_v where id = 2").await,
            "40",
            "{var}"
        );
        assert_eq!(
            value(&session, "select note from dump_t where id = 1").await,
            "it's; here",
            "{var}"
        );
        // The trigger runs: it adds one to `a`.
        run(&session, "insert into dump_t (id) values (3)").await;
        assert_eq!(
            value(&session, "select a from dump_t where id = 3").await,
            "1",
            "{var}"
        );
        // The dump put the session's settings back as they were.
        assert_eq!(
            value(&session, "select @@session.time_zone = @@global.time_zone").await,
            "1",
            "{var}"
        );
        run(&session, CLEAN).await;
    }
}

/// What the server would run inside an executable comment is confirmed like any statement:
/// once such a statement is run, the guard must see inside it (MariaDB's `/*M!` too).
#[test]
fn the_guard_reads_executable_comments_as_the_server_does() {
    let dialect = spelaion_dialect::for_engine(MysqlDriver::mysql().engine()).expect("a dialect");
    let risk = |sql: &str| {
        check_script(dialect.as_ref(), sql, &Safeguards::default(), false)
            .err()
            .map(|e| e.risks[0].risk)
    };
    assert_eq!(risk("/*!50000 drop table t */"), Some(Risk::Drop));
    assert_eq!(risk("/*!delete from t */"), Some(Risk::DeleteAll));
    assert_eq!(risk("/*M!100000 drop table t */"), Some(Risk::Drop));
    assert_eq!(risk("/*m!100000 truncate table t */"), Some(Risk::Truncate));
    assert_eq!(
        risk("-- comment\n/*M!100000 update t set a = 1 */;"),
        Some(Risk::UpdateAll)
    );
    assert_eq!(risk("/*!40101 SET @x = 1 */"), None);
    assert_eq!(risk("/*M!100616 SET @x = 1 */"), None);
    // A server older than the comment skips its body, and with it the WHERE.
    assert_eq!(
        risk("delete from t /*!99999 where id = 1 */"),
        Some(Risk::DeleteAll)
    );
}
