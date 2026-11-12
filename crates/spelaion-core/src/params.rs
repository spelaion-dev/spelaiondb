//! `:name` placeholders in a script (the generated templates use them): which ones there are
//! and the script with the values written in as literals. Only the code is searched, as each
//! engine's own scanner reads it (`Dialect::code_ranges`): strings, quoted names, comments
//! (nested where the engine nests them) and dollar-quoted bodies are not, and neither are
//! `::casts`, `:=` or a colon joined to a word or a quote (SDB-135).

use std::collections::HashMap;

use spelaion_driver_api::{Dialect, Value};

use crate::export::sql_literal;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParamError {
    #[error("no value for :{0}")]
    Missing(String),
}

/// The distinct placeholder names of `script`, in order of first appearance.
pub fn script_params(dialect: &dyn Dialect, script: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (_, _, name) in placeholders(dialect, script) {
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
    for (start, end, name) in placeholders(dialect, script) {
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

/// `(start, end, name)` of each placeholder, in byte offsets (`start` is the `:`). Only the
/// script's code is searched, as the engine's own scanner reads it
/// ([`Dialect::code_ranges`]): its strings, quoted names and comments are not.
fn placeholders<'a>(dialect: &dyn Dialect, script: &'a str) -> Vec<(usize, usize, &'a str)> {
    let b = script.as_bytes();
    let mut found = Vec::new();
    for range in dialect.code_ranges(script) {
        let mut i = range.start;
        while i < range.end {
            if b[i] != b':' {
                i += 1;
                continue;
            }
            let after_colon = b.get(i + 1).copied().filter(|_| i + 1 < range.end);
            let before = i.checked_sub(1).map(|j| b[j]);
            // `x::int` is a cast, `@v:=1` an assignment.
            let cast = after_colon == Some(b':') || before == Some(b':');
            // `a:b` (a label, a slice bound) is no placeholder; nor is a colon right after a
            // string or a quoted name: DuckDB's `{'k':v}`, SQL Server's `[a]:b`.
            let joined = before.is_some_and(|c| {
                c.is_ascii_alphanumeric() || matches!(c, b'_' | b'\'' | b'"' | b'`' | b']')
            });
            let starts_name = after_colon.is_some_and(|c| c.is_ascii_alphabetic() || c == b'_');
            if !cast && !joined && starts_name {
                let end = b[i + 1..range.end]
                    .iter()
                    .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                    .map_or(range.end, |p| i + 1 + p);
                found.push((i, end, &script[i + 1..end]));
                i = end;
            } else {
                i += if after_colon == Some(b':') { 2 } else { 1 };
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_duckdb::DuckdbDialect;
    use spelaion_driver_mysql::MysqlDialect;
    use spelaion_driver_postgres::PostgresDialect;
    use spelaion_driver_sqlite::SqliteDialect;
    use spelaion_driver_sqlserver::SqlServerDialect;

    fn every_engine() -> Vec<Box<dyn Dialect>> {
        vec![
            Box::new(PostgresDialect),
            Box::new(MysqlDialect),
            Box::new(SqliteDialect),
            Box::new(SqlServerDialect),
            Box::new(DuckdbDialect),
        ]
    }

    #[test]
    fn placeholders_skip_strings_comments_and_casts() {
        let script = "select :id, ':no', \":no\", x::text, a:=1, -- :no\n\
                      /* :no */ $$ :no $$, $body$ :no $body$, $1, :Total_2 from t where id = :id";
        assert_eq!(script_params(&PostgresDialect, script), ["id", "Total_2"]);
        assert_eq!(script_params(&DuckdbDialect, script), ["id", "Total_2"]);
        assert_eq!(
            script_params(&MysqlDialect, "select 'it\\'s :no', :yes # :no"),
            ["yes"]
        );
        assert!(script_params(&SqliteDialect, "select 1").is_empty());
    }

    /// Placeholders after `=`, `(`, `,` and white space, and at the very start or end, on
    /// every engine; a cast (`::`) and an assignment (`:=`) are not.
    #[test]
    fn placeholders_are_found_where_values_go_on_every_engine() {
        let script = ":first, f(:a,:b) where x=:c and y = :d and z::int = 1 and @v:=1 or :last";
        for dialect in every_engine() {
            assert_eq!(
                script_params(dialect.as_ref(), script),
                ["first", "a", "b", "c", "d", "last"],
                "{}",
                dialect.name()
            );
        }
    }

    /// What each engine's own scanner reads as a name, a string, a comment or a literal is no
    /// placeholder, and binding leaves the statement as it was written (SDB-135).
    #[test]
    fn what_each_engine_does_not_read_as_code_has_no_placeholder() {
        let cases: Vec<(Box<dyn Dialect>, &[&str])> = vec![
            (
                Box::new(SqlServerDialect),
                &[
                    "SELECT [start:time] FROM t",
                    "SELECT [a]]:b] FROM t",
                    "/* a /* b */ :x */ SELECT 1",
                    "SELECT N'x :y'",
                    // A label in a procedure body is `word:`.
                    "CREATE PROCEDURE p AS BEGIN retry: SELECT 1; GOTO retry END",
                ],
            ),
            (
                Box::new(SqliteDialect),
                &["SELECT [start:time] FROM t", "SELECT `a:b` FROM t"],
            ),
            (
                Box::new(PostgresDialect),
                &[
                    "SELECT E'it\\'s :x'",
                    "SELECT e'\\\\', E'a\\'b:c' FROM t",
                    "/* a /* b */ :x */ SELECT 1",
                    "SELECT $fn$ :x $fn$",
                    "SELECT 'a':b",
                ],
            ),
            (
                Box::new(DuckdbDialect),
                &[
                    "SELECT E'it\\'s :x'",
                    "SELECT {'a':b, 'c':d}",
                    "SELECT MAP {'k':v}",
                    "/* a /* b */ :x */ SELECT 1",
                ],
            ),
            (
                Box::new(MysqlDialect),
                &[
                    "SELECT `a:b` FROM t",
                    "SELECT \"it\\\"s :x\"",
                    "/*!50001 SELECT :x */",
                    "-- :x\nSELECT 1",
                    "CREATE PROCEDURE p() BEGIN lbl: LOOP LEAVE lbl; END LOOP lbl; END",
                ],
            ),
        ];
        for (dialect, scripts) in cases {
            for script in scripts {
                assert_eq!(
                    script_params(dialect.as_ref(), script),
                    Vec::<String>::new(),
                    "{}: {script}",
                    dialect.name()
                );
                assert_eq!(
                    bind_params(dialect.as_ref(), script, &HashMap::new()).as_deref(),
                    Ok(*script),
                    "{}: {script}",
                    dialect.name()
                );
            }
        }
    }

    /// After a string, a name or a comment the scan goes on: a real placeholder further on is
    /// still found.
    #[test]
    fn placeholders_after_what_is_skipped_are_still_found() {
        assert_eq!(
            script_params(&SqlServerDialect, "SELECT [a:b] FROM t WHERE id = :id"),
            ["id"]
        );
        assert_eq!(
            script_params(&PostgresDialect, "SELECT E'it\\'s :x' WHERE id = :id"),
            ["id"]
        );
        assert_eq!(
            script_params(&DuckdbDialect, "SELECT {'a': :v} WHERE id = :id"),
            ["v", "id"]
        );
        assert_eq!(
            script_params(&PostgresDialect, "/* a /* b */ :x */ SELECT :y"),
            ["y"]
        );
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
            bind_params(&MysqlDialect, "select :name", &values).unwrap(),
            "select _utf8mb4 X'4F27427269656E205C20636F'"
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
