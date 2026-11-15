use spelaion_driver_api::{Dialect, StatementSpan};

/// Spanner's GoogleSQL dialect, as its lexer reads text:
///
/// - `'…'` and `"…"` strings, and `'''…'''` and `"""…"""` ones that may span lines; a backslash
///   escapes the next character (a doubled quote is not an escape: it ends one string and starts
///   another). Prefixes `r` (raw), `b` (bytes) and both read the same way here;
/// - `` `…` `` names, where a backslash escapes the next character;
/// - `--` and `#` comments to the end of the line, and block comments that do not nest.
///
/// The server runs one statement per request, so a script is always split here first.
pub struct SpannerDialect;

impl Dialect for SpannerDialect {
    fn name(&self) -> &'static str {
        "spanner"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        spelaion_driver_api::code_ranges(script, skip_non_code)
    }

    /// Always in backticks, with `\` and `` ` `` escaped.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('\\', "\\\\").replace('`', "\\`"))
    }
}

fn split(script: &str) -> Vec<StatementSpan> {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    let mut i = 0;

    while i < b.len() {
        if let Some(end) = skip_non_code(script, i) {
            let comment = matches!(b[i], b'-' | b'#' | b'/');
            if !comment {
                has_code = true;
            }
            i = end.max(i + 1);
            continue;
        }
        match b[i] {
            b';' => {
                if has_code {
                    spans.push(trimmed(script, start, i));
                }
                i += 1;
                start = i;
                has_code = false;
            }
            c => {
                if !c.is_ascii_whitespace() {
                    has_code = true;
                }
                i += 1;
            }
        }
    }
    if has_code {
        spans.push(trimmed(script, start, b.len()));
    }
    spans
}

/// The byte after the string, quoted name or comment that starts at `i`; `None` when none
/// starts there. A string's prefix (`r`, `b`, `rb`, `br`) is part of the word before it, so
/// the string starts at its quote.
fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match *b.get(i)? {
        q @ (b'\'' | b'"') => {
            if b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q) {
                Some(skip_triple(b, i, q))
            } else {
                Some(skip_quoted(b, i, q))
            }
        }
        b'`' => Some(skip_quoted(b, i, b'`')),
        b'-' if b.get(i + 1) == Some(&b'-') => Some(line_end(b, i)),
        b'#' => Some(line_end(b, i)),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(
            script[i + 2..]
                .find("*/")
                .map_or(b.len(), |p| i + 2 + p + 2),
        ),
        _ => None,
    }
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p + 1)
}

/// Skips a quoted string or name starting at `i` (the opening quote): a backslash escapes the
/// next byte; the next unescaped quote ends it.
fn skip_quoted(b: &[u8], i: usize, quote: u8) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\\' {
            j += 2;
        } else if b[j] == quote {
            return j + 1;
        } else {
            j += 1;
        }
    }
    b.len()
}

/// Skips a triple-quoted string starting at `i`: it ends at the next three unescaped quotes.
fn skip_triple(b: &[u8], i: usize, quote: u8) -> usize {
    let mut j = i + 3;
    while j < b.len() {
        if b[j] == b'\\' {
            j += 2;
        } else if b[j] == quote && b.get(j + 1) == Some(&quote) && b.get(j + 2) == Some(&quote) {
            return j + 3;
        } else {
            j += 1;
        }
    }
    b.len()
}

fn trimmed(script: &str, start: usize, end: usize) -> StatementSpan {
    let text = &script[start..end];
    let lead = text.len() - text.trim_start().len();
    let trail = text.len() - text.trim_end().len();
    StatementSpan {
        start: start + lead,
        end: end - trail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(script: &str) -> Vec<&str> {
        split(script).iter().map(|s| s.text(script)).collect()
    }

    fn code(script: &str) -> Vec<&str> {
        SpannerDialect
            .code_ranges(script)
            .into_iter()
            .map(|r| &script[r])
            .collect()
    }

    #[test]
    fn splits_simple_statements() {
        assert_eq!(
            parts("SELECT 1; SELECT 2;\n\n SELECT 3"),
            ["SELECT 1", "SELECT 2", "SELECT 3"]
        );
    }

    #[test]
    fn ignores_empty_and_comment_only_statements() {
        assert_eq!(
            parts(";; -- nothing\n; # nor this\n; /* neither */ ;SELECT 1;\n-- end"),
            ["SELECT 1"]
        );
    }

    #[test]
    fn strings_and_names_escape_with_backslashes() {
        let script = r#"SELECT 'a\';b', "c\";d", `e\`;f`, r'g;h', b"i;j"; SELECT 2"#;
        assert_eq!(
            parts(script),
            [
                r#"SELECT 'a\';b', "c\";d", `e\`;f`, r'g;h', b"i;j""#,
                "SELECT 2"
            ]
        );
    }

    #[test]
    fn a_doubled_quote_is_two_strings() {
        // `'a''b'` is 'a' then 'b': the `;` between the second pair is code.
        assert_eq!(
            parts("SELECT 'a'';'; SELECT 2"),
            ["SELECT 'a'';'", "SELECT 2"]
        );
    }

    #[test]
    fn triple_quoted_strings_span_lines_and_hide_semicolons() {
        let script = "SELECT '''a;\n'b';\n''', \"\"\"c\"\";\"\"\"; SELECT 3";
        assert_eq!(
            parts(script),
            ["SELECT '''a;\n'b';\n''', \"\"\"c\"\";\"\"\"", "SELECT 3"]
        );
    }

    #[test]
    fn comments_hide_semicolons_and_do_not_nest() {
        let script = "SELECT 1 -- a;b\n# c;d\n/* e; /* f; */; SELECT 2";
        assert_eq!(
            parts(script),
            ["SELECT 1 -- a;b\n# c;d\n/* e; /* f; */", "SELECT 2"]
        );
    }

    #[test]
    fn unterminated_string_keeps_rest_as_one_statement() {
        assert_eq!(parts("SELECT 'abc; SELECT 2"), ["SELECT 'abc; SELECT 2"]);
    }

    #[test]
    fn code_is_what_lies_outside_strings_names_and_comments() {
        assert_eq!(
            code("SELECT @a, ':b', `:c` # :d\n, :e /* :f */ '''x'''"),
            ["SELECT @a, ", ", ", " ", ", :e ", " "]
        );
    }

    #[test]
    fn names_are_quoted_in_backticks_and_escaped() {
        let d = SpannerDialect;
        assert_eq!(d.quote_identifier("Singers"), "`Singers`");
        assert_eq!(d.quote_identifier("a`b"), "`a\\`b`");
        assert_eq!(d.quote_identifier("a\\b"), "`a\\\\b`");
    }
}
