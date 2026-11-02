//! Sorting a stored result by one column (the grid's header click). The order is computed
//! here from the typed values, never in the UI: numbers sort as numbers, decimals (which
//! travel as text) by their numeric value, dates by date, text by Unicode code point and
//! bytes by byte. The sort is stable and NULLs always go last.

use std::cmp::Ordering;

use arrow::array::{
    Array, BinaryArray, BooleanArray, Date32Array, Float64Array, Int64Array, StringArray,
    Time64MicrosecondArray, TimestampMicrosecondArray,
};
use spelunk_driver_api::LogicalType;

/// Which column a result is sorted by, and in which direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub column: usize,
    pub descending: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SortError {
    #[error("the result is still arriving; it can be sorted when it is complete")]
    Incomplete,
    #[error("the result has no column {0}")]
    NoSuchColumn(usize),
    #[error("the result is too large to sort in the app")]
    TooManyRows,
    #[error("column {0} does not hold the values its type announces")]
    UnexpectedArray(usize),
}

/// Row numbers of `values` in sorted order.
pub(crate) fn sorted_rows(
    values: &dyn Array,
    logical: LogicalType,
    key: SortKey,
) -> Result<Vec<u32>, SortError> {
    let unexpected = || SortError::UnexpectedArray(key.column);
    let d = key.descending;
    match logical {
        LogicalType::Bool => {
            let a = downcast::<BooleanArray>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).cmp(&a.value(y))))
        }
        LogicalType::Int => {
            let a = downcast::<Int64Array>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).cmp(&a.value(y))))
        }
        LogicalType::Float => {
            let a = downcast::<Float64Array>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).total_cmp(&a.value(y))))
        }
        LogicalType::Bytes => {
            let a = downcast::<BinaryArray>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).cmp(a.value(y))))
        }
        LogicalType::Date => {
            let a = downcast::<Date32Array>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).cmp(&a.value(y))))
        }
        LogicalType::Time => {
            let a = downcast::<Time64MicrosecondArray>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).cmp(&a.value(y))))
        }
        LogicalType::Timestamp | LogicalType::TimestampTz => {
            let a = downcast::<TimestampMicrosecondArray>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).cmp(&a.value(y))))
        }
        LogicalType::Decimal => {
            let a = downcast::<StringArray>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| cmp_decimal(a.value(x), a.value(y))))
        }
        LogicalType::Text
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => {
            let a = downcast::<StringArray>(values).ok_or_else(unexpected)?;
            Ok(order(a, d, |x, y| a.value(x).cmp(a.value(y))))
        }
    }
}

fn downcast<T: 'static>(values: &dyn Array) -> Option<&T> {
    values.as_any().downcast_ref::<T>()
}

/// Stable sort of the non-null rows by `cmp`, then the NULL rows in their original order.
fn order(values: &dyn Array, descending: bool, cmp: impl Fn(usize, usize) -> Ordering) -> Vec<u32> {
    let (mut rows, nulls): (Vec<u32>, Vec<u32>) =
        (0..values.len() as u32).partition(|&i| values.is_valid(i as usize));
    if descending {
        rows.sort_by(|&x, &y| cmp(y as usize, x as usize));
    } else {
        rows.sort_by(|&x, &y| cmp(x as usize, y as usize));
    }
    rows.extend(nulls);
    rows
}

/// A decimal as text (`-12.50`, `NaN`, `Infinity`) ready to compare.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Decimal<'a> {
    NegativeInfinity,
    /// Digits without leading zeros in the integer part nor trailing zeros in the fraction.
    Negative {
        int: &'a str,
        frac: &'a str,
    },
    Positive {
        int: &'a str,
        frac: &'a str,
    },
    Infinity,
    NaN,
    /// Not a number we understand: after every number, by text.
    Other(&'a str),
}

fn parse_decimal(s: &str) -> Decimal<'_> {
    let t = s.trim();
    match t {
        "NaN" | "nan" => return Decimal::NaN,
        "Infinity" | "+Infinity" | "inf" | "+inf" => return Decimal::Infinity,
        "-Infinity" | "-inf" => return Decimal::NegativeInfinity,
        _ => {}
    }
    let (negative, digits) = match t.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let valid = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !valid(int) || !valid(frac) {
        return Decimal::Other(s);
    }
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    if negative && !(int.is_empty() && frac.is_empty()) {
        Decimal::Negative { int, frac }
    } else {
        Decimal::Positive { int, frac }
    }
}

fn cmp_decimal(a: &str, b: &str) -> Ordering {
    // Fractions without trailing zeros compare digit by digit (`.5` > `.45`); integer parts
    // without leading zeros compare by length first.
    let magnitude = |ai: &str, af: &str, bi: &str, bf: &str| {
        ai.len()
            .cmp(&bi.len())
            .then_with(|| ai.cmp(bi))
            .then_with(|| af.cmp(bf))
    };
    match (parse_decimal(a), parse_decimal(b)) {
        (Decimal::Positive { int: ai, frac: af }, Decimal::Positive { int: bi, frac: bf }) => {
            magnitude(ai, af, bi, bf)
        }
        (Decimal::Negative { int: ai, frac: af }, Decimal::Negative { int: bi, frac: bf }) => {
            magnitude(bi, bf, ai, af)
        }
        (x, y) => x.cmp(&y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_compare_by_value() {
        let mut values = vec![
            "10",
            "-2.5",
            "NaN",
            "0.45",
            "abc",
            "2",
            "-10",
            "0.5",
            "-0.000",
            "Infinity",
            "-Infinity",
            "002.50",
            "+3",
            "1e3",
        ];
        values.sort_by(|a, b| cmp_decimal(a, b));
        assert_eq!(
            values,
            [
                "-Infinity",
                "-10",
                "-2.5",
                "-0.000",
                "0.45",
                "0.5",
                "2",
                "002.50",
                "+3",
                "10",
                "Infinity",
                "NaN",
                "1e3",
                "abc"
            ]
        );
        assert_eq!(cmp_decimal("2.50", "2.5"), Ordering::Equal);
        assert_eq!(cmp_decimal("-0", "0"), Ordering::Equal);
    }

    #[test]
    fn nulls_go_last_in_both_directions_and_ties_keep_their_order() {
        let a = Int64Array::from(vec![Some(3), None, Some(1), Some(3), None, Some(2)]);
        let up = SortKey {
            column: 0,
            descending: false,
        };
        let down = SortKey {
            column: 0,
            descending: true,
        };
        assert_eq!(
            sorted_rows(&a, LogicalType::Int, up).unwrap(),
            [2, 5, 0, 3, 1, 4]
        );
        assert_eq!(
            sorted_rows(&a, LogicalType::Int, down).unwrap(),
            [0, 3, 5, 2, 1, 4]
        );
    }

    #[test]
    fn text_floats_and_a_wrong_array() {
        let key = SortKey {
            column: 4,
            descending: false,
        };
        let t = StringArray::from(vec!["b", "B", "á", "a"]);
        assert_eq!(
            sorted_rows(&t, LogicalType::Text, key).unwrap(),
            [1, 3, 0, 2]
        );
        let f = Float64Array::from(vec![f64::NAN, 1.5, -0.5, f64::INFINITY]);
        assert_eq!(
            sorted_rows(&f, LogicalType::Float, key).unwrap(),
            [2, 1, 3, 0]
        );
        assert_eq!(
            sorted_rows(&t, LogicalType::Int, key),
            Err(SortError::UnexpectedArray(4))
        );
    }
}
