//! Column masking (ADR-051): the values of chosen columns are replaced by [`MASK`] where
//! results enter the core, so the grid, sorting, filters, Ctrl+F, the calc panel, charts,
//! copies and exports never see them. The few paths that read the database without the result
//! store (streaming exports, column statistics, the data search, agents' reads) ask the same
//! [`MaskPolicy`]. A masked result keeps its original values aside, readable only by revealing
//! one cell (`StoredResult::reveal`).
//!
//! Masking is not access control: whoever runs SQL can alias a column past it. Any change here
//! needs tests that try to get around it (`tests/masking.rs`).

use std::sync::Arc;

use arrow::array::{Array, ArrayRef, RecordBatch, StringArray};
use arrow::datatypes::{Field, Schema};
use spelaion_config::MaskingRules;
use spelaion_driver_api::{ColumnMeta, LogicalType, ResultSchema, Value};

/// What a masked value shows: always six characters, so its length tells nothing.
pub const MASK: &str = "••••••";

/// Arrow field metadata of a masked column (`"true"`).
pub const META_MASKED: &str = "spelaion.masked";

/// One masking pattern, compiled: case-insensitive, `*` any characters, `?` one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern(Vec<char>);

impl Pattern {
    fn new(pattern: &str) -> Self {
        Self(fold(pattern.trim()))
    }

    /// The whole name matches (glob with backtracking over the last `*`).
    fn matches(&self, name: &[char]) -> bool {
        let pattern = &self.0;
        let (mut p, mut n) = (0, 0);
        let mut star: Option<(usize, usize)> = None;
        while n < name.len() {
            match pattern.get(p) {
                Some('*') => {
                    star = Some((p, n));
                    p += 1;
                }
                Some(&c) if c == '?' || c == name[n] => {
                    p += 1;
                    n += 1;
                }
                _ => match star {
                    Some((sp, sn)) => {
                        p = sp + 1;
                        n = sn + 1;
                        star = Some((sp, sn + 1));
                    }
                    None => return false,
                },
            }
        }
        pattern[p..].iter().all(|&c| c == '*')
    }
}

/// Lowercase, character by character (Unicode), so `PASSWORD` and `password` are one name.
fn fold(text: &str) -> Vec<char> {
    text.chars().flat_map(char::to_lowercase).collect()
}

/// Which result columns a connection masks (ADR-051), compiled from its [`MaskingRules`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaskPolicy {
    required: Vec<Pattern>,
    exemptible: Vec<Pattern>,
    never: Vec<Pattern>,
}

impl MaskPolicy {
    pub fn new(rules: &MaskingRules) -> Self {
        let compile = |patterns: &[String]| {
            patterns
                .iter()
                .filter(|p| !p.trim().is_empty())
                .map(|p| Pattern::new(p))
                .collect()
        };
        Self {
            required: compile(&rules.required),
            exemptible: compile(&rules.exemptible),
            never: compile(&rules.never),
        }
    }

    /// Nothing is masked.
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.exemptible.is_empty()
    }

    /// Whether a column with this name is masked.
    pub fn masks(&self, column: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let name = fold(column);
        let any = |patterns: &[Pattern]| patterns.iter().any(|p| p.matches(&name));
        any(&self.required) || (any(&self.exemptible) && !any(&self.never))
    }

    /// The positions of the masked columns of a result.
    pub fn masked_columns(&self, columns: &[ColumnMeta]) -> Vec<usize> {
        columns
            .iter()
            .enumerate()
            .filter(|(_, c)| self.masks(&c.name))
            .map(|(i, _)| i)
            .collect()
    }

    /// A value of `column` as it may be shown: the mask, unless NULL or not masked.
    pub fn shown_value(&self, column: &str, value: Value) -> Value {
        if matches!(value, Value::Null) || !self.masks(column) {
            value
        } else {
            Value::Text(MASK.into())
        }
    }

    /// Masks the values of rows read with these columns (agents' reads).
    pub fn mask_rows(&self, columns: &[ColumnMeta], rows: &mut [Vec<Value>]) -> Vec<usize> {
        let masked = self.masked_columns(columns);
        for row in rows.iter_mut() {
            for &i in &masked {
                if let Some(cell) = row.get_mut(i)
                    && !matches!(cell, Value::Null)
                {
                    *cell = Value::Text(MASK.into());
                }
            }
        }
        masked
    }
}

