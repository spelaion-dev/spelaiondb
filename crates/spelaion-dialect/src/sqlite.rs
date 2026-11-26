use spelaion_driver_api::{Dialect, StatementSpan};

use crate::scanner::{Region, RegionKind};
use crate::{is_white_space_at, trimmed};

/// SQLite dialect.
pub struct SqliteDialect;

impl Dialect for SqliteDialect {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    /// A client command line (ADR-078) is resolved only by the editor's run: never sent.
    fn refuse_statement(&self, statement: &str) -> Option<spelaion_driver_api::RequestError> {
        crate::refuse_client_command(statement)
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        code_ranges(script)
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

/// SQLite's lexical scanner (ADR-065): the script as regions. Quotes (`'…'`, `"…"`, `` `…` ``,
/// `[…]`) and comments (`--…`, `/*…*/`) are read by [`skip_quote_or_comment`], the one reading of
/// them; a statement ends at a `;` outside them, except inside the `BEGIN … END` body of a
/// `CREATE [TEMP] TRIGGER` (which may hold `CASE … END` too), whose depth the scanner keeps from
/// the statement's first words. The splitter and `code_ranges` read it; it holds no more than
/// the statement's first words and that depth.
struct SqliteScanner<'a> {
    script: &'a str,
    /// Where the next region starts.
    i: usize,
    /// Where the current statement starts.
    start: usize,
    /// Whether the current statement holds code yet: a quote or any byte that is not white
    /// space. Comments do not count.
    has_code: bool,
    /// The statement's first words, lower case, to recognise `CREATE [TEMP] TRIGGER`.
    words: Vec<String>,
    /// `BEGIN`/`CASE` minus `END` inside a trigger: a `;` ends the statement only at zero or
    /// below.
    depth: i32,
    /// The end of the script was reached.
    ended: bool,
}

impl<'a> SqliteScanner<'a> {
    fn new(script: &'a str) -> Self {
        Self {
            script,
            i: 0,
            start: 0,
            has_code: false,
            words: Vec::new(),
            depth: 0,
            ended: false,
        }
    }

    /// Reads one word of code (letters, digits and `_`, from a letter or `_`) at the position.
    fn word(&mut self) {
        let b = self.script.as_bytes();
        let i = self.i;
        let end = b[i..]
            .iter()
            .position(|x| !(x.is_ascii_alphanumeric() || *x == b'_'))
            .map_or(b.len(), |p| i + p);
        let word = self.script[i..end].to_ascii_lowercase();
        if is_trigger(&self.words) {
            match word.as_str() {
                "begin" | "case" => self.depth += 1,
                "end" => self.depth -= 1,
                _ => {}
            }
        }
        if self.words.len() < 4 {
            self.words.push(word);
        }
        self.has_code = true;
        self.i = end;
    }
}

impl Iterator for SqliteScanner<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        let script = self.script;
        let b = script.as_bytes();
        let len = b.len();
        let code = self.i;
        while self.i < len {
            let i = self.i;
            if !self.has_code
                && let Some(end) = crate::scanner::command_line(script, i)
            {
                if code < i {
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..i,
                    });
                }
                // A client command line (ADR-078): a statement of its own, which the session
                // refuses wherever nothing resolves it. Its newline is white space of what
                // follows.
                let statement = trimmed(script, i, end);
                self.i = end;
                self.start = end;
                return Some(Region {
                    kind: RegionKind::MetaLine { statement },
                    range: i..end,
                });
            }
            if let Some(skipped) = skip_quote_or_comment(script, i) {
                if code < i {
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..i,
                    });
                }
                let kind = match skipped {
                    Skipped::Quoted(_) => {
                        self.has_code = true;
                        if b[i] == b'\'' {
                            RegionKind::String
                        } else {
                            RegionKind::QuotedName
                        }
                    }
                    Skipped::Comment(_) => RegionKind::Comment,
                };
                let (Skipped::Quoted(end) | Skipped::Comment(end)) = skipped;
                self.i = end;
                return Some(Region {
                    kind,
                    range: i..end,
                });
            }
            match b[i] {
                b';' if self.depth <= 0 => {
                    if code < i {
                        return Some(Region {
                            kind: RegionKind::Code,
                            range: code..i,
                        });
                    }
                    let statement = trimmed(script, self.start, i).filter(|_| self.has_code);
                    self.i = i + 1;
                    self.start = i + 1;
                    self.has_code = false;
                    self.words.clear();
                    self.depth = 0;
                    return Some(Region {
                        kind: RegionKind::StatementEnd { statement },
                        range: i..i + 1,
                    });
                }
                c if c.is_ascii_alphabetic() || c == b'_' => self.word(),
                _ => {
                    if !is_white_space_at(script, i) {
                        self.has_code = true;
                    }
                    self.i += 1;
                }
            }
        }
        if code < len {
            return Some(Region {
                kind: RegionKind::Code,
                range: code..len,
            });
        }
        if self.ended {
            return None;
        }
        self.ended = true;
        // The end of the script ends the last statement.
        let statement = trimmed(script, self.start, len).filter(|_| self.has_code)?;
        Some(Region {
            kind: RegionKind::StatementEnd {
                statement: Some(statement),
            },
            range: len..len,
        })
    }
}

