//! Column filters of a stored result, like SQLyog's: `column = 'value'`, `<>`, `>`, `<`,
//! `>=`, `<=`, `LIKE`, `NOT LIKE`, contains, does not contain, `IS NULL`, `IS NOT NULL`, all
//! of them combined with AND, or with OR when the user asks to match any ([`FilterMode`]).
//! They follow SQL: a NULL cell only matches `IS NULL` (`NULL <> 'Sleep'` is not true).
//! Numbers compare by value; everything else by the text the grid shows, ignoring case like
//! MySQL's default collations unless the user asks to match case. They run in the core on the loaded rows, so they also work
//! for statements that cannot take a WHERE (`SHOW FULL PROCESSLIST`).

use std::cmp::Ordering;

use spelaion_driver_api::{LogicalType, Value};

use crate::sort::{ViewError, cmp_decimal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    NotEq,
    Gt,
    Lt,
    Ge,
    Le,
    /// SQL `LIKE`: `%` any text, `_` one character, `\` escapes them.
    Like,
    NotLike,
    /// The cell's text holds the value (ignoring case); no wildcards, so the UI passes the
    /// value as it is ("filter by this value").
    Contains,
    NotContains,
    IsNull,
    IsNotNull,
}

impl FilterOp {
    /// Whether the operator compares with a value (`IS NULL` does not).
    pub fn takes_value(self) -> bool {
        match self {
            Self::Eq
            | Self::NotEq
            | Self::Gt
            | Self::Lt
            | Self::Ge
            | Self::Le
            | Self::Like
            | Self::NotLike
            | Self::Contains
            | Self::NotContains => true,
            Self::IsNull | Self::IsNotNull => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnFilter {
    pub column: usize,
    pub op: FilterOp,
    /// The value to compare with, as typed; ignored by `IS NULL` / `IS NOT NULL`.
    pub value: String,
}

/// How a result's column filters are read together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilterMode {
    /// A row passes when any filter does ("Match any"); otherwise it must pass all of them.
    pub any: bool,
    /// Text conditions tell upper case from lower case. Numbers compare by value either way.
    pub match_case: bool,
}

/// Checks a filter against the column it applies to, before any row is read.
pub(crate) fn validate(filter: &ColumnFilter, logical: LogicalType) -> Result<(), ViewError> {
    if filter.op.takes_value()
        && is_numeric(logical)
        && !matches!(
            filter.op,
            FilterOp::Like | FilterOp::NotLike | FilterOp::Contains | FilterOp::NotContains
        )
        && filter.value.trim().parse::<f64>().is_err()
    {
        return Err(ViewError::NotANumber {
            column: filter.column,
            value: filter.value.clone(),
        });
    }
    Ok(())
}

/// Whether a cell passes the filter, ignoring case.
#[cfg(test)]
pub(crate) fn matches(cell: &Value, logical: LogicalType, filter: &ColumnFilter) -> bool {
    matches_as(cell, logical, filter, false)
}

/// Whether a cell passes the filter; `match_case` for its text comparisons.
pub(crate) fn matches_as(
    cell: &Value,
    logical: LogicalType,
    filter: &ColumnFilter,
    match_case: bool,
) -> bool {
    if matches!(cell, Value::Null) {
        return filter.op == FilterOp::IsNull;
    }
    let text = cell.to_string();
    let order = || compare(cell, &text, logical, &filter.value, match_case);
    match filter.op {
        FilterOp::IsNull => false,
        FilterOp::IsNotNull => true,
        FilterOp::Like => like(&text, &filter.value, match_case),
        FilterOp::NotLike => !like(&text, &filter.value, match_case),
        FilterOp::Contains => contains(&text, &filter.value, match_case),
        FilterOp::NotContains => !contains(&text, &filter.value, match_case),
        FilterOp::Eq => order() == Ordering::Equal,
        FilterOp::NotEq => order() != Ordering::Equal,
        FilterOp::Gt => order() == Ordering::Greater,
        FilterOp::Lt => order() == Ordering::Less,
        FilterOp::Ge => order() != Ordering::Less,
        FilterOp::Le => order() != Ordering::Greater,
    }
}

/// The cell against the filter value: numbers by value, the rest as text, ignoring case
/// unless `match_case`.
fn compare(
    cell: &Value,
    text: &str,
    logical: LogicalType,
    value: &str,
    match_case: bool,
) -> Ordering {
    let value = value.trim();
    if is_numeric(logical) {
        match cell {
            Value::Decimal(d) => return cmp_decimal(d, value),
            Value::Int(i) => {
                if let Ok(v) = value.parse::<i64>() {
                    return i.cmp(&v);
                }
                if let Ok(v) = value.parse::<f64>() {
                    return (*i as f64).total_cmp(&v);
                }
            }
            Value::Float(f) => {
                if let Ok(v) = value.parse::<f64>() {
                    return f.total_cmp(&v);
                }
            }
            _ => {}
        }
    }
    if match_case {
        text.cmp(value)
    } else {
        text.to_lowercase().cmp(&value.to_lowercase())
    }
}

fn is_numeric(logical: LogicalType) -> bool {
    match logical {
        LogicalType::Int | LogicalType::Float | LogicalType::Decimal => true,
        LogicalType::Bool
        | LogicalType::Text
        | LogicalType::Bytes
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => false,
    }
}

/// The text holds `value`, ignoring case unless `match_case`.
fn contains(text: &str, value: &str, match_case: bool) -> bool {
    if match_case {
        text.contains(value)
    } else {
        text.to_lowercase().contains(&value.to_lowercase())
    }
}

/// SQL `LIKE`, ignoring case unless `match_case`.
fn like(text: &str, pattern: &str, match_case: bool) -> bool {
    let folded = |c: char| -> Vec<char> {
        if match_case {
            vec![c]
        } else {
            c.to_lowercase().collect()
        }
    };
    #[derive(Clone, Copy)]
    enum Token {
        Any,
        One,
        Char(char),
    }
    let mut tokens = Vec::new();
    let mut chars = pattern.chars().flat_map(folded).peekable();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '\\' => Token::Char(chars.next().unwrap_or('\\')),
            c => Token::Char(c),
        });
    }
    let text: Vec<char> = text.chars().flat_map(folded).collect();
    // Classic wildcard matching with backtracking to the last `%`.
    let (mut t, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match tokens.get(p) {
            Some(Token::Char(c)) if *c == text[t] => {
                t += 1;
                p += 1;
            }
            Some(Token::One) => {
                t += 1;
                p += 1;
            }
            Some(Token::Any) => {
                star = Some((p, t));
                p += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    tokens[p..].iter().all(|token| matches!(token, Token::Any))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(op: FilterOp, value: &str) -> ColumnFilter {
        ColumnFilter {
            column: 0,
            op,
            value: value.into(),
        }
    }

    #[test]
    fn like_follows_sql_wildcards_ignoring_case() {
        let like = |text: &str, pattern: &str| super::like(text, pattern, false);
        assert!(like("Sleep", "%sle%"));
        assert!(like("Sleep", "s_eep"));
        assert!(like("Sleep", "%"));
        assert!(like("", "%"));
        assert!(!like("Sleep", "s_ep"));
        assert!(like("100%", "100\\%"));
        assert!(!like("1000", "100\\%"));
        assert!(like("Query", "%e%y"));
        assert!(!like("Query", "%x%"));
        assert!(like("ÁRBOL", "árbol"));
    }

    #[test]
    fn nulls_only_match_is_null() {
        for op in [
            FilterOp::Eq,
            FilterOp::NotEq,
            FilterOp::Like,
            FilterOp::NotLike,
            FilterOp::Gt,
        ] {
            assert!(
                !matches(&Value::Null, LogicalType::Text, &f(op, "Sleep")),
                "{op:?}"
            );
        }
        assert!(matches(
            &Value::Null,
            LogicalType::Text,
            &f(FilterOp::IsNull, "")
        ));
        assert!(!matches(
            &Value::Null,
            LogicalType::Text,
            &f(FilterOp::IsNotNull, "")
        ));
        assert!(matches(
            &Value::Text("x".into()),
            LogicalType::Text,
            &f(FilterOp::IsNotNull, "")
        ));
    }

    #[test]
    fn numbers_by_value_text_ignoring_case() {
        let text = |s: &str| Value::Text(s.into());
        assert!(matches(
            &text("Sleep"),
            LogicalType::Text,
            &f(FilterOp::Eq, "sleep")
        ));
        assert!(!matches(
            &text("Sleep"),
            LogicalType::Text,
            &f(FilterOp::NotEq, "SLEEP")
        ));
        assert!(matches(
            &text("Query"),
            LogicalType::Text,
            &f(FilterOp::NotEq, "Sleep")
        ));
        // 10 > 9 as numbers, although "10" < "9" as text.
        assert!(matches(
            &Value::Int(10),
            LogicalType::Int,
            &f(FilterOp::Gt, "9")
        ));
        assert!(matches(
            &Value::Int(10),
            LogicalType::Int,
            &f(FilterOp::Ge, "9.5")
        ));
        assert!(matches(
            &Value::Float(1.5),
            LogicalType::Float,
            &f(FilterOp::Le, "1.5")
        ));
        assert!(matches(
            &Value::Decimal("12345678901234567890.5".into()),
            LogicalType::Decimal,
            &f(FilterOp::Gt, "12345678901234567890.25")
        ));
        assert!(matches(
            &text("2026-03-01"),
            LogicalType::Date,
            &f(FilterOp::Lt, "2026-10-01")
        ));
    }

    #[test]
    fn contains_takes_the_value_literally_ignoring_case() {
        let text = |s: &str| Value::Text(s.into());
        assert!(matches(
            &text("50% off"),
            LogicalType::Text,
            &f(FilterOp::Contains, "% OFF")
        ));
        // `%` and `_` are plain characters, unlike LIKE.
        assert!(!matches(
            &text("50 off"),
            LogicalType::Text,
            &f(FilterOp::Contains, "5_ off")
        ));
        assert!(matches(
            &text("50 off"),
            LogicalType::Text,
            &f(FilterOp::NotContains, "%")
        ));
        // Numbers by their text, and words are fine for them.
        assert!(matches(
            &Value::Int(12345),
            LogicalType::Int,
            &f(FilterOp::Contains, "234")
        ));
        assert!(validate(&f(FilterOp::Contains, "abc"), LogicalType::Int).is_ok());
        // NULL contains nothing and does not "not contain" either (SQL).
        assert!(!matches(
            &Value::Null,
            LogicalType::Text,
            &f(FilterOp::Contains, "")
        ));
        assert!(!matches(
            &Value::Null,
            LogicalType::Text,
            &f(FilterOp::NotContains, "x")
        ));
    }

    #[test]
    fn matching_case_tells_upper_from_lower_in_text_only() {
        let text = |s: &str| Value::Text(s.into());
        let passes = |cell: &Value, logical, op, value: &str| {
            (
                matches_as(cell, logical, &f(op, value), false),
                matches_as(cell, logical, &f(op, value), true),
            )
        };
        let sleep = text("Sleep");
        // (ignoring case, matching case)
        for (op, value, expected) in [
            (FilterOp::Eq, "sleep", (true, false)),
            (FilterOp::Eq, "Sleep", (true, true)),
            (FilterOp::NotEq, "SLEEP", (false, true)),
            (FilterOp::Contains, "LEE", (true, false)),
            (FilterOp::Contains, "lee", (true, true)),
            (FilterOp::NotContains, "LEE", (false, true)),
            (FilterOp::Like, "s%", (true, false)),
            (FilterOp::Like, "S_ee%", (true, true)),
            (FilterOp::NotLike, "s%", (false, true)),
            // Upper case sorts before lower case when case counts.
            (FilterOp::Lt, "apple", (false, true)),
            (FilterOp::Gt, "apple", (true, false)),
        ] {
            assert_eq!(
                passes(&sleep, LogicalType::Text, op, value),
                expected,
                "{op:?} {value}"
            );
        }
        // Numbers compare by value either way, and NULL still only matches IS NULL.
        assert_eq!(
            passes(&Value::Int(10), LogicalType::Int, FilterOp::Gt, "9"),
            (true, true)
        );
        assert_eq!(
            passes(&Value::Null, LogicalType::Text, FilterOp::NotEq, "x"),
            (false, false)
        );
        assert_eq!(
            passes(&text("ÁRBOL"), LogicalType::Text, FilterOp::Like, "árbol"),
            (true, false)
        );
    }

    #[test]
    fn numeric_columns_refuse_words() {
        assert_eq!(
            validate(&f(FilterOp::Gt, "lots"), LogicalType::Int),
            Err(ViewError::NotANumber {
                column: 0,
                value: "lots".into()
            })
        );
        assert!(validate(&f(FilterOp::Like, "1%"), LogicalType::Int).is_ok());
        assert!(validate(&f(FilterOp::IsNull, ""), LogicalType::Int).is_ok());
        assert!(validate(&f(FilterOp::Eq, "anything"), LogicalType::Text).is_ok());
    }
}
