use spelaion_driver_api::{Dialect, StatementSpan};

/// SQLite dialect.
pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        spelaion_driver_api::code_ranges(script, |script, i| {
            match skip_quote_or_comment(script, i)? {
                Skipped::Quoted(end) | Skipped::Comment(end) => Some(end),
            }
        })
    }

    fn quote_identifier(&self, ident: &str) -> String {
        let simple = ident
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            && ident
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            && !RESERVED.iter().any(|k| k.eq_ignore_ascii_case(ident));
        if simple {
            ident.to_string()
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }
}

/// Words that cannot be used unquoted as an identifier (common subset).
const RESERVED: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "as",
    "autoincrement",
    "between",
    "case",
    "check",
    "collate",
    "commit",
    "constraint",
    "create",
    "default",
    "deferrable",
    "delete",
    "distinct",
    "drop",
    "else",
    "escape",
    "except",
    "exists",
    "foreign",
    "from",
    "group",
    "having",
    "if",
    "in",
    "index",
    "insert",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "limit",
    "not",
    "notnull",
    "null",
    "on",
    "or",
    "order",
    "primary",
    "references",
    "select",
    "set",
    "table",
    "then",
    "to",
    "transaction",
    "union",
    "unique",
    "update",
    "using",
    "values",
    "when",
    "where",
];

/// Splits respecting quotes (`'…'`, `"…"`, `` `…` ``, `[…]`), comments and the
/// `BEGIN … END` body of `CREATE TRIGGER`, which contains `;` (and may have `CASE … END`).
fn split(script: &str) -> Vec<StatementSpan> {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    // First words of the statement, to detect CREATE [TEMP] TRIGGER.
    let mut words: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut i = 0;

    while i < b.len() {
        match skip_quote_or_comment(script, i) {
            Some(Skipped::Quoted(end)) => {
                i = end;
                has_code = true;
                continue;
            }
            Some(Skipped::Comment(end)) => {
                i = end;
                continue;
            }
            None => {}
        }
        let c = b[i];
        match c {
            b';' if depth <= 0 => {
                if has_code {
                    spans.push(trimmed(script, start, i));
                }
                i += 1;
                start = i;
                has_code = false;
                words.clear();
                depth = 0;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' => {
                let end = b[i..]
                    .iter()
                    .position(|x| !(x.is_ascii_alphanumeric() || *x == b'_'))
                    .map_or(b.len(), |p| i + p);
                let word = script[i..end].to_ascii_lowercase();
                if is_trigger(&words) {
                    match word.as_str() {
                        "begin" | "case" => depth += 1,
                        "end" => depth -= 1,
                        _ => {}
                    }
                }
                if words.len() < 4 {
                    words.push(word);
                }
                has_code = true;
                i = end;
            }
            _ => {
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

/// A quoted string or identifier, or a comment, with the offset just past it (the end of the
/// text when it is not closed).
pub(crate) enum Skipped {
    /// `'…'`, `"…"`, `` `…` `` or `[…]`: part of the statement.
    Quoted(usize),
    /// `--…` up to and with its line break, or `/*…*/`.
    Comment(usize),
}

/// What starts at byte `i` of `script`, read as the splitter reads it.
pub(crate) fn skip_quote_or_comment(script: &str, i: usize) -> Option<Skipped> {
    let b = script.as_bytes();
    match *b.get(i)? {
        c @ (b'\'' | b'"' | b'`') => Some(Skipped::Quoted(skip_until(b, i + 1, c, true))),
        b'[' => Some(Skipped::Quoted(skip_until(b, i + 1, b']', false))),
        b'-' if b.get(i + 1) == Some(&b'-') => Some(Skipped::Comment(
            b[i..]
                .iter()
                .position(|&x| x == b'\n')
                .map_or(b.len(), |p| i + p + 1),
        )),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(Skipped::Comment(
            script[i + 2..]
                .find("*/")
                .map_or(b.len(), |p| i + 2 + p + 2),
        )),
        _ => None,
    }
}

/// The offsets of the lines of `sql` that start outside a quote or a comment (the first
/// line included).
pub(crate) fn code_line_starts(sql: &str) -> Vec<usize> {
    let b = sql.as_bytes();
    let mut starts = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if i == 0 || b[i - 1] == b'\n' {
            starts.push(i);
        }
        i = match skip_quote_or_comment(sql, i) {
            Some(Skipped::Quoted(end) | Skipped::Comment(end)) => end,
            None => i + 1,
        };
    }
    starts
}

fn is_trigger(words: &[String]) -> bool {
    match words {
        [create, trigger, ..] if create == "create" && trigger == "trigger" => true,
        [create, temp, trigger, ..]
            if create == "create"
                && (temp == "temp" || temp == "temporary")
                && trigger == "trigger" =>
        {
            true
        }
        _ => false,
    }
}

/// Advances past the closing `end`; if `doubled`, two closings in a row are an escape.
fn skip_until(b: &[u8], mut j: usize, end: u8, doubled: bool) -> usize {
    while j < b.len() {
        if b[j] == end {
            if doubled && b.get(j + 1) == Some(&end) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
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

    #[test]
    fn splits_and_respects_quotes() {
        assert_eq!(
            parts("select 'a;b', \"c;d\", `e;f`, [g;h]; -- x;y\nselect 2;; /* z; */"),
            ["select 'a;b', \"c;d\", `e;f`, [g;h]", "-- x;y\nselect 2"]
        );
    }

    #[test]
    fn keeps_trigger_bodies_together() {
        let script = "create temp trigger t after insert on a begin
                        insert into log values (case when new.x > 0 then 'pos' else 'neg' end);
                        update a set y = 1;
                      end; select 1";
        let p = parts(script);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].ends_with("end"));
        assert_eq!(p[1], "select 1");
    }

    #[test]
    fn lines_inside_quotes_and_comments_are_not_code() {
        let sql = "select 'a\nb', \"c\nd\", [e\nf], `g\nh` -- i\nj /* k\nl */\nm";
        let starts: Vec<&str> = code_line_starts(sql)
            .into_iter()
            .map(|s| &sql[s..s + 1])
            .collect();
        assert_eq!(starts, ["s", "j", "m"]);
        assert_eq!(code_line_starts("'open\n.x"), [0]);
        assert!(code_line_starts("").is_empty());
    }

    #[test]
    fn begin_transaction_is_not_a_trigger() {
        assert_eq!(
            parts("begin; insert into t values (1); commit"),
            ["begin", "insert into t values (1)", "commit"]
        );
    }

    #[test]
    fn quotes_identifiers_only_when_needed() {
        let d = SqliteDialect;
        assert_eq!(d.quote_identifier("main"), "main");
        assert_eq!(d.quote_identifier("Orders"), "Orders");
        assert_eq!(d.quote_identifier("order"), "\"order\"");
        assert_eq!(d.quote_identifier("my db"), "\"my db\"");
    }
}
