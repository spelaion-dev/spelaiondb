//! What the table data tab remembers about a table's filter (SDB-381): the filters used on
//! it, newest first, and the one the tab opens with. Per workspace and connection like the
//! explorer's filters and bookmarks, and only ever here: local data that never leaves the
//! machine. The text is kept as it was typed; whoever reads it back checks it again before
//! anything runs.

use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError, now_ms};

/// Filters kept per table; using one more forgets the oldest.
pub const TABLE_FILTER_HISTORY: usize = 50;

/// Longest filter kept, in characters. A longer one still runs; it is not remembered.
pub const TABLE_FILTER_MAX_CHARS: usize = 4_000;

pub(crate) const MIGRATION: &str = "
create table table_filter_history (
    workspace   text not null,
    connection  text not null,
    schema      text not null,
    name        text not null,
    filter      text not null check (length(filter) between 1 and 4000),
    used_ms     integer not null,
    primary key (workspace, connection, schema, name, filter)
);
create index table_filter_history_recent
    on table_filter_history(workspace, connection, schema, name, used_ms desc);
create table table_filter_defaults (
    workspace   text not null,
    connection  text not null,
    schema      text not null,
    name        text not null,
    filter      text not null check (length(filter) between 1 and 4000),
    primary key (workspace, connection, schema, name)
);
";

/// The connection's other databases (ADR-074, SDB-505): a table's filters are kept per database,
/// empty for the connection's own. SQLite cannot change a primary key, so both tables are made
/// again with the database in theirs and every row copied into them as it was, its database
/// empty: what a row meant, it still means.
pub(crate) const BY_DATABASE: &str = "
create table table_filter_history_by_database (
    workspace   text not null,
    connection  text not null,
    database    text not null default '',
    schema      text not null,
    name        text not null,
    filter      text not null check (length(filter) between 1 and 4000),
    used_ms     integer not null,
    primary key (workspace, connection, database, schema, name, filter)
);
insert into table_filter_history_by_database
    (workspace, connection, database, schema, name, filter, used_ms)
    select workspace, connection, '', schema, name, filter, used_ms from table_filter_history;
drop index table_filter_history_recent;
drop table table_filter_history;
alter table table_filter_history_by_database rename to table_filter_history;
create index table_filter_history_recent
    on table_filter_history(workspace, connection, database, schema, name, used_ms desc);
create table table_filter_defaults_by_database (
    workspace   text not null,
    connection  text not null,
    database    text not null default '',
    schema      text not null,
    name        text not null,
    filter      text not null check (length(filter) between 1 and 4000),
    primary key (workspace, connection, database, schema, name)
);
insert into table_filter_defaults_by_database
    (workspace, connection, database, schema, name, filter)
    select workspace, connection, '', schema, name, filter from table_filter_defaults;
drop table table_filter_defaults;
alter table table_filter_defaults_by_database rename to table_filter_defaults;
";

/// A table of a connection, as the store names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredTable {
    pub workspace: String,
    pub connection: String,
    /// The other database of the connection it is in (ADR-074, SDB-505); `None` for the
    /// connection's own, as every row written before databases had a level.
    pub database: Option<String>,
    pub schema: String,
    pub name: String,
}

impl FilteredTable {
    /// The database as the store keeps it: empty for the connection's own.
    fn database(&self) -> &str {
        self.database.as_deref().unwrap_or("")
    }
}

/// `filter` as it is kept: trimmed, and `None` when there is nothing to keep (blank, too
/// long, or holding a NUL, which SQLite's text would cut).
fn kept(filter: &str) -> Option<&str> {
    let filter = filter.trim();
    let fits = !filter.is_empty()
        && filter.chars().count() <= TABLE_FILTER_MAX_CHARS
        && !filter.contains('\0');
    fits.then_some(filter)
}

