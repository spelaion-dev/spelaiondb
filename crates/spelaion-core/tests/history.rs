//! History is tested against real SQLite sessions and the on-disk personal store.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, HistoryEntry, QueryHistory, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver, ExecOptions};
use spelaion_driver_sqlite::SqliteDriver;

fn history(name: &str) -> (QueryHistory, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "spelaion-core-history-{name}-{}-{nonce}.db",
        std::process::id()
    ));
    (QueryHistory::open(path.clone()).unwrap(), path)
}

async fn session() -> Session {
    let driver = SqliteDriver::default();
    let connection = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    Session::new(connection, driver.dialect(), Arc::new(ResultStore::new()))
        .with_options(ExecOptions { batch_size: 2 })
}

async fn entries(history: &QueryHistory) -> Vec<HistoryEntry> {
    history
        .list("/repo".into(), "".into(), 100, 0)
        .await
        .unwrap()
}

#[tokio::test]
async fn records_each_executed_statement_and_counts_returned_rows_once() {
    let (history, path) = history("success");
    let session = session().await;
    let sql = "create table t (value text);\ninsert into t values ('uno'), ('dos'), ('tres'), ('ñ'), ('五');\nselect * from t order by value; -- only a comment\n;";
    let expected: Vec<_> = session
        .dialect()
        .split_statements(sql)
        .into_iter()
        .map(|span| span.text(sql).to_string())
        .collect();
    let events: Vec<_> = history
        .run_script(&session, sql, "/repo".into(), "shop".into())
        .collect()
        .await;
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished {
            executed: 3,
            failed: false
        })
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, CoreEvent::HistoryWarning { .. }))
    );
    let mut saved = entries(&history).await;
    saved.reverse();
    assert_eq!(
        saved.iter().map(|e| &e.statement).collect::<Vec<_>>(),
        expected.iter().collect::<Vec<_>>()
    );
    assert_eq!(saved.iter().map(|e| e.rows).collect::<Vec<_>>(), [0, 0, 5]);
    assert!(
        saved
            .iter()
            .all(|e| e.at_ms > 0 && e.duration_ms >= 0 && e.error.is_none() && !e.cancelled)
    );
    assert!(
        saved
            .iter()
            .all(|e| e.connection == "shop" && e.workspace == "/repo")
    );

    // Completion is delivered after writes finish; another process can read them immediately.
    let reopened = QueryHistory::open(path).unwrap();
    assert_eq!(entries(&reopened).await.len(), 3);
}

#[tokio::test]
async fn paged_queries_keep_the_original_sql_in_history() {
    let (history, _path) = history("paged");
    let session = session().await;
    let _: Vec<_> = session
        .run_script("create table t (id integer); insert into t values (1), (2), (3)")
        .collect()
        .await;
    let original = "select * from t order by id;";
    let page = spelaion_core::query_page_sql(session.dialect().as_ref(), original, 2, 0).unwrap();
    let _: Vec<_> = history
        .run_script_with_original(
            &session,
            &page,
            "/repo".into(),
            "shop".into(),
            Some(original.into()),
        )
        .collect()
        .await;
    let saved = entries(&history).await;
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].statement, original);
    assert_eq!(saved[0].rows, 2);
}

