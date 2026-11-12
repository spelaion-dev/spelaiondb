//! The dialect a session splits its scripts with, as the session's `sql_mode` reads SQL text
//! (SDB-161). On MySQL and MariaDB, `NO_BACKSLASH_ESCAPES` makes `\` an ordinary character in
//! a string and `ANSI_QUOTES` makes `"…"` a name, so where a statement ends depends on the
//! session. The mode is read when the core opens a session (after its setup, ADR-037) and
//! again after a statement that names `sql_mode`; the next run splits as the session now
//! reads text. A script is split once, when it starts, as the `mysql` client does.
//!
//! Literals the core writes do not depend on it: `export::sql_literal` writes a MySQL string
//! that holds a backslash in a form every mode reads the same way.

use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use spelaion_driver_api::{Connection, Dialect, ExecEvent, ExecOptions, Value};

/// The driver's dialect, and the one for the session's current `sql_mode`.
pub(crate) struct SessionDialect {
    base: Arc<dyn Dialect>,
    current: RwLock<Arc<dyn Dialect>>,
    /// The dialect reads text differently by `sql_mode` (MySQL, MariaDB).
    follows_sql_mode: bool,
}

impl SessionDialect {
    pub(crate) fn new(base: Arc<dyn Dialect>) -> Self {
        let follows_sql_mode = base.with_sql_mode("").is_some();
        Self {
            current: RwLock::new(base.clone()),
            base,
            follows_sql_mode,
        }
    }

    pub(crate) fn current(&self) -> Arc<dyn Dialect> {
        self.current.read().expect("dialect lock poisoned").clone()
    }

    /// Reads the session's `sql_mode` and splits from now on as it reads text. A mode that
    /// cannot be read keeps the dialect as it was.
    pub(crate) async fn refresh(&self, connection: &dyn Connection) {
        if !self.follows_sql_mode {
            return;
        }
        let Some(mode) = read_sql_mode(connection).await else {
            return;
        };
        if let Some(dialect) = self.base.with_sql_mode(&mode) {
            *self.current.write().expect("dialect lock poisoned") = dialect;
        }
    }

    pub(crate) fn follows_sql_mode(&self) -> bool {
        self.follows_sql_mode
    }
}

/// A statement that may change the session's `sql_mode` (`SET sql_mode = …`,
/// `SET @@SESSION.sql_mode = …`, `SET SESSION sql_mode …`): read it again afterwards.
pub(crate) fn may_change_sql_mode(statement: &str) -> bool {
    statement.to_ascii_lowercase().contains("sql_mode")
}

async fn read_sql_mode(connection: &dyn Connection) -> Option<String> {
    let mut events = connection
        .execute("select @@session.sql_mode", &ExecOptions::default())
        .await
        .ok()?;
    let mut mode = None;
    let mut logical = None;
    while let Some(event) = events.next().await {
        match event.ok()? {
            ExecEvent::Columns(schema) => logical = schema.columns.first().map(|c| c.logical),
            ExecEvent::Batch(batch) if mode.is_none() && batch.num_rows() > 0 => {
                mode = match Value::from_array(batch.column(0).as_ref(), 0, logical?)? {
                    Value::Null => Some(String::new()),
                    Value::Text(text) => Some(text),
                    other => Some(other.to_string()),
                };
            }
            _ => {}
        }
    }
    mode
}

#[cfg(test)]
mod tests {
    use super::may_change_sql_mode;

    #[test]
    fn statements_that_may_change_the_mode() {
        for sql in [
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
            "set @@session.SQL_MODE = concat(@@sql_mode, ',ANSI_QUOTES')",
            "SET SESSION sql_mode='ANSI'",
        ] {
            assert!(may_change_sql_mode(sql), "{sql}");
        }
        assert!(!may_change_sql_mode("set names utf8mb4"));
    }
}
