//! Values the store keeps as text are read as their type, and a value this build does not know
//! is never read as one it does (SDB-376). Rows written by the previous release still load.

use rusqlite::{Connection, params};

use super::tests::temp;
use super::*;

/// The migration that checks these values, found by its text.
fn checks_migration() -> usize {
    MIGRATIONS
        .iter()
        .position(|m| m.contains("create trigger audit_decision_insert"))
        .expect("the migration of SDB-376")
}

/// A store as the previous release left it: every migration before the checks, then `rows`.
fn previous_release(name: &str, rows: &str) -> std::path::PathBuf {
    let path = temp(name);
    let mut conn = Connection::open(&path).unwrap();
    let tx = conn.transaction().unwrap();
    let before = checks_migration();
    for migration in &MIGRATIONS[..before] {
        tx.execute_batch(migration).unwrap();
    }
    tx.pragma_update(None, "user_version", before as i64)
        .unwrap();
    tx.execute_batch(rows).unwrap();
    tx.commit().unwrap();
    path
}

#[test]
fn every_decision_reads_as_itself_and_an_unknown_one_as_unknown() {
    let path = previous_release(
        "typed-audit",
        "insert into audit (at_ms, client, connection, statement, decision)
         values (1, 'old', 'c', 'select 1', 'executed'),
                (2, 'old', 'c', 'select 2', 'rejected'),
                (3, 'old', 'c', 'select 3', 'failed'),
                (4, 'old', 'c', 'select 4', 'cleared'),
                (5, 'newer', 'c', 'select 5', 'deferred');",
    );
    let store = Store::open(&path).unwrap();
    let read: Vec<(String, Decision)> = store
        .recent(10)
        .unwrap()
        .into_iter()
        .map(|e| (e.statement, e.decision))
        .collect();
    assert_eq!(
        read,
        [
            ("select 5".to_string(), Decision::Unknown),
            ("select 4".to_string(), Decision::Cleared),
            ("select 3".to_string(), Decision::Failed),
            ("select 2".to_string(), Decision::Rejected),
            ("select 1".to_string(), Decision::Executed),
        ]
    );
    // Filtering by each decision finds its own records only; "unknown" finds the rest.
    let count = |decision| {
        store
            .audit_count(&AuditFilter {
                decision: Some(decision),
                ..AuditFilter::default()
            })
            .unwrap()
    };
    for decision in [
        Decision::Executed,
        Decision::Rejected,
        Decision::Failed,
        Decision::Cleared,
        Decision::Unknown,
    ] {
        assert_eq!(count(decision), 1, "{decision:?}");
    }
}

#[test]
fn an_unknown_decision_is_never_written() {
    let path = temp("typed-audit-write");
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .record(&AuditEntry::now("x", "c", "select 1", Decision::Unknown))
            .is_err()
    );
    // Nor by hand: the store refuses it.
    assert!(
        store
            .conn
            .execute(
                "insert into audit (at_ms, client, connection, statement, decision)
                 values (1, 'x', 'c', 'select 1', 'maybe')",
                [],
            )
            .is_err()
    );
    assert_eq!(store.audit_count(&AuditFilter::default()).unwrap(), 0);
}

#[test]
fn the_update_check_reads_on_off_or_refuses_what_it_does_not_know() {
    let path = previous_release(
        "typed-update-check",
        "insert into personal_settings (key, value) values ('update_check', 'of');",
    );
    let store = Store::open(&path).unwrap();
    // A typo is neither on nor off.
    assert!(store.get_update_check().is_err());
    store.set_update_check(false).unwrap();
    assert!(!store.get_update_check().unwrap());
    store.set_update_check(true).unwrap();
    assert!(store.get_update_check().unwrap());
    for value in ["yes", "", "OFF"] {
        assert!(
            store
                .conn
                .execute(
                    "update personal_settings set value = ?1 where key = 'update_check'",
                    [value],
                )
                .is_err(),
            "{value:?}"
        );
    }
    assert!(store.get_update_check().unwrap());
}

