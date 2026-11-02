use spelunk_driver_api::{Dialect, StatementSpan};

/// PostgreSQL dialect.
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn quote_identifier(&self, ident: &str) -> String {
        let simple = ident
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == b'_')
            && ident
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            && !RESERVED.contains(&ident);
        if simple {
            ident.to_string()
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }
}

/// PostgreSQL reserved words that cannot be used unquoted as identifiers.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

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
        match b[i] {
            b'\'' => {
                // E'…' supports backslash escapes.
                let escapes =
                    i > 0 && matches!(b[i - 1], b'E' | b'e') && (i < 2 || !is_ident_byte(b[i - 2]));
                i = skip_quoted(b, i, b'\'', escapes);
                has_code = true;
            }
            b'"' => {
                i = skip_quoted(b, i, b'"', false);
                has_code = true;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p + 1);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // Block comments nest in PostgreSQL.
                let mut depth = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'$' if i == 0 || !is_ident_byte(b[i - 1]) => match dollar_tag(b, i) {
                Some(tag_end) => {
                    let tag = &script[i..=tag_end];
                    i = script[tag_end + 1..]
                        .find(tag)
                        .map_or(b.len(), |p| tag_end + 1 + p + tag.len());
                    has_code = true;
                }
                None => {
                    i += 1;
                    has_code = true;
                }
            },
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

/// Skips a quoted string starting at `i` (the opening quote).
/// A doubled quote is an escaped quote.
fn skip_quoted(b: &[u8], i: usize, quote: u8, backslash_escapes: bool) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if backslash_escapes && b[j] == b'\\' {
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

/// If a `$tag$` (or `$$`) starts at `i`, returns the position of its closing `$`.
/// `$1` is not a tag: it is a parameter.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
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
    fn respects_quotes_and_comments() {
        let script = "select 'a;b', \"c;d\", E'e\\';f' -- g;h\n; /* i; /* j; */ k; */ select 2";
        assert_eq!(
            parts(script),
            [
                "select 'a;b', \"c;d\", E'e\\';f' -- g;h",
                "/* i; /* j; */ k; */ select 2"
            ]
        );
    }

    #[test]
    fn doubled_quotes_do_not_end_strings() {
        assert_eq!(
            parts("select 'it''s; fine'; select 2"),
            ["select 'it''s; fine'", "select 2"]
        );
    }

    #[test]
    fn respects_dollar_quoted_bodies() {
        let script = "create function f() returns int as $body$ begin; return 1; end $body$ language plpgsql; select $$;$$, $1";
        assert_eq!(
            parts(script),
            [
                "create function f() returns int as $body$ begin; return 1; end $body$ language plpgsql",
                "select $$;$$, $1"
            ]
        );
    }

    #[test]
    fn dollar_inside_identifiers_is_not_a_tag() {
        assert_eq!(
            parts("select a$b$ from t; select 2"),
            ["select a$b$ from t", "select 2"]
        );
    }

    #[test]
    fn unterminated_string_keeps_rest_as_one_statement() {
        assert_eq!(parts("select 'abc; select 2"), ["select 'abc; select 2"]);
    }

    #[test]
    fn quotes_identifiers_only_when_needed() {
        let d = PostgresDialect;
        assert_eq!(d.quote_identifier("orders"), "orders");
        assert_eq!(d.quote_identifier("Orders"), "\"Orders\"");
        assert_eq!(d.quote_identifier("user"), "\"user\"");
        assert_eq!(d.quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(d.quote_identifier("1x"), "\"1x\"");
    }
}
