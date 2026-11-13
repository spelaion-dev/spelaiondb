//! How long the grid's core features take on a million rows (made by DuckDB, nothing
//! external). Ignored by default: it measures, it does not check behaviour. Run it in release:
//!
//!     cargo test --release -p spelaion-core --test grid_million -- --ignored --nocapture

mod engines;

use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use spelaion_core::{
    ColumnFilter, CoreEvent, FilterOp, FindQuery, ResultStore, Session, SortKey, column_in_list,
};
use spelaion_driver_api::{ConnectParams, Driver};

#[tokio::test]
#[ignore = "a measurement: run it in release with --ignored --nocapture"]
async fn a_million_rows() {
    let driver = engines::duckdb();
    let conn = driver
        .connect(&ConnectParams {
            database: ":memory:".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let session = Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    );
    let started = Instant::now();
    let events: Vec<CoreEvent> = session
        .run_script(
            "select range as id, 'item ' || range as name, range * 1.5 as price, \
             range % 7 = 0 as lucky from range(1000000)",
        )
        .collect()
        .await;
    let id = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap();
    println!("load 1,000,000 rows × 4 columns: {:?}", started.elapsed());
    let stored = session.results().get(id).unwrap();
    let mut r = stored.write().unwrap();
    assert_eq!(r.rows(), 1_000_000);

    let time = |label: &str, f: &mut dyn FnMut() -> String| {
        let started = Instant::now();
        let outcome = f();
        println!("{label}: {:?} ({outcome})", started.elapsed());
    };
    let find = |r: &spelaion_core::StoredResult, text: &str| {
        let query = FindQuery {
            text: text.into(),
            match_case: false,
            columns: vec![0, 1, 2, 3],
        };
        let found = r.find_matches(&query, &r.find_ticket()).unwrap().unwrap();
        format!(
            "{} matches{}",
            found.len(),
            if found.is_truncated() { "+" } else { "" }
        )
    };
    time("find 'item 99999' (4M cells)", &mut || {
        find(&r, "item 99999")
    });
    time("find '7' (most cells match)", &mut || find(&r, "7"));
    time("window of 500 rows", &mut || {
        format!("{} batches", r.window(500_000, 500).len())
    });
    time("sort by price, descending", &mut || {
        r.sort(Some(SortKey {
            column: 2,
            descending: true,
        }))
        .unwrap();
        "sorted".into()
    });
    time("find 'item 99999' after the sort", &mut || {
        find(&r, "item 99999")
    });
    time("filter: name contains 'item 12345'", &mut || {
        let rows = r
            .filter_columns(vec![ColumnFilter {
                column: 1,
                op: FilterOp::Contains,
                value: "item 12345".into(),
            }])
            .unwrap();
        format!("{rows} rows")
    });
    r.filter_columns(Vec::new()).unwrap();
    r.sort(None).unwrap();
    let dialect = session.dialect().clone();
    time("IN list of lucky (2 values)", &mut || {
        column_in_list(&r, 3, dialect.as_ref()).unwrap()
    });
    time("IN list of id (refused past 10,000)", &mut || {
        column_in_list(&r, 0, dialect.as_ref())
            .unwrap_err()
            .to_string()
    });
}
