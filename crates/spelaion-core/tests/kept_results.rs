//! Results an editor tab keeps (SDB-378), over a real session on in-memory SQLite: they
//! always run, with nothing external. What a run replaces is the core's: a pinned result
//! survives the next run with its rows, and closing it frees it.

mod engines;

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{Placed, Placement, ResultId, ResultStore, ResultTabs, Session};
use spelaion_driver_api::{ConnectParams, Driver, Value};

const EDITOR: &str = "tab-1";
const LIMIT: usize = 20;

struct Editor {
    session: Session,
    store: Arc<ResultStore>,
    tabs: Arc<ResultTabs>,
}

async fn editor() -> Editor {
    let driver = engines::sqlite();
    let conn = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .expect("opening :memory:");
    let store = Arc::new(ResultStore::new());
    let session = Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        store.clone(),
    );
    let tabs = Arc::new(ResultTabs::new(store.clone()));
    Editor {
        session,
        store,
        tabs,
    }
}

impl Editor {
    /// Runs `script` as the app does: the core says what the run replaces, and every event
    /// of the run goes through it.
    async fn run(&self, placement: Placement, script: &str) -> Vec<Placed> {
        let mut results = self
            .tabs
            .begin_run(
                EDITOR,
                placement,
                self.session.dialect(),
                script,
                None,
                LIMIT,
            )
            .expect("the run may start");
        let mut placed = Vec::new();
        let mut events = self.session.run_script(script);
        while let Some(event) = events.next().await {
            placed.extend(results.observe(&event));
        }
        placed
    }

    fn first_cell(&self, result: ResultId) -> Option<Value> {
        let stored = self.store.get(result)?;
        let stored = stored.read().unwrap();
        stored.value_at(0, 0)
    }

    fn shown(&self) -> Vec<ResultId> {
        self.tabs
            .list(EDITOR)
            .into_iter()
            .map(|tab| tab.result)
            .collect()
    }
}

#[tokio::test]
async fn a_pinned_result_survives_the_next_run_and_is_freed_when_closed() {
    let editor = editor().await;
    editor
        .run(
            Placement::Replace,
            "create table t (n int); insert into t values (1)",
        )
        .await;

    let first = editor
        .run(Placement::Replace, "-- Before\nselect n from t")
        .await;
    let before = first[0].tab.result;
    assert_eq!(first[0].tab.name.as_deref(), Some("Before"));
    assert!(editor.tabs.pin(EDITOR, before, true).unwrap().pinned);

    // The data changes and the same query runs again: the pinned result still holds what it
    // read, next to the new one.
    editor.run(Placement::Replace, "update t set n = 2").await;
    let second = editor.run(Placement::Replace, "select n from t").await;
    let after = second[0].tab.result;
    assert_eq!(editor.shown(), [before, after]);
    assert_eq!(editor.first_cell(before), Some(Value::Int(1)));
    assert_eq!(editor.first_cell(after), Some(Value::Int(2)));

    // A run that is not pinned is replaced; the pinned one is still there.
    let third = editor.run(Placement::Replace, "select n + 10 from t").await;
    assert!(
        editor.store.get(after).is_none(),
        "the unpinned result is freed"
    );
    assert_eq!(editor.shown(), [before, third[0].tab.result]);
    assert_eq!(editor.first_cell(before), Some(Value::Int(1)));

    // Closing the pinned tab frees its rows: nothing was copied anywhere.
    assert!(editor.tabs.close(before));
    assert!(editor.store.get(before).is_none());
    assert_eq!(editor.shown(), [third[0].tab.result]);
    assert_eq!(editor.store.len(), 1);
}

#[tokio::test]
async fn a_run_into_a_new_tab_keeps_the_results_and_a_re_run_reads_again_in_place() {
    let editor = editor().await;
    editor
        .run(
            Placement::Replace,
            "create table t (n int); insert into t values (1)",
        )
        .await;
    let one = editor.run(Placement::Replace, "select n from t").await[0]
        .tab
        .result;
    let two = editor.run(Placement::NewTab, "select n * 100 from t").await[0]
        .tab
        .result;
    assert_eq!(editor.shown(), [one, two]);
    assert_eq!(editor.store.len(), 2);

    editor.tabs.rename(EDITOR, one, "Totals").unwrap();
    editor.run(Placement::NewTab, "update t set n = 5").await;
    // The first result's query again, into its own tab: the new rows take its place and its
    // name, and the old rows are freed.
    let again = editor.run(Placement::Into(one), "select n from t").await;
    assert_eq!(again[0].replaced, Some(one));
    assert_eq!(again[0].tab.name.as_deref(), Some("Totals"));
    let again = again[0].tab.result;
    assert_eq!(editor.shown(), [again, two]);
    assert!(editor.store.get(one).is_none());
    assert_eq!(editor.first_cell(again), Some(Value::Int(5)));
    assert_eq!(editor.first_cell(two), Some(Value::Int(100)));

    // A re-run that fails leaves the result it would have replaced.
    let failed = editor
        .run(Placement::Into(again), "select n from no_such_table")
        .await;
    assert!(failed.is_empty());
    assert_eq!(editor.shown(), [again, two]);
    assert_eq!(editor.first_cell(again), Some(Value::Int(5)));

    // The editor tab closes: everything it kept is freed.
    assert_eq!(editor.tabs.close_editor(EDITOR), 2);
    assert!(editor.store.is_empty());
}
