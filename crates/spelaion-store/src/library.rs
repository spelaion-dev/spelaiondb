//! Indexed, bounded pages for the desktop's local query library.
use rusqlite::{OptionalExtension, params, params_from_iter, types::Value};

use crate::{HistoryEntry, Snippet, Store, StoreError, now_ms};

pub(crate) const MIGRATION: &str = "
alter table history add column database text;
alter table history add column pinned integer not null default 0 check(pinned in (0,1));
alter table history add column status text generated always as
 (case when cancelled then 'cancelled' when error is not null then 'error' else 'success' end) virtual;
create index history_status_at on history(workspace,status,at_ms desc,id desc);
create index history_database_at on history(workspace,database,at_ms desc,id desc);
create index history_status_database_at on history(workspace,status,database,at_ms desc,id desc);
create index history_pinned_at on history(workspace,at_ms desc,id desc) where pinned=1;
create index history_pinned_status_at on history(workspace,status,at_ms desc,id desc) where pinned=1;
create index history_pinned_database_at on history(workspace,database,at_ms desc,id desc) where pinned=1;
create index history_pinned_status_database_at on history(workspace,status,database,at_ms desc,id desc) where pinned=1;
create table snippet_folders(
 id integer primary key, workspace text not null,
 name text not null check(length(trim(name)) between 1 and 200), unique(workspace,name)
);
alter table snippets add column folder_id integer references snippet_folders(id);
create index snippets_workspace_name on snippets(workspace,name collate nocase,id);
create index snippets_folder_name on snippets(workspace,folder_id,name collate nocase,id);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryStatus {
    Success,
    Error,
    Cancelled,
}
impl HistoryStatus {
    fn text(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HistoryFilter {
    pub search: String,
    pub status: Option<HistoryStatus>,
    /// `Some("")` selects historical entries whose database was not recorded.
    pub database: Option<String>,
    pub from_ms: Option<i64>,
    /// Exclusive end, so adjacent date ranges do not overlap.
    pub until_ms: Option<i64>,
    pub pinned: bool,
    /// Stable upper ID captured on the first page; later executions do not shift pages.
    pub through_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub has_more: bool,
    pub through_id: i64,
}

#[derive(Debug, Clone, Default)]
pub struct SnippetFilter {
    pub search: String,
    pub folder: Option<i64>,
    pub unfiled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetFolder {
    pub id: i64,
    pub workspace: String,
    pub name: String,
}

fn history_sql(
    workspace: &str,
    filter: &HistoryFilter,
    limit: usize,
    offset: usize,
) -> (String, Vec<Value>) {
    let index = match (
        filter.pinned,
        filter.status.is_some(),
        filter.database.is_some(),
    ) {
        (false, false, false) => "history_workspace_at",
        (false, true, false) => "history_status_at",
        (false, false, true) => "history_database_at",
        (false, true, true) => "history_status_database_at",
        (true, false, false) => "history_pinned_at",
        (true, true, false) => "history_pinned_status_at",
        (true, false, true) => "history_pinned_database_at",
        (true, true, true) => "history_pinned_status_database_at",
    };
    let mut sql = format!(
        "select h.id,h.at_ms,h.workspace,h.connection,h.statement,h.duration_ms,h.rows,h.error,h.cancelled,h.database,h.pinned from history h indexed by {index} where h.workspace in (?, '')"
    );
    let mut values = vec![Value::Text(workspace.into())];
    if filter.pinned {
        sql.push_str(" and h.pinned=1");
    }
    if let Some(status) = filter.status {
        sql.push_str(" and h.status=?");
        values.push(Value::Text(status.text().into()));
    }
    if let Some(database) = &filter.database {
        if database.is_empty() {
            sql.push_str(" and h.database is null");
        } else {
            sql.push_str(" and h.database=?");
            values.push(Value::Text(database.clone()));
        }
    }
    for (condition, value) in [
        (" and h.at_ms>=?", filter.from_ms),
        (" and h.at_ms<?", filter.until_ms),
        (" and h.id<=?", filter.through_id),
    ] {
        if let Some(value) = value {
            sql.push_str(condition);
            values.push(Value::Integer(value));
        }
    }
    if !filter.search.trim().is_empty() {
        sql.push_str(" and h.id in (select rowid from history_fts where history_fts match ?)");
        values.push(Value::Text(format!(
            "\"{}\"*",
            filter.search.trim().replace('"', "\"\"")
        )));
    }
    sql.push_str(" order by h.at_ms desc,h.id desc limit ? offset ?");
    values.push(Value::Integer(limit.min(501) as i64));
    values.push(Value::Integer(i64::try_from(offset).unwrap_or(i64::MAX)));
    (sql, values)
}

fn read_history(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        at_ms: row.get(1)?,
        workspace: row.get(2)?,
        connection: row.get(3)?,
        statement: row.get(4)?,
        duration_ms: row.get(5)?,
        rows: row.get(6)?,
        error: row.get(7)?,
        cancelled: row.get(8)?,
        database: row.get(9)?,
        pinned: row.get(10)?,
    })
}

impl Store {
    pub fn history_page(
        &self,
        workspace: &str,
        filter: &HistoryFilter,
        limit: usize,
        offset: usize,
    ) -> Result<HistoryPage, StoreError> {
        if filter
            .from_ms
            .zip(filter.until_ms)
            .is_some_and(|(from, until)| from >= until)
        {
            return Err(StoreError(
                "the end of the time range must follow its start".into(),
            ));
        }
        let through_id = match filter.through_id {
            Some(id) => id,
            None => self
                .conn
                .query_row("select coalesce(max(id),0) from history", [], |row| {
                    row.get(0)
                })?,
        };
        let filter = HistoryFilter {
            through_id: Some(through_id),
            ..filter.clone()
        };
        let limit = limit.clamp(1, 500);
        let (sql, values) = history_sql(workspace, &filter, limit + 1, offset);
        let mut stmt = self.conn.prepare(&sql)?;
        let mut entries = stmt
            .query_map(params_from_iter(values), read_history)?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(HistoryPage {
            entries,
            has_more,
            through_id,
        })
    }

