//! The grouping panel (SDB-387): how many rows of a stored result share the same values in
//! the columns chosen, like a `GROUP BY … COUNT(*)` over what the grid shows. It is computed
//! here over the result's current view (its sort and filters included), as the calc panel's
//! numbers are (`selection_stats.rs`): the UI sends the columns and receives the groups, so no
//! row travels to React.
//!
//! - Rows are in one group when each chosen column holds the same text, as the grid shows it
//!   (the database's own text), or NULL in both: NULL is a group of its own, apart from an
//!   empty text.
//! - A masked column (ADR-051) is read as the result serves it: by its mask. The original
//!   values kept aside for "reveal" are never read here.
//! - The answer is bounded: at most [`MAX_GROUPS_RETURNED`] groups come back (the largest
//!   first), each value cut at [`GROUP_KEY_CHARS`] characters, and at most
//!   [`MAX_GROUPS_COUNTED`] different groups are told apart.
//! - A newer grouping of the same result, or a cancel, stops one still running.
//! - "Filter to a group" is the result's column filters (`filter.rs`), written here from one
//!   row of the group: never a filter path of its own.

use std::collections::HashMap;
use std::fmt::Write as _;

use spelaion_driver_api::{LogicalType, Value};

use crate::filter::{ColumnFilter, FilterOp};
use crate::results::StoredResult;
use crate::selection_stats::StatsTicket;
use crate::sort::ViewError;

/// Most columns a grouping takes.
pub const MAX_GROUP_COLUMNS: usize = 8;

/// Most groups returned: the largest ones.
pub const MAX_GROUPS_RETURNED: usize = 1_000;

/// Most different groups told apart. Past them, rows of further groups are only counted as
/// rows ([`GroupCounts::uncounted_rows`]).
pub const MAX_GROUPS_COUNTED: usize = 1_000_000;

/// Most bytes of group keys kept while counting; past them no new group is told apart.
const MAX_KEY_BYTES: usize = 256 * 1024 * 1024;

/// Longest text returned for one value of a group's key (longer ones end in "…").
pub const GROUP_KEY_CHARS: usize = 200;

/// Rows read between two checks of whether the grouping was replaced or cancelled.
const CHECK_EVERY: usize = 4_096;

/// What to group: columns of the result by position, in the order the panel shows them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRequest {
    pub columns: Vec<usize>,
    /// Only groups with more than one row.
    pub duplicates_only: bool,
}

/// One group: its values and how many rows have them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The value of each grouped column, as text; `None` is NULL.
    pub key: Vec<Option<String>>,
    pub count: u64,
    /// The position in the current view of the group's first row: "filter to this group" is
    /// written from that row's own values.
    pub first_row: u64,
}

/// The groups of a result's current view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupCounts {
    /// The groups with the most rows first (among equals, the one met first), at most
    /// [`MAX_GROUPS_RETURNED`].
    pub groups: Vec<Group>,
    /// Rows of the view: what a group's percent is of.
    pub rows: u64,
    /// Groups there are (with "duplicates only", those of more than one row), returned or not.
    pub total_groups: u64,
    /// Different groups told apart, whatever "duplicates only" says.
    pub distinct: u64,
    /// Rows left out of every group because [`MAX_GROUPS_COUNTED`] groups (or their keys'
    /// memory) were already being counted: their groups are not in the answer.
    pub uncounted_rows: u64,
    /// The view these positions and counts belong to: a sort, a filter or more rows change it.
    pub generation: u64,
}

/// The grid filtered to a group: the column filters now in force, the rows they leave, and
/// whether filters that were in force before are now read matching case, which they did not
/// (SDB-410: the group's rows must be exactly its own).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupFiltered {
    pub filters: Vec<crate::filter::ColumnFilter>,
    pub rows: usize,
    pub case_switched: bool,
}

