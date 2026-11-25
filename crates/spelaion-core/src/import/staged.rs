//! Imports into ClickHouse (ADR-066), which has no transactions: the rows load into a table of
//! our own beside the target, and one atomic `RENAME TABLE` or `EXCHANGE TABLES` puts it in
//! place when every row is in. A failure or a cancel drops that table, so the target is either
//! the whole import or what it was.
//!
//! Only `Atomic` databases promise the swap, and only a new table (or one replaced whole) can be
//! loaded this way: rows added to an existing table could not be taken back.

use futures_util::StreamExt;
use spelaion_driver_api::{Dialect, ExecEvent, ExecOptions, LogicalType, Value};

use super::ImportError;
use crate::Session;
use crate::objects::qualified_name;

/// The start of every table of our own: tells them apart in the explorer and `system.tables`.
pub(crate) const STAGING_PREFIX: &str = "__spelaion_import_";

/// The table engine of a new table: no sort key the user did not choose.
pub(super) const TABLE_ENGINE: &str = "ENGINE = MergeTree ORDER BY tuple()";

/// Whether an import on this dialect loads aside and swaps in, instead of in transactions.
pub(super) fn applies(dialect: &dyn Dialect) -> bool {
    applies_to(dialect.name())
}

/// The same, by the dialect's name: what the preview tells the import dialog (SDB-460).
pub(super) fn applies_to(engine: &str) -> bool {
    engine == "clickhouse"
}

/// A new name for a table of our own: the prefix and a random v4 UUID (122 random bits). Its
/// `CREATE TABLE` has no `IF NOT EXISTS`, so a name already taken fails instead of being reused.
pub(super) fn staging_name() -> String {
    format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

/// Why rows cannot be added to the existing table `table`.
pub(super) fn no_append(table: &str) -> ImportError {
    ImportError::Invalid(format!(
        "ClickHouse has no transactions, so rows added to {table} could not be taken back if \
         the import failed: import into a new table, which is created whole or not at all, and \
         run INSERT … SELECT yourself"
    ))
}

/// What puts the loaded table in place: a rename for a new table, or a swap with the table it
/// replaces and the drop of what was swapped out.
pub(super) fn finish(
    dialect: &dyn Dialect,
    schema: &str,
    staging: &str,
    table: &str,
    replaces: bool,
) -> Vec<String> {
    let ours = qualified_name(dialect, schema, &[staging.to_string()]);
    let target = qualified_name(dialect, schema, &[table.to_string()]);
    if replaces {
        vec![
            format!("EXCHANGE TABLES {ours} AND {target}"),
            format!("DROP TABLE {ours}"),
        ]
    } else {
        vec![format!("RENAME TABLE {ours} TO {target}")]
    }
}

/// Refuses a database whose engine does not promise the swap (anything but `Atomic`).
pub(super) async fn check_atomic(session: &Session, schema: &str) -> Result<(), ImportError> {
    let (sql, params) = if schema.is_empty() {
        (
            "SELECT engine FROM system.databases WHERE name = currentDatabase()".to_string(),
            Vec::new(),
        )
    } else {
        (
            "SELECT engine FROM system.databases WHERE name = {p1:String}".to_string(),
            vec![schema.to_string()],
        )
    };
    let mut events = session
        .connection()
        .execute_bound(&sql, params, &ExecOptions::default())
        .await?;
    let mut engine = None;
    while let Some(event) = events.next().await {
        if let ExecEvent::Batch(batch) = event?
            && batch.num_rows() > 0
            && batch.num_columns() == 1
            && engine.is_none()
        {
            engine =
                Value::from_array(batch.column(0), 0, LogicalType::Text).map(|v| v.to_string());
        }
    }
    match engine.as_deref() {
        Some("Atomic") => Ok(()),
        Some(other) => Err(ImportError::Invalid(format!(
            "the database's engine is {other}, which does not rename tables atomically: an \
             import into ClickHouse needs an Atomic database, so a failed import leaves nothing \
             half loaded"
        ))),
        None => Err(ImportError::Invalid(format!(
            "there is no database {schema} to import into"
        ))),
    }
}

/// The tables of our own in `schema` that an import left behind (the app or the connection died
/// before it ended). They are listed, never dropped here: another import may be loading into one.
pub(super) async fn leftovers(session: &Session, schema: &str) -> Result<Vec<String>, ImportError> {
    let mut names: Vec<String> = session
        .connection()
        .list_objects(schema)
        .await?
        .into_iter()
        .map(|o| o.name)
        .filter(|name| name.starts_with(STAGING_PREFIX))
        .collect();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_of_our_own_are_recognisable_and_never_the_same() {
        let a = staging_name();
        let b = staging_name();
        assert!(a.starts_with(STAGING_PREFIX), "{a}");
        assert_eq!(a.len(), STAGING_PREFIX.len() + 32, "{a}");
        assert!(
            a[STAGING_PREFIX.len()..]
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
        );
        assert_ne!(a, b);
    }
}
