//! What a table data tab remembers about its table's filter (SDB-381): the filters used on
//! it, newest first, and the default one the tab opens with. Both are local data in the store
//! (`spelaion-store`'s `table_filters.rs`), so they are text someone or something else may
//! have written: a filter is checked like a page's (`table_data.rs`: it cannot change the
//! SELECT) before it is kept and **again every time it is read back**, and one that no longer
//! passes is never handed to the tab as a filter. The page that runs it checks it once more.

use spelaion_driver_api::Dialect;
use spelaion_store::{FilteredTable, Store, StoreError};

use crate::history::{HistoryError, QueryHistory};
use crate::table_data::{PageError, table_count_sql};

/// A change to what the store remembers of a table's filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableFilterChange {
    /// Only read.
    None,
    /// The filter was used: it becomes the newest of the history.
    Used(String),
    /// Take one filter out of the history.
    Forget(String),
    /// Set the default filter, or drop it.
    Default(Option<String>),
}

/// What the store holds for a table, as it is written there: not checked.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct StoredTableFilters {
    history: Vec<String>,
    default: Option<String>,
}

/// Applies `change` and reads the table's filters back (the store actor's step).
pub(crate) fn stored_table_filters(
    store: &Store,
    table: &FilteredTable,
    change: TableFilterChange,
) -> Result<StoredTableFilters, StoreError> {
    match change {
        TableFilterChange::None => {}
        TableFilterChange::Used(filter) => {
            store.record_table_filter(table, &filter)?;
        }
        TableFilterChange::Forget(filter) => store.forget_table_filter(table, &filter)?,
        TableFilterChange::Default(filter) => {
            store.set_table_default_filter(table, filter.as_deref())?
        }
    }
    Ok(StoredTableFilters {
        history: store.table_filter_history(table)?,
        default: store.table_default_filter(table)?,
    })
}

/// A stored default filter that did not pass the check when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedFilter {
    pub filter: String,
    /// Why, in the words of the page's check.
    pub reason: String,
}

/// A table's remembered filters, every one checked just now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TableFilters {
    /// Newest first. One that no longer passes the check is left out.
    pub history: Vec<String>,
    /// The filter the tab opens with.
    pub default: Option<String>,
    /// The stored default did not pass the check: the tab opens without a filter and says so.
    pub refused_default: Option<RefusedFilter>,
}