/// Why a result cannot be grouped as asked.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GroupError {
    #[error("choose a column to group by")]
    NoColumns,
    #[error("a grouping takes at most {MAX_GROUP_COLUMNS} columns")]
    TooManyColumns,
    #[error("column {0} is in the grouping twice")]
    RepeatedColumn(usize),
    #[error("the rows changed since they were grouped: group them again")]
    Stale,
    #[error("there is no such row")]
    NoSuchRow,
    #[error(
        "a group's conditions must all hold: switch the column filters to Match all to filter to a group"
    )]
    MatchAny,
    #[error(transparent)]
    View(#[from] ViewError),
}

/// The columns of a request with their types, checked.
fn columns_of(
    result: &StoredResult,
    columns: &[usize],
) -> Result<Vec<(usize, LogicalType)>, GroupError> {
    if columns.is_empty() {
        return Err(GroupError::NoColumns);
    }
    if columns.len() > MAX_GROUP_COLUMNS {
        return Err(GroupError::TooManyColumns);
    }
    let schema = result.schema();
    let mut checked = Vec::with_capacity(columns.len());
    for (i, &column) in columns.iter().enumerate() {
        let meta = schema
            .columns
            .get(column)
            .ok_or(ViewError::NoSuchColumn(column))?;
        if columns[..i].contains(&column) {
            return Err(GroupError::RepeatedColumn(column));
        }
        checked.push((column, meta.logical));
    }
    Ok(checked)
}

/// Groups the rows of `result`'s current view by the request's columns; `None` if a newer
/// grouping, or a cancel, stopped this one.
pub(crate) fn compute(
    result: &StoredResult,
    request: &GroupRequest,
    ticket: &StatsTicket,
) -> Result<Option<GroupCounts>, GroupError> {
    let columns = columns_of(result, &request.columns)?;
    let rows = result.rows();
    // The key of a row: each value's length and text, or a NULL mark. It is built in one
    // buffer and allocated only for a group not seen before.
    let mut counts: HashMap<Box<str>, (u64, usize)> = HashMap::new();
    let mut key = String::new();
    let mut text = String::new();
    let mut key_bytes = 0usize;
    let mut uncounted_rows = 0u64;
    for position in 0..rows {
        if position % CHECK_EVERY == 0 && ticket.is_stale() {
            return Ok(None);
        }
        let (batch, row) = result.locate(result.original_row(position));
        key.clear();
        for &(column, logical) in &columns {
            let value = Value::from_array(batch.column(column).as_ref(), row, logical)
                .ok_or(ViewError::UnexpectedArray(column))?;
            if matches!(value, Value::Null) {
                key.push_str("N;");
            } else {
                text.clear();
                let _ = write!(text, "{value}");
                let _ = write!(key, "{}:", text.len());
                key.push_str(&text);
            }
        }
        if let Some(entry) = counts.get_mut(key.as_str()) {
            entry.0 += 1;
        } else if counts.len() < MAX_GROUPS_COUNTED && key_bytes + key.len() <= MAX_KEY_BYTES {
            key_bytes += key.len();
            counts.insert(key.as_str().into(), (1, position));
        } else {
            uncounted_rows += 1;
        }
    }
    if ticket.is_stale() {
        return Ok(None);
    }
    let distinct = counts.len() as u64;
    let mut found: Vec<(u64, usize)> = counts
        .into_values()
        .filter(|&(count, _)| !request.duplicates_only || count > 1)
        .collect();
    let total_groups = found.len() as u64;
    // The most rows first; among equals, the group met first in the view.
    found.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found.truncate(MAX_GROUPS_RETURNED);
    let mut groups = Vec::with_capacity(found.len());
    for (count, first) in found {
        // The values are read again from the group's first row: the counting key is not kept
        // as text to show.
        let (batch, row) = result.locate(result.original_row(first));
        let mut shown = Vec::with_capacity(columns.len());
        for &(column, logical) in &columns {
            let value = Value::from_array(batch.column(column).as_ref(), row, logical)
                .ok_or(ViewError::UnexpectedArray(column))?;
            shown.push(match value {
                Value::Null => None,
                value => Some(short(&value.to_string())),
            });
        }
        groups.push(Group {
            key: shown,
            count,
            first_row: first as u64,
        });
    }
    Ok(Some(GroupCounts {
        groups,
        rows: rows as u64,
        total_groups,
        distinct,
        uncounted_rows,
        generation: result.generation(),
    }))
}

