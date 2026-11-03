//! `:name` placeholders in a script (the generated templates use them): which ones there are
//! and the script with the values written in as literals. Strings, quoted identifiers,
//! comments, PostgreSQL `::casts` and dollar-quoted bodies are not placeholders.

use std::collections::HashMap;

use spelunk_driver_api::{Dialect, Value};

use crate::export::sql_literal;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParamError {
    #[error("no value for :{0}")]
    Missing(String),
}

/// The distinct placeholder names of `script`, in order of first appearance.
pub fn script_params(dialect: &dyn Dialect, script: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (_, _, name) in placeholders(dialect.name(), script) {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// `script` with every placeholder replaced by its value as a literal: numbers, `NULL`,
/// `TRUE` and `FALSE` as they are, anything else as a string with the engine's quoting.
pub fn bind_params(
    dialect: &dyn Dialect,
    script: &str,
    values: &HashMap<String, String>,
) -> Result<String, ParamError> {
    let mut out = String::with_capacity(script.len());
    let mut copied = 0;
    for (start, end, name) in placeholders(dialect.name(), script) {
        let raw = values
            .get(name)
            .ok_or_else(|| ParamError::Missing(name.to_string()))?;
        out.push_str(&script[copied..start]);
        out.push_str(&literal(dialect.name(), raw));
        copied = end;
    }
    out.push_str(&script[copied..]);
    Ok(out)
}

fn literal(dialect: &str, raw: &str) -> String {
    let trimmed = raw.trim();
    let value = if is_number(trimmed) {
        Value::Decimal(trimmed.to_string())
    } else if trimmed.eq_ignore_ascii_case("null") {
        Value::Null
    } else if trimmed.eq_ignore_ascii_case("true") {
        Value::Bool(true)
    } else if trimmed.eq_ignore_ascii_case("false") {
        Value::Bool(false)
    } else {
        // Text keeps its spaces: they may matter.
        Value::Text(raw.to_string())
    };
    sql_literal(&value, dialect)
}

/// `-12`, `3.5`, `.5`: plain decimal numbers only (no exponents, no hex).
pub(crate) fn is_number(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    !(int.is_empty() && frac.is_empty())
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.bytes().all(|b| b.is_ascii_digit())
        && !(digits.contains('.') && frac.is_empty())
}

/// `(start, end, name)` of each placeholder, in byte offsets (`start` is the `:`).
fn placeholders<'a>(dialect: &str, script: &'a str) -> Vec<(usize, usize, &'a str)> {
    let b = script.as_bytes();
    let mysql = dialect == "mysql" || dialect == "mariadb";
    let postgres = dialect == "postgres";
    let mut found = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => i = skip_quoted(b, i + 1, q, mysql && q != b'`'),
            b'-' if b.get(i + 1) == Some(&b'-') => i = skip_line(b, i),
            b'#' if mysql => i = skip_line(b, i),
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = script[i + 2..]
                    .find("*/")
                    .map_or(b.len(), |p| i + 2 + p + 2);
            }
            b'$' if postgres => i = skip_dollar_quoted(script, i),
            b':' => {
                let after_colon = b.get(i + 1).copied();
                let cast = after_colon == Some(b':') || (i > 0 && b[i - 1] == b':');
                let word_before = i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
                let starts_name = after_colon.is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
                if !cast && !word_before && starts_name {
                    let end = b[i + 1..]
                        .iter()
                        .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                        .map_or(b.len(), |p| i + 1 + p);
                    found.push((i, end, &script[i + 1..end]));
                    i = end;
                } else {
                    i += if after_colon == Some(b':') { 2 } else { 1 };
                }
            }
            _ => i += 1,
        }
    }
    found
}

/// Index after the closing quote; a doubled quote is part of the text, and so is a
/// backslash-escaped one where backslashes escape (MySQL strings).
fn skip_quoted(b: &[u8], mut i: usize, quote: u8, backslash: bool) -> usize {
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
        } else if b[i] == quote {
            if b.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

fn skip_line(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p + 1)
}

/// `$tag$ … $tag$` (PostgreSQL); a `$` that does not open one (`$1`) is skipped alone.
fn skip_dollar_quoted(script: &str, i: usize) -> usize {
    let rest = &script[i + 1..];
    let tag_len = rest
        .bytes()
        .position(|c| !(c.is_ascii_alphanumeric() || c == b'_'))
        .unwrap_or(rest.len());
    if rest.as_bytes().get(tag_len) != Some(&b'$')
        || rest[..tag_len].starts_with(|c: char| c.is_ascii_digit())
    {
        return i + 1;
    }
    let tag = &script[i..i + tag_len + 2];
    let body = i + tag.len();
    script[body..]
        .find(tag)
        .map_or(script.len(), |p| body + p + tag.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_driver_sqlite::SqliteDialect;

    struct Named(&'static str);
    impl Dialect for Named {
        fn name(&self) -> &'static str {
            self.0
        }
        fn split_statements(&self, _: &str) -> Vec<spelunk_driver_api::StatementSpan> {
            Vec::new()
        }
        fn quote_identifier(&self, ident: &str) -> String {
            ident.into()
        }
    }

    #[test]
    fn placeholders_skip_strings_comments_and_casts() {
        let script = "select :id, ':no', \":no\", x::text, a:=1, -- :no\n\
                      /* :no */ $$ :no $$, $body$ :no $body$, $1, :Total_2 from t where id = :id";
        assert_eq!(script_params(&Named("postgres"), script), ["id", "Total_2"]);
        assert_eq!(
            script_params(&Named("mysql"), "select 'it\\'s :no', :yes # :no"),
            ["yes"]
        );
        assert!(script_params(&SqliteDialect, "select 1").is_empty());
    }

    #[test]
    fn values_become_literals_of_the_engine() {
        let values = HashMap::from([
            ("id".to_string(), " 42 ".to_string()),
            ("name".to_string(), "O'Brien \\ co".to_string()),
            ("gone".to_string(), "null".to_string()),
            ("ok".to_string(), "True".to_string()),
            ("price".to_string(), "-0.5".to_string()),
        ]);
        let script = "update t set name = :name, gone = :gone, ok = :ok, price = :price where id = :id and :id > 0";
        assert_eq!(
            bind_params(&SqliteDialect, script, &values).unwrap(),
            "update t set name = 'O''Brien \\ co', gone = NULL, ok = TRUE, price = -0.5 where id = 42 and 42 > 0"
        );
        assert_eq!(
            bind_params(&Named("mysql"), "select :name", &values).unwrap(),
            "select 'O''Brien \\\\ co'"
        );
        assert_eq!(
            bind_params(&SqliteDialect, "select :x", &values),
            Err(ParamError::Missing("x".into()))
        );
    }

    #[test]
    fn only_plain_numbers_stay_unquoted() {
        for number in ["1", "-12", "3.25", ".5"] {
            assert!(is_number(number), "{number}");
        }
        for text in ["", "-", "1.", "1e5", "0x1F", "1,5", "12 "] {
            assert!(!is_number(text), "{text}");
        }
    }
}
