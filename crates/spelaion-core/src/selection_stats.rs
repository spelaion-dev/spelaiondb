//! Numbers about the grid's selection (the calc panel, ADR-010): count, distinct, NULLs, sum,
//! average, minimum, maximum, median and mode of the selected cells. They are computed here
//! over the stored result: the UI sends only the selection's bounds (rows of the current view
//! and the columns it shows) and receives the numbers, so no cell travels to React.
//!
//! - Sum, average and median take the numeric cells only (integers, decimals and floats).
//!   Integers and decimals add up exactly; a float, or a total too large to keep exactly,
//!   makes the result a float.
//! - Minimum and maximum compare the cells by their type: numbers as numbers, dates and times
//!   in time order, booleans false before true, text and UUIDs by Unicode code point. A
//!   selection mixing kinds that do not compare (numbers and text) has neither.
//! - Distinct and mode compare the cells by their text, as the grid shows them.
//! - A newer calculation on the same result, or a cancel, stops one still running.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use spelaion_driver_api::{LogicalType, Value};

use crate::results::StoredResult;
use crate::sort::ViewError;

/// Most distinct values counted; beyond them the count reads "1,000,000+" and there is no mode.
pub const MAX_DISTINCT: usize = 1_000_000;

/// Most numeric cells kept for the median; beyond them it is not computed.
pub const MAX_MEDIAN_VALUES: usize = 5_000_000;

/// Longest text returned for a minimum, maximum or mode (longer ones end in "…").
pub const STAT_TEXT_CHARS: usize = 200;

/// Cells read between two checks of whether the calculation was replaced or cancelled.
const CHECK_EVERY: u64 = 4_096;

/// Digits added after the decimal point of an exact average.
const AVG_EXTRA_DIGITS: u32 = 6;

/// The selection: rows `[first_row, first_row + rows)` of the current view (after the sort and
/// the filters) and the columns shown in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellBlock {
    pub first_row: usize,
    pub rows: usize,
    pub columns: Vec<usize>,
}

/// The numbers about a selection. Values (sum, minimum…) are text as the grid would show them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectionStats {
    /// Cells selected.
    pub cells: u64,
    pub nulls: u64,
    /// Cells that are not NULL.
    pub values: u64,
    /// Different texts among the values.
    pub distinct: u64,
    /// There were more than [`MAX_DISTINCT`] different values: `distinct` is a lower bound.
    pub distinct_capped: bool,
    /// Numeric values (what sum, average and median take).
    pub numbers: u64,
    pub sum: Option<String>,
    pub avg: Option<String>,
    pub median: Option<String>,
    /// There were more than [`MAX_MEDIAN_VALUES`] numbers: no median.
    pub median_capped: bool,
    pub min: Option<String>,
    pub max: Option<String>,
    /// The most frequent value (the first one reached among equals), when some value repeats.
    pub mode: Option<String>,
    pub mode_count: u64,
}

/// Lets a newer calculation on the same result, or a cancel, stop one that is still running.
#[derive(Debug, Clone)]
pub struct StatsTicket {
    latest: Arc<AtomicU64>,
    number: u64,
}

impl StatsTicket {
    pub(crate) fn new(latest: &Arc<AtomicU64>) -> Self {
        Self {
            latest: latest.clone(),
            number: latest.fetch_add(1, AtomicOrdering::Relaxed) + 1,
        }
    }

    /// A newer calculation started on the same result, or it was cancelled.
    pub fn is_stale(&self) -> bool {
        self.latest.load(AtomicOrdering::Relaxed) != self.number
    }
}

/// Stops the calculation running on a result, if any.
pub(crate) fn cancel(latest: &Arc<AtomicU64>) {
    latest.fetch_add(1, AtomicOrdering::Relaxed);
}

/// An exact number: `mantissa / 10^scale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Exact {
    mantissa: i128,
    scale: u32,
}

