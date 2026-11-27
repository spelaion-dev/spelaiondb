use spelaion_driver_api::{Dialect, StatementSpan};

use crate::scanner::{Region, RegionKind};
use crate::{is_white_space_at, white_space_before};

/// SQL Server (T-SQL) dialect.
///
/// T-SQL does not need `;` between statements and wraps bodies in `BEGIN … END` without any
/// quoting, so the splitter:
/// - ends a batch at a line that is only `GO` (the line is not sent);
/// - splits at `;` only outside quotes, comments and `BEGIN … END` / `CASE … END` blocks;
/// - keeps the rest of the batch as one statement after `CREATE`/`ALTER` of a procedure,
///   function, trigger or view, which must be alone in their batch anyway, and after a
///   `DECLARE`: a variable only lives for its batch, so the statements that use it go with it
///   (statements before the `DECLARE` still split).
///
/// Several statements without `;` between them go to the server as one batch: SQL Server runs
/// them in order and each result set is shown.
pub struct SqlServerDialect;

impl Dialect for SqlServerDialect {
    fn name(&self) -> &'static str {
        "sqlserver"
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

    fn first_word(&self, statement: &str) -> Option<String> {
        Some(first_word(statement))
    }

    fn quote_identifier(&self, ident: &str) -> String {
        let simple = ident
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            && ident
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            && !RESERVED.contains(&ident.to_ascii_lowercase().as_str());
        if simple {
            ident.to_string()
        } else {
            format!("[{}]", ident.replace(']', "]]"))
        }
    }
}

/// T-SQL reserved keywords (they cannot be used unquoted as identifiers).
const RESERVED: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "any",
    "as",
    "asc",
    "authorization",
    "backup",
    "begin",
    "between",
    "break",
    "browse",
    "bulk",
    "by",
    "cascade",
    "case",
    "check",
    "checkpoint",
    "close",
    "clustered",
    "coalesce",
    "collate",
    "column",
    "commit",
    "compute",
    "constraint",
    "contains",
    "containstable",
    "continue",
    "convert",
    "create",
    "cross",
    "current",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "cursor",
    "database",
    "dbcc",
    "deallocate",
    "declare",
    "default",
    "delete",
    "deny",
    "desc",
    "disk",
    "distinct",
    "distributed",
    "double",
    "drop",
    "dump",
    "else",
    "end",
    "errlvl",
    "escape",
    "except",
    "exec",
    "execute",
    "exists",
    "exit",
    "external",
    "fetch",
    "file",
    "fillfactor",
    "for",
    "foreign",
    "freetext",
    "freetexttable",
    "from",
    "full",
    "function",
    "goto",
    "grant",
    "group",
    "having",
    "holdlock",
    "identity",
    "identity_insert",
    "identitycol",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "kill",
    "left",
    "like",
    "lineno",
    "load",
    "merge",
    "national",
    "nocheck",
    "nonclustered",
    "not",
    "null",
    "nullif",
    "of",
    "off",
    "offsets",
    "on",
    "open",
    "opendatasource",
    "openquery",
    "openrowset",
    "openxml",
    "option",
    "or",
    "order",
    "outer",
    "over",
    "percent",
    "pivot",
    "plan",
    "precision",
    "primary",
    "print",
    "proc",
    "procedure",
    "public",
    "raiserror",
    "read",
    "readtext",
    "reconfigure",
    "references",
    "replication",
    "restore",
    "restrict",
    "return",
    "revert",
    "revoke",
    "right",
    "rollback",
    "rowcount",
    "rowguidcol",
    "rule",
    "save",
    "schema",
    "securityaudit",
    "select",
    "semantickeyphrasetable",
    "semanticsimilaritydetailstable",
    "semanticsimilaritytable",
    "session_user",
    "set",
    "setuser",
    "shutdown",
    "some",
    "statistics",
    "system_user",
    "table",
    "tablesample",
    "textsize",
    "then",
    "to",
    "top",
    "tran",
    "transaction",
    "trigger",
    "truncate",
    "try_convert",
    "tsequal",
    "union",
    "unique",
    "unpivot",
    "update",
    "updatetext",
    "use",
    "user",
    "values",
    "varying",
    "view",
    "waitfor",
    "when",
    "where",
    "while",
    "with",
    "within",
    "writetext",
];

