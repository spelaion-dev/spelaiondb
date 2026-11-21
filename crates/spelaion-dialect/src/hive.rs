use spelaion_driver_api::{Dialect, StatementSpan};

use crate::{is_white_space_at, trimmed};

/// HiveQL's lexical rules, which Spark SQL and Impala share, under each engine's own name (`hive`,
/// `spark`, `kyuubi`, `impala`): `` `…` `` names with a doubled backquote, strings in `'…'` and in
/// `"…"` (a double quote is a string here, not a name) where a backslash escapes the next
/// character, `--` comments and block comments that do not nest. HiveServer2 runs one statement
/// per call, so a script is always split here first.
///
/// The name is the engine's, never another's: the core's barriers are chosen by dialect name, and
/// none answers to these (they have no read barrier, ADR-020).
pub struct HiveDialect {
    name: &'static str,
}

impl HiveDialect {
    pub const fn named(name: &'static str) -> Self {
        Self { name }
    }
}

impl Dialect for HiveDialect {
    fn name(&self) -> &'static str {
        self.name
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        spelaion_driver_api::code_ranges(script, skip_non_code)
    }

    /// Always in backquotes, a `` ` `` doubled: the only way these engines quote a name.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }
}

pub(crate) fn split(script: &str) -> Vec<StatementSpan> {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    let mut i = 0;
    while i < b.len() {
        if let Some(end) = skip_non_code(script, i) {
            if matches!(b[i], b'\'' | b'"' | b'`') {
                has_code = true;
            }
            i = end.max(i + 1);
            continue;
        }
        match b[i] {
            b';' => {
                if has_code {
                    spans.extend(trimmed(script, start, i));
                }
                i += 1;
                start = i;
                has_code = false;
            }
            _ => {
                if !is_white_space_at(script, i) {
                    has_code = true;
                }
                i += 1;
            }
        }
    }
    if has_code {
        spans.extend(trimmed(script, start, b.len()));
    }
    spans
}

/// The byte after the string, quoted name or comment that starts at `i`; `None` when none
/// starts there. An unterminated one runs to the end of the script.
pub(crate) fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match *b.get(i)? {
        q @ (b'\'' | b'"') => {
            let mut j = i + 1;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 2,
                    c if c == q => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(b.len())
        }
        b'`' => {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'`' {
                    if b.get(j + 1) == Some(&b'`') {
                        j += 2;
                        continue;
                    }
                    return Some(j + 1);
                }
                j += 1;
            }
            Some(b.len())
        }
        b'-' if b.get(i + 1) == Some(&b'-') => Some(
            b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |p| i + p + 1),
        ),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(
            script[i + 2..]
                .find("*/")
                .map_or(b.len(), |p| i + 2 + p + 2),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(script: &str) -> Vec<&str> {
        split(script).iter().map(|s| s.text(script)).collect()
    }

    #[test]
    fn splits_simple_statements() {
        assert_eq!(
            parts("use sales; select 1;\n\n select 2"),
            ["use sales", "select 1", "select 2"]
        );
    }

    #[test]
    fn ignores_empty_and_comment_only_statements() {
        assert_eq!(
            parts(";; -- nothing\n; /* neither; */ ;select 1;\n-- end"),
            ["select 1"]
        );
    }

    #[test]
    fn both_quotes_are_strings_with_backslash_escapes() {
        assert_eq!(
            parts("select 'it\\'s; fine', \"a\\\";b\", 'c\\\\'; select 2"),
            ["select 'it\\'s; fine', \"a\\\";b\", 'c\\\\'", "select 2"]
        );
    }

    #[test]
    fn backquoted_names_double_their_quote() {
        assert_eq!(
            parts("select `a;b`, `c``;d` from t; select 2"),
            ["select `a;b`, `c``;d` from t", "select 2"]
        );
    }

    #[test]
    fn an_open_quote_runs_to_the_end() {
        assert_eq!(parts("select 'a; select 2"), ["select 'a; select 2"]);
        assert_eq!(parts("select 'a\\'; select 2"), ["select 'a\\'; select 2"]);
    }

    #[test]
    fn it_quotes_with_backquotes_under_each_engines_name() {
        for name in ["hive", "spark", "kyuubi", "impala"] {
            let d = HiveDialect::named(name);
            assert_eq!(d.name(), name);
            assert_eq!(d.quote_identifier("a`b"), "`a``b`");
        }
    }

    #[test]
    fn code_ranges_leave_out_strings_names_and_comments() {
        let d = HiveDialect::named("hive");
        let script = "select ':x', `:y`, :z -- :w";
        let code: String = d
            .code_ranges(script)
            .into_iter()
            .map(|r| &script[r])
            .collect();
        assert!(code.contains(":z"), "{code}");
        assert!(!code.contains(":x") && !code.contains(":y") && !code.contains(":w"));
    }
}