#[derive(Debug, Clone, Copy)]
enum Num {
    Exact(Exact),
    Float(f64),
}

fn pow10(exp: u32) -> Option<i128> {
    10i128.checked_pow(exp)
}

impl Exact {
    fn to_f64(self) -> f64 {
        // Precise enough for a fallback; the text form keeps every digit when it can.
        format_exact(self.mantissa, self.scale)
            .parse()
            .unwrap_or(f64::NAN)
    }

    /// The same number with `scale` digits after the point (`scale >= self.scale`).
    fn rescaled(self, scale: u32) -> Option<i128> {
        self.mantissa.checked_mul(pow10(scale - self.scale)?)
    }
}

impl Num {
    fn to_f64(self) -> f64 {
        match self {
            Num::Exact(e) => e.to_f64(),
            Num::Float(f) => f,
        }
    }
}

/// A decimal written plainly (`-12.340`), with at most 38 digits.
fn parse_exact(text: &str) -> Option<Exact> {
    let t = text.trim();
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
        || int.len() + frac.len() > 38
    {
        return None;
    }
    let mut mantissa: i128 = 0;
    for b in int.bytes().chain(frac.bytes()) {
        mantissa = mantissa
            .checked_mul(10)?
            .checked_add(i128::from(b - b'0'))?;
    }
    Some(Exact {
        mantissa: if negative { -mantissa } else { mantissa },
        scale: u32::try_from(frac.len()).ok()?,
    })
}

/// `mantissa / 10^scale` written out, keeping its scale (`3050, 2` → `30.50`).
fn format_exact(mantissa: i128, scale: u32) -> String {
    let digits = mantissa.unsigned_abs().to_string();
    let scale = scale as usize;
    let mut out = String::with_capacity(digits.len() + 3);
    if mantissa < 0 {
        out.push('-');
    }
    if scale == 0 {
        out.push_str(&digits);
    } else if digits.len() > scale {
        let (int, frac) = digits.split_at(digits.len() - scale);
        out.push_str(int);
        out.push('.');
        out.push_str(frac);
    } else {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', scale - digits.len()));
        out.push_str(&digits);
    }
    out
}

/// Drops trailing zeros after the point, keeping at least `keep` digits there.
fn trim_zeros(mut text: String, keep: usize) -> String {
    if let Some(point) = text.find('.') {
        let min_len = point + 1 + keep;
        while text.len() > min_len && text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

fn float_text(value: f64) -> String {
    Value::Float(value).to_string()
}

/// Divides rounding half away from zero.
fn div_round(numerator: i128, denominator: i128) -> i128 {
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    if remainder.unsigned_abs() * 2 >= denominator.unsigned_abs() {
        quotient
            + if (numerator < 0) == (denominator < 0) {
                1
            } else {
                -1
            }
    } else {
        quotient
    }
}

/// The running sum of the numeric cells: exact while it can be.
#[derive(Debug, Clone, Copy)]
enum Sum {
    Exact(Exact),
    Float(f64),
}

impl Sum {
    fn add(&mut self, n: Num) {
        *self = match (*self, n) {
            (Sum::Exact(total), Num::Exact(e)) => {
                let scale = total.scale.max(e.scale);
                match (total.rescaled(scale), e.rescaled(scale)) {
                    (Some(a), Some(b)) => match a.checked_add(b) {
                        Some(mantissa) => Sum::Exact(Exact { mantissa, scale }),
                        None => Sum::Float(total.to_f64() + e.to_f64()),
                    },
                    _ => Sum::Float(total.to_f64() + e.to_f64()),
                }
            }
            (Sum::Exact(total), Num::Float(f)) => Sum::Float(total.to_f64() + f),
            (Sum::Float(total), n) => Sum::Float(total + n.to_f64()),
        };
    }

    fn text(self) -> String {
        match self {
            Sum::Exact(e) => format_exact(e.mantissa, e.scale),
            Sum::Float(f) => float_text(f),
        }
    }

    fn avg_text(self, count: u64) -> String {
        match self {
            Sum::Exact(e) => {
                let exact = pow10(AVG_EXTRA_DIGITS)
                    .and_then(|p| e.mantissa.checked_mul(p))
                    .map(|scaled| div_round(scaled, i128::from(count)));
                match exact {
                    Some(mantissa) => trim_zeros(
                        format_exact(mantissa, e.scale + AVG_EXTRA_DIGITS),
                        e.scale as usize,
                    ),
                    None => float_text(e.to_f64() / count as f64),
                }
            }
            Sum::Float(f) => float_text(f / count as f64),
        }
    }
}

fn cmp_num(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::Exact(x), Num::Exact(y)) => {
            let scale = x.scale.max(y.scale);
            match (x.rescaled(scale), y.rescaled(scale)) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => x.to_f64().total_cmp(&y.to_f64()),
            }
        }
        (a, b) => a.to_f64().total_cmp(&b.to_f64()),
    }
}

