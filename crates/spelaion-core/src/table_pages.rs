//! The table data pages a session's core wrote (SDB-440). The page SELECT that the core writes
//! for a table data tab ([`crate::table_page_sql_by_key`], [`crate::table_rows_sql_by_key`]) is
//! recorded here with the table it reads. When a run sends exactly that statement, the core
//! binds the results it creates to that table. Only those results, and the query results the
//! core found editable, are ever edited: a page cannot name a table for rows the core did not
//! read from it.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::results::ResultTable;

/// How many recent page statements a session remembers: far more than the table data tabs one
/// connection has open, each of which asks for its page again before every load.
const KEPT: usize = 64;

#[derive(Default)]
pub(crate) struct TablePages {
    written: Mutex<VecDeque<(String, ResultTable)>>,
}

/// A statement's text as it is compared: without the spaces around it or a final semicolon.
fn statement_key(sql: &str) -> &str {
    let sql = sql.trim();
    sql.strip_suffix(';').unwrap_or(sql).trim_end()
}

impl TablePages {
    /// Records that `sql`, written by the core, reads `table`. The newest record of the same
    /// statement replaces an older one.
    pub(crate) fn record(&self, sql: &str, table: ResultTable) {
        let key = statement_key(sql).to_string();
        let mut written = self.written.lock().expect("table pages lock poisoned");
        written.retain(|(sql, _)| *sql != key);
        if written.len() == KEPT {
            written.pop_front();
        }
        written.push_back((key, table));
    }

    /// The table a statement reads, when it is a page statement the core wrote.
    pub(crate) fn table_of(&self, statement: &str) -> Option<ResultTable> {
        let key = statement_key(statement);
        let written = self.written.lock().expect("table pages lock poisoned");
        written
            .iter()
            .rev()
            .find(|(sql, _)| sql == key)
            .map(|(_, table)| table.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str, identity: &str) -> ResultTable {
        ResultTable {
            schema: "public".into(),
            name: name.into(),
            identity: Some(identity.into()),
        }
    }

    #[test]
    fn only_a_statement_the_core_wrote_names_a_table() {
        let pages = TablePages::default();
        pages.record("SELECT * FROM \"public\".\"t\" LIMIT 10", table("t", "1"));
        assert_eq!(
            pages.table_of("  SELECT * FROM \"public\".\"t\" LIMIT 10;\n"),
            Some(table("t", "1"))
        );
        assert_eq!(pages.table_of("SELECT * FROM \"public\".\"t\""), None);
        assert_eq!(pages.table_of("select * from t limit 10"), None);
    }

    #[test]
    fn the_newest_record_of_a_statement_wins_and_old_ones_are_forgotten() {
        let pages = TablePages::default();
        pages.record("SELECT 1 FROM t", table("t", "1"));
        pages.record("SELECT 1 FROM t", table("t", "2"));
        assert_eq!(pages.table_of("SELECT 1 FROM t"), Some(table("t", "2")));
        for n in 0..KEPT {
            pages.record(&format!("SELECT {n} FROM u"), table("u", "3"));
        }
        assert_eq!(pages.table_of("SELECT 1 FROM t"), None);
        assert!(
            pages
                .table_of(&format!("SELECT {} FROM u", KEPT - 1))
                .is_some()
        );
    }
}