#[derive(Debug, thiserror::Error)]
pub enum TableFilterError {
    /// The filter is not one a page would run.
    #[error(transparent)]
    Refused(#[from] PageError),
    #[error(transparent)]
    Store(#[from] HistoryError),
}

/// `filter` trimmed, if a page of `table` would run it: a single condition after `WHERE`
/// that leaves the SELECT what it is.
fn checked(
    dialect: &dyn Dialect,
    table: &FilteredTable,
    filter: &str,
) -> Result<String, PageError> {
    let filter = filter.trim();
    table_count_sql(dialect, &table.schema, &table.name, filter)?;
    Ok(filter.to_string())
}

/// What was read from the store, checked: nothing here is trusted because it was kept.
fn read_back(
    dialect: &dyn Dialect,
    table: &FilteredTable,
    stored: StoredTableFilters,
) -> TableFilters {
    let history = stored
        .history
        .into_iter()
        .filter(|filter| checked(dialect, table, filter).is_ok())
        .collect();
    let (default, refused_default) = match stored.default {
        None => (None, None),
        Some(filter) => match checked(dialect, table, &filter) {
            Ok(filter) => (Some(filter), None),
            Err(e) => (
                None,
                Some(RefusedFilter {
                    filter,
                    reason: e.to_string(),
                }),
            ),
        },
    };
    TableFilters {
        history,
        default,
        refused_default,
    }
}

impl QueryHistory {
    /// The filters remembered for `table`, for a tab that opens: its history and its default,
    /// each checked again with `dialect` now.
    pub async fn table_filters(
        &self,
        dialect: &dyn Dialect,
        table: FilteredTable,
    ) -> Result<TableFilters, TableFilterError> {
        let stored = self
            .stored_table_filters(table.clone(), TableFilterChange::None)
            .await?;
        Ok(read_back(dialect, &table, stored))
    }

    /// Remembers a filter the tab just used. One a page would not run is refused, not kept;
    /// a blank one is nothing to remember, and a connection whose query history is switched
    /// off remembers no filter either (a filter holds values, as a query does). A default is
    /// not history: saving one is the user's own act.
    pub async fn record_table_filter(
        &self,
        dialect: &dyn Dialect,
        table: FilteredTable,
        filter: &str,
    ) -> Result<TableFilters, TableFilterError> {
        let change = if filter.trim().is_empty() {
            TableFilterChange::None
        } else {
            let used = checked(dialect, &table, filter)?;
            let remembers = self
                .enabled(table.workspace.clone(), table.connection.clone())
                .await?;
            if remembers {
                TableFilterChange::Used(used)
            } else {
                TableFilterChange::None
            }
        };
        let stored = self.stored_table_filters(table.clone(), change).await?;
        Ok(read_back(dialect, &table, stored))
    }

    /// Takes one filter out of `table`'s history.
    pub async fn forget_table_filter(
        &self,
        dialect: &dyn Dialect,
        table: FilteredTable,
        filter: &str,
    ) -> Result<TableFilters, TableFilterError> {
        let stored = self
            .stored_table_filters(table.clone(), TableFilterChange::Forget(filter.to_string()))
            .await?;
        Ok(read_back(dialect, &table, stored))
    }

    /// Saves `filter` as the one `table`'s tab opens with; `None` or a blank one drops the
    /// default. One a page would not run is refused, and the default stays as it was.
    pub async fn set_table_default_filter(
        &self,
        dialect: &dyn Dialect,
        table: FilteredTable,
        filter: Option<&str>,
    ) -> Result<TableFilters, TableFilterError> {
        let filter = match filter.map(str::trim).filter(|f| !f.is_empty()) {
            None => None,
            Some(filter) => Some(checked(dialect, &table, filter)?),
        };
        let stored = self
            .stored_table_filters(table.clone(), TableFilterChange::Default(filter))
            .await?;
        Ok(read_back(dialect, &table, stored))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use spelaion_dialect::{MysqlDialect, PostgresDialect, SqlServerDialect, SqliteDialect};

    use super::*;

    fn path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "spelaion-table-filters-{name}-{}-{}/state.db",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    fn orders() -> FilteredTable {
        FilteredTable {
            workspace: "/repo".into(),
            connection: "shop".into(),
            database: None,
            schema: "public".into(),
            name: "orders".into(),
        }
    }

    /// Filters that would change the page's SELECT, read another table or run something else.
    const HOSTILE: [&str; 8] = [
        "1 = 1; drop table orders",
        "1 = 1) union select * from secrets where (1 = 1",
        "1 = 1) or (select count(*) from secrets) > 0 limit 5 -- ",
        "id in (1, 2",
        "1 = 1) limit 1 offset (0",
        "1 = 1) group by id having (1 = 1",
        "1 = 1 order by id",
        "1 = 1) ; delete from orders where (1 = 1",
    ];

    #[tokio::test]
    async fn a_filter_is_checked_before_it_is_remembered() {
        let history = QueryHistory::open(path("record")).unwrap();
        let dialect = PostgresDialect;
        for filter in ["status = 'new'", "  total > 10 ", "status = 'new'"] {
            history
                .record_table_filter(&dialect, orders(), filter)
                .await
                .unwrap();
        }
        for filter in HOSTILE {
            let refused = history
                .record_table_filter(&dialect, orders(), filter)
                .await;
            assert!(
                matches!(refused, Err(TableFilterError::Refused(_))),
                "{filter}: {refused:?}"
            );
            let refused = history
                .set_table_default_filter(&dialect, orders(), Some(filter))
                .await;
            assert!(
                matches!(refused, Err(TableFilterError::Refused(_))),
                "{filter}: {refused:?}"
            );
        }
        // A blank filter is nothing to remember, and no error.
        let kept = history
            .record_table_filter(&dialect, orders(), "  ")
            .await
            .unwrap();
        assert_eq!(kept.history, ["status = 'new'", "total > 10"]);
        assert_eq!(kept.default, None);
        let kept = history
            .forget_table_filter(&dialect, orders(), "total > 10")
            .await
            .unwrap();
        assert_eq!(kept.history, ["status = 'new'"]);
    }

    #[tokio::test]
    async fn a_connection_with_its_history_off_remembers_no_filter() {
        let history = QueryHistory::open(path("history-off")).unwrap();
        let dialect = PostgresDialect;
        history
            .record_table_filter(&dialect, orders(), "id = 1")
            .await
            .unwrap();
        history
            .set_enabled("/repo".into(), "shop".into(), false)
            .await
            .unwrap();
        let kept = history
            .record_table_filter(&dialect, orders(), "email = 'ada@example.com'")
            .await
            .unwrap();
        // What was remembered before stays; the new one, with its value, is not kept.
        assert_eq!(kept.history, ["id = 1"]);
        // A filter a page would not run is still refused, history or not.
        assert!(matches!(
            history
                .record_table_filter(&dialect, orders(), "1 = 1; drop table orders")
                .await,
            Err(TableFilterError::Refused(_))
        ));
        // Saving a default is the user's own act: it does not depend on the history.
        let saved = history
            .set_table_default_filter(&dialect, orders(), Some("id > 5"))
            .await
            .unwrap();
        assert_eq!(saved.default.as_deref(), Some("id > 5"));
        // Another connection's history is its own.
        let other = FilteredTable {
            connection: "other".into(),
            ..orders()
        };
        let kept = history
            .record_table_filter(&dialect, other, "id = 2")
            .await
            .unwrap();
        assert_eq!(kept.history, ["id = 2"]);
    }

    #[tokio::test]
    async fn a_saved_default_is_checked_again_when_the_tab_opens() {
        let path = path("default");
        let history = QueryHistory::open(path.clone()).unwrap();
        let dialect = PostgresDialect;
        let saved = history
            .set_table_default_filter(&dialect, orders(), Some(" status = 'new' "))
            .await
            .unwrap();
        assert_eq!(saved.default.as_deref(), Some("status = 'new'"));
        drop(history);

        // The tab opens later, in another run of the app: the default is read and checked.
        let history = QueryHistory::open(path.clone()).unwrap();
        let opened = history.table_filters(&dialect, orders()).await.unwrap();
        assert_eq!(opened.default.as_deref(), Some("status = 'new'"));
        assert_eq!(opened.refused_default, None);
        drop(history);

        // Something else wrote the store meanwhile (a file copied from another machine, an
        // older build, a hand edit): what it holds is not a filter because it is there.
        for filter in HOSTILE {
            let store = Store::open(&path).unwrap();
            store
                .set_table_default_filter(&orders(), Some(filter))
                .unwrap();
            store.record_table_filter(&orders(), filter).unwrap();
            store.record_table_filter(&orders(), "id = 7").unwrap();
            drop(store);
            let history = QueryHistory::open(path.clone()).unwrap();
            let opened = history.table_filters(&dialect, orders()).await.unwrap();
            assert_eq!(opened.default, None, "{filter}");
            let refused = opened.refused_default.expect(filter);
            // The store keeps a filter trimmed.
            assert_eq!(refused.filter, filter.trim());
            assert!(!refused.reason.is_empty());
            assert!(
                !opened.history.iter().any(|kept| kept == filter.trim()),
                "{filter}: {:?}",
                opened.history
            );
            assert_eq!(opened.history[0], "id = 7", "{filter}");
        }

        // Dropping it is always possible, and then the tab opens with nothing.
        let history = QueryHistory::open(path).unwrap();
        let dropped = history
            .set_table_default_filter(&dialect, orders(), None)
            .await
            .unwrap();
        assert_eq!((dropped.default, dropped.refused_default), (None, None));
    }

    #[tokio::test]
    async fn the_check_is_the_dialect_s_of_the_tab_that_opens() {
        // A filter that is one condition for MySQL (a `#` comment to the end of the line) is
        // something else for the others: it was kept for a MySQL table, and stays there.
        let history = QueryHistory::open(path("dialects")).unwrap();
        let filter = "id = 1 # the first";
        let saved = history
            .set_table_default_filter(&MysqlDialect, orders(), Some(filter))
            .await;
        if saved.is_ok() {
            for dialect in [
                &PostgresDialect as &dyn Dialect,
                &SqlServerDialect,
                &SqliteDialect,
            ] {
                let opened = history.table_filters(dialect, orders()).await.unwrap();
                assert_eq!(opened.default, None, "{}", dialect.name());
                assert!(opened.refused_default.is_some(), "{}", dialect.name());
            }
        }
        // And every dialect keeps and gives back a plain one.
        for dialect in [
            &PostgresDialect as &dyn Dialect,
            &MysqlDialect,
            &SqlServerDialect,
            &SqliteDialect,
        ] {
            let saved = history
                .set_table_default_filter(dialect, orders(), Some("total >= 10 OR status IS NULL"))
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", dialect.name()));
            assert_eq!(
                saved.default.as_deref(),
                Some("total >= 10 OR status IS NULL"),
                "{}",
                dialect.name()
            );
        }
    }
}