/// Statements whose batch has a row count worth reporting when they return no rows: read from
/// the statement's first word, after its comments, as the scanner reads them.
pub fn counts_rows(sql: &str) -> bool {
    matches!(
        first_word(sql).as_str(),
        "insert" | "update" | "delete" | "merge" | "with"
    )
}

/// The first word of `sql` in lower case, read by the scanner (ADR-065): after white space and
/// comments (nested ones too), the letters its first code starts with. Empty when a string, a
/// name or a sign comes first. The guard reads it through `Dialect::first_word`.
pub fn first_word(sql: &str) -> String {
    for region in SqlServerScanner::new(sql) {
        match region.kind {
            // SQL Server has no executable comments: the scanner never gives one.
            RegionKind::Comment | RegionKind::ExecutableComment => {}
            // A `GO` line and a `;` end a statement and are code: read like any other code.
            RegionKind::Code | RegionKind::StatementEnd { .. } | RegionKind::MetaLine { .. } => {
                let rest = sql[region.range].trim_start();
                // White space alone: what follows it may still be a comment.
                if rest.is_empty() {
                    continue;
                }
                return rest
                    .bytes()
                    .take_while(u8::is_ascii_alphabetic)
                    .map(|b| char::from(b.to_ascii_lowercase()))
                    .collect();
            }
            RegionKind::String
            | RegionKind::QuotedName
            | RegionKind::QuotedBody
            | RegionKind::CopyData
            | RegionKind::CopyEnd => return String::new(),
        }
    }
    String::new()
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'#' || c == b'$' || c >= 0x80
}

/// Is the line starting at `start` only `GO` (optionally with a count and a `--` comment)?
/// Returns where the line ends.
fn go_line(b: &[u8], start: usize) -> Option<usize> {
    let end = b[start..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| start + p);
    let line = std::str::from_utf8(&b[start..end]).ok()?;
    let line = line.split("--").next().unwrap_or("").trim();
    let mut words = line.split_whitespace();
    let first = words.next()?;
    let count_ok = words
        .next()
        .is_none_or(|n| n.bytes().all(|c| c.is_ascii_digit()));
    (first.eq_ignore_ascii_case("go") && count_ok && words.next().is_none()).then_some(end)
}

/// Statements the rest of the batch belongs to: those that must be alone in their batch, and
/// `DECLARE`, whose variables (and cursors) only live for the batch, so the statements that use
/// them must go with it.
fn takes_rest_of_batch(words: &[String]) -> bool {
    let mut rest = words.iter().map(String::as_str);
    let Some(first) = rest.next() else {
        return false;
    };
    if first == "declare" {
        return true;
    }
    if first != "create" && first != "alter" {
        return false;
    }
    let mut next = rest.next();
    if first == "create" && next == Some("or") {
        rest.next(); // `alter`
        next = rest.next();
    }
    matches!(
        next,
        Some("procedure" | "proc" | "function" | "trigger" | "view")
    )
}

/// SQL Server's lexical scanner (ADR-065): the script as regions. Strings (`'…'`), names
/// (`"…"`, `[…]`) and comments (`--`, and `/* */`, which nest) are regions of their own. A
/// line that is only `GO` (with a count and a `--` comment allowed) ends the batch: it is a
/// statement end, code, and is never sent; a comment after it is a comment. A `;` ends a
/// statement outside `BEGIN … END` and `CASE … END` blocks, and not in the rest of a batch that
/// a `DECLARE` or a module's `CREATE` / `ALTER` takes. The splitter, `code_ranges` and
/// `first_word` all read it; it holds the current statement's first words and block depth, and
/// no more than the region at hand.
struct SqlServerScanner<'a> {
    script: &'a str,
    /// Where the next region starts.
    i: usize,
    /// Where the current statement starts.
    start: usize,
    /// Whether the current statement holds code yet: a string, a name, a word or any other byte
    /// that is not white space. Comments do not count.
    has_code: bool,
    /// The current statement's first words (up to four), lower case.
    words: Vec<String>,
    /// How deep the scanner is in `BEGIN … END` and `CASE … END` blocks.
    depth: usize,
    /// The rest of the batch belongs to the current statement.
    rest_of_batch: bool,
    /// The position is at the start of a line, where a `GO` line may be.
    at_line_start: bool,
    /// What was found to open at the position while the code before it was given: read
    /// next, without scanning it again.
    pending: Option<(Opening, usize)>,
    /// The end of the script was reached.
    ended: bool,
}

