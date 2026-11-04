use spelaion_driver_api::{Dialect, StatementSpan};

/// MySQL/MariaDB dialect.
pub struct MysqlDialect;

impl Dialect for MysqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn quote_identifier(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }
}

/// First word of a statement, after comments (to recognise its kind).
pub(crate) fn first_word(sql: &str) -> &str {
    let bytes = sql.as_bytes();
    let mut i = 0;
    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if let Some(end) = comment_end(bytes, i) {
            i = end;
        } else {
            break;
        }
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    &sql[start..i]
}

/// If a comment starts at `i`, the byte after it. `-- ` needs a space (or end of line) after
/// the dashes, as in MySQL. `/*! … */` (versioned code) is also skipped for splitting.
fn comment_end(bytes: &[u8], i: usize) -> Option<usize> {
    let rest = &bytes[i.min(bytes.len())..];
    let line_end = |from: usize| {
        bytes[from..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |p| from + p + 1)
    };
    if rest.starts_with(b"#") {
        return Some(line_end(i));
    }
    if rest.starts_with(b"--") && rest.get(2).is_none_or(|b| b.is_ascii_whitespace()) {
        return Some(line_end(i));
    }
    if rest.starts_with(b"/*") {
        return Some(
            bytes[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(bytes.len(), |p| i + 2 + p + 2),
        );
    }
    None
}

/// Splits on the current delimiter (`;` by default), respecting quotes (`'…'`, `"…"` with
/// backslash escapes, `` `…` ``) and comments. `DELIMITER $$` lines, as in the `mysql`
/// client, change the delimiter for procedures and triggers and are not statements.
fn split(script: &str) -> Vec<StatementSpan> {
    let bytes = script.as_bytes();
    let mut spans = Vec::new();
    let mut delimiter = String::from(";");
    let mut start = 0;
    let mut i = 0;
    let mut has_code = false;
    let push = |spans: &mut Vec<StatementSpan>, from: usize, to: usize, has_code: bool| {
        let text = &script[from..to];
        let trimmed_start = from + (text.len() - text.trim_start().len());
        let trimmed_end = from + text.trim_end().len();
        if has_code && trimmed_end > trimmed_start {
            spans.push(StatementSpan {
                start: trimmed_start,
                end: trimmed_end,
            });
        }
    };
    while i < bytes.len() {
        // `DELIMITER x` is only recognised where a statement would start.
        if !has_code && at_line_start(bytes, i) && starts_with_ci(&bytes[i..], b"delimiter") {
            let after = i + "delimiter".len();
            if bytes.get(after).is_some_and(|b| *b == b' ' || *b == b'\t') {
                let line_end = bytes[after..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |p| after + p);
                let new = script[after..line_end].trim();
                if !new.is_empty() {
                    delimiter = new.to_string();
                }
                i = (line_end + 1).min(bytes.len());
                start = i;
                continue;
            }
        }
        if let Some(end) = comment_end(bytes, i) {
            i = end;
            continue;
        }
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => {
                has_code = true;
                i = skip_quoted(bytes, i, quote);
                continue;
            }
            b if b.is_ascii_whitespace() => {}
            _ => {
                if bytes[i..].starts_with(delimiter.as_bytes()) {
                    push(&mut spans, start, i, has_code);
                    i += delimiter.len();
                    start = i;
                    has_code = false;
                    continue;
                }
                has_code = true;
            }
        }
        i += 1;
    }
    push(&mut spans, start, bytes.len(), has_code);
    spans
}

fn at_line_start(bytes: &[u8], i: usize) -> bool {
    bytes[..i]
        .iter()
        .rev()
        .take_while(|&&b| b != b'\n')
        .all(|b| b.is_ascii_whitespace())
}

fn starts_with_ci(bytes: &[u8], word: &[u8]) -> bool {
    bytes.len() >= word.len() && bytes[..word.len()].eq_ignore_ascii_case(word)
}

/// Byte after the closing quote. A doubled quote is an escape; inside `'` and `"` so is a
/// backslash (MySQL's default `sql_mode`).
fn skip_quoted(bytes: &[u8], open: usize, quote: u8) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if quote != b'`' => i += 2,
            b if b == quote => {
                if bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                } else {
                    return i + 1;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(script: &str) -> Vec<&str> {
        split(script).iter().map(|s| s.text(script)).collect()
    }

    #[test]
    fn splits_respecting_quotes_and_comments() {
        assert_eq!(
            parts(
                "select 'a;b', \"c;d\", `e;f`; # x;y\nselect 'it''s; \\' ok';; /* z; */ -- w;\nselect 3"
            ),
            [
                "select 'a;b', \"c;d\", `e;f`",
                // Leading comments stay with their statement, as in the other dialects.
                "# x;y\nselect 'it''s; \\' ok'",
                "/* z; */ -- w;\nselect 3"
            ]
        );
        // `--` without a space is not a comment in MySQL: `1--1` is arithmetic.
        assert_eq!(parts("select 1--1; select 2"), ["select 1--1", "select 2"]);
        assert!(parts("  -- only a comment\n# another\n/* and */ ;").is_empty());
    }

    #[test]
    fn delimiter_lines_change_the_delimiter() {
        let script = "DELIMITER $$\ncreate procedure p() begin select 1; select 2; end$$\nDELIMITER ;\ncall p();\nselect 3";
        assert_eq!(
            parts(script),
            [
                "create procedure p() begin select 1; select 2; end",
                "call p()",
                "select 3"
            ]
        );
    }

    #[test]
    fn first_word_skips_comments() {
        assert_eq!(first_word("  /* x */ -- y\n# z\nUPDATE t"), "UPDATE");
        assert_eq!(first_word("select 1"), "select");
    }

    #[test]
    fn identifiers_are_quoted_with_backticks() {
        assert_eq!(MysqlDialect.quote_identifier("we`ird"), "`we``ird`");
    }
}