/// How a cell compares for the minimum and maximum.
enum Key {
    Num(Num),
    Bool(bool),
    Date(i32),
    Time(i64),
    Timestamp(i64),
    TimestampTz(i64),
    /// Compared by the text (already in the reading buffer).
    Text,
    /// Does not compare (bytes, JSON, intervals, arrays, engine types).
    None,
}

/// Which keys compare with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Num,
    Bool,
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Text,
}

/// A minimum or maximum so far: its key (without text) and its text.
struct Extreme {
    key: OwnedKey,
    text: String,
}

#[derive(Debug, Clone, Copy)]
enum OwnedKey {
    Num(Num),
    Int(i64),
    Text,
}

/// Comparability of the values seen so far.
enum Range {
    Empty,
    Some {
        family: Family,
        min: Extreme,
        max: Extreme,
    },
    /// Mixed kinds, or a kind that does not compare.
    None,
}

impl Range {
    fn see(&mut self, key: Key, text: &str) {
        let (family, owned) = match key {
            Key::Num(n) => (Family::Num, OwnedKey::Num(n)),
            Key::Bool(b) => (Family::Bool, OwnedKey::Int(i64::from(b))),
            Key::Date(d) => (Family::Date, OwnedKey::Int(i64::from(d))),
            Key::Time(t) => (Family::Time, OwnedKey::Int(t)),
            Key::Timestamp(t) => (Family::Timestamp, OwnedKey::Int(t)),
            Key::TimestampTz(t) => (Family::TimestampTz, OwnedKey::Int(t)),
            Key::Text => (Family::Text, OwnedKey::Text),
            Key::None => {
                *self = Range::None;
                return;
            }
        };
        match self {
            Range::None => {}
            Range::Empty => {
                *self = Range::Some {
                    family,
                    min: Extreme {
                        key: owned,
                        text: text.to_string(),
                    },
                    max: Extreme {
                        key: owned,
                        text: text.to_string(),
                    },
                };
            }
            Range::Some {
                family: seen,
                min,
                max,
            } => {
                if *seen != family {
                    *self = Range::None;
                    return;
                }
                if compare(owned, text, min) == Ordering::Less {
                    *min = Extreme {
                        key: owned,
                        text: text.to_string(),
                    };
                }
                if compare(owned, text, max) == Ordering::Greater {
                    *max = Extreme {
                        key: owned,
                        text: text.to_string(),
                    };
                }
            }
        }
    }
}

fn compare(key: OwnedKey, text: &str, extreme: &Extreme) -> Ordering {
    match (key, extreme.key) {
        (OwnedKey::Num(a), OwnedKey::Num(b)) => cmp_num(a, b),
        (OwnedKey::Int(a), OwnedKey::Int(b)) => a.cmp(&b),
        (OwnedKey::Text, OwnedKey::Text) => text.cmp(extreme.text.as_str()),
        // Never mixed: the family is checked first.
        (OwnedKey::Num(_) | OwnedKey::Int(_) | OwnedKey::Text, _) => Ordering::Equal,
    }
}