/// What starts at the scanner's position, other than code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    /// A `GO` line, which ends where its line does (before its newline).
    Go {
        line_end: usize,
    },
    Comment {
        line: bool,
    },
    Quoted {
        string: bool,
    },
    /// A client command line (ADR-078), where a statement could begin.
    Command,
    StatementEnd,
}

impl<'a> SqlServerScanner<'a> {
    fn new(script: &'a str) -> Self {
        Self {
            script,
            i: 0,
            start: 0,
            has_code: false,
            words: Vec::new(),
            depth: 0,
            rest_of_batch: false,
            at_line_start: true,
            pending: None,
            ended: false,
        }
    }

    /// What starts at the position, other than code, and the byte after it. A `GO` line is
    /// looked for at the start of each line first, then a quote, a comment and a `;` that ends
    /// a statement. A line that is not a `GO` line is not looked at again.
    fn opening(&mut self) -> Option<(Opening, usize)> {
        let b = self.script.as_bytes();
        let i = self.i;
        if self.at_line_start {
            let indent = b[i..]
                .iter()
                .take_while(|c| **c == b' ' || **c == b'\t')
                .count();
            if let Some(line_end) = go_line(b, i + indent) {
                // A `--` comment after the `GO` is a comment of its own.
                let end = b[i + indent..line_end]
                    .windows(2)
                    .position(|w| w == b"--")
                    .map_or(line_end, |p| i + indent + p);
                return Some((Opening::Go { line_end }, end));
            }
            self.at_line_start = false;
        }
        match b[i] {
            b'\'' => Some((Opening::Quoted { string: true }, skip_quoted(b, i, b'\''))),
            b'"' => Some((Opening::Quoted { string: false }, skip_quoted(b, i, b'"'))),
            b'[' => Some((Opening::Quoted { string: false }, skip_quoted(b, i, b']'))),
            b'-' if b.get(i + 1) == Some(&b'-') => {
                // With its newline, which starts a line where a `GO` may be.
                let end = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p + 1);
                Some((Opening::Comment { line: true }, end))
            }
            // Block comments nest in T-SQL; a `GO` line inside one does not end the batch.
            b'/' if b.get(i + 1) == Some(&b'*') => {
                Some((Opening::Comment { line: false }, block_comment_end(b, i)))
            }
            b';' if self.depth == 0 && !self.rest_of_batch => Some((Opening::StatementEnd, i + 1)),
            b'@' if !self.has_code => {
                crate::scanner::command_line(self.script, i).map(|end| (Opening::Command, end))
            }
            _ => None,
        }
    }

    /// Reads the region that `opening` starts at the position, up to `end`.
    fn take(&mut self, opening: Opening, end: usize) -> Region {
        let range = self.i..end;
        let kind = match opening {
            Opening::Go { line_end } => {
                let statement =
                    crate::trimmed(self.script, self.start, range.start).filter(|_| self.has_code);
                // The next batch starts after the line, which is never sent.
                self.start = line_end;
                self.reset();
                RegionKind::StatementEnd { statement }
            }
            Opening::Comment { line } => {
                if line && self.script.as_bytes()[end - 1] == b'\n' {
                    self.at_line_start = true;
                }
                RegionKind::Comment
            }
            Opening::Command => {
                // A statement of its own, which the session refuses wherever nothing resolves
                // it; its newline is white space of what follows.
                let statement = crate::trimmed(self.script, range.start, range.end);
                self.start = end;
                RegionKind::MetaLine { statement }
            }
            Opening::Quoted { string } => {
                self.has_code = true;
                if string {
                    RegionKind::String
                } else {
                    RegionKind::QuotedName
                }
            }
            Opening::StatementEnd => {
                // MERGE is the one statement T-SQL refuses without its `;`: it keeps it.
                let statement_end = if self.words.first().is_some_and(|w| w == "merge") {
                    end
                } else {
                    range.start
                };
                let statement = crate::trimmed(self.script, self.start, statement_end)
                    .filter(|_| self.has_code);
                self.start = end;
                self.has_code = false;
                self.words.clear();
                RegionKind::StatementEnd { statement }
            }
        };
        self.i = end;
        Region { kind, range }
    }

    /// A batch ends: the next statement starts with nothing read.
    fn reset(&mut self) {
        self.has_code = false;
        self.words.clear();
        self.depth = 0;
        self.rest_of_batch = false;
    }

    /// Reads code at the position: a newline (which starts a line), a whole word (which may
    /// open or close a block, or make the rest of the batch the statement's), or another byte,
    /// which is code unless it is white space.
    fn code_step(&mut self) {
        let script = self.script;
        let b = script.as_bytes();
        let i = self.i;
        let c = b[i];
        if c == b'\n' {
            self.at_line_start = true;
            self.i += 1;
            return;
        }
        // White space outside ASCII is no part of a word, and a word may follow it (SDB-434).
        // An ASCII word byte is never white space: only a byte outside ASCII is looked up.
        if is_word_byte(c)
            && (i == 0 || !is_word_byte(b[i - 1]) || white_space_before(script, i))
            && (c.is_ascii() || !is_white_space_at(script, i))
        {
            let end = (i..b.len())
                .find(|&j| {
                    !is_word_byte(b[j])
                        || (!b[j].is_ascii()
                            && script.is_char_boundary(j)
                            && is_white_space_at(script, j))
                })
                .unwrap_or(b.len());
            let word = &script[i..end];
            self.has_code = true;
            if self.words.len() < 4 {
                self.words.push(word.to_ascii_lowercase());
                if takes_rest_of_batch(&self.words) {
                    self.rest_of_batch = true;
                }
            }
            if word.eq_ignore_ascii_case("begin") {
                // `BEGIN TRAN`, `BEGIN DISTRIBUTED`, `BEGIN DIALOG` are not blocks.
                let next = next_word(script, end);
                if !matches!(
                    next.as_str(),
                    "tran" | "transaction" | "distributed" | "dialog" | "conversation"
                ) {
                    self.depth += 1;
                }
            } else if word.eq_ignore_ascii_case("case") {
                self.depth += 1;
            } else if word.eq_ignore_ascii_case("end") && next_word(script, end) != "conversation" {
                // `END CONVERSATION` is a statement, not the end of a block.
                self.depth = self.depth.saturating_sub(1);
            }
            self.i = end;
            return;
        }
        if !is_white_space_at(script, i) {
            self.has_code = true;
        }
        self.i += 1;
    }
}

