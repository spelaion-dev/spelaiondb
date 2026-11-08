//! Foreign keys you can follow, both ways: from a row of a table data tab to the row its key
//! references, and back from a row to the rows of other tables whose keys reference it.
//!
//! The keys always come from the database catalog (`describe_table` for the keys a table has,
//! `list_referencing_keys` for the keys of other tables that point to it, ADR-024), read again
//! each time a key is followed: the UI only names the key it wants. The values come from the
//! stored, typed result (not from the text on screen), and the filter of the table data tab a
//! key opens is written by [`key_filter`] and checked as a page of that table, like any filter
//! of a table data tab.

use futures_util::StreamExt;
use spelaion_driver_api::{Connection, Dialect, DriverError, Value};

use crate::results::StoredResult;
use crate::session::{CoreEvent, Session};
use crate::table_data::{ConditionError, PageError, TablePage, key_filter, table_page_sql};

/// Which way a key goes, seen from the table it is listed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyDirection {
    /// A key of this table: it leads to the row it references.
    Outgoing,
    /// A key of another table that points to this one: it leads to the rows that reference
    /// this row.
    Incoming,
}

/// A foreign key seen from one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableKey {
    pub direction: KeyDirection,
    /// The constraint's name (SQLite's are `fk_<id>`).
    pub name: String,
    /// The other table: the referenced one (outgoing) or the referencing one (incoming).
    pub schema: String,
    pub table: String,
    /// This table's columns, in the key's order.
    pub columns: Vec<String>,
    /// The other table's columns, in the same order.
    pub other_columns: Vec<String>,
}

/// How the UI names a key it wants to follow: its direction, the other table and its name.
/// The key itself (its columns) is looked up in the catalog again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRef {
    pub direction: KeyDirection,
    pub schema: String,
    pub table: String,
    pub name: String,
}

impl TableKey {
    fn is(&self, key: &KeyRef) -> bool {
        self.direction == key.direction
            && self.schema == key.schema
            && self.table == key.table
            && self.name == key.name
    }
}

/// Where following a key leads: a table data tab of `schema.table` filtered by `filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyTarget {
    pub schema: String,
    pub table: String,
    /// What goes after `WHERE`, written by [`key_filter`].
    pub filter: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FollowError {
    #[error("could not read the foreign keys: {0}")]
    Catalog(String),
    #[error("the foreign key {0} is no longer in the catalog: refresh and try again")]
    NoSuchKey(String),
    #[error("the column {0} of the foreign key is not in this result")]
    MissingColumn(String),
    #[error("the row is no longer in the result")]
    NoSuchRow,
    #[error("{column} is NULL: this row references nothing through {key}")]
    Null { column: String, key: String },
    #[error("{0}")]
    Condition(ConditionError),
    #[error("{0}")]
    Page(PageError),
}

/// Every key that can be followed from `schema.table`: its own keys (outgoing) first, then
/// those of other tables that point to it (incoming), each in the catalog's order. A key of a
/// table that points to itself is listed both ways.
pub async fn table_keys(
    conn: &dyn Connection,
    schema: &str,
    table: &str,
) -> Result<Vec<TableKey>, DriverError> {
    let described = conn.describe_table(schema, table).await?;
    let referencing = conn.list_referencing_keys(schema, table).await?;
    let outgoing = described.foreign_keys.into_iter().map(|fk| TableKey {
        direction: KeyDirection::Outgoing,
        name: fk.name,
        schema: fk.referenced_schema,
        table: fk.referenced_table,
        columns: fk.columns,
        other_columns: fk.referenced_columns,
    });
    let incoming = referencing.into_iter().map(|key| TableKey {
        direction: KeyDirection::Incoming,
        name: key.name,
        schema: key.schema,
        table: key.table,
        columns: key.referenced_columns,
        other_columns: key.columns,
    });
    Ok(outgoing.chain(incoming).collect())
}

/// The key `key` names among `schema.table`'s, as the catalog has it now.
pub async fn find_key(
    conn: &dyn Connection,
    schema: &str,
    table: &str,
    key: &KeyRef,
) -> Result<TableKey, FollowError> {
    table_keys(conn, schema, table)
        .await
        .map_err(|e| FollowError::Catalog(e.to_string()))?
        .into_iter()
        .find(|k| k.is(key))
        .ok_or_else(|| FollowError::NoSuchKey(key.name.clone()))
}

/// The position of `name` among the result's columns: the exact name, or else the only one
/// that matches ignoring case (engines that fold names may write them differently).
fn column_index(stored: &StoredResult, name: &str) -> Option<usize> {
    let columns = &stored.schema().columns;
    columns.iter().position(|c| c.name == name).or_else(|| {
        let mut matching = columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.eq_ignore_ascii_case(name));
        match (matching.next(), matching.next()) {
            (Some((i, _)), None) => Some(i),
            _ => None,
        }
    })
}