#[tokio::test]
async fn records_errors_without_recording_the_unexecuted_remainder() {
    let (history, _) = history("failure");
    let session = session().await;
    let sql = "select 'ñ';\nselect * from missing_table;\nselect 'not executed'";
    let events: Vec<_> = history
        .run_script(&session, sql, "/repo".into(), "shop".into())
        .collect()
        .await;
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished {
            executed: 2,
            failed: true
        })
    ));
    let saved = entries(&history).await;
    assert_eq!(saved.len(), 2);
    assert!(saved[0].statement.contains("missing_table"));
    assert!(saved[0].error.as_ref().unwrap().contains("missing_table"));
    assert_eq!(saved[0].rows, 0);
    assert!(!saved[0].cancelled);
    assert_eq!(saved[1].rows, 1);
    assert_eq!(saved[1].error, None);
    assert!(!saved[1].cancelled);
    assert!(
        history
            .list("/repo".into(), "not executed".into(), 10, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_error_message_cannot_turn_an_ordinary_failure_into_cancellation() {
    let (history, _) = history("cancel-message");
    let session = session().await;
    let setup: Vec<_> = session
        .run_script(
            "create table t (value int); create trigger reject_insert before insert on t begin select raise(abort, 'Query cancelled'); end;",
        )
        .collect()
        .await;
    assert!(matches!(
        setup.last(),
        Some(CoreEvent::ScriptFinished { failed: false, .. })
    ));
    let events: Vec<_> = history
        .run_script(
            &session,
            "insert into t values (1)",
            "/repo".into(),
            "shop".into(),
        )
        .collect()
        .await;
    assert!(events.iter().any(|event| matches!(
        event,
        CoreEvent::StatementFailed { error, .. } if !error.cancelled
    )));
    let saved = entries(&history).await;
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].error.as_deref(), Some("Query cancelled"));
    assert!(!saved[0].cancelled);
}

#[tokio::test]
async fn toggles_cover_open_sessions_and_scripts_already_in_flight() {
    let (history, path) = history("disabled");
    let session = session().await;
    history
        .set_enabled("/repo".into(), "shop".into(), false)
        .await
        .unwrap();
    let reopened = QueryHistory::open(path).unwrap();
    assert!(
        !reopened
            .enabled("/repo".into(), "shop".into())
            .await
            .unwrap()
    );

    // Enabling after a disabled script has started must not save that script's SQL.
    let mut stream = history.run_script(
        &session,
        "select 'private_value'",
        "/repo".into(),
        "shop".into(),
    );
    assert!(matches!(
        stream.next().await,
        Some(CoreEvent::StatementStarted { .. })
    ));
    history
        .set_enabled("/repo".into(), "shop".into(), true)
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    assert!(entries(&history).await.is_empty());

    // Disabling on an already open session takes effect before an in-flight insert.
    let mut stream = history.run_script(
        &session,
        "select 'another_private_value'",
        "/repo".into(),
        "shop".into(),
    );
    assert!(matches!(
        stream.next().await,
        Some(CoreEvent::StatementStarted { .. })
    ));
    history
        .set_enabled("/repo".into(), "shop".into(), false)
        .await
        .unwrap();
    let _: Vec<_> = stream.collect().await;
    assert!(entries(&history).await.is_empty());

    // A failed sensitive statement and its server error are also skipped.
    let _: Vec<_> = history
        .run_script(
            &session,
            "select missing_private_value",
            "/repo".into(),
            "shop".into(),
        )
        .collect()
        .await;
    assert!(entries(&history).await.is_empty());
    assert!(
        reopened
            .enabled("/repo".into(), "other".into())
            .await
            .unwrap()
    );
    assert!(
        reopened
            .enabled("/other".into(), "shop".into())
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn cancellation_saves_partial_row_count_and_error() {
    let (history, _) = history("cancelled");
    let session = session().await;
    let sql = "with recursive n(x) as (select 1 union all select x + 1 from n where x < 100000000) select x from n";
    let task = async {
        let mut stream = history.run_script(&session, sql, "/repo".into(), "shop".into());
        let mut returned = 0;
        while let Some(event) = stream.next().await {
            if let CoreEvent::Rows { total_rows, .. } = event {
                returned = total_rows;
                session.cancel().await.unwrap();
                break;
            }
        }
        assert!(returned > 0);
        let remaining: Vec<_> = stream.collect().await;
        assert!(remaining.iter().any(
            |event| matches!(event, CoreEvent::StatementFailed { error, .. } if error.cancelled)
        ));
        for event in &remaining {
            if let CoreEvent::Rows { total_rows, .. } = event {
                returned = *total_rows;
            }
        }
        let saved = entries(&history).await;
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].rows, returned as i64);
        assert_eq!(saved[0].error.as_deref(), Some("Query cancelled"));
        assert!(saved[0].cancelled);
        assert_eq!(saved[0].statement, sql);
    };
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap();
}