    /// Visits bounded pages in one read snapshot. The callback never receives the whole log.
    pub fn visit_history(
        &self,
        workspace: &str,
        filter: &HistoryFilter,
        mut visit: impl FnMut(&[HistoryEntry]) -> Result<(), StoreError>,
    ) -> Result<usize, StoreError> {
        let snapshot = self.conn.unchecked_transaction()?;
        let mut offset = 0;
        loop {
            let page = self.history_page(workspace, filter, 500, offset)?;
            visit(&page.entries)?;
            offset += page.entries.len();
            if !page.has_more {
                break;
            }
        }
        snapshot.commit()?;
        Ok(offset)
    }

    pub fn pin_history(&self, workspace: &str, id: i64, pinned: bool) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "update history set pinned=? where id=? and workspace in (?, '')",
            params![pinned, id, workspace],
        )? > 0)
    }

    pub fn history_databases(&self, workspace: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt=self.conn.prepare("select distinct coalesce(database,'') from history indexed by history_database_at where workspace in (?, '') order by 1")?;
        Ok(stmt
            .query_map([workspace], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    pub fn record_history_database(
        &self,
        entry: &crate::NewHistoryEntry,
        database: Option<&str>,
    ) -> Result<(), StoreError> {
        self.conn.execute("insert into history(at_ms,workspace,connection,statement,duration_ms,rows,error,cancelled,database) select ?1,?2,?3,?4,?5,?6,?7,?8,?9 where coalesce((select enabled from history_preferences where workspace=?2 and connection=?3),1)",params![entry.at_ms,entry.workspace,entry.connection,entry.statement,entry.duration_ms,entry.rows,entry.error,entry.cancelled,database.filter(|value|!value.is_empty())])?;
        Ok(())
    }

    pub fn snippet_page(
        &self,
        workspace: &str,
        filter: &SnippetFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Snippet>, StoreError> {
        if filter.folder.is_some() && filter.unfiled {
            return Err(StoreError("choose a folder or unfiled queries".into()));
        }
        let mut sql = String::from(
            "select id,workspace,name,statement,connection,updated_ms,folder_id from snippets where workspace in (?, '')",
        );
        let mut values = vec![Value::Text(workspace.into())];
        if let Some(folder) = filter.folder {
            sql.push_str(" and folder_id=?");
            values.push(Value::Integer(folder));
        }
        if filter.unfiled {
            sql.push_str(" and folder_id is null");
        }
        if !filter.search.trim().is_empty() {
            sql.push_str(" and (name like ? escape '\\' or statement like ? escape '\\')");
            let pattern = format!(
                "%{}%",
                filter
                    .search
                    .trim()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            values.extend([Value::Text(pattern.clone()), Value::Text(pattern)]);
        }
        sql.push_str(" order by name collate nocase,id limit ? offset ?");
        values.extend([
            Value::Integer(limit.clamp(1, 501) as i64),
            Value::Integer(i64::try_from(offset).unwrap_or(i64::MAX)),
        ]);
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_map(params_from_iter(values), |r| {
                Ok(Snippet {
                    id: r.get(0)?,
                    workspace: r.get(1)?,
                    name: r.get(2)?,
                    statement: r.get(3)?,
                    connection: r.get(4)?,
                    updated_ms: r.get(5)?,
                    folder_id: r.get(6)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn snippet_folders(&self, workspace: &str) -> Result<Vec<SnippetFolder>, StoreError> {
        let mut stmt=self.conn.prepare("select id,workspace,name from snippet_folders where workspace in (?, '') order by name collate nocase,id")?;
        Ok(stmt
            .query_map([workspace], |r| {
                Ok(SnippetFolder {
                    id: r.get(0)?,
                    workspace: r.get(1)?,
                    name: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    }

    pub fn create_snippet_folder(&self, workspace: &str, name: &str) -> Result<i64, StoreError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 200 || name.chars().any(char::is_control) {
            return Err(StoreError(
                "a folder needs a name of 1 to 200 characters without control characters".into(),
            ));
        }
        self.conn
            .query_row(
                "insert into snippet_folders(workspace,name) values(?,?) returning id",
                params![workspace, name],
                |r| r.get(0),
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(ref code, _)
                    if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
                {
                    StoreError(
                        "a saved-query folder with this name already exists; choose another name"
                            .into(),
                    )
                }
                error => error.into(),
            })
    }

    pub fn move_snippet(
        &self,
        workspace: &str,
        id: i64,
        folder: Option<i64>,
    ) -> Result<(), StoreError> {
        let source: Option<String> = self
            .conn
            .query_row(
                "select workspace from snippets where id=? and workspace in (?, '')",
                params![id, workspace],
                |r| r.get(0),
            )
            .optional()?;
        let source = source
            .ok_or_else(|| StoreError("the saved query is unavailable in this workspace".into()))?;
        if let Some(folder) = folder {
            let valid: bool = self.conn.query_row(
                "select exists(select 1 from snippet_folders where id=? and workspace=?)",
                params![folder, source],
                |r| r.get(0),
            )?;
            if !valid {
                return Err(StoreError(
                    "the folder must belong to the same workspace as the saved query".into(),
                ));
            }
        }
        self.conn.execute(
            "update snippets set folder_id=?,updated_ms=? where id=?",
            params![folder, now_ms(), id],
        )?;
        Ok(())
    }

    pub fn delete_visible_snippet(&self, workspace: &str, id: i64) -> Result<bool, StoreError> {
        Ok(self.conn.execute(
            "delete from snippets where id=? and workspace in (?, '')",
            params![id, workspace],
        )? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{history_entry, temp};

    #[test]
    fn ten_thousand_history_entries_use_filtered_indexes_and_stable_bounded_pages() {
        let path = temp("library-indexes");
        let store = Store::open(&path).unwrap();
        store.conn.execute_batch("begin").unwrap();
        for n in 0..10_000 {
            let mut entry = history_entry(
                if n % 3 == 0 { "/other" } else { "/repo" },
                "shop",
                &format!("select report_{n} from orders"),
                n,
            );
            entry.cancelled = n % 7 == 0;
            entry.error = (n % 5 == 0).then(|| "failed".into());
            store
                .record_history_database(
                    &entry,
                    Some(if n % 2 == 0 { "shop" } else { "analytics" }),
                )
                .unwrap();
        }
        store
            .conn
            .execute_batch("update history set pinned=1 where id%3=0; commit; analyze")
            .unwrap();
        for pinned in [false, true] {
            for status in [
                None,
                Some(HistoryStatus::Success),
                Some(HistoryStatus::Error),
                Some(HistoryStatus::Cancelled),
            ] {
                for database in [None, Some("shop".to_string()), Some(String::new())] {
                    for timed in [false, true] {
                        for search in ["", "orders"] {
                            let filter = HistoryFilter {
                                pinned,
                                status,
                                database: database.clone(),
                                from_ms: timed.then_some(1_000),
                                until_ms: timed.then_some(9_000),
                                search: search.into(),
                                through_id: Some(10_000),
                            };
                            let (sql, values) = history_sql("/repo", &filter, 51, 50);
                            let plan = store
                                .conn
                                .prepare(&format!("explain query plan {sql}"))
                                .unwrap()
                                .query_map(params_from_iter(values), |r| r.get::<_, String>(3))
                                .unwrap()
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap();
                            assert!(
                                plan.iter()
                                    .any(|line| line.contains("SEARCH h USING INDEX history_")),
                                "{filter:?}: {plan:?}"
                            );
                            assert!(
                                !plan
                                    .iter()
                                    .any(|line| line.starts_with("SCAN h ") || line == "SCAN h"),
                                "{plan:?}"
                            );
                            if !search.is_empty() {
                                assert!(
                                    plan.iter().any(|line| line.contains("VIRTUAL TABLE INDEX")
                                        && line.contains("M")),
                                    "FTS must use MATCH: {plan:?}"
                                );
                            }
                            let page = store.history_page("/repo", &filter, 50, 0).unwrap();
                            assert!(page.entries.len() <= 50);
                            assert!(
                                page.entries
                                    .iter()
                                    .all(|e| e.workspace != "/other" && (!pinned || e.pinned))
                            );
                            for entry in &page.entries {
                                if let Some(status) = status {
                                    assert!(match status {
                                        HistoryStatus::Cancelled => entry.cancelled,
                                        HistoryStatus::Error =>
                                            entry.error.is_some() && !entry.cancelled,
                                        HistoryStatus::Success =>
                                            entry.error.is_none() && !entry.cancelled,
                                    });
                                }
                                if let Some(database) = &database {
                                    assert_eq!(entry.database.as_deref().unwrap_or(""), database);
                                }
                                if timed {
                                    assert!((1_000..9_000).contains(&entry.at_ms));
                                }
                                if !search.is_empty() {
                                    assert!(entry.statement.contains("orders"));
                                }
                            }
                            let expected_index =
                                match (pinned, status.is_some(), database.is_some()) {
                                    (false, false, false) => "history_workspace_at",
                                    (false, true, false) => "history_status_at",
                                    (false, false, true) => "history_database_at",
                                    (false, true, true) => "history_status_database_at",
                                    (true, false, false) => "history_pinned_at",
                                    (true, true, false) => "history_pinned_status_at",
                                    (true, false, true) => "history_pinned_database_at",
                                    (true, true, true) => "history_pinned_status_database_at",
                                };
                            assert!(
                                plan.iter().any(|line| line.contains(&format!(
                                    "USING INDEX {expected_index} (workspace=?"
                                ))),
                                "{plan:?}"
                            );
                        }
                    }
                }
            }
        }
        let first = store
            .history_page("/repo", &HistoryFilter::default(), 10, 0)
            .unwrap();
        store
            .record_history(&history_entry("/repo", "shop", "select newest", 99_999))
            .unwrap();
        let filter = HistoryFilter {
            through_id: Some(first.through_id),
            ..Default::default()
        };
        let second = store.history_page("/repo", &filter, 10, 10).unwrap();
        assert!(
            second
                .entries
                .iter()
                .all(|entry| !first.entries.iter().any(|first| first.id == entry.id))
        );
        assert!(
            second
                .entries
                .iter()
                .all(|entry| entry.id <= first.through_id)
        );
        assert_eq!(
            store
                .history_page("/repo", &filter, usize::MAX, 0)
                .unwrap()
                .entries
                .len(),
            500
        );
        assert!(
            store
                .history_page(
                    "/repo",
                    &HistoryFilter {
                        from_ms: Some(2),
                        until_ms: Some(1),
                        ..Default::default()
                    },
                    10,
                    0
                )
                .is_err()
        );
        for (at, sql) in [
            (42, "select lower_bound"),
            (43, "select tie_first"),
            (43, "select tie_second"),
            (44, "select upper_bound"),
        ] {
            store
                .record_history_database(&history_entry("/repo", "boundary", sql, at), Some(""))
                .unwrap();
        }
        let bounded = store
            .history_page(
                "/repo",
                &HistoryFilter {
                    database: Some(String::new()),
                    from_ms: Some(42),
                    until_ms: Some(44),
                    ..Default::default()
                },
                20,
                0,
            )
            .unwrap();
        assert_eq!(
            bounded
                .entries
                .iter()
                .map(|entry| entry.statement.as_str())
                .collect::<Vec<_>>(),
            [
                "select tie_second",
                "select tie_first",
                "select lower_bound"
            ]
        );
    }

    #[test]
    fn folders_search_moves_and_mutations_keep_workspace_boundaries() {
        let path = temp("library-folders");
        let store = Store::open(&path).unwrap();
        let personal = store
            .save_snippet("", "Personal", "select literal_100%", "shop")
            .unwrap();
        let query = store
            .save_snippet("/repo", "Report", "select revenue from orders", "shop")
            .unwrap();
        let foreign = store
            .save_snippet("/other", "Hidden", "select secret", "shop")
            .unwrap();
        let a = store.create_snippet_folder("/repo", "Reports").unwrap();
        let b = store.create_snippet_folder("/repo", "Archive").unwrap();
        assert_eq!(
            store
                .create_snippet_folder("/repo", " Reports ")
                .unwrap_err()
                .to_string(),
            "local store: a saved-query folder with this name already exists; choose another name"
        );
        assert!(store.create_snippet_folder("/repo", "\n").is_err());
        store.move_snippet("/repo", query, Some(a)).unwrap();
        store.move_snippet("/repo", query, Some(b)).unwrap();
        assert!(store.move_snippet("/repo", foreign, Some(a)).is_err());
        assert!(store.move_snippet("/repo", personal, Some(a)).is_err());
        assert!(store.move_snippet("/repo", query, Some(9999)).is_err());
        assert!(!store.delete_visible_snippet("/repo", foreign).unwrap());
        let page = store
            .snippet_page(
                "/repo",
                &SnippetFilter {
                    search: "revenue".into(),
                    folder: Some(b),
                    unfiled: false,
                },
                50,
                0,
            )
            .unwrap();
        assert_eq!(page.iter().map(|q| q.id).collect::<Vec<_>>(), [query]);
        assert_eq!(
            store
                .snippet_page(
                    "/repo",
                    &SnippetFilter {
                        search: "100%".into(),
                        ..Default::default()
                    },
                    1,
                    0
                )
                .unwrap()[0]
                .id,
            personal
        );
        store
            .save_snippet("/repo", "Report", "select revised", "shop")
            .unwrap();
        assert_eq!(
            store
                .snippet_page(
                    "/repo",
                    &SnippetFilter {
                        folder: Some(b),
                        ..Default::default()
                    },
                    50,
                    0
                )
                .unwrap()[0]
                .id,
            query
        );
        store.move_snippet("/repo", query, None).unwrap();
        assert_eq!(
            store
                .snippet_page(
                    "/repo",
                    &SnippetFilter {
                        unfiled: true,
                        ..Default::default()
                    },
                    1,
                    0
                )
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn appended_migration_preserves_old_entries_settings_and_opt_out() {
        let path = temp("library-migration");
        let conn = rusqlite::Connection::open(&path).unwrap();
        let old = &crate::MIGRATIONS[..crate::MIGRATIONS.len() - 1];
        for migration in old {
            conn.execute_batch(migration).unwrap();
        }
        conn.pragma_update(None, "user_version", old.len() as i64)
            .unwrap();
        conn.execute_batch("insert into history values(1,123,'/repo','shop','select 1',12,1,null,0); insert into snippets values(1,'/repo','Report','select 1','shop',123); insert into history_preferences values('/repo','private',0); insert into personal_settings values('language','es');").unwrap();
        drop(conn);
        let store = Store::open(&path).unwrap();
        let entry = store
            .history_page("/repo", &HistoryFilter::default(), 50, 0)
            .unwrap()
            .entries
            .remove(0);
        assert_eq!(entry.statement, "select 1");
        assert_eq!(entry.database, None);
        assert!(!entry.pinned);
        assert_eq!(store.get_language().unwrap(), "es");
        store
            .record_history_database(
                &history_entry("/repo", "private", "select no_record", 999),
                Some("private"),
            )
            .unwrap();
        assert_eq!(
            store
                .history_page("/repo", &HistoryFilter::default(), 50, 0)
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(store.pin_history("/repo", 1, true).unwrap());
        assert!(!store.pin_history("/other", 1, false).unwrap());
        assert_eq!(store.history_databases("/repo").unwrap(), [""]);
    }

    #[test]
    fn history_export_snapshot_is_bounded_and_stable_while_another_connection_writes() {
        let path = temp("history-snapshot");
        let store = Store::open(&path).unwrap();
        store.conn.execute_batch("begin").unwrap();
        for at in 0..1_101 {
            store
                .record_history(&history_entry("/repo", "shop", "select snapshot", at))
                .unwrap();
        }
        store.conn.execute_batch("commit").unwrap();
        let other = Store::open(&path).unwrap();
        let mut ids = vec![];
        let mut pages = 0;
        let count = store
            .visit_history("/repo", &HistoryFilter::default(), |entries| {
                pages += 1;
                assert!(entries.len() <= 500);
                ids.extend(entries.iter().map(|entry| entry.id));
                if pages == 1 {
                    other
                        .conn
                        .execute_batch("delete from history where id<=501")
                        .unwrap();
                    other
                        .record_history(&history_entry("/repo", "shop", "select appended", 99_999))
                        .unwrap();
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 1_101);
        assert_eq!(pages, 3);
        assert_eq!(ids.len(), 1_101);
        assert_eq!(
            ids.iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            1_101
        );
        assert_eq!(
            store
                .history_page("/repo", &HistoryFilter::default(), 1, 0)
                .unwrap()
                .entries[0]
                .statement,
            "select appended"
        );
    }
}