/// Where `key` leads from the row at `position` of `stored` (a page of the key's table): the
/// other table filtered by the row's values of the key's columns. The filter is checked as a
/// page of that table before it is returned.
pub fn follow(
    dialect: &dyn Dialect,
    stored: &StoredResult,
    position: usize,
    key: &TableKey,
) -> Result<KeyTarget, FollowError> {
    if key.columns.len() != key.other_columns.len() {
        return Err(FollowError::Catalog(format!(
            "the foreign key {} has {} columns but references {}",
            key.name,
            key.columns.len(),
            key.other_columns.len()
        )));
    }
    let mut columns = Vec::with_capacity(key.columns.len());
    for (column, other) in key.columns.iter().zip(&key.other_columns) {
        let index = column_index(stored, column)
            .ok_or_else(|| FollowError::MissingColumn(column.clone()))?;
        let logical = stored.schema().columns[index].logical;
        let value = stored
            .value_at(position, index)
            .ok_or(FollowError::NoSuchRow)?;
        if value == Value::Null {
            return Err(FollowError::Null {
                column: column.clone(),
                key: key.name.clone(),
            });
        }
        columns.push((other.clone(), logical, value));
    }
    let filter = key_filter(dialect, &columns).map_err(FollowError::Condition)?;
    table_page_sql(
        dialect,
        &TablePage {
            schema: key.schema.clone(),
            name: key.table.clone(),
            filter: filter.clone(),
            order_by: String::new(),
            limit: 1,
            offset: 0,
        },
    )
    .map_err(FollowError::Page)?;
    Ok(KeyTarget {
        schema: key.schema.clone(),
        table: key.table.clone(),
        filter,
    })
}

/// Longest value [`key_rows`] returns per cell; the table data tab shows them whole.
pub const PREVIEW_CHARS: usize = 200;

/// Most rows [`key_rows`] may ask for.
pub const MAX_PREVIEW_ROWS: u64 = 50;

/// The first rows a key leads to, as text, for the references panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRows {
    pub columns: Vec<String>,
    /// Each cell as text (`None` for NULL), cut at [`PREVIEW_CHARS`].
    pub rows: Vec<Vec<Option<String>>>,
    /// There are more rows than those returned.
    pub more: bool,
}

/// Up to `limit` rows of `target`, read on `session` with the page's checked SELECT (one more
/// row is asked for, to know whether there are more). The result is freed, also on failure.
pub async fn key_rows(
    session: &Session,
    target: &KeyTarget,
    limit: u64,
) -> Result<KeyRows, String> {
    let limit = limit.clamp(1, MAX_PREVIEW_ROWS);
    let sql = table_page_sql(
        session.dialect().as_ref(),
        &TablePage {
            schema: target.schema.clone(),
            name: target.table.clone(),
            filter: target.filter.clone(),
            order_by: String::new(),
            limit: limit + 1,
            offset: 0,
        },
    )
    .map_err(|e| e.to_string())?;
    let mut events = session.run_generated(&sql);
    let mut results = Vec::new();
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result: id, .. } => results.push(id),
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
    let rows = results.first().and_then(|id| {
        let stored = store.get(*id)?;
        let stored = stored.read().ok()?;
        Some(preview(&stored, limit as usize))
    });
    for id in results {
        store.remove(id);
    }
    match (failure, rows) {
        (Some(message), _) => Err(message),
        (None, Some(rows)) => Ok(rows),
        (None, None) => Err("the query returned no result".into()),
    }
}

fn preview(stored: &StoredResult, limit: usize) -> KeyRows {
    let columns: Vec<String> = stored
        .schema()
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect();
    let shown = stored.rows().min(limit);
    let rows = (0..shown)
        .map(|row| {
            (0..columns.len())
                .map(|column| match stored.value_at(row, column) {
                    None | Some(Value::Null) => None,
                    Some(value) => Some(cut(&value.to_string())),
                })
                .collect()
        })
        .collect();
    KeyRows {
        columns,
        rows,
        more: stored.rows() > limit,
    }
}

fn cut(text: &str) -> String {
    match text.char_indices().nth(PREVIEW_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// What one key of a row leads to, in the references panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceOutcome {
    /// Where it leads, and its first rows (none: nothing matches the key).
    Rows(KeyTarget, KeyRows),
    /// A column of the key is NULL in this row: it references nothing through this key.
    Null { column: String },
    /// It could not be followed or read.
    Failed(String),
}

/// One key of a row in the references panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowReference {
    pub key: TableKey,
    pub outcome: ReferenceOutcome,
}

/// Each of `keys` (a table's, from [`table_keys`]) followed from the row at `position` of
/// `stored`, a page of that table. Kept apart from [`read_references`] so no lock on the
/// stored result is held while the targets are read.
pub fn reference_targets(
    dialect: &dyn Dialect,
    stored: &StoredResult,
    position: usize,
    keys: Vec<TableKey>,
) -> Vec<(TableKey, Result<KeyTarget, FollowError>)> {
    keys.into_iter()
        .map(|key| {
            let target = follow(dialect, stored, position, &key);
            (key, target)
        })
        .collect()
}

/// The first `limit` rows of each target of [`reference_targets`], read in turn on `session`.
pub async fn read_references(
    session: &Session,
    targets: Vec<(TableKey, Result<KeyTarget, FollowError>)>,
    limit: u64,
) -> Vec<RowReference> {
    let mut out = Vec::with_capacity(targets.len());
    for (key, target) in targets {
        let outcome = match target {
            Ok(target) => match key_rows(session, &target, limit).await {
                Ok(rows) => ReferenceOutcome::Rows(target, rows),
                Err(e) => ReferenceOutcome::Failed(e),
            },
            Err(FollowError::Null { column, .. }) => ReferenceOutcome::Null { column },
            Err(e) => ReferenceOutcome::Failed(e.to_string()),
        };
        out.push(RowReference { key, outcome });
    }
    out
}
