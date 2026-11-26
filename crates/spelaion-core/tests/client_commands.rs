//! Client command lines (ADR-078, SDB-466) on a real session (in-memory SQLite, always run):
//! nothing resolves a command yet, so a run that reaches one refuses it before anything is
//! sent, with the sentence for what it is, and stops there as any failed statement does. The
//! table a statement after it would have created is never there.

use crate::engines;

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{CoreEvent, ResultStore, Session};
use spelaion_driver_api::{ConnectParams, Driver};

async fn session() -> Session {
    let driver = engines::sqlite();
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

/// The failures of a run, by statement, and how many statements finished.
async fn run(session: &Session, script: &str) -> (Vec<(usize, String)>, usize) {
    let mut failures = Vec::new();
    let mut finished = 0;
    let mut events = session.run_script(script);
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFailed { index, error } => failures.push((index, error.message)),
            CoreEvent::StatementFinished { .. } => finished += 1,
            _ => {}
        }
    }
    (failures, finished)
}

#[tokio::test]
async fn a_command_line_is_refused_before_anything_is_sent() {
    let _turn = crate::serial::file_turn(module_path!());
    let session = session().await;
    for (script, sentence) in [
        (
            "@set x = 1\ncreate table after_set (a int)",
            "nothing runs it here",
        ),
        (
            "-- report\n@export {\"format\": \"csv\"}\ncreate table after_export (a int)",
            "run the query, then use Export",
        ),
        (
            "@spool out.txt\ncreate table after_spool (a int)",
            "@spool is not a command",
        ),
    ] {
        let (failures, finished) = run(&session, script).await;
        assert_eq!(failures.len(), 1, "{script:?}: {failures:?}");
        let (index, message) = &failures[0];
        assert_eq!(*index, 0, "{script:?}");
        assert!(message.contains(sentence), "{script:?}: {message}");
        assert_eq!(finished, 0, "{script:?}: a statement after the line ran");
    }
    // Nothing after a refused line was sent: none of those tables exists.
    let (failures, _) = run(
        &session,
        "select count(*) from sqlite_master where name like 'after_%' having count(*) > 0",
    )
    .await;
    assert!(failures.is_empty(), "{failures:?}");
    let mut events = session.run_script("select name from sqlite_master where name like 'after_%'");
    let mut rows = 0;
    while let Some(event) = events.next().await {
        if let CoreEvent::Rows { total_rows, .. } = event {
            rows = total_rows;
        }
    }
    assert_eq!(rows, 0, "a table after a refused command line exists");
}

#[tokio::test]
async fn an_at_sign_in_sql_still_runs() {
    let _turn = crate::serial::file_turn(module_path!());
    let session = session().await;
    // In a string and in the middle of a statement: SQL as it always was.
    let (failures, finished) = run(&session, "select '\n@set x = 1' as a;\nselect 1,\n  2").await;
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(finished, 2);
}

/// What a resolved run yields, in order: `echo:<text>`, `start:<index>` and `end`.
async fn items(session: &Session, script: &str) -> Vec<String> {
    let resolved = spelaion_core::resolve_script(
        session.dialect().as_ref(),
        script,
        &std::collections::HashMap::new(),
    )
    .expect("resolved");
    let mut out = Vec::new();
    let mut run = spelaion_core::with_echoes(session.run_script(&resolved.text), resolved.echoes);
    while let Some(item) = run.next().await {
        match item {
            spelaion_core::RunItem::Echo(text) => out.push(format!("echo:{text}")),
            spelaion_core::RunItem::Event(CoreEvent::StatementStarted { index, .. }) => {
                out.push(format!("start:{index}"))
            }
            spelaion_core::RunItem::Event(CoreEvent::ScriptFinished { .. }) => {
                out.push("end".into())
            }
            spelaion_core::RunItem::Event(_) => {}
        }
    }
    out
}

#[tokio::test]
async fn echoes_come_when_the_run_reaches_them() {
    let _turn = crate::serial::file_turn(module_path!());
    let session = session().await;
    assert_eq!(
        items(
            &session,
            "@echo first\nselect 1;\n@echo between\nselect 2;\n@echo last"
        )
        .await,
        [
            "echo:first",
            "start:0",
            "echo:between",
            "start:1",
            "echo:last",
            "end"
        ]
    );
    // A run that stops at a failed statement never reaches what comes after it.
    assert_eq!(
        items(
            &session,
            "select 1;\n@echo before\nselect nope from no_such_table;\n@echo never\nselect 3"
        )
        .await,
        ["start:0", "echo:before", "start:1", "end"]
    );
}
