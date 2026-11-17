//! `${name}` variables in the editor's scripts (SDB-383): which ones a script uses and the
//! script with their values written in, before anything else reads it. The guard, the
//! read-only checks and masking are given the text this returns, so they see exactly what
//! runs.
//!
//! A value is text the user set for a tab or a connection, written in as it is (a table
//! name, a list, a condition), unlike a `:name` parameter, which becomes a literal
//! (`params.rs`). So that a value cannot change what the script is:
//!
//! - `${…}` is looked for only in code, as the engine's own scanner reads it
//!   ([`Dialect::code_ranges`]): never in a string, a quoted name or a comment;
//! - values are written in after the script is split, one statement at a time, and the
//!   statement is split again: a value that ends it or starts another is refused;
//! - a value that opens a string, a quoted name or a comment it does not close itself is
//!   refused: what follows it in the statement stays code;
//! - a name is only ever looked up in the values given: never the environment, a file or
//!   anything else (`${env:HOME}` is not even a name).
//!
//! No error here carries a value: one may be sensitive.

use std::collections::HashMap;
use std::ops::Range;

use spelaion_driver_api::Dialect;

// What can be a name and a value is the local store's rule too: one definition.
pub use spelaion_store::{
    MAX_VARIABLE_NAME, MAX_VARIABLE_VALUE, is_variable_name, is_variable_value,
};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VariableError {
    #[error("no value for ${{{0}}}")]
    Missing(String),
    #[error(
        "the value of {0} would end its statement or start another one: a variable cannot hold a statement separator"
    )]
    SplitsStatement(String),
    #[error(
        "the value of {0} opens a string, a quoted name or a comment that it does not close: the rest of the statement would no longer be read as written"
    )]
    OpensText(String),
    #[error("the value of ${{{0}}} is too long or holds a character that cannot be sent")]
    InvalidValue(String),
}

