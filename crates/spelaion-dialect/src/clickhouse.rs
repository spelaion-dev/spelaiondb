use spelaion_driver_api::{Dialect, StatementSpan};

/// ClickHouse dialect, as ClickHouse's own lexer reads text: `'…'`, `"…"` and `` `…` `` with
/// backslash escapes and doubled quotes, `--` comments and `#` ones (followed by a space or
/// `!`), nested block comments, and `$tag$ … $tag$` heredocs. The server runs one statement
/// per request, so a script is always split here first.
pub struct ClickhouseDialect;

impl Dialect for ClickhouseDialect {
    fn name(&self) -> &'static str {
        "clickhouse"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        spelaion_driver_api::code_ranges(script, skip_non_code)
    }

    /// Always in backticks, with `\` and `` ` `` escaped: names are case-sensitive and many
    /// words are keywords somewhere.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('\\', "\\\\").replace('`', "\\`"))
    }
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
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

/// The byte after the string, quoted name, comment or heredoc that starts at `i`; `None` when
/// none starts there.
fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match *b.get(i)? {
        q @ (b'\'' | b'"' | b'`') => Some(skip_quoted(b, i, q)),
        b'-' if b.get(i + 1) == Some(&b'-') => Some(line_end(b, i)),
        b'#' if matches!(b.get(i + 1), Some(b' ' | b'!')) => Some(line_end(b, i)),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(block_comment_end(b, i)),
        b'$' if i == 0 || !is_ident_byte(b[i - 1]) => {
            let tag_end = heredoc_tag(b, i)?;
            let tag = &script[i..=tag_end];
            Some(
                script[tag_end + 1..]
                    .find(tag)
                    .map_or(b.len(), |p| tag_end + 1 + p + tag.len()),
            )
        }
        _ => None,
    }
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p + 1)
}

/// The byte after a block comment that starts at `i` (`/*`); block comments nest.
fn block_comment_end(b: &[u8], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

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

/// If a heredoc's `$tag$` (or `$$`) starts at `i`, the position of its closing `$`.
fn heredoc_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j)
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
        ClickhouseDialect
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
            parts(";; -- nothing\n; /* neither */ ; # nor this\n;select 1;\n#! end"),
            ["select 1"]
        );
    }

    #[test]
    fn quotes_escape_with_backslashes_and_doubling() {
        let script = "select 'a\\';b', 'c'';d', \"e\\\";f\", `g\\`;h`, `i``;j`; select 2";
        assert_eq!(
            parts(script),
            [
                "select 'a\\';b', 'c'';d', \"e\\\";f\", `g\\`;h`, `i``;j`",
                "select 2"
            ]
        );
    }

    #[test]
    fn comments_hide_semicolons_and_nest() {
        let script = "select 1 -- a;b\n# c;d\n/* e; /* f; */ g; */; select 2 #!h;i\n; select 3";
        assert_eq!(
            parts(script),
            [
                "select 1 -- a;b\n# c;d\n/* e; /* f; */ g; */",
                "select 2 #!h;i",
                "select 3"
            ]
        );
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_comment() {
        // The server refuses `#x`; it is not a comment, so a `;` after it still ends the
        // statement.
        assert_eq!(parts("select 1 #x; select 2"), ["select 1 #x", "select 2"]);
    }

    #[test]
    fn heredocs_hide_semicolons() {
        assert_eq!(
            parts("select $$a;b$$, $tag$x$$y;$tag$; select a$b$ from t; select 3"),
            [
                "select $$a;b$$, $tag$x$$y;$tag$",
                "select a$b$ from t",
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
            code("select :a, ':b', `:c` # :d\n, :e /* :f */ $$:g$$"),
            ["select :a, ", ", ", " ", ", :e ", " "]
        );
    }

    #[test]
    fn names_are_always_quoted_and_escaped() {
        let d = ClickhouseDialect;
        assert_eq!(d.quote_identifier("events"), "`events`");
        assert_eq!(d.quote_identifier("a`b"), "`a\\`b`");
        assert_eq!(d.quote_identifier("a\\b"), "`a\\\\b`");
    }
}