#[test]
fn a_workspace_keeps_one_active_tab_and_two_from_before_mean_none() {
    let path = previous_release(
        "typed-active-tab",
        "insert into open_tabs (workspace, position, active, text, pinned)
         values ('/repo', 0, 1, 'select 1', 0), ('/repo', 1, 1, 'select 2', 0),
                ('/other', 0, 0, 'select 3', 0), ('/other', 1, 1, 'select 4', 0);",
    );
    let mut store = Store::open(&path).unwrap();
    // Two active tabs do not say which one was shown: none is taken for it.
    let both = store.open_tabs("/repo").unwrap();
    assert_eq!(both.tabs.len(), 2);
    assert_eq!(both.active, None);
    assert_eq!(store.open_tabs("/other").unwrap().active, Some(1));
    // A second active tab cannot be written.
    assert!(
        store
            .conn
            .execute(
                "insert into open_tabs (workspace, position, active, text, pinned)
                 values ('/other', 2, 1, 'select 5', 0)",
                [],
            )
            .is_err()
    );
    // Saving replaces them with one.
    store
        .save_open_tabs(
            "/repo",
            &SavedTabs {
                tabs: both.tabs,
                active: Some(1),
            },
        )
        .unwrap();
    assert_eq!(store.open_tabs("/repo").unwrap().active, Some(1));
}

#[test]
fn every_bookmark_kind_reads_as_itself_and_an_unknown_one_is_left_out() {
    for kind in BookmarkKind::ALL {
        assert_eq!(BookmarkKind::parse(kind.as_str()), Some(kind), "{kind:?}");
    }
    let mut rows = String::new();
    for (i, kind) in BookmarkKind::ALL.iter().enumerate() {
        rows.push_str(&format!(
            "insert into explorer_bookmarks values ('/repo', 'shop', 'public', 'o{i}', '{}', '', {i});\n",
            kind.as_str()
        ));
    }
    rows.push_str(
        "insert into explorer_bookmarks values ('/repo', 'shop', 'public', 'later', 'dictionary', '', 99);",
    );
    let path = previous_release("typed-bookmarks", &rows);
    let store = Store::open(&path).unwrap();
    let read = store.bookmarks("/repo").unwrap();
    assert_eq!(read.len(), BookmarkKind::ALL.len());
    for kind in BookmarkKind::ALL {
        assert!(read.iter().any(|b| b.kind == kind), "{kind:?}");
    }
    assert!(!read.iter().any(|b| b.name == "later"));
    // It stays in the store, for the build that wrote it.
    let kept: i64 = store
        .conn
        .query_row(
            "select count(*) from explorer_bookmarks where kind = 'dictionary'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1);
}

#[test]
fn a_history_entry_ends_one_way_as_the_store_s_status_says() {
    let path = previous_release(
        "typed-history",
        "insert into history (at_ms, workspace, connection, statement, duration_ms, rows, error, cancelled)
         values (1, '', 'c', 'select 1', 1, 1, null, 0),
                (2, '', 'c', 'select 2', 1, 0, 'syntax error', 0),
                (3, '', 'c', 'select 3', 1, 0, null, 1),
                (4, '', 'c', 'select 4', 1, 0, 'canceling statement', 1);",
    );
    let store = Store::open(&path).unwrap();
    let mut read: Vec<(String, HistoryOutcome, String)> = Vec::new();
    for entry in store.history("", "", 10, 0).unwrap() {
        let status: String = store
            .conn
            .query_row(
                "select status from history where id = ?1",
                params![entry.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(entry.status().text(), status, "{}", entry.statement);
        read.push((entry.statement.clone(), entry.outcome(), status));
    }
    read.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        read,
        [
            (
                "select 1".to_string(),
                HistoryOutcome::Succeeded,
                "success".into()
            ),
            (
                "select 2".to_string(),
                HistoryOutcome::Failed {
                    error: "syntax error".into()
                },
                "error".into()
            ),
            (
                "select 3".to_string(),
                HistoryOutcome::Cancelled,
                "cancelled".into()
            ),
            (
                "select 4".to_string(),
                HistoryOutcome::Cancelled,
                "cancelled".into()
            ),
        ]
    );
}