/// At most [`GROUP_KEY_CHARS`] characters of a value, ending in "…" when it was cut.
fn short(text: &str) -> String {
    match text.char_indices().nth(GROUP_KEY_CHARS) {
        None => text.to_string(),
        Some((end, _)) => format!("{}…", &text[..end]),
    }
}

/// The column filters that keep the rows of one group: one condition per grouped column,
/// written from the values of the row at `position` of the view the grouping was made over
/// (`generation`), so the UI never sends a value and a value cut for display is still
/// compared whole. NULL becomes `IS NULL`; a number is compared by value; a text is compared
/// as the column filters compare text. Refused when the view changed since the grouping.
pub(crate) fn group_filters(
    result: &StoredResult,
    columns: &[usize],
    position: usize,
    generation: u64,
) -> Result<Vec<ColumnFilter>, GroupError> {
    let columns = columns_of(result, columns)?;
    if result.generation() != generation {
        return Err(GroupError::Stale);
    }
    let mut filters = Vec::with_capacity(columns.len());
    for (column, _) in columns {
        let value = result
            .value_at(position, column)
            .ok_or(GroupError::NoSuchRow)?;
        filters.push(match value {
            Value::Null => ColumnFilter {
                column,
                op: FilterOp::IsNull,
                value: String::new(),
            },
            value => {
                let text = value.to_string();
                // `=` reads its value without the white space around it: a text that starts
                // or ends with some is matched whole with LIKE, its wildcards escaped.
                if text.trim() == text {
                    ColumnFilter {
                        column,
                        op: FilterOp::Eq,
                        value: text,
                    }
                } else {
                    ColumnFilter {
                        column,
                        op: FilterOp::Like,
                        value: escape_like(&text),
                    }
                }
            }
        });
    }
    Ok(filters)
}