impl Store {
    /// Remembers that `filter` was used on `table`, as its newest one. Using it again moves
    /// it to the front instead of listing it twice. Returns whether it was kept.
    pub fn record_table_filter(
        &self,
        table: &FilteredTable,
        filter: &str,
    ) -> Result<bool, StoreError> {
        let Some(filter) = kept(filter) else {
            return Ok(false);
        };
        let transaction = self.conn.unchecked_transaction()?;
        // Never the same instant as another: two filters used within one millisecond still
        // have an order.
        transaction.execute(
            "insert into table_filter_history
                 (workspace, connection, database, schema, name, filter, used_ms)
             values (?1, ?2, ?7, ?3, ?4, ?5, max(?6, coalesce((
                select max(used_ms) + 1 from table_filter_history
                where workspace = ?1 and connection = ?2 and database = ?7 and schema = ?3
                  and name = ?4), 0)))
             on conflict (workspace, connection, database, schema, name, filter)
             do update set used_ms = excluded.used_ms",
            params![
                table.workspace,
                table.connection,
                table.schema,
                table.name,
                filter,
                now_ms(),
                table.database()
            ],
        )?;
        transaction.execute(
            "delete from table_filter_history
             where workspace = ?1 and connection = ?2 and database = ?6 and schema = ?3
               and name = ?4 and filter not in (
                select filter from table_filter_history
                where workspace = ?1 and connection = ?2 and database = ?6 and schema = ?3
                  and name = ?4
                order by used_ms desc limit ?5)",
            params![
                table.workspace,
                table.connection,
                table.schema,
                table.name,
                TABLE_FILTER_HISTORY as i64,
                table.database()
            ],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// The filters used on `table`, newest first.
    pub fn table_filter_history(&self, table: &FilteredTable) -> Result<Vec<String>, StoreError> {
        let mut statement = self.conn.prepare(
            "select filter from table_filter_history
             where workspace = ?1 and connection = ?2 and database = ?6 and schema = ?3
               and name = ?4
             order by used_ms desc limit ?5",
        )?;
        let rows = statement.query_map(
            params![
                table.workspace,
                table.connection,
                table.schema,
                table.name,
                TABLE_FILTER_HISTORY as i64,
                table.database()
            ],
            |r| r.get(0),
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forgets one filter of `table`'s history.
    pub fn forget_table_filter(
        &self,
        table: &FilteredTable,
        filter: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "delete from table_filter_history
             where workspace = ?1 and connection = ?2 and database = ?6 and schema = ?3
               and name = ?4 and filter = ?5",
            params![
                table.workspace,
                table.connection,
                table.schema,
                table.name,
                filter.trim(),
                table.database()
            ],
        )?;
        Ok(())
    }

    /// The filter `table`'s tab opens with, if it has one.
    pub fn table_default_filter(
        &self,
        table: &FilteredTable,
    ) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "select filter from table_filter_defaults
                 where workspace = ?1 and connection = ?2 and database = ?5 and schema = ?3
                   and name = ?4",
                params![
                    table.workspace,
                    table.connection,
                    table.schema,
                    table.name,
                    table.database()
                ],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Sets the filter `table`'s tab opens with; `None` or a blank one drops it. One that
    /// cannot be kept (too long) is an error: the tab must not say it saved it.
    pub fn set_table_default_filter(
        &self,
        table: &FilteredTable,
        filter: Option<&str>,
    ) -> Result<(), StoreError> {
        let filter = filter.map(str::trim).filter(|f| !f.is_empty());
        let Some(filter) = filter else {
            self.conn.execute(
                "delete from table_filter_defaults
                 where workspace = ?1 and connection = ?2 and database = ?5 and schema = ?3
                   and name = ?4",
                params![
                    table.workspace,
                    table.connection,
                    table.schema,
                    table.name,
                    table.database()
                ],
            )?;
            return Ok(());
        };
        let Some(filter) = kept(filter) else {
            return Err(StoreError::new(format!(
                "a default filter holds at most {TABLE_FILTER_MAX_CHARS} characters"
            )));
        };
        self.conn.execute(
            "insert into table_filter_defaults (workspace, connection, database, schema, name, filter)
             values (?1, ?2, ?6, ?3, ?4, ?5)
             on conflict (workspace, connection, database, schema, name)
             do update set filter = excluded.filter",
            params![
                table.workspace,
                table.connection,
                table.schema,
                table.name,
                filter,
                table.database()
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIGRATIONS;
    use crate::tests::temp;

    fn table(workspace: &str, connection: &str, name: &str) -> FilteredTable {
        FilteredTable {
            workspace: workspace.into(),
            connection: connection.into(),
            database: None,
            schema: "public".into(),
            name: name.into(),
        }
    }

    #[test]
    fn history_is_newest_first_once_each_and_survives_a_restart() {
        let path = temp("table-filter-history");
        let store = Store::open(&path).unwrap();
        let orders = table("/repo", "shop", "orders");
        assert!(store.table_filter_history(&orders).unwrap().is_empty());
        for filter in ["status = 'new'", "  total > 10  ", "id < 5"] {
            assert!(store.record_table_filter(&orders, filter).unwrap());
        }
        assert_eq!(
            store.table_filter_history(&orders).unwrap(),
            ["id < 5", "total > 10", "status = 'new'"]
        );
        // Used again: moved to the front, not listed twice.
        assert!(
            store
                .record_table_filter(&orders, "status = 'new'")
                .unwrap()
        );
        assert_eq!(
            store.table_filter_history(&orders).unwrap(),
            ["status = 'new'", "id < 5", "total > 10"]
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.table_filter_history(&orders).unwrap().len(), 3);
        store.forget_table_filter(&orders, " id < 5 ").unwrap();
        assert_eq!(
            store.table_filter_history(&orders).unwrap(),
            ["status = 'new'", "total > 10"]
        );
    }

    #[test]
    fn history_is_per_workspace_connection_schema_and_table() {
        let store = Store::open(&temp("table-filter-scope")).unwrap();
        let orders = table("/repo", "shop", "orders");
        store.record_table_filter(&orders, "id = 1").unwrap();
        store
            .set_table_default_filter(&orders, Some("id = 1"))
            .unwrap();
        for other in [
            table("/other", "shop", "orders"),
            table("/repo", "other", "orders"),
            table("/repo", "shop", "customers"),
            // The same table of another database of the connection (ADR-074, SDB-505).
            FilteredTable {
                database: Some("billing".into()),
                ..orders.clone()
            },
            FilteredTable {
                schema: "audit".into(),
                ..orders.clone()
            },
        ] {
            assert!(
                store.table_filter_history(&other).unwrap().is_empty(),
                "{other:?}"
            );
            assert_eq!(store.table_default_filter(&other).unwrap(), None);
        }
    }

    #[test]
    fn history_is_bounded_and_keeps_only_what_fits() {
        let store = Store::open(&temp("table-filter-bounds")).unwrap();
        let orders = table("/repo", "shop", "orders");
        for i in 0..TABLE_FILTER_HISTORY + 7 {
            store
                .record_table_filter(&orders, &format!("id = {i}"))
                .unwrap();
        }
        let history = store.table_filter_history(&orders).unwrap();
        assert_eq!(history.len(), TABLE_FILTER_HISTORY);
        assert_eq!(history[0], format!("id = {}", TABLE_FILTER_HISTORY + 6));
        assert_eq!(history[TABLE_FILTER_HISTORY - 1], "id = 7");
        // Nothing to remember in a blank filter, and one too long or with a NUL is not kept.
        for filter in [
            String::new(),
            "  \n ".to_string(),
            "x".repeat(TABLE_FILTER_MAX_CHARS + 1),
            "a = '\0'".to_string(),
        ] {
            assert!(!store.record_table_filter(&orders, &filter).unwrap());
        }
        assert_eq!(store.table_filter_history(&orders).unwrap(), history);
        // Characters, not bytes: the longest one that fits.
        let longest = "ñ".repeat(TABLE_FILTER_MAX_CHARS);
        assert!(store.record_table_filter(&orders, &longest).unwrap());
        assert_eq!(store.table_filter_history(&orders).unwrap()[0], longest);
    }

    #[test]
    fn a_default_filter_is_one_per_table_and_can_be_dropped() {
        let path = temp("table-filter-default");
        let store = Store::open(&path).unwrap();
        let orders = table("/repo", "shop", "orders");
        assert_eq!(store.table_default_filter(&orders).unwrap(), None);
        store
            .set_table_default_filter(&orders, Some(" status = 'new' "))
            .unwrap();
        store
            .set_table_default_filter(&orders, Some("total > 10"))
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.table_default_filter(&orders).unwrap().as_deref(),
            Some("total > 10")
        );
        // A default is not history, and history is not a default.
        assert!(store.table_filter_history(&orders).unwrap().is_empty());
        assert!(
            store
                .set_table_default_filter(&orders, Some(&"x".repeat(TABLE_FILTER_MAX_CHARS + 1)))
                .is_err()
        );
        assert_eq!(
            store.table_default_filter(&orders).unwrap().as_deref(),
            Some("total > 10")
        );
        store.set_table_default_filter(&orders, Some("  ")).unwrap();
        assert_eq!(store.table_default_filter(&orders).unwrap(), None);
        store
            .set_table_default_filter(&orders, Some("a = 1"))
            .unwrap();
        store.set_table_default_filter(&orders, None).unwrap();
        assert_eq!(store.table_default_filter(&orders).unwrap(), None);
    }

    #[test]
    fn a_moved_connection_keeps_its_filters() {
        let store = Store::open(&temp("table-filter-move")).unwrap();
        let old = table("/repo", "shop", "orders");
        let new = table("/repo", "team/shop", "orders");
        store.record_table_filter(&old, "id = 1").unwrap();
        store
            .set_table_default_filter(&old, Some("id = 1"))
            .unwrap();
        // What an older connection left under the new identifier is replaced.
        store.record_table_filter(&new, "stale = 1").unwrap();
        store
            .move_connection_settings(Some("/repo"), "shop", "team/shop")
            .unwrap();
        assert_eq!(store.table_filter_history(&new).unwrap(), ["id = 1"]);
        assert_eq!(
            store.table_default_filter(&new).unwrap().as_deref(),
            Some("id = 1")
        );
        assert!(store.table_filter_history(&old).unwrap().is_empty());
        assert_eq!(store.table_default_filter(&old).unwrap(), None);
    }

    #[test]
    fn the_migration_keeps_what_the_store_already_had() {
        let path = temp("table-filter-migration");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let before = MIGRATIONS
                .iter()
                .position(|migration| *migration == MIGRATION)
                .unwrap();
            for migration in &MIGRATIONS[..before] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before as i64)
                .unwrap();
            conn.execute_batch(
                "insert into personal_settings values('language','es');
                 insert into explorer_bookmarks values('/repo','shop','public','orders','table','',1);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.personal_setting("language").unwrap().as_deref(),
            Some("es")
        );
        assert_eq!(store.bookmarks("/repo").unwrap().len(), 1);
        let orders = table("/repo", "shop", "orders");
        assert!(store.record_table_filter(&orders, "id = 1").unwrap());
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        // The checks are the tables' own: nothing blank gets in behind the functions.
        assert!(
            store
                .conn
                .execute(
                    "insert into table_filter_defaults values('/repo','shop','public','t','')",
                    [],
                )
                .is_err()
        );
    }
}
