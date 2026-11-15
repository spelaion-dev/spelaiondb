use spelaion_driver_api::{Dialect, StatementSpan};

/// Trino's dialect, and Presto's (the same grammar): standard SQL's lexical rules. `'…'`
/// strings and `"…"` names with doubled quotes and no backslash escapes, `--` comments and
/// block comments that do not nest. The server runs one statement per request, so a script is
/// always split here first.
pub struct TrinoDialect;

impl Dialect for TrinoDialect {
    fn name(&self) -> &'static str {
        "trino"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        spelaion_driver_api::code_ranges(script, skip_non_code)
    }

    /// Always in double quotes, a `"` doubled: names keep their case, and many words are
    /// reserved.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
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
            if matches!(b[i], b'\'' | b'"') {
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
/// starts there.
fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match *b.get(i)? {
        q @ (b'\'' | b'"') => {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == q {
                    if b.get(j + 1) == Some(&q) {
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
    fn quotes_double_and_backslashes_are_plain() {
        assert_eq!(
            parts("select 'it''s; fine', \"a\"\";b\", 'c\\'; select 2"),
            ["select 'it''s; fine', \"a\"\";b\", 'c\\'", "select 2"]
        );
    }

    #[test]
    fn block_comments_do_not_nest() {
        // The first `*/` ends the comment: what follows is code, and its `;` splits.
        assert_eq!(
            parts("select 1 /* a /* b */; select 2"),
            ["select 1 /* a /* b */", "select 2"]
        );
    }

    #[test]
    fn code_is_what_lies_outside_strings_names_and_comments() {
        let script = "select :a, ':b', \":c\" -- :d\n, :e /* :f */";
        let code: Vec<&str> = TrinoDialect
            .code_ranges(script)
            .into_iter()
            .map(|r| &script[r])
            .collect();
        assert_eq!(code, ["select :a, ", ", ", " ", ", :e "]);
    }

    #[test]
    fn names_are_always_quoted() {
        assert_eq!(TrinoDialect.quote_identifier("orders"), "\"orders\"");
        assert_eq!(TrinoDialect.quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
