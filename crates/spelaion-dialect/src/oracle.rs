use spelaion_driver_api::{Dialect, StatementSpan};

use crate::scanner::{self, Region, RegionKind};
use crate::{is_white_space_at, trimmed};

/// Oracle's dialect, as SQL*Plus and SQL Developer read a script (ADR-059):
///
/// - `;` ends a SQL statement and is not sent (Oracle refuses it: ORA-00933, ORA-00911).
/// - A PL/SQL unit is not ended by its `;`s: an anonymous block (`DECLARE`, `BEGIN`), a
///   `CREATE [OR REPLACE] [EDITIONABLE | NONEDITIONABLE]` function, procedure, package,
///   package body, trigger, type, type body, library or Java source, and a query with
///   `WITH FUNCTION` or `WITH PROCEDURE`. It ends at a line holding only `/`, or at the end of
///   the script, and keeps its last `;` (`END;`), which PL/SQL needs.
/// - A line holding only `/` also ends a SQL statement that has no `;`.
/// - Strings are `'…'` with doubled quotes, `N'…'`, and the alternative quoting
///   `q'[…]'` (`Q`, `nq`, `NQ` too), whose delimiter is `[ ]`, `{ }`, `( )`, `< >` or any
///   other character repeated: a `'` inside it does not end it. Names are `"…"`. Comments are
///   `--` and `/* */` (not nested).
pub struct OracleDialect;

impl Dialect for OracleDialect {
    fn name(&self) -> &'static str {
        "oracle"
    }

    /// A client command line (ADR-078) is resolved only by the editor's run: never sent.
    fn refuse_statement(&self, statement: &str) -> Option<spelaion_driver_api::RequestError> {
        crate::refuse_client_command(statement)
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        scanner::code_ranges(OracleScanner::new(script))
    }

    /// Always in double quotes: an unquoted name is folded to upper case by Oracle, so quoting
    /// keeps the name exactly as the catalog has it. A `"` cannot be in an Oracle name; it is
    /// doubled anyway, so a name read from anywhere cannot end the quoted one.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

/// The byte after the string, quoted name or comment that starts at `i`; `None` when none
/// starts there. The scanner's reading ([`opening`]), without the region's kind.
pub fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    opening(script, i).map(|(_, end)| end)
}

/// The string, quoted name or comment that starts at byte `i`, with the byte after it; `None`
/// when none starts there. The scanner's one reading of them: the splitter, `code_ranges` and
/// the PL/SQL check's words all read it.
fn opening(script: &str, i: usize) -> Option<(RegionKind, usize)> {
    let b = script.as_bytes();
    match *b.get(i)? {
        b'\'' => Some((RegionKind::String, skip_string(b, i))),
        b'"' => Some((RegionKind::QuotedName, skip_name(b, i))),
        b'-' if b.get(i + 1) == Some(&b'-') => Some((RegionKind::Comment, line_end(b, i))),
        b'/' if b.get(i + 1) == Some(&b'*') => Some((RegionKind::Comment, block_comment_end(b, i))),
        // `q'…'`, `nq'…'` and `n'…'`, only where a word starts.
        c @ (b'q' | b'Q' | b'n' | b'N') if !(i > 0 && is_ident_byte(b[i - 1])) => {
            let quote = if matches!(c, b'n' | b'N') && matches!(b.get(i + 1), Some(b'q' | b'Q')) {
                i + 2
            } else {
                i + 1
            };
            if b.get(quote) != Some(&b'\'') {
                return None;
            }
            if matches!(b.get(quote - 1), Some(b'q' | b'Q')) {
                Some((RegionKind::String, skip_q_quote(b, quote)))
            } else {
                Some((RegionKind::String, skip_string(b, quote)))
            }
        }
        _ => None,
    }
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'#') || c >= 0x80
}

