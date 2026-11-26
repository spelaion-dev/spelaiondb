//! History is tested against real SQLite sessions and the on-disk personal store.

use crate::engines;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, HistoryEntry, QueryHistory, ResultStore, ScriptOptions, Session};
use spelaion_driver_api::{ConnectParams, Driver, ExecOptions};

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
    let driver = engines::sqlite();
    let connection = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    Session::new(
        connection,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
    .with_options(ExecOptions {
        batch_size: 2,
        ..Default::default()
    })
}

async fn entries(history: &QueryHistory) -> Vec<HistoryEntry> {
    history
        .list("/repo".into(), "".into(), 100, 0)
        .await
        .unwrap()
}

#[tokio::test]
async fn records_each_executed_statement_and_counts_returned_rows_once() {
    let _turn = crate::serial::file_turn(module_path!());
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
            failed: false,
            ..
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
    let _turn = crate::serial::file_turn(module_path!());
    let (history, _path) = history("paged");
    let session = session().await;
    let _: Vec<_> = session
        .run_script("create table t (id integer primary key); insert into t values (1), (2), (3)")
        .collect()
        .await;
    let original = "select * from t order by id;";
    let page = spelaion_core::first_query_page_sql(&session, original, 2, &[])
        .await
        .unwrap();
    let _: Vec<_> = history
        .run_script_with_original(
            &session,
            &page,
            "/repo".into(),
            "shop".into(),
            Some(original.into()),
            ScriptOptions::default(),
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
    let _turn = crate::serial::file_turn(module_path!());
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
            failed: true,
            ..
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
async fn a_run_that_continues_records_the_statements_after_the_error() {
    let _turn = crate::serial::file_turn(module_path!());
    let (history, _) = history("continue");
    let session = session().await;
    let sql = "select 'ñ';
select * from missing_table;
select 'after the error'";
    let events: Vec<_> = history
        .run_script_with_original(
            &session,
            sql,
            "/repo".into(),
            "shop".into(),
            None,
            ScriptOptions {
                on_error: spelaion_core::OnError::Continue,
                statement_timeout: None,
                statement_savepoints: false,
            },
        )
        .collect()
        .await;
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished {
            executed: 3,
            failed: true,
            ..
        })
    ));
    let saved = entries(&history).await;
    assert_eq!(saved.len(), 3);
    assert!(saved[0].statement.contains("after the error"));
    assert_eq!(saved[0].error, None);
    assert!(saved[1].error.as_ref().unwrap().contains("missing_table"));
}

#[tokio::test]
async fn an_error_message_cannot_turn_an_ordinary_failure_into_cancellation() {
    let _turn = crate::serial::file_turn(module_path!());
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
    let _turn = crate::serial::file_turn(module_path!());
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
    let _turn = crate::serial::file_turn(module_path!());
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

/// A script with `${name}` variables (SDB-383) is recorded as it ran: with the values the
/// connection's saved variables and the tab's gave it, the tab's winning.
#[tokio::test]
async fn a_statement_with_variables_is_recorded_as_it_ran() {
    let _turn = crate::serial::file_turn(module_path!());
    let (history, _path) = history("variables");
    let session = session().await;
    history
        .set_connection_variable(
            "/repo".into(),
            "shop".into(),
            "low".into(),
            Some("1".into()),
        )
        .await
        .unwrap();
    history
        .set_connection_variable(
            "/repo".into(),
            "shop".into(),
            "high".into(),
            Some("5".into()),
        )
        .await
        .unwrap();
    // A name that cannot be a variable never gets into the store.
    assert!(
        history
            .set_connection_variable(
                "/repo".into(),
                "shop".into(),
                "env:HOME".into(),
                Some("x".into())
            )
            .await
            .is_err()
    );
    let mut values: std::collections::HashMap<String, String> = history
        .connection_variables("/repo".into(), "shop".into())
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(values.len(), 2);
    // The tab's own value wins over the connection's.
    values.insert("high".into(), "3".into());
    let script = "select ${low} + ${high} as total, '${low}' as text";
    let ran =
        spelaion_core::substitute_variables(session.dialect().as_ref(), script, &values).unwrap();
    assert_eq!(ran, "select 1 + 3 as total, '${low}' as text");
    let events: Vec<_> = history
        .run_script(&session, &ran, "/repo".into(), "shop".into())
        .collect()
        .await;
    assert!(matches!(
        events.last(),
        Some(CoreEvent::ScriptFinished { failed: false, .. })
    ));
    let saved = entries(&history).await;
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].statement, ran);
    // Another connection, or another workspace, has none of these variables.
    assert!(
        history
            .connection_variables("/repo".into(), "other".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        history
            .connection_variables("/elsewhere".into(), "shop".into())
            .await
            .unwrap()
            .is_empty()
    );
}