impl Iterator for SqlServerScanner<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        if let Some((opening, end)) = self.pending.take() {
            return Some(self.take(opening, end));
        }
        let len = self.script.len();
        let bytes = self.script.as_bytes();
        let code = self.i;
        while self.i < len {
            // The fast path: an ASCII byte that opens nothing, starts no word and ends no line
            // is code, and only says whether the statement holds code yet.
            let c = bytes[self.i];
            if !self.at_line_start && c.is_ascii() && !may_open(c) && !is_word_byte(c) {
                // ASCII white space, as `is_white_space_at` reads it (`char::is_whitespace`).
                if !matches!(c, 0x09..=0x0d | 0x20) {
                    self.has_code = true;
                }
                self.i += 1;
                continue;
            }
            if let Some((opening, end)) = self.opening() {
                if code < self.i {
                    self.pending = Some((opening, end));
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..self.i,
                    });
                }
                return Some(self.take(opening, end));
            }
            self.code_step();
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
        let statement = crate::trimmed(self.script, self.start, len).filter(|_| self.has_code)?;
        Some(Region {
            kind: RegionKind::StatementEnd {
                statement: Some(statement),
            },
            range: len..len,
        })
    }
}

/// Whether a region or a line may start at ASCII byte `b`: a quote, a comment, a `;`, or a
/// newline (after which a `GO` line may come).
fn may_open(b: u8) -> bool {
    matches!(b, b'\'' | b'"' | b'[' | b'-' | b'/' | b';' | b'\n')
}