/// The distinct variables of `script`'s statements, in order of first appearance.
pub fn script_variables(dialect: &dyn Dialect, script: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for span in dialect.split_statements(script) {
        let Some(statement) = script.get(span.start..span.end) else {
            continue;
        };
        for (_, name) in variables(dialect, statement) {
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// `script` with every `${name}` of its statements replaced by its value in `values`, as it
/// is. Refused when a variable has no value, or when a value would change how the script is
/// split or read (see the module).
pub fn substitute_variables(
    dialect: &dyn Dialect,
    script: &str,
    values: &HashMap<String, String>,
) -> Result<String, VariableError> {
    let spans = dialect.split_statements(script);
    let mut out = String::with_capacity(script.len());
    let mut copied = 0;
    let mut changed = false;
    for span in &spans {
        let Some(statement) = script.get(span.start..span.end) else {
            continue;
        };
        if span.start < copied {
            continue;
        }
        let found = variables(dialect, statement);
        if found.is_empty() {
            continue;
        }
        changed = true;
        out.push_str(&script[copied..span.start]);
        out.push_str(&substitute_statement(dialect, statement, &found, values)?);
        copied = span.end;
    }
    if !changed {
        return Ok(script.to_string());
    }
    out.push_str(&script[copied..]);
    // The script as a whole still has the statements it had.
    if dialect.split_statements(&out).len() != spans.len() {
        return Err(VariableError::SplitsStatement(names_of(
            &script_variables(dialect, script)
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        )));
    }
    Ok(out)
}

/// One statement with its variables' values written in, checked.
fn substitute_statement(
    dialect: &dyn Dialect,
    statement: &str,
    found: &[(Range<usize>, &str)],
    values: &HashMap<String, String>,
) -> Result<String, VariableError> {
    let mut out = String::with_capacity(statement.len());
    // Where each value went in `out`, with its variable's name.
    let mut written: Vec<(Range<usize>, &str)> = Vec::new();
    let mut copied = 0;
    for (place, name) in found {
        let value = values
            .get(*name)
            .ok_or_else(|| VariableError::Missing((*name).to_string()))?;
        if !is_variable_value(value) {
            return Err(VariableError::InvalidValue((*name).to_string()));
        }
        out.push_str(&statement[copied..place.start]);
        let start = out.len();
        out.push_str(value);
        written.push((start..out.len(), name));
        copied = place.end;
    }
    out.push_str(&statement[copied..]);

    // Still one statement, as the engine's splitter reads it.
    if dialect.split_statements(&out).len() != 1 {
        let names: Vec<&str> = written.iter().map(|(_, name)| *name).collect();
        return Err(VariableError::SplitsStatement(names_of(&names)));
    }
    // What a value opens (a string, a quoted name, a comment), it closes: nothing that is
    // not code may reach from inside a value to outside it. The statement is read with a
    // line break and one more word after it, so what a value at its very end leaves open
    // (a string, a block comment) reaches that word and is seen; a line comment ends at the
    // line break, and is the value's own.
    let probe = format!("{out}\n_");
    let mut end_of_code = 0;
    let mut not_code: Vec<Range<usize>> = Vec::new();
    for code in dialect.code_ranges(&probe) {
        if end_of_code < code.start {
            not_code.push(end_of_code..code.start);
        }
        end_of_code = code.end;
    }
    if end_of_code < probe.len() {
        not_code.push(end_of_code..probe.len());
    }
    let bytes = probe.as_bytes();
    for (value, name) in &written {
        // A scanner may count the line break that ends a line comment as part of it.
        let closes_by = value.end + usize::from(bytes.get(value.end) == Some(&b'\n'));
        for region in &not_code {
            let overlaps = region.start < value.end && value.start < region.end;
            let inside = value.start <= region.start && region.end <= closes_by;
            if overlaps && !inside {
                return Err(VariableError::OpensText(format!("${{{name}}}")));
            }
        }
    }
    Ok(out)
}

/// `${a}`, or `${a} and ${b}`: the variables an error is about.
fn names_of(names: &[&str]) -> String {
    let mut distinct: Vec<&str> = Vec::new();
    for name in names {
        if !distinct.contains(name) {
            distinct.push(name);
        }
    }
    distinct
        .iter()
        .map(|name| format!("${{{name}}}"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Where each `${name}` of a statement is (the whole `${…}`), in order. Only the statement's
/// code is searched.
fn variables<'a>(dialect: &dyn Dialect, statement: &'a str) -> Vec<(Range<usize>, &'a str)> {
    let bytes = statement.as_bytes();
    let mut found = Vec::new();
    for code in dialect.code_ranges(statement) {
        let mut i = code.start;
        while i + 1 < code.end {
            if bytes[i] != b'$' || bytes[i + 1] != b'{' {
                i += 1;
                continue;
            }
            let name_start = i + 2;
            let name_end = bytes[name_start..code.end]
                .iter()
                .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                .map_or(code.end, |p| name_start + p);
            // The closing brace is in the same piece of code, right after the name.
            if name_end < code.end
                && bytes[name_end] == b'}'
                && is_variable_name(&statement[name_start..name_end])
            {
                found.push((i..name_end + 1, &statement[name_start..name_end]));
                i = name_end + 1;
            } else {
                i += 2;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_dialect::{MysqlDialect, PostgresDialect, SqlServerDialect};

    fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn variables_are_found_only_in_code() {
        let script = "select ${col}, '${no}', \"${no}\" -- ${no}\n\
                      from ${table} /* ${no} */ where a = $${no}$$ and b = ${col}; select ${last}";
        assert_eq!(
            script_variables(&PostgresDialect, script),
            ["col", "table", "last"]
        );
        assert_eq!(
            script_variables(&MysqlDialect, "select `${no}`, ${yes} # ${no}"),
            ["yes"]
        );
        assert_eq!(
            script_variables(&SqlServerDialect, "select [${no}], ${yes}"),
            ["yes"]
        );
    }

    #[test]
    fn what_is_not_a_name_is_left_as_written() {
        for script in [
            "select ${}",
            "select ${ a }",
            "select ${a-b}",
            "select ${env:HOME}",
            "select ${user.name}",
            "select ${1a}",
            "select ${a",
            "select $a, $1, {a}",
        ] {
            assert!(
                script_variables(&PostgresDialect, script).is_empty(),
                "{script}"
            );
            assert_eq!(
                substitute_variables(&PostgresDialect, script, &values(&[("a", "1")])).as_deref(),
                Ok(script)
            );
        }
        let long = format!("select ${{{}}}", "a".repeat(MAX_VARIABLE_NAME + 1));
        assert!(script_variables(&PostgresDialect, &long).is_empty());
    }

    #[test]
    fn values_are_written_in_as_they_are() {
        let script =
            "select ${cols} from ${table} where id in (${ids}) and '${cols}' <> ''; select ${cols}";
        let written = substitute_variables(
            &PostgresDialect,
            script,
            &values(&[
                ("cols", "id, name"),
                ("table", "shop.\"order\""),
                ("ids", "1, 2, 3"),
            ]),
        )
        .unwrap();
        assert_eq!(
            written,
            "select id, name from shop.\"order\" where id in (1, 2, 3) and '${cols}' <> ''; select id, name"
        );
    }

    #[test]
    fn a_variable_without_a_value_is_named_and_nothing_else_is_read() {
        // The environment has a PATH everywhere: it is never where a value comes from.
        assert!(std::env::var_os("PATH").is_some());
        assert_eq!(
            substitute_variables(&PostgresDialect, "select '${PATH}', ${PATH}", &values(&[])),
            Err(VariableError::Missing("PATH".into()))
        );
    }

    #[test]
    fn a_value_cannot_end_its_statement_or_start_another() {
        for value in ["1; drop table t", "1;", ";", "1; select 2 --"] {
            let refused = substitute_variables(
                &PostgresDialect,
                "select * from t where id = ${id} and tenant = 1",
                &values(&[("id", value)]),
            );
            assert!(
                matches!(
                    refused,
                    Err(VariableError::SplitsStatement(_) | VariableError::OpensText(_))
                ),
                "{value}: {refused:?}"
            );
        }
        // Nothing left of the statement is not one statement either.
        assert_eq!(
            substitute_variables(&PostgresDialect, "${all}", &values(&[("all", " ")])),
            Err(VariableError::SplitsStatement("${all}".into()))
        );
    }

    #[test]
    fn a_value_cannot_open_what_it_does_not_close() {
        for value in ["1 --", "1 /*", "'", "'a", "\"", "$$", "1 /"] {
            let script = if value == "1 /" {
                "select * from t where id = ${id}* 2 */ and tenant = 1"
            } else {
                "select * from t where id = ${id} and tenant = 1"
            };
            assert_eq!(
                substitute_variables(&PostgresDialect, script, &values(&[("id", value)])),
                Err(VariableError::OpensText("${id}".into())),
                "{value}"
            );
        }
        // At the very end of its statement too: what it leaves open has nothing after it,
        // and is still refused. A line comment there ends with the line, and is its own.
        for value in ["1 /*", "'", "'a", "s3cret'; drop table t; --", "$$"] {
            assert_eq!(
                substitute_variables(
                    &PostgresDialect,
                    "select * from t where id = ${id}",
                    &values(&[("id", value)])
                ),
                Err(VariableError::OpensText("${id}".into())),
                "{value}"
            );
        }
        assert_eq!(
            substitute_variables(
                &PostgresDialect,
                "select * from t where id = ${id}",
                &values(&[("id", "1 -- the first")])
            )
            .as_deref(),
            Ok("select * from t where id = 1 -- the first")
        );
        // What it opens and closes itself is its own.
        assert_eq!(
            substitute_variables(
                &PostgresDialect,
                "select * from t where name = ${name} and tenant = 1",
                &values(&[("name", "'it''s; fine' /* ; */")]),
            )
            .unwrap(),
            "select * from t where name = 'it''s; fine' /* ; */ and tenant = 1"
        );
    }

    #[test]
    fn errors_never_carry_a_value() {
        let secret = "s3cret'; drop table t; --";
        let error =
            substitute_variables(&PostgresDialect, "select ${v}", &values(&[("v", secret)]))
                .unwrap_err()
                .to_string();
        assert!(!error.contains("s3cret"), "{error}");
        assert!(error.contains("${v}"), "{error}");
    }

    #[test]
    fn names_and_values_are_bounded() {
        assert!(is_variable_name("customer_id") && is_variable_name("_x1"));
        for name in ["", "1x", "a b", "a-b", "env:HOME", "ñ"] {
            assert!(!is_variable_name(name), "{name}");
        }
        assert!(is_variable_value("") && is_variable_value("a\nb\tc"));
        assert!(!is_variable_value("a\0b"));
        assert!(!is_variable_value(&"x".repeat(MAX_VARIABLE_VALUE + 1)));
        assert_eq!(
            substitute_variables(&PostgresDialect, "select ${v}", &values(&[("v", "a\0b")])),
            Err(VariableError::InvalidValue("v".into()))
        );
    }
}