/// `text` cut to [`STAT_TEXT_CHARS`] characters, with "…" when it was longer.
fn short(text: &str) -> String {
    match text.char_indices().nth(STAT_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// The numeric value of a cell, if it is a number.
fn number(value: &Value, text: &str) -> Option<Num> {
    match value {
        Value::Int(i) => Some(Num::Exact(Exact {
            mantissa: i128::from(*i),
            scale: 0,
        })),
        Value::Float(f) => Some(Num::Float(*f)),
        // NaN, Infinity or an exponent: as a float.
        Value::Decimal(_) => parse_exact(text)
            .map(Num::Exact)
            .or_else(|| text.trim().parse::<f64>().ok().map(Num::Float)),
        Value::Null
        | Value::Bool(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::TimestampTz(_)
        | Value::Uuid(_)
        | Value::Json(_)
        | Value::Interval(_)
        | Value::Array(_)
        | Value::Native(_) => None,
    }
}

fn key(value: &Value, n: Option<Num>) -> Key {
    if let Some(n) = n {
        return Key::Num(n);
    }
    match value {
        Value::Bool(b) => Key::Bool(*b),
        Value::Date(d) => Key::Date(*d),
        Value::Time(t) => Key::Time(*t),
        Value::Timestamp(t) => Key::Timestamp(*t),
        Value::TimestampTz(t) => Key::TimestampTz(*t),
        Value::Text(_) | Value::Uuid(_) => Key::Text,
        Value::Null
        | Value::Int(_)
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Bytes(_)
        | Value::Json(_)
        | Value::Interval(_)
        | Value::Array(_)
        | Value::Native(_) => Key::None,
    }
}

/// The median of numeric values, exact when they all are and fit.
fn median(values: &mut [Num]) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    let mid = values.len() / 2;
    let even = values.len().is_multiple_of(2);
    let exact: Option<(Vec<i128>, u32)> = (|| {
        let scale = values
            .iter()
            .map(|n| match n {
                Num::Exact(e) => Some(e.scale),
                Num::Float(_) => None,
            })
            .try_fold(0u32, |acc, s| s.map(|s| acc.max(s)))?;
        let all = values
            .iter()
            .map(|n| match n {
                Num::Exact(e) => e.rescaled(scale),
                Num::Float(_) => None,
            })
            .collect::<Option<Vec<i128>>>()?;
        Some((all, scale))
    })();
    if let Some((mut all, scale)) = exact {
        let (_, &mut upper, _) = all.select_nth_unstable(mid);
        if !even {
            return Some(format_exact(upper, scale));
        }
        let lower = *all[..mid]
            .iter()
            .max()
            .expect("an even count has a lower half");
        let total = lower.checked_add(upper)?;
        return Some(if total % 2 == 0 {
            format_exact(total / 2, scale)
        } else {
            match total.checked_mul(5) {
                Some(m) => format_exact(m, scale + 1),
                None => float_text(total as f64 / 2.0 / 10f64.powi(scale as i32)),
            }
        });
    }
    let mut all: Vec<f64> = values.iter().map(|n| n.to_f64()).collect();
    let (_, &mut upper, _) = all.select_nth_unstable_by(mid, f64::total_cmp);
    if !even {
        return Some(float_text(upper));
    }
    let lower = all[..mid]
        .iter()
        .copied()
        .max_by(f64::total_cmp)
        .expect("an even count has a lower half");
    Some(float_text((lower + upper) / 2.0))
}

/// Computes the numbers about `block` of `result`'s current view; `None` if a newer
/// calculation, or a cancel, stopped this one.
pub(crate) fn compute(
    result: &StoredResult,
    block: &CellBlock,
    ticket: &StatsTicket,
) -> Result<Option<SelectionStats>, ViewError> {
    let schema = result.schema();
    let mut columns: Vec<(usize, LogicalType)> = Vec::with_capacity(block.columns.len());
    for &column in &block.columns {
        let meta = schema
            .columns
            .get(column)
            .ok_or(ViewError::NoSuchColumn(column))?;
        if !columns.iter().any(|&(c, _)| c == column) {
            columns.push((column, meta.logical));
        }
    }
    let end = block
        .first_row
        .saturating_add(block.rows)
        .min(result.rows());
    let mut stats = SelectionStats::default();
    let mut counts: HashMap<Box<str>, (u64, u64)> = HashMap::new();
    let mut sum: Option<Sum> = None;
    let mut numbers: Vec<Num> = Vec::new();
    let mut range = Range::Empty;
    let mut text = String::new();
    for position in block.first_row..end {
        let (batch, row) = result.locate(result.original_row(position));
        for &(column, logical) in &columns {
            if stats.cells % CHECK_EVERY == 0 && ticket.is_stale() {
                return Ok(None);
            }
            stats.cells += 1;
            let value = Value::from_array(batch.column(column).as_ref(), row, logical)
                .ok_or(ViewError::UnexpectedArray(column))?;
            if matches!(value, Value::Null) {
                stats.nulls += 1;
                continue;
            }
            stats.values += 1;
            text.clear();
            let _ = write!(text, "{value}");
            // Distinct and mode, by text; the key is allocated only for a new value.
            if let Some(entry) = counts.get_mut(text.as_str()) {
                entry.0 += 1;
            } else if counts.len() < MAX_DISTINCT {
                counts.insert(text.as_str().into(), (1, stats.values));
            } else {
                stats.distinct_capped = true;
            }
            let n = number(&value, &text);
            if let Some(n) = n {
                stats.numbers += 1;
                sum.get_or_insert(Sum::Exact(Exact {
                    mantissa: 0,
                    scale: 0,
                }))
                .add(n);
                if numbers.len() < MAX_MEDIAN_VALUES {
                    numbers.push(n);
                } else {
                    stats.median_capped = true;
                }
            }
            range.see(key(&value, n), &text);
        }
    }
    if ticket.is_stale() {
        return Ok(None);
    }
    stats.distinct = counts.len() as u64;
    if !stats.distinct_capped {
        // The most frequent value; among equals, the one reached first.
        let best = counts
            .iter()
            .max_by(|a, b| a.1.0.cmp(&b.1.0).then_with(|| b.1.1.cmp(&a.1.1)));
        if let Some((value, &(count, _))) = best
            && count > 1
        {
            stats.mode = Some(short(value));
            stats.mode_count = count;
        }
    }
    if let Some(sum) = sum {
        stats.sum = Some(sum.text());
        stats.avg = Some(sum.avg_text(stats.numbers));
    }
    if !stats.median_capped {
        stats.median = median(&mut numbers);
    }
    if let Range::Some { min, max, .. } = range {
        stats.min = Some(short(&min.text));
        stats.max = Some(short(&max.text));
    }
    Ok(Some(stats))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
    };
    use spelaion_driver_api::{ColumnMeta, ResultSchema};

    use super::*;
    use crate::ResultStore;
    use crate::results::ResultId;
    use crate::sort::SortKey;

    fn meta(name: &str, logical: LogicalType) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
            origin: None,
        }
    }

    /// id (int), price (decimal), name (text), ratio (float), ok (bool), in two batches.
    fn store() -> (ResultStore, ResultId) {
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int),
            meta("price", LogicalType::Decimal),
            meta("name", LogicalType::Text),
            meta("ratio", LogicalType::Float),
            meta("ok", LogicalType::Bool),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        type Batch = (
            Vec<i64>,
            Vec<Option<&'static str>>,
            Vec<Option<&'static str>>,
            Vec<f64>,
            Vec<bool>,
        );
        let batches: [Batch; 2] = [
            (
                vec![1, 2, 3],
                vec![Some("10.50"), Some("20.00"), None],
                vec![Some("Ana"), Some("bob"), None],
                vec![0.5, 0.25, 1.0],
                vec![true, false, true],
            ),
            (
                vec![4, 5, 12],
                vec![Some("-3.125"), Some("20.00"), Some("7")],
                vec![Some("Ana"), Some("Zoe"), Some("ana")],
                vec![2.0, 0.5, 0.75],
                vec![true, true, false],
            ),
        ];
        for (ids, prices, names, ratios, oks) in batches {
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(prices)),
                Arc::new(StringArray::from(names)),
                Arc::new(Float64Array::from(ratios)),
                Arc::new(BooleanArray::from(oks)),
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

    fn block(first_row: usize, rows: usize, columns: &[usize]) -> CellBlock {
        CellBlock {
            first_row,
            rows,
            columns: columns.to_vec(),
        }
    }

    fn run(r: &StoredResult, b: &CellBlock) -> SelectionStats {
        r.selection_stats(b, &r.stats_ticket()).unwrap().unwrap()
    }

    #[test]
    fn integers_add_up_exactly() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let s = run(&r, &block(0, 6, &[0]));
        assert_eq!(s.cells, 6);
        assert_eq!(s.values, 6);
        assert_eq!(s.nulls, 0);
        assert_eq!(s.distinct, 6);
        assert_eq!(s.numbers, 6);
        assert_eq!(s.sum.as_deref(), Some("27"));
        assert_eq!(s.avg.as_deref(), Some("4.5"));
        assert_eq!(s.median.as_deref(), Some("3.5"));
        assert_eq!(s.min.as_deref(), Some("1"));
        assert_eq!(s.max.as_deref(), Some("12"));
        // Every value once: no mode.
        assert_eq!(s.mode, None);
    }

    #[test]
    fn decimals_keep_their_scale_and_nulls_are_counted_apart() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let s = run(&r, &block(0, 6, &[1]));
        assert_eq!((s.cells, s.values, s.nulls, s.numbers), (6, 5, 1, 5));
        // 10.50 + 20.00 - 3.125 + 20.00 + 7
        assert_eq!(s.sum.as_deref(), Some("54.375"));
        assert_eq!(s.avg.as_deref(), Some("10.875"));
        // -3.125, 7, 10.50, 20.00, 20.00
        assert_eq!(s.median.as_deref(), Some("10.500"));
        assert_eq!(s.min.as_deref(), Some("-3.125"));
        assert_eq!(s.max.as_deref(), Some("20.00"));
        assert_eq!(s.mode.as_deref(), Some("20.00"));
        assert_eq!(s.mode_count, 2);
        assert_eq!(s.distinct, 4);
    }

    #[test]
    fn a_float_makes_the_sum_a_float() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let s = run(&r, &block(0, 2, &[0, 3]));
        // 1 + 0.5 + 2 + 0.25
        assert_eq!(s.sum.as_deref(), Some("3.75"));
        assert_eq!(s.avg.as_deref(), Some("0.9375"));
        assert_eq!(s.median.as_deref(), Some("0.75"));
        assert_eq!(s.min.as_deref(), Some("0.25"));
        assert_eq!(s.max.as_deref(), Some("2"));
    }

    #[test]
    fn text_has_no_sum_and_compares_by_code_point() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let s = run(&r, &block(0, 6, &[2]));
        assert_eq!((s.values, s.nulls, s.numbers), (5, 1, 0));
        assert_eq!(s.sum, None);
        assert_eq!(s.avg, None);
        assert_eq!(s.median, None);
        // Upper case before lower case.
        assert_eq!(s.min.as_deref(), Some("Ana"));
        assert_eq!(s.max.as_deref(), Some("bob"));
        assert_eq!(s.mode.as_deref(), Some("Ana"));
        assert_eq!(s.mode_count, 2);
        // "Ana" and "ana" are different texts.
        assert_eq!(s.distinct, 4);
    }

    #[test]
    fn mixed_kinds_have_no_minimum_but_still_add_their_numbers() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let s = run(&r, &block(0, 2, &[0, 2]));
        assert_eq!(s.numbers, 2);
        assert_eq!(s.sum.as_deref(), Some("3"));
        assert_eq!((s.min, s.max), (None, None));
        // Booleans compare among themselves.
        let s = run(&r, &block(0, 6, &[4]));
        assert_eq!(
            (s.min.as_deref(), s.max.as_deref()),
            (Some("false"), Some("true"))
        );
        assert_eq!(s.mode.as_deref(), Some("true"));
        assert_eq!(s.mode_count, 4);
    }

    #[test]
    fn the_selection_follows_the_view_and_clips_at_its_end() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        r.sort(Some(SortKey {
            column: 0,
            descending: true,
        }))
        .unwrap();
        // The first two rows of the view are now ids 12 and 5.
        let s = run(&r, &block(0, 2, &[0]));
        assert_eq!(s.sum.as_deref(), Some("17"));
        // Past the end: only the rows there are.
        let s = run(&r, &block(4, 100, &[0]));
        assert_eq!(s.cells, 2);
        assert_eq!(s.sum.as_deref(), Some("3"));
        // A column given twice counts once.
        assert_eq!(run(&r, &block(0, 1, &[0, 0])).cells, 1);
        assert_eq!(
            r.selection_stats(&block(0, 1, &[9]), &r.stats_ticket()),
            Err(ViewError::NoSuchColumn(9))
        );
        // An empty selection has nothing.
        let s = run(&r, &block(0, 0, &[0]));
        assert_eq!(s, SelectionStats::default());
    }

    #[test]
    fn a_newer_calculation_or_a_cancel_stops_an_older_one() {
        let (store, id) = store();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let first = r.stats_ticket();
        let second = r.stats_ticket();
        assert!(first.is_stale() && !second.is_stale());
        assert_eq!(r.selection_stats(&block(0, 6, &[0]), &first), Ok(None));
        r.cancel_stats();
        assert!(second.is_stale());
        assert_eq!(r.selection_stats(&block(0, 6, &[0]), &second), Ok(None));
    }

    #[test]
    fn long_values_are_cut() {
        assert_eq!(short("abc"), "abc");
        let long = "é".repeat(STAT_TEXT_CHARS + 5);
        let cut = short(&long);
        assert_eq!(cut.chars().count(), STAT_TEXT_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn exact_numbers_parse_and_print() {
        assert_eq!(
            parse_exact("-12.340"),
            Some(Exact {
                mantissa: -12340,
                scale: 3
            })
        );
        assert_eq!(parse_exact(".5").map(|e| e.mantissa), Some(5));
        assert_eq!(parse_exact("1e5"), None);
        assert_eq!(parse_exact("NaN"), None);
        assert_eq!(parse_exact(""), None);
        assert_eq!(parse_exact(&"9".repeat(39)), None);
        assert_eq!(format_exact(3050, 2), "30.50");
        assert_eq!(format_exact(-5, 3), "-0.005");
        assert_eq!(format_exact(0, 0), "0");
        assert_eq!(trim_zeros("1.500000".into(), 0), "1.5");
        assert_eq!(trim_zeros("2.000000".into(), 0), "2");
        assert_eq!(trim_zeros("2.000000".into(), 2), "2.00");
        assert_eq!(div_round(5, 2), 3);
        assert_eq!(div_round(-5, 2), -3);
        assert_eq!(div_round(4, 3), 1);
    }

    #[test]
    fn sums_beyond_exact_range_become_floats() {
        let mut sum = Sum::Exact(Exact {
            mantissa: i128::MAX - 1,
            scale: 0,
        });
        sum.add(Num::Exact(Exact {
            mantissa: 10,
            scale: 0,
        }));
        assert!(matches!(sum, Sum::Float(_)));
        // NaN decimals (PostgreSQL's numeric) read as floats.
        let n = number(&Value::Decimal("NaN".into()), "NaN");
        assert!(matches!(n, Some(Num::Float(f)) if f.is_nan()));
    }

    /// A million rows selected (the item's "Done when"): every aggregate over three columns,
    /// checked against the numbers worked out by hand.
    #[test]
    fn a_million_row_selection() {
        const ROWS: i64 = 1_000_000;
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int),
            meta("price", LogicalType::Decimal),
            meta("tag", LogicalType::Text),
        ]));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let mut start = 0;
        while start < ROWS {
            let end = (start + 65_536).min(ROWS);
            let ids: Vec<i64> = (start..end).collect();
            // price: i / 100 with two decimals; NULL on every tenth row.
            let prices: Vec<Option<String>> = (start..end)
                .map(|i| (i % 10 != 0).then(|| format!("{}.{:02}", i / 100, i % 100)))
                .collect();
            let tags: Vec<String> = (start..end).map(|i| format!("tag {}", i % 7)).collect();
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(prices)),
                Arc::new(StringArray::from(tags)),
            ];
            store
                .append(
                    id,
                    RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
                )
                .unwrap();
            start = end;
        }
        store.complete(id);
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let all = block(0, ROWS as usize, &[0, 1, 2]);
        let s = run(&r, &all);
        let n = ROWS as u64;
        assert_eq!(s.cells, 3 * n);
        assert_eq!(s.nulls, n / 10);
        assert_eq!(s.values, 3 * n - n / 10);
        // ids and prices are numbers; the tags are not.
        assert_eq!(s.numbers, n + n - n / 10);
        // ids: 0..999,999 sum to 499,999,500,000. Prices in cents: the same sum minus every
        // tenth row (0, 10, …, 999,990: 49,999,500,000), divided by 100.
        let ids_sum: i128 = 499_999_500_000;
        let cents: i128 = ids_sum - 49_999_500_000;
        let total_cents = ids_sum * 100 + cents;
        assert_eq!(
            s.sum.as_deref(),
            Some(format_exact(total_cents, 2).as_str())
        );
        // Mixed with text: no minimum or maximum.
        assert_eq!((s.min.as_deref(), s.max.as_deref()), (None, None));
        // Ids and prices are all different texts: more than a million, so the count stops
        // there and there is no mode.
        assert!(s.distinct_capped);
        assert_eq!(s.distinct, MAX_DISTINCT as u64);
        assert_eq!(s.mode, None);

        // The tags alone: "tag 0" 142,858 times, the others 142,857.
        let s = run(&r, &block(0, ROWS as usize, &[2]));
        assert_eq!(s.distinct, 7);
        assert_eq!(s.mode.as_deref(), Some("tag 0"));
        assert_eq!(s.mode_count, 142_858);
        assert_eq!(
            (s.min.as_deref(), s.max.as_deref()),
            (Some("tag 0"), Some("tag 6"))
        );
        assert_eq!(s.sum, None);

        // The ids alone: a million distinct values, the cap exactly.
        let s = run(&r, &block(0, ROWS as usize, &[0]));
        assert_eq!(s.distinct, n);
        assert!(!s.distinct_capped);
        assert_eq!(s.sum.as_deref(), Some("499999500000"));
        assert_eq!(s.avg.as_deref(), Some("499999.5"));
        assert_eq!(s.median.as_deref(), Some("499999.5"));
        assert_eq!(s.min.as_deref(), Some("0"));
        assert_eq!(s.max.as_deref(), Some("999999"));
        assert_eq!(s.mode, None);

        // The prices alone.
        let s = run(&r, &block(0, ROWS as usize, &[1]));
        assert_eq!(s.min.as_deref(), Some("0.01"));
        assert_eq!(s.max.as_deref(), Some("9999.99"));
        assert_eq!(s.sum.as_deref(), Some(format_exact(cents, 2).as_str()));
    }
}