/// A result's schema with its masked columns turned into text marked [`META_MASKED`] (their
/// native type kept for the header), and the positions of those columns.
pub fn masked_schema(
    policy: &MaskPolicy,
    schema: &Arc<ResultSchema>,
) -> (Arc<ResultSchema>, Vec<usize>) {
    let masked = policy.masked_columns(&schema.columns);
    if masked.is_empty() {
        return (schema.clone(), masked);
    }
    let mut columns = schema.columns.clone();
    let mut fields: Vec<Field> = Vec::with_capacity(columns.len());
    for (i, column) in columns.iter_mut().enumerate() {
        if masked.contains(&i) {
            column.logical = LogicalType::Text;
            let mut field = column.to_field();
            field.metadata_mut().insert(META_MASKED, "true");
            fields.push(field);
        } else {
            fields.push(schema.arrow.field(i).clone());
        }
    }
    let arrow = Arc::new(Schema::new_with_metadata(
        fields,
        schema.arrow.metadata().clone(),
    ));
    (Arc::new(ResultSchema { columns, arrow }), masked)
}

/// Whether a column of a stored result is masked (its field carries [`META_MASKED`]).
pub fn is_masked_field(field: &Field) -> bool {
    field
        .metadata()
        .get(META_MASKED)
        .is_some_and(|v| v == "true")
}

/// The mask of one column: [`MASK`] for every value, NULL where it was NULL.
fn mask_array(array: &dyn Array) -> ArrayRef {
    let values: StringArray = (0..array.len())
        .map(|row| (!array.is_null(row)).then_some(MASK))
        .collect();
    Arc::new(values)
}

/// A batch with the `masked` columns replaced by their mask, in `schema` (from
/// [`masked_schema`]), and the original columns that were replaced, in the order of `masked`.
pub fn mask_batch(
    batch: &RecordBatch,
    masked: &[usize],
    schema: &Arc<ResultSchema>,
) -> (RecordBatch, Vec<ArrayRef>) {
    if masked.is_empty() {
        return (batch.clone(), Vec::new());
    }
    let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
    let mut originals = Vec::with_capacity(masked.len());
    for &i in masked {
        let original = std::mem::replace(&mut columns[i], mask_array(batch.column(i).as_ref()));
        originals.push(original);
    }
    let options = arrow::array::RecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
    let masked_batch = RecordBatch::try_new_with_options(schema.arrow.clone(), columns, &options)
        .expect("a masked column is text in the masked schema");
    (masked_batch, originals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(required: &[&str], exemptible: &[&str], never: &[&str]) -> MaskPolicy {
        let v = |items: &[&str]| items.iter().map(|s| s.to_string()).collect();
        MaskPolicy::new(&MaskingRules {
            required: v(required),
            exemptible: v(exemptible),
            never: v(never),
        })
    }

    #[test]
    fn patterns_match_whole_names_ignoring_case() {
        let p = policy(&[], &["*password*", "email", "card_??"], &[]);
        for name in [
            "password",
            "user_password",
            "PasswordHash",
            "EMAIL",
            "card_no",
        ] {
            assert!(p.masks(name), "{name}");
        }
        for name in ["emails", "e_mail", "card_n", "card_num", "pass", ""] {
            assert!(!p.masks(name), "{name}");
        }
    }

    #[test]
    fn unicode_names_fold_case() {
        let p = policy(&["CÓDIGO"], &[], &[]);
        assert!(p.masks("código"));
    }

    #[test]
    fn stars_backtrack() {
        let p = policy(&["*a*b*c"], &[], &[]);
        assert!(p.masks("xaxbxc"));
        assert!(p.masks("abcabc"));
        assert!(!p.masks("abcab"));
        assert!(policy(&["*"], &[], &[]).masks(""));
        assert!(policy(&["**x**"], &[], &[]).masks("x"));
    }

    #[test]
    fn never_exempts_only_exemptible_patterns() {
        let p = policy(&["ssn"], &["*email*"], &["email_verified", "ssn"]);
        assert!(p.masks("email"));
        assert!(!p.masks("email_verified"));
        assert!(!p.masks("EMAIL_VERIFIED"));
        assert!(p.masks("ssn"), "a required pattern cannot be exempted");
    }

    #[test]
    fn empty_policy_masks_nothing() {
        assert!(!MaskPolicy::none().masks("password"));
        assert!(policy(&[], &[], &["*"]).is_empty());
    }

    #[test]
    fn nulls_stay_null() {
        let p = policy(&["x"], &[], &[]);
        assert_eq!(p.shown_value("x", Value::Null), Value::Null);
        assert_eq!(p.shown_value("x", Value::Int(1)), Value::Text(MASK.into()));
        assert_eq!(p.shown_value("y", Value::Int(1)), Value::Int(1));
    }
}
