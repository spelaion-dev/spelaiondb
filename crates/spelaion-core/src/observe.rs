//! Reads for server observation (locks, server settings, table statistics): every statement
//! is written by the core and must pass the read-only classifier before it runs, so nothing
//! here can change the server, whatever a later edit to the SQL does.

use futures_util::StreamExt;
use spelaion_driver_api::Value;

use crate::classify::classify_read_only;
use crate::{CoreEvent, Session};

/// The rows of one read, every cell as the server's text (`None` for `NULL`).
pub(crate) struct Rows {
    columns: Vec<String>,
    pub(crate) rows: Vec<Vec<Option<String>>>,
}

impl Rows {
    /// The index of a column by its name (case-insensitive).
    pub(crate) fn column(&self, name: &str) -> Result<usize, String> {
        self.columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("the server's answer has no {name} column"))
    }
}

/// Runs one read written by the core on `session`, as internal metadata (never in the user's
/// transaction, never counted as one of their statements), and frees its result. A statement
/// the read-only classifier does not accept is refused before it reaches the server.
pub(crate) async fn read(session: &Session, sql: &str) -> Result<Rows, String> {
    let engine = session.dialect().name();
    classify_read_only(engine, sql).map_err(|reason| {
        format!("refused an observation statement that is not a read: {reason}")
    })?;
    let mut events = session.run_metadata(sql);
    let mut result = None;
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result: id, .. } => result = Some(id),
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    let store = session.results();
    if let Some(error) = failure {
        if let Some(id) = result {
            store.remove(id);
        }
        return Err(error);
    }
    let id = result.ok_or("the server returned no result")?;
    let rows = (|| {
        let stored = store.get(id).ok_or("the result is no longer available")?;
        let stored = stored.read().map_err(|_| "the result is unavailable")?;
        let schema = stored.schema();
        let columns: Vec<String> = schema.columns.iter().map(|c| c.name.clone()).collect();
        let mut rows = Vec::new();
        for batch in stored.window(0, stored.rows()) {
            for row in 0..batch.num_rows() {
                let mut cells = Vec::with_capacity(columns.len());
                for (index, column) in schema.columns.iter().enumerate() {
                    let value =
                        Value::from_array(batch.column(index).as_ref(), row, column.logical)
                            .ok_or("the server returned a value that could not be read")?;
                    cells.push(match value {
                        Value::Null => None,
                        other => Some(other.to_string()),
                    });
                }
                rows.push(cells);
            }
        }
        Ok::<_, String>(Rows { columns, rows })
    })();
    store.remove(id);
    rows
}

/// A whole number from a server's text: `123`, `123.0` (a `SUM` in PostgreSQL is `numeric`).
pub(crate) fn whole_number(text: Option<&str>) -> Option<i64> {
    let text = text?.trim();
    let integer = text.split_once('.').map_or(text, |(int, _)| int);
    integer.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::whole_number;

    #[test]
    fn whole_numbers_from_server_text() {
        assert_eq!(whole_number(Some("123")), Some(123));
        assert_eq!(whole_number(Some("123.000")), Some(123));
        assert_eq!(whole_number(Some(" -4 ")), Some(-4));
        assert_eq!(whole_number(Some("abc")), None);
        assert_eq!(whole_number(None), None);
    }
}
