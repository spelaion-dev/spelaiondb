//! Column filters of a stored result, like SQLyog's: `column = 'value'`, `<>`, `>`, `<`,
//! `>=`, `<=`, `LIKE`, `NOT LIKE`, contains, does not contain, `IS NULL`, `IS NOT NULL`, all
//! of them combined with AND.
//! They follow SQL: a NULL cell only matches `IS NULL` (`NULL <> 'Sleep'` is not true).
//! Numbers compare by value; everything else by the text the grid shows, ignoring case like
//! MySQL's default collations. They run in the core on the loaded rows, so they also work
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

/// Whether a cell passes the filter.
pub(crate) fn matches(cell: &Value, logical: LogicalType, filter: &ColumnFilter) -> bool {
    if matches!(cell, Value::Null) {
        return filter.op == FilterOp::IsNull;
    }
    let text = cell.to_string();
    match filter.op {
        FilterOp::IsNull => false,
        FilterOp::IsNotNull => true,
        FilterOp::Like => like(&text, &filter.value),
        FilterOp::NotLike => !like(&text, &filter.value),
        FilterOp::Contains => contains(&text, &filter.value),
        FilterOp::NotContains => !contains(&text, &filter.value),
        FilterOp::Eq => compare(cell, &text, logical, &filter.value) == Ordering::Equal,
        FilterOp::NotEq => compare(cell, &text, logical, &filter.value) != Ordering::Equal,
        FilterOp::Gt => compare(cell, &text, logical, &filter.value) == Ordering::Greater,
        FilterOp::Lt => compare(cell, &text, logical, &filter.value) == Ordering::Less,
        FilterOp::Ge => compare(cell, &text, logical, &filter.value) != Ordering::Less,
        FilterOp::Le => compare(cell, &text, logical, &filter.value) != Ordering::Greater,
    }
}

/// The cell against the filter value: numbers by value, the rest as text ignoring case.
fn compare(cell: &Value, text: &str, logical: LogicalType, value: &str) -> Ordering {
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
    text.to_lowercase().cmp(&value.to_lowercase())
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

/// The text holds `value`, ignoring case.
fn contains(text: &str, value: &str) -> bool {
    text.to_lowercase().contains(&value.to_lowercase())
}

/// SQL `LIKE`, ignoring case.
fn like(text: &str, pattern: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Token {
        Any,
        One,
        Char(char),
    }
    let mut tokens = Vec::new();
    let mut chars = pattern.chars().flat_map(char::to_lowercase).peekable();
    while let Some(c) = chars.next() {
        tokens.push(match c {
            '%' => Token::Any,
            '_' => Token::One,
            '\\' => Token::Char(chars.next().unwrap_or('\\')),
            c => Token::Char(c),
        });
    }
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
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
