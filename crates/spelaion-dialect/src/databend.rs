use spelaion_driver_api::{Dialect, StatementSpan};

use crate::scanner::{self, RegionKind, SemicolonScanner};

/// Databend's dialect, as Databend's own lexer reads text (checked with 1.2.881):
///
/// - `'…'` strings and `"…"` names, where a backslash escapes the next character and a doubled
///   quote is a quote;
/// - `` `…` `` names, which end at the next backtick: neither a backslash nor a doubled
///   backtick escapes one;
/// - `--` comments to the end of the line, and block comments that do not nest (the first `*/`
///   ends one); `#` is an operator, not a comment;
/// - `$$ … $$` strings (a function's body).
///
/// The server runs one statement per request, so a script is always split here first.
pub struct DatabendDialect;

impl Dialect for DatabendDialect {
    fn name(&self) -> &'static str {
        "databend"
    }

    /// A client command line (ADR-078) is resolved only by the editor's run: never sent.
    fn refuse_statement(&self, statement: &str) -> Option<spelaion_driver_api::RequestError> {
        crate::refuse_client_command(statement)
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        scanner::code_ranges(SemicolonScanner::new(script, opening))
    }

    /// Always in double quotes, a quote doubled: a backtick cannot hold a backtick at all, and
    /// a backslash inside double quotes is kept as it is.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

fn split(script: &str) -> Vec<StatementSpan> {
    scanner::statements(SemicolonScanner::new(script, opening))
}

/// The string, quoted name, comment or `$$` body that starts at byte `i`, with the byte after it;
/// `None` when none starts there. The scanner's one reading of them (Databend's statements end at
/// a `;` outside them: [`SemicolonScanner`]).
fn opening(script: &str, i: usize) -> Option<(RegionKind, usize)> {
    let b = script.as_bytes();
    match *b.get(i)? {
        b'\'' => Some((RegionKind::String, skip_quoted(b, i, b'\''))),
        b'"' => Some((RegionKind::QuotedName, skip_quoted(b, i, b'"'))),
        b'`' => Some((
            RegionKind::QuotedName,
            b[i + 1..]
                .iter()
                .position(|&c| c == b'`')
                .map_or(b.len(), |p| i + 1 + p + 1),
        )),
        b'-' if b.get(i + 1) == Some(&b'-') => Some((
            RegionKind::Comment,
            b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |p| i + p + 1),
        )),
        b'/' if b.get(i + 1) == Some(&b'*') => Some((
            RegionKind::Comment,
            script[i + 2..]
                .find("*/")
                .map_or(b.len(), |p| i + 2 + p + 2),
        )),
        b'$' if b.get(i + 1) == Some(&b'$') => Some((
            RegionKind::QuotedBody,
            script[i + 2..]
                .find("$$")
                .map_or(b.len(), |p| i + 2 + p + 2),
        )),
        _ => None,
    }
}

/// The byte after the string, quoted name or comment that starts at `i`; `None` when none
/// Skips a quoted string or name starting at `i` (the opening quote): a backslash escapes the
/// next byte and a doubled quote is a quote.
fn skip_quoted(b: &[u8], i: usize, quote: u8) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\\' {
            j += 2;
        } else if b[j] == quote {
            if b.get(j + 1) == Some(&quote) {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(script: &str) -> Vec<&str> {
        split(script).iter().map(|s| s.text(script)).collect()
    }

    fn code(script: &str) -> Vec<&str> {
        DatabendDialect
            .code_ranges(script)
            .into_iter()
            .map(|r| &script[r])
            .collect()
    }

    #[test]
    fn splits_simple_statements() {
        assert_eq!(
            parts("select 1; select 2;\n\n select 3"),
            ["select 1", "select 2", "select 3"]
        );
    }

    #[test]
    fn ignores_empty_and_comment_only_statements() {
        assert_eq!(
            parts(";; -- nothing\n; /* neither */ ;select 1;\n-- end"),
            ["select 1"]
        );
    }

    #[test]
    fn strings_and_double_quoted_names_escape_with_backslashes_and_doubling() {
        let script = "select 'a\\';b', 'c'';d', \"e\\\";f\", \"g\"\";h\"; select 2";
        assert_eq!(
            parts(script),
            [
                "select 'a\\';b', 'c'';d', \"e\\\";f\", \"g\"\";h\"",
                "select 2"
            ]
        );
    }

    #[test]
    fn backticks_end_at_the_next_backtick() {
        // `a\` is a whole name: the backslash does not escape the backtick after it.
        assert_eq!(
            parts("select `a\\`; select `b;c`"),
            ["select `a\\`", "select `b;c`"]
        );
    }

    #[test]
    fn comments_hide_semicolons_and_do_not_nest() {
        let script = "select 1 -- a;b\n/* c; /* d; */; select 2";
        assert_eq!(
            parts(script),
            ["select 1 -- a;b\n/* c; /* d; */", "select 2"]
        );
    }

    #[test]
    fn a_hash_is_not_a_comment() {
        assert_eq!(
            parts("select 1 # 2; select 3"),
            ["select 1 # 2", "select 3"]
        );
    }

    #[test]
    fn dollar_strings_hide_semicolons() {
        assert_eq!(
            parts("create function f() returns int language python as $$a;b$$; select 3"),
            [
                "create function f() returns int language python as $$a;b$$",
                "select 3"
            ]
        );
    }

    #[test]
    fn unterminated_string_keeps_rest_as_one_statement() {
        assert_eq!(parts("select 'abc; select 2"), ["select 'abc; select 2"]);
    }

    #[test]
    fn code_is_what_lies_outside_strings_names_and_comments() {
        assert_eq!(
            code("select :a, ':b', `:c`, \":d\" -- :e\n, :f /* :g */ $$:h$$"),
            ["select :a, ", ", ", ", ", " ", ", :f ", " "]
        );
    }

    #[test]
    fn names_are_quoted_in_double_quotes() {
        let d = DatabendDialect;
        assert_eq!(d.quote_identifier("events"), "\"events\"");
        assert_eq!(d.quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(d.quote_identifier("a`b"), "\"a`b\"");
    }
}