/// The statements of `script`, read by the scanner.
fn split(script: &str) -> Vec<StatementSpan> {
    SqliteScanner::new(script)
        .filter_map(|region| region.kind.statement())
        .collect()
}

/// What lies outside strings, names and comments, read by the scanner.
fn code_ranges(script: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for region in SqliteScanner::new(script) {
        if !region.kind.is_code() || region.range.is_empty() {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end >= region.range.start => {
                last.end = last.end.max(region.range.end);
            }
            _ => ranges.push(region.range),
        }
    }
    ranges
}

/// A quoted string or identifier, or a comment, with the offset just past it (the end of the
/// text when it is not closed).
enum Skipped {
    /// `'…'`, `"…"`, `` `…` `` or `[…]`: part of the statement.
    Quoted(usize),
    /// `--…` up to and with its line break, or `/*…*/`.
    Comment(usize),
}

/// What starts at byte `i` of `script`: the scanner's one reading of quotes and comments.
fn skip_quote_or_comment(script: &str, i: usize) -> Option<Skipped> {
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

/// The offsets of the lines of `sql` that start outside a quote or a comment (the first line
/// included), read from the scanner's regions: a line that starts where a quote or a comment
/// starts counts, one that starts inside it does not. The remote SQLite check reads it to keep
/// a line the `sqlite3` shell would take for one of its commands away from the shell.
pub fn code_line_starts(sql: &str) -> Vec<usize> {
    let b = sql.as_bytes();
    let starts_line = |p: usize| p == 0 || b[p - 1] == b'\n';
    let mut starts = Vec::new();
    for region in SqliteScanner::new(sql) {
        if region.kind.is_code() {
            starts.extend(region.range.filter(|&p| starts_line(p)));
        } else if starts_line(region.range.start) {
            starts.push(region.range.start);
        }
    }
    starts
}

/// Where the statement's first word is: past the white space and the comments that lead it,
/// read from the scanner's regions; the end of `sql` when nothing else is in it. The remote
/// SQLite check looks for the `sqlite3` shell's own lines there.
pub fn statement_start(sql: &str) -> usize {
    for region in SqliteScanner::new(sql) {
        match region.kind {
            RegionKind::Comment => {}
            RegionKind::Code => {
                let text = &sql[region.range.clone()];
                let rest = text.trim_start();
                // White space alone: what follows it may still be a comment.
                if !rest.is_empty() {
                    return region.range.end - rest.len();
                }
            }
            RegionKind::String
            | RegionKind::QuotedName
            | RegionKind::ExecutableComment
            | RegionKind::QuotedBody
            | RegionKind::MetaLine { .. }
            | RegionKind::CopyData
            | RegionKind::CopyEnd
            | RegionKind::StatementEnd { .. } => return region.range.start,
        }
    }
    sql.len()
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