/// A text as a LIKE pattern that matches only itself.
fn escape_like(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use spelaion_driver_api::{ColumnMeta, ResultSchema};

    use super::*;
    use crate::results::{ResultId, ResultStore};
    use crate::sort::SortKey;

    fn meta(name: &str, logical: LogicalType) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            logical,
            native_type: String::new(),
            native_id: None,
            origin: None,
        }
    }

    /// id (int), status (text), email (text): nine rows in two batches, with repeats, NULLs,
    /// an empty text and texts that differ only by case or by the space around them.
    fn store() -> (ResultStore, ResultId) {
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int),
            meta("status", LogicalType::Text),
            meta("email", LogicalType::Text),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        type Batch = (
            Vec<i64>,
            Vec<Option<&'static str>>,
            Vec<Option<&'static str>>,
        );
        let batches: [Batch; 2] = [
            (
                vec![1, 2, 3, 4, 5],
                vec![Some("open"), Some("done"), None, Some("open"), Some("")],
                vec![Some("a@x"), Some("b@x"), Some("a@x"), Some("A@x"), None],
            ),
            (
                vec![6, 7, 8, 9],
                vec![Some("open"), None, Some("done"), Some(" open")],
                vec![Some("a@x"), None, Some("c@x"), Some("a@x")],
            ),
        ];
        for (ids, statuses, emails) in batches {
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(statuses)),
                Arc::new(StringArray::from(emails)),
            ];
            store
                .append(
                    id,
                    RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
                )
                .unwrap();
        }
        store.complete(id);
        (store, id)
    }

    fn request(columns: &[usize], duplicates_only: bool) -> GroupRequest {
        GroupRequest {
            columns: columns.to_vec(),
            duplicates_only,
        }
    }

    fn run(r: &StoredResult, request: &GroupRequest) -> GroupCounts {
        r.grouping(request, &r.group_ticket())
            .unwrap()
            .expect("not cancelled")
    }

    /// Each group as `value|value=count@first row`, NULL written `∅`.
    fn shown(grouping: &GroupCounts) -> Vec<String> {
        grouping
            .groups
            .iter()
            .map(|g| {
                let key: Vec<&str> = g.key.iter().map(|v| v.as_deref().unwrap_or("∅")).collect();
                format!("{}={}@{}", key.join("|"), g.count, g.first_row)
            })
            .collect()
    }

    fn ids(r: &StoredResult) -> Vec<i64> {
        (0..r.rows())
            .map(|row| match r.value_at(row, 0) {
                Some(Value::Int(n)) => n,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn groups_count_rows_by_their_text_with_null_as_a_group_of_its_own() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let g = run(&r, &request(&[1], false));
        // The most rows first; among equals, the group met first. NULL, the empty text and
        // " open" are groups apart.
        assert_eq!(
            shown(&g),
            ["open=3@0", "done=2@1", "∅=2@2", "=1@4", " open=1@8"]
        );
        assert_eq!((g.rows, g.total_groups, g.distinct), (9, 5, 5));
        assert_eq!(g.uncounted_rows, 0);
        // Case tells groups apart: "a@x" and "A@x".
        let g = run(&r, &request(&[2], false));
        assert_eq!(
            shown(&g),
            ["a@x=4@0", "∅=2@4", "b@x=1@1", "A@x=1@3", "c@x=1@7"]
        );
        // Duplicates only: groups of more than one row; `distinct` still counts them all.
        let g = run(&r, &request(&[2], true));
        assert_eq!(shown(&g), ["a@x=4@0", "∅=2@4"]);
        assert_eq!((g.total_groups, g.distinct), (2, 5));
    }

    #[test]
    fn several_columns_make_one_key_and_their_values_never_run_together() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let g = run(&r, &request(&[1, 2], false));
        assert_eq!(
            shown(&g),
            [
                "open|a@x=2@0",
                "done|b@x=1@1",
                "∅|a@x=1@2",
                "open|A@x=1@3",
                "|∅=1@4",
                "∅|∅=1@6",
                "done|c@x=1@7",
                " open|a@x=1@8",
            ]
        );
        // The order of the columns is the order of the key.
        let g = run(&r, &request(&[2, 1], true));
        assert_eq!(shown(&g), ["a@x|open=2@0"]);
        // Values that would read the same glued together are different keys.
        let schema = Arc::new(ResultSchema::new(vec![
            meta("a", LogicalType::Text),
            meta("b", LogicalType::Text),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(vec![
                Some("ab"),
                Some("a"),
                Some("N;"),
                None,
            ])),
            Arc::new(StringArray::from(vec![
                Some("c"),
                Some("bc"),
                None,
                Some("N;"),
            ])),
        ];
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
            )
            .unwrap();
        store.complete(id);
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        assert_eq!(run(&r, &request(&[0, 1], false)).distinct, 4);
    }

    #[test]
    fn the_grouping_follows_the_views_sort_and_filters() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        r.filter_columns(vec![ColumnFilter {
            column: 2,
            op: FilterOp::Eq,
            value: "a@x".into(),
        }])
        .unwrap();
        // The column filter ignores case: ids 1, 3, 4, 6 and 9 are left.
        assert_eq!(ids(&r), [1, 3, 4, 6, 9]);
        let g = run(&r, &request(&[1], false));
        assert_eq!(shown(&g), ["open=3@0", "∅=1@1", " open=1@4"]);
        assert_eq!(g.rows, 5);
        // After a sort the first row of a group is its first in the new order.
        r.sort(&[SortKey {
            column: 0,
            descending: true,
        }])
        .unwrap();
        let g = run(&r, &request(&[1], false));
        assert_eq!(shown(&g), ["open=3@1", " open=1@0", "∅=1@3"]);
    }

    #[test]
    fn filtering_to_a_group_goes_through_the_column_filters() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        let g = run(&r, &request(&[1], false));
        // "done": one `=` condition, added to the result's column filters.
        let GroupFiltered { filters, rows, .. } = r.filter_to_group(&[1], 1, g.generation).unwrap();
        assert_eq!(
            filters,
            [ColumnFilter {
                column: 1,
                op: FilterOp::Eq,
                value: "done".into()
            }]
        );
        assert_eq!((rows, ids(&r)), (2, vec![2, 8]));
        assert_eq!(r.column_filters(), filters.as_slice());
        // The filter changed the view: the old grouping's positions are refused.
        assert_eq!(
            r.filter_to_group(&[1], 0, g.generation),
            Err(GroupError::Stale)
        );
        r.filter_columns(Vec::new()).unwrap();
        // The NULL group: IS NULL.
        let g = run(&r, &request(&[1], false));
        let GroupFiltered { filters, rows, .. } = r.filter_to_group(&[1], 2, g.generation).unwrap();
        assert_eq!(filters[0].op, FilterOp::IsNull);
        assert_eq!((rows, ids(&r)), (2, vec![3, 7]));
        r.filter_columns(Vec::new()).unwrap();
        // A text with white space around it is matched whole (LIKE): `=` would read "open".
        let g = run(&r, &request(&[1], false));
        let GroupFiltered { filters, rows, .. } = r.filter_to_group(&[1], 8, g.generation).unwrap();
        assert_eq!(
            (filters[0].op, filters[0].value.as_str()),
            (FilterOp::Like, " open")
        );
        assert_eq!((rows, ids(&r)), (1, vec![9]));
        r.filter_columns(Vec::new()).unwrap();
        // Two columns, one of them NULL, on top of a filter already in force.
        r.filter_columns(vec![ColumnFilter {
            column: 0,
            op: FilterOp::Gt,
            value: "2".into(),
        }])
        .unwrap();
        let g = run(&r, &request(&[1, 2], false));
        assert_eq!(shown(&g)[0], "∅|a@x=1@0");
        let GroupFiltered { filters, rows, .. } =
            r.filter_to_group(&[1, 2], 0, g.generation).unwrap();
        assert_eq!(filters.len(), 3);
        assert_eq!((rows, ids(&r)), (1, vec![3]));
        // The same group again adds nothing.
        let g = run(&r, &request(&[1, 2], false));
        let GroupFiltered { filters, .. } = r.filter_to_group(&[1, 2], 0, g.generation).unwrap();
        assert_eq!(filters.len(), 3);
        // A row that is not there.
        let g = run(&r, &request(&[1], false));
        assert_eq!(
            r.filter_to_group(&[1], 99, g.generation),
            Err(GroupError::NoSuchRow)
        );
    }

    #[test]
    fn filtering_to_a_group_reads_its_conditions_with_match_all_and_always_matches_case() {
        use crate::filter::FilterMode;
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        // No filter in force: case is matched, so "a@x" leaves its four rows and not "A@x".
        let g = run(&r, &request(&[2], false));
        assert_eq!(shown(&g)[0], "a@x=4@0");
        let GroupFiltered { rows, .. } = r.filter_to_group(&[2], 0, g.generation).unwrap();
        assert_eq!((rows, ids(&r)), (4, vec![1, 3, 6, 9]));
        assert_eq!(
            r.filter_mode(),
            FilterMode {
                any: false,
                match_case: true
            }
        );
        r.filter_columns(Vec::new()).unwrap();
        // A filter in force read ignoring case is read matching it from now on, and the
        // answer says so: the rows left are still exactly the group's, "A@x" is not let
        // through (SDB-410).
        let id_above = |value: &str| ColumnFilter {
            column: 0,
            op: FilterOp::Gt,
            value: value.into(),
        };
        let match_case = FilterMode {
            any: false,
            match_case: true,
        };
        r.filter_columns(vec![id_above("0")]).unwrap();
        let g = run(&r, &request(&[2], false));
        let filtered = r.filter_to_group(&[2], 0, g.generation).unwrap();
        assert_eq!((filtered.filters.len(), filtered.rows), (2, 4));
        assert!(filtered.case_switched);
        assert_eq!(ids(&r), vec![1, 3, 6, 9]);
        assert_eq!(r.filter_mode(), match_case);
        // A filter in force already read matching case: exact, and nothing switched.
        r.filter_columns_as(vec![id_above("0")], match_case)
            .unwrap();
        let g = run(&r, &request(&[2], false));
        let filtered = r.filter_to_group(&[2], 0, g.generation).unwrap();
        assert_eq!((filtered.rows, r.filter_mode()), (4, match_case));
        assert!(!filtered.case_switched);
        // "Match any" over one filter means the same as "Match all": the group's conditions
        // are added and must all hold.
        let any = FilterMode {
            any: true,
            match_case: false,
        };
        r.filter_columns_as(vec![id_above("5")], any).unwrap();
        let g = run(&r, &request(&[1], false));
        assert_eq!(shown(&g)[0], "open=1@0");
        let GroupFiltered { rows, .. } = r.filter_to_group(&[1], 0, g.generation).unwrap();
        assert_eq!((rows, ids(&r)), (1, vec![6]));
        assert!(!r.filter_mode().any);
        // Over two filters it would change what they mean: refused, nothing changed.
        r.filter_columns_as(vec![id_above("5"), id_above("1")], any)
            .unwrap();
        let g = run(&r, &request(&[1], false));
        assert_eq!(
            r.filter_to_group(&[1], 0, g.generation),
            Err(GroupError::MatchAny)
        );
        assert_eq!((r.rows(), r.filter_mode()), (8, any));
    }

    #[test]
    fn like_wildcards_in_a_value_are_matched_as_text() {
        assert_eq!(escape_like(" 50%_a\\b "), " 50\\%\\_a\\\\b ");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[test]
    fn what_cannot_be_grouped_is_refused() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let ticket = r.group_ticket();
        assert_eq!(
            r.grouping(&request(&[], false), &ticket),
            Err(GroupError::NoColumns)
        );
        assert_eq!(
            r.grouping(&request(&[1, 2, 1], false), &ticket),
            Err(GroupError::RepeatedColumn(1))
        );
        assert_eq!(
            r.grouping(&request(&[7], false), &ticket),
            Err(GroupError::View(ViewError::NoSuchColumn(7)))
        );
        let many: Vec<usize> = (0..=MAX_GROUP_COLUMNS).collect();
        assert_eq!(
            r.grouping(&request(&many, false), &ticket),
            Err(GroupError::TooManyColumns)
        );
        // A result still arriving is not grouped.
        let store = ResultStore::new();
        let id = store.create(Arc::new(ResultSchema::new(vec![meta(
            "a",
            LogicalType::Int,
        )])));
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        assert_eq!(
            r.grouping(&request(&[0], false), &r.group_ticket()),
            Err(GroupError::View(ViewError::Incomplete))
        );
    }

    #[test]
    fn a_newer_grouping_or_a_cancel_stops_an_older_one() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let first = r.group_ticket();
        let second = r.group_ticket();
        assert!(first.is_stale() && !second.is_stale());
        assert_eq!(r.grouping(&request(&[1], false), &first), Ok(None));
        r.cancel_group();
        assert!(second.is_stale());
        assert_eq!(r.grouping(&request(&[1], false), &second), Ok(None));
        // The calc panel's own calculations are another matter: neither stops the other.
        let stats = r.stats_ticket();
        let group = r.group_ticket();
        r.cancel_stats();
        assert!(stats.is_stale() && !group.is_stale());
    }

    #[test]
    fn long_values_are_cut_and_only_the_largest_groups_come_back() {
        let long = "é".repeat(GROUP_KEY_CHARS + 5);
        let cut = short(&long);
        assert_eq!(cut.chars().count(), GROUP_KEY_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(short("abc"), "abc");
        // More groups than are returned: every row is its own group but two.
        let schema = Arc::new(ResultSchema::new(vec![
            meta("n", LogicalType::Int),
            meta("t", LogicalType::Text),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let n = MAX_GROUPS_RETURNED as i64 + 500;
        let mut numbers: Vec<i64> = (0..n).collect();
        numbers.push(7);
        let texts: Vec<String> = numbers.iter().map(|_| long.clone()).collect();
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(numbers)),
            Arc::new(StringArray::from(texts)),
        ];
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
            )
            .unwrap();
        store.complete(id);
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        let g = run(&r, &request(&[0, 1], false));
        assert_eq!(g.groups.len(), MAX_GROUPS_RETURNED);
        assert_eq!(
            (g.total_groups, g.distinct, g.rows),
            (n as u64, n as u64, n as u64 + 1)
        );
        assert_eq!(g.groups[0].count, 2);
        assert_eq!(g.groups[0].key[0].as_deref(), Some("7"));
        // The long text is cut for display, but the filter compares it whole.
        assert_eq!(g.groups[0].key[1].as_deref(), Some(cut.as_str()));
        let GroupFiltered { filters, rows, .. } =
            r.filter_to_group(&[0, 1], 7, g.generation).unwrap();
        assert_eq!(filters[1].value, long);
        assert_eq!(rows, 2);
    }

    /// A million rows in batches: `g` repeats (with a NULL every tenth row) and `n` is the
    /// row's own number.
    fn million() -> (ResultStore, ResultId) {
        let schema = Arc::new(ResultSchema::new(vec![
            meta("g", LogicalType::Text),
            meta("n", LogicalType::Int),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        const BATCH: i64 = 50_000;
        for batch in 0..20 {
            let rows = (batch * BATCH)..((batch + 1) * BATCH);
            let groups: Vec<Option<String>> = rows
                .clone()
                .map(|i| (i % 10 != 0).then(|| format!("group {}", i % 1_000)))
                .collect();
            let columns: Vec<ArrayRef> = vec![
                Arc::new(StringArray::from(groups)),
                Arc::new(Int64Array::from_iter_values(rows)),
            ];
            store
                .append(
                    id,
                    RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
                )
                .unwrap();
        }
        store.complete(id);
        (store, id)
    }

    #[test]
    fn a_million_rows_group_with_null_as_a_group_and_a_cancel_stops_them() {
        let (store, id) = million();
        let stored = store.get(id).unwrap();
        {
            let r = stored.read().unwrap();
            assert_eq!(r.rows(), 1_000_000);
            let g = run(&r, &request(&[0], false));
            // 900 values that are not multiples of ten, 1,000 rows each, and the NULLs of
            // every tenth row: one group of 100,000, the largest.
            assert_eq!((g.rows, g.distinct, g.total_groups), (1_000_000, 901, 901));
            assert_eq!(g.groups.len(), 901);
            assert_eq!(g.groups[0].key, [None]);
            assert_eq!((g.groups[0].count, g.groups[0].first_row), (100_000, 0));
            assert_eq!(g.groups[1].key, [Some("group 1".to_string())]);
            assert!(g.groups[1..].iter().all(|group| group.count == 1_000));
            assert_eq!(
                g.groups.iter().map(|group| group.count).sum::<u64>(),
                1_000_000
            );
            assert_eq!(g.uncounted_rows, 0);
            // Every row its own group: more groups than are returned, none a duplicate.
            let g = run(&r, &request(&[1], false));
            assert_eq!((g.distinct, g.total_groups), (1_000_000, 1_000_000));
            assert_eq!(g.groups.len(), MAX_GROUPS_RETURNED);
            let g = run(&r, &request(&[1], true));
            assert_eq!((g.distinct, g.total_groups), (1_000_000, 0));
            assert!(g.groups.is_empty());
        }
        // A cancel while it runs: the grouping, started on another thread with only a read
        // lock, stops and answers `None`. It is cancelled as soon as it has started, long
        // before a million rows of two columns are read.
        let ticket = stored.read().unwrap().group_ticket();
        let (started, running) = std::sync::mpsc::channel();
        let worker = {
            let stored = stored.clone();
            let ticket = ticket.clone();
            std::thread::spawn(move || {
                let r = stored.read().unwrap();
                started.send(()).unwrap();
                r.grouping(&request(&[0, 1], false), &ticket)
            })
        };
        running.recv().unwrap();
        stored.read().unwrap().cancel_group();
        assert!(ticket.is_stale());
        assert_eq!(worker.join().unwrap(), Ok(None));
        // The NULL group of the million rows, filtered to through the column filters.
        let mut r = stored.write().unwrap();
        let g = run(&r, &request(&[0], false));
        let GroupFiltered { filters, rows, .. } = r.filter_to_group(&[0], 0, g.generation).unwrap();
        assert_eq!(filters[0].op, FilterOp::IsNull);
        assert_eq!(rows, 100_000);
    }
}