/// `'…'` with `''` inside; an unterminated one runs to the end.
fn skip_string(b: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if b[i] == b'\'' {
            if b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// `"…"`, with `""` read as part of the name.
fn skip_name(b: &[u8], open: usize) -> usize {
    let mut i = open + 1;
    while i < b.len() {
        if b[i] == b'"' {
            if b.get(i + 1) == Some(&b'"') {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

/// `q'X…X'` from its first `'`: the delimiter is the character after it, closed by its pair
/// for brackets, then `'`.
fn skip_q_quote(b: &[u8], quote: usize) -> usize {
    let Some(&open) = b.get(quote + 1) else {
        return b.len();
    };
    let close = match open {
        b'[' => b']',
        b'{' => b'}',
        b'(' => b')',
        b'<' => b'>',
        other => other,
    };
    let mut i = quote + 2;
    while i + 1 < b.len() {
        if b[i] == close && b[i + 1] == b'\'' {
            return i + 2;
        }
        i += 1;
    }
    b.len()
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |n| i + n)
}

fn block_comment_end(b: &[u8], i: usize) -> usize {
    b[i + 2..]
        .windows(2)
        .position(|w| w == b"*/")
        .map_or(b.len(), |n| i + 2 + n + 2)
}

/// Whether `i` starts a line that holds only `/` (and spaces), returning the line's end.
fn slash_line(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'/') {
        return None;
    }
    let line_start = b[..i]
        .iter()
        .rposition(|&c| c == b'\n')
        .map_or(0, |n| n + 1);
    if !b[line_start..i].iter().all(|c| c.is_ascii_whitespace()) {
        return None;
    }
    let end = line_end(b, i);
    b[i + 1..end]
        .iter()
        .all(|c| c.is_ascii_whitespace())
        .then_some(end)
}

/// The first words of a statement, in upper case, skipping comments: enough to know whether
/// it is a PL/SQL unit.
fn first_words(script: &str, start: usize, count: usize) -> Vec<String> {
    let b = script.as_bytes();
    let mut words = Vec::new();
    let mut i = start;
    while i < b.len() && words.len() < count {
        if let Some((kind, end)) = opening(script, i) {
            // A string or name before the unit's words: not a PL/SQL unit.
            if kind == RegionKind::Comment {
                i = end;
                continue;
            }
            break;
        }
        // White space by the one rule of every splitter (SDB-434): never part of a word.
        if is_white_space_at(script, i) {
            i += 1;
            continue;
        }
        if !is_ident_byte(b[i]) {
            break;
        }
        let word_start = i;
        while i < b.len()
            && is_ident_byte(b[i])
            && !(script.is_char_boundary(i) && is_white_space_at(script, i))
        {
            i += 1;
        }
        words.push(script[word_start..i].to_ascii_uppercase());
    }
    words
}

/// Whether the statement starting at `start` is a PL/SQL unit, ended only by a `/` line.
pub fn is_plsql(script: &str, start: usize) -> bool {
    let words = first_words(script, start, 6);
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    match w.as_slice() {
        ["DECLARE", ..] | ["BEGIN", ..] => true,
        ["WITH", "FUNCTION" | "PROCEDURE", ..] => true,
        ["CREATE", rest @ ..] => {
            let mut rest = rest;
            if let ["OR", "REPLACE", tail @ ..] = rest {
                rest = tail;
            }
            if let ["EDITIONABLE" | "NONEDITIONABLE" | "EDITIONING", tail @ ..] = rest {
                rest = tail;
            }
            matches!(
                rest,
                [
                    "FUNCTION" | "PROCEDURE" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY" | "JAVA",
                    ..
                ]
            )
        }
        _ => false,
    }
}

/// Oracle's lexical scanner (ADR-065): the script as regions, read as SQL*Plus reads it.
/// Strings, names and comments are read by [`opening`]; a line holding only `/` ends any
/// statement (a statement end of its own, code); a `;` ends a statement unless the statement is
/// a PL/SQL unit, which the scanner decides at the statement's first code ([`is_plsql`]). The
/// splitter and `code_ranges` read it.
struct OracleScanner<'a> {
    script: &'a str,
    /// Where the next region starts.
    i: usize,
    /// Where the current statement starts.
    start: usize,
    /// Whether the current statement holds code yet: anything but white space and comments.
    has_code: bool,
    /// The current statement is a PL/SQL unit: only a `/` line or the end of the script ends it.
    plsql: bool,
    /// The end of the script was reached.
    ended: bool,
}

impl<'a> OracleScanner<'a> {
    fn new(script: &'a str) -> Self {
        Self {
            script,
            i: 0,
            start: 0,
            has_code: false,
            plsql: false,
            ended: false,
        }
    }

    /// Ends the current statement at `at`, the next one starting at `next`: the statement, if it
    /// held code.
    fn end_statement(&mut self, at: usize, next: usize) -> Option<StatementSpan> {
        let statement = trimmed(self.script, self.start, at).filter(|_| self.has_code);
        self.start = next;
        self.has_code = false;
        self.plsql = false;
        statement
    }
}

impl Iterator for OracleScanner<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        let script = self.script;
        let b = script.as_bytes();
        let len = b.len();
        let code = self.i;
        while self.i < len {
            let i = self.i;
            if let Some(end) = slash_line(b, i) {
                if code < i {
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..i,
                    });
                }
                let statement = self.end_statement(i, end);
                self.i = end;
                return Some(Region {
                    kind: RegionKind::StatementEnd { statement },
                    range: i..end,
                });
            }
            if !self.has_code
                && let Some(end) = scanner::command_line(script, i)
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
                self.plsql = false;
                return Some(Region {
                    kind: RegionKind::MetaLine { statement },
                    range: i..end,
                });
            }
            let open = opening(script, i);
            let comment = matches!(open, Some((RegionKind::Comment, _)));
            if !self.has_code && !comment && !is_white_space_at(script, i) {
                // The statement's first code: is it a PL/SQL unit?
                self.plsql = is_plsql(script, i);
            }
            if let Some((kind, end)) = open {
                if code < i {
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..i,
                    });
                }
                if !comment {
                    self.has_code = true;
                }
                self.i = end.max(i + 1);
                return Some(Region {
                    kind,
                    range: i..self.i,
                });
            }
            if b[i] == b';' && !self.plsql {
                if code < i {
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..i,
                    });
                }
                let statement = self.end_statement(i, i + 1);
                self.i = i + 1;
                return Some(Region {
                    kind: RegionKind::StatementEnd { statement },
                    range: i..i + 1,
                });
            }
            if !is_white_space_at(script, i) {
                self.has_code = true;
            }
            self.i += 1;
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