/// The statements of `script`, read by the scanner.
fn split(script: &str) -> Vec<StatementSpan> {
    SqlServerScanner::new(script)
        .filter_map(|region| region.kind.statement())
        .collect()
}

/// What lies outside strings, names and comments, read by the scanner: a `GO` line is code.
fn code_ranges(script: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for region in SqlServerScanner::new(script) {
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

/// The next word after `from`, lowercased (empty at the end).
fn next_word(script: &str, from: usize) -> String {
    script[from..]
        .trim_start()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The byte after a block comment that starts at `i` (`/*`); block comments nest in T-SQL.
fn block_comment_end(b: &[u8], mut i: usize) -> usize {
    let mut nested = 0usize;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            nested += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            nested -= 1;
            i += 2;
            if nested == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// Skips a quoted text starting at `i` (the opening character) up to its `close`; a doubled
/// closing character is an escaped one.
fn skip_quoted(b: &[u8], i: usize, close: u8) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == close {
            if b.get(j + 1) == Some(&close) {
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

    #[test]
    fn splits_at_semicolons_and_go_lines() {
        assert_eq!(
            parts("select 1; select 2\nGO\nselect 3\n  go 2 -- twice\nselect 4"),
            ["select 1", "select 2", "select 3", "select 4"]
        );
        // `go` inside a line is not a separator.
        assert_eq!(parts("select 1 as go"), ["select 1 as go"]);
    }

    #[test]
    fn merge_keeps_the_semicolon_it_requires() {
        assert_eq!(
            parts("merge into t using s on t.id = s.id when matched then delete; select 1;"),
            [
                "merge into t using s on t.id = s.id when matched then delete;",
                "select 1"
            ]
        );
        // A MERGE without one is sent as written: SQL Server says what is missing.
        assert_eq!(
            parts("merge into t using s on 1 = 1 when matched then delete"),
            ["merge into t using s on 1 = 1 when matched then delete"]
        );
    }

    #[test]
    fn respects_quotes_brackets_and_comments() {
        let script = "select 'a;b', \"c;d\", [e;]]f] -- g;h\n; /* i; /* j; */ k; */ select 2";
        assert_eq!(
            parts(script),
            [
                "select 'a;b', \"c;d\", [e;]]f] -- g;h",
                "/* i; /* j; */ k; */ select 2"
            ]
        );
    }

    #[test]
    fn blocks_are_not_split() {
        let script = "if 1 = 1 begin print 'a'; print 'b'; end; \
                      begin try select 1/0; end try begin catch select 2; end catch; \
                      select case when 1 = 1 then 'x' end; begin tran; commit";
        assert_eq!(
            parts(script),
            [
                "if 1 = 1 begin print 'a'; print 'b'; end",
                "begin try select 1/0; end try begin catch select 2; end catch",
                "select case when 1 = 1 then 'x' end",
                "begin tran",
                "commit"
            ]
        );
    }

    #[test]
    fn procedures_keep_the_rest_of_their_batch() {
        let script =
            "create or alter procedure p as\nset nocount on;\nselect 1;\nselect 2;\ngo\nexec p";
        assert_eq!(
            parts(script),
            [
                "create or alter procedure p as\nset nocount on;\nselect 1;\nselect 2;",
                "exec p"
            ]
        );
        assert_eq!(
            parts("create view v as select 1 as a; select 2"),
            ["create view v as select 1 as a; select 2"]
        );
        // Other CREATE statements split as usual.
        assert_eq!(
            parts("create table t (a int); select 2"),
            ["create table t (a int)", "select 2"]
        );
    }

    #[test]
    fn variables_keep_the_rest_of_their_batch() {
        // A variable lives for one batch: the statements that use it go with its DECLARE.
        assert_eq!(
            parts("DECLARE @id int = 5;\nSELECT * FROM orders WHERE id = @id;\nSET @id = 6;"),
            ["DECLARE @id int = 5;\nSELECT * FROM orders WHERE id = @id;\nSET @id = 6;"]
        );
        // A table variable too, and statements before the DECLARE still split.
        assert_eq!(
            parts(
                "select 1;\nset nocount on;\n-- the rows\ndeclare @t table (id int);\ninsert into @t values (1);\nselect * from @t;"
            ),
            [
                "select 1",
                "set nocount on",
                "-- the rows\ndeclare @t table (id int);\ninsert into @t values (1);\nselect * from @t;"
            ]
        );
        // A GO line ends it: the next batch splits as usual.
        assert_eq!(
            parts("declare @x int = 1; select @x;\nGO\nselect 2; select 3"),
            ["declare @x int = 1; select @x;", "select 2", "select 3"]
        );
        // A DECLARE inside a block is the block's: the block ends at its `;` as before.
        assert_eq!(
            parts("if 1 = 1 begin declare @y int; set @y = 1; end; select 4"),
            ["if 1 = 1 begin declare @y int; set @y = 1; end", "select 4"]
        );
        // `declare` as a later word does not count.
        assert_eq!(
            parts("select 1 as [declare]; select 2"),
            ["select 1 as [declare]", "select 2"]
        );
    }

    #[test]
    fn row_counts_only_for_writes() {
        assert!(counts_rows("-- note\n/* x */ UPDATE t set a = 1"));
        assert!(counts_rows("with x as (select 1 as a) delete from t"));
        assert!(!counts_rows("create table t (a int)"));
        assert!(!counts_rows("exec p"));
    }

    #[test]
    fn code_ranges_read_go_lines_as_the_splitter_does() {
        let d = SqlServerDialect;
        let code = |script: &str| -> String {
            d.code_ranges(script)
                .into_iter()
                .map(|r| &script[r])
                .collect()
        };
        // A `GO` line is code; a `--` comment after it is a comment, with its newline.
        assert_eq!(
            code("select :a\nGO -- :b\nselect :c"),
            "select :a\nGO select :c"
        );
        // Strings, names and nested comments are not code; a `GO` inside one ends nothing.
        assert_eq!(
            code("select ':x', [:y], /* :z /* \nGO\n */ */ :w"),
            "select , ,  :w"
        );
        assert_eq!(
            parts("select 1 /* \nGO\n */; select 2"),
            ["select 1 /* \nGO\n */", "select 2"]
        );
    }

    #[test]
    fn first_word_reads_comments_as_the_scanner_does() {
        assert_eq!(
            first_word("  -- note\n/* a /* b */ c */ DROP table t"),
            "drop"
        );
        assert_eq!(first_word("\u{a0}\u{3000}Truncate table t"), "truncate");
        assert_eq!(first_word("[drop] table t"), "");
        assert_eq!(first_word("'drop'"), "");
        assert_eq!(first_word("/* never closed drop"), "");
        assert_eq!(first_word("GO\ndrop table t"), "go");
        assert_eq!(first_word(""), "");
        // The guard reads it through the dialect.
        assert_eq!(
            SqlServerDialect
                .first_word("/* x */ delete from t")
                .as_deref(),
            Some("delete")
        );
        // A nested comment is passed whole before the row count is decided.
        assert!(counts_rows("/* a /* b */ c */ update t set a = 1"));
    }

    #[test]
    fn quotes_identifiers_only_when_needed() {
        let d = SqlServerDialect;
        assert_eq!(d.quote_identifier("orders"), "orders");
        assert_eq!(d.quote_identifier("Orders"), "Orders");
        assert_eq!(d.quote_identifier("user"), "[user]");
        assert_eq!(d.quote_identifier("order lines"), "[order lines]");
        assert_eq!(d.quote_identifier("a]b"), "[a]]b]");
    }
}