fn split(script: &str) -> Vec<StatementSpan> {
    scanner::statements(OracleScanner::new(script))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(script: &str) -> Vec<&str> {
        OracleDialect
            .split_statements(script)
            .iter()
            .map(|s| s.text(script))
            .collect()
    }

    #[test]
    fn sql_statements_end_at_semicolons_which_are_not_sent() {
        assert_eq!(
            statements("select 1 from dual; select 2 from dual;\n\nselect 3 from dual"),
            [
                "select 1 from dual",
                "select 2 from dual",
                "select 3 from dual"
            ]
        );
        assert_eq!(
            statements("  ;; -- nothing\n/* here */ ;"),
            Vec::<&str>::new()
        );
    }

    #[test]
    fn strings_names_and_comments_hide_semicolons_and_slashes() {
        assert_eq!(
            statements(
                "select 'a;b', \"x;y\", 'it''s; fine' from dual; -- c;\nselect /* ; */ 2 from dual"
            ),
            [
                "select 'a;b', \"x;y\", 'it''s; fine' from dual",
                "-- c;\nselect /* ; */ 2 from dual"
            ]
        );
        assert_eq!(
            statements("select '\n/\n' from dual; select 2 from dual"),
            ["select '\n/\n' from dual", "select 2 from dual"]
        );
    }

    #[test]
    fn alternative_quoting_hides_quotes_and_semicolons() {
        for script in [
            "select q'[it's; [here]]' from dual; select 2 from dual",
            "select Q'{it's; here}' from dual; select 2 from dual",
            "select q'(it's; here)' from dual; select 2 from dual",
            "select q'<it's; here>' from dual; select 2 from dual",
            "select q'!it's; here!' from dual; select 2 from dual",
            "select nq'[it's; here]' from dual; select 2 from dual",
            "select N'it''s; here' from dual; select 2 from dual",
        ] {
            let found = statements(script);
            assert_eq!(found.len(), 2, "{script}: {found:?}");
            assert_eq!(found[1], "select 2 from dual", "{script}");
        }
        // `q` inside a word is not a quote.
        assert_eq!(
            statements("select seq'x' from dual; select 2 from dual").len(),
            2
        );
    }

    #[test]
    fn plsql_units_keep_their_semicolons_until_a_slash_line() {
        let script = "begin\n  null;\n  dbms_output.put_line('a;b');\nend;\n/\nselect 1 from dual;\n\
                      declare x number := 1; begin x := x + 1; end;\n  /  \n\
                      create or replace procedure p as begin null; end;\n/\n\
                      create or replace editionable package body k as procedure q is begin null; end; end;\n/\n\
                      CREATE TRIGGER t BEFORE INSERT ON a FOR EACH ROW BEGIN :new.id := 1; END;\n/\n\
                      create type pt as object (a number);\n/\n\
                      with function f return number is begin return 1; end; select f from dual\n/";
        assert_eq!(
            statements(script),
            [
                "begin\n  null;\n  dbms_output.put_line('a;b');\nend;",
                "select 1 from dual",
                "declare x number := 1; begin x := x + 1; end;",
                "create or replace procedure p as begin null; end;",
                "create or replace editionable package body k as procedure q is begin null; end; end;",
                "CREATE TRIGGER t BEFORE INSERT ON a FOR EACH ROW BEGIN :new.id := 1; END;",
                "create type pt as object (a number);",
                "with function f return number is begin return 1; end; select f from dual",
            ]
        );
    }

    #[test]
    fn a_unit_without_a_slash_runs_to_the_end_and_a_slash_ends_plain_sql() {
        assert_eq!(statements("begin\n  null;\nend;"), ["begin\n  null;\nend;"]);
        assert_eq!(
            statements("select 1 from dual\n/\nselect 2 from dual"),
            ["select 1 from dual", "select 2 from dual"]
        );
        // A `/` that is not alone on its line is division.
        assert_eq!(
            statements("select 4 / 2 from dual"),
            ["select 4 / 2 from dual"]
        );
        assert_eq!(
            statements("select 4\n/ 2 from dual"),
            ["select 4\n/ 2 from dual"]
        );
        // A comment before a block does not hide it.
        assert_eq!(
            statements("-- first\nbegin null; end;\n/\nselect 1 from dual"),
            ["-- first\nbegin null; end;", "select 1 from dual"]
        );
    }

    #[test]
    fn create_table_and_other_ddl_are_not_plsql() {
        assert_eq!(
            statements(
                "create table t (a number); create or replace view v as select 1 x from dual; create index i on t (a)"
            ),
            [
                "create table t (a number)",
                "create or replace view v as select 1 x from dual",
                "create index i on t (a)"
            ]
        );
    }

    #[test]
    fn names_are_always_quoted() {
        assert_eq!(OracleDialect.quote_identifier("ORDERS"), "\"ORDERS\"");
        assert_eq!(OracleDialect.quote_identifier("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn code_ranges_leave_out_strings_names_and_comments() {
        let script = "select :a, ':b', \"c:d\", q'[:e]' -- :f\n from dual /* :g */";
        let code: String = OracleDialect
            .code_ranges(script)
            .into_iter()
            .map(|r| &script[r])
            .collect();
        assert!(code.contains(":a"));
        for hidden in [":b", ":d", ":e", ":f", ":g"] {
            assert!(!code.contains(hidden), "{hidden} in {code:?}");
        }
    }
}
