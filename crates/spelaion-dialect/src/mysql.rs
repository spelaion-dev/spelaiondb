use std::sync::Arc;

use spelaion_driver_api::{Dialect, StatementSpan};

use crate::scanner::{Region, RegionKind};

/// MySQL/MariaDB dialect, as the default `sql_mode` reads text.
pub struct MysqlDialect;

/// How a session's `sql_mode` reads quoted text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Quoting {
    /// `\` escapes the next character inside `'…'` (and `"…"` when it is a string): off under
    /// `NO_BACKSLASH_ESCAPES`.
    backslash_escapes: bool,
    /// `"…"` is a string: off under `ANSI_QUOTES`, where it is a name.
    double_quoted_strings: bool,
}

impl Quoting {
    const DEFAULT: Quoting = Quoting {
        backslash_escapes: true,
        double_quoted_strings: true,
    };

    /// From the server's comma-separated `sql_mode` (`ANSI` includes `ANSI_QUOTES`).
    fn of(sql_mode: &str) -> Quoting {
        let has = |name: &str| {
            sql_mode
                .split(',')
                .any(|mode| mode.trim().eq_ignore_ascii_case(name))
        };
        Quoting {
            backslash_escapes: !has("NO_BACKSLASH_ESCAPES"),
            double_quoted_strings: !(has("ANSI_QUOTES") || has("ANSI")),
        }
    }

    /// The dialect for `sql_mode`, which this quoting was read from.
    fn dialect(self, sql_mode: &str) -> Arc<dyn Dialect> {
        if self == Quoting::DEFAULT {
            Arc::new(MysqlDialect)
        } else {
            Arc::new(MysqlModeDialect {
                quoting: self,
                sql_mode: sql_mode.to_string(),
            })
        }
    }
}

impl Dialect for MysqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    /// A client command line (ADR-078) is resolved only by the editor's run: never sent.
    fn refuse_statement(&self, statement: &str) -> Option<spelaion_driver_api::RequestError> {
        crate::refuse_client_command(statement)
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script, Quoting::DEFAULT)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        code_ranges(script, Quoting::DEFAULT)
    }

    fn quote_identifier(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn with_sql_mode(&self, sql_mode: &str) -> Option<Arc<dyn Dialect>> {
        Some(Quoting::of(sql_mode).dialect(sql_mode))
    }
}

/// The dialect of a session whose `sql_mode` reads quoted text another way
/// (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`).
struct MysqlModeDialect {
    quoting: Quoting,
    sql_mode: String,
}

impl Dialect for MysqlModeDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    /// A client command line (ADR-078) is resolved only by the editor's run: never sent.
    fn refuse_statement(&self, statement: &str) -> Option<spelaion_driver_api::RequestError> {
        crate::refuse_client_command(statement)
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script, self.quoting)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        code_ranges(script, self.quoting)
    }

    fn quote_identifier(&self, ident: &str) -> String {
        MysqlDialect.quote_identifier(ident)
    }

    fn with_sql_mode(&self, sql_mode: &str) -> Option<Arc<dyn Dialect>> {
        Some(Quoting::of(sql_mode).dialect(sql_mode))
    }

    fn sql_mode(&self) -> Option<String> {
        Some(self.sql_mode.clone())
    }
}

/// First word of a statement, after comments (to recognise its kind), read from the scanner's
/// regions: comments (executable ones too) are passed over, and the word is the letters that
/// start the first code after them, after ASCII white space.
pub fn first_word(sql: &str) -> &str {
    for region in MysqlScanner::new(sql, Quoting::DEFAULT) {
        match region.kind {
            RegionKind::Comment | RegionKind::ExecutableComment => {}
            RegionKind::Code | RegionKind::MetaLine { .. } => {
                let text = &sql[region.range.clone()];
                let rest = text.trim_start_matches(|c: char| c.is_ascii_whitespace());
                // White space alone: what follows it may still be a comment.
                if rest.is_empty() {
                    continue;
                }
                let start = region.range.end - rest.len();
                let letters = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                return &sql[start..start + letters];
            }
            RegionKind::String
            | RegionKind::QuotedName
            | RegionKind::QuotedBody
            | RegionKind::CopyData
            | RegionKind::CopyEnd
            | RegionKind::StatementEnd { .. } => return "",
        }
    }
    ""
}

/// If a comment starts at `i`, the byte after it. `-- ` needs a space (or end of line) after
/// the dashes, as in MySQL. An executable comment (`/*! … */`, `/*M! … */`) ends the same way.
#[inline]
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

/// MySQL's lexical scanner (ADR-065): the script as regions, read as the session's `sql_mode`
/// reads quotes. Statements end at the current delimiter (`;` by default) outside quotes (`'…'`,
/// `"…"`, `` `…` ``) and comments. A `DELIMITER $$` line, as in the `mysql` client, changes the
/// delimiter for procedures and triggers: a region of its own, which is not a statement and is
/// code as a whole (no quote or comment opens in it). An executable comment makes its statement
/// one even when the statement holds nothing else (as `mysqldump` writes views and triggers).
/// The splitter, `code_ranges` and `first_word` all read it; it holds the current delimiter and
/// no more than the region at hand.
struct MysqlScanner<'a> {
    script: &'a str,
    quoting: Quoting,
    /// Where the next region starts.
    i: usize,
    /// Where the current statement starts.
    start: usize,
    /// Whether the current statement holds code yet: a string, a name, an executable comment
    /// or any other byte that is not white space. Plain comments do not count.
    has_code: bool,
    /// What ends a statement now.
    delimiter: String,
    /// Its first byte, for the fast path over ordinary code.
    delimiter_start: u8,
    /// The end of the script was reached.
    ended: bool,
}

/// What starts at the scanner's position, other than code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    /// A `DELIMITER x` line, up to its newline.
    Delimiter {
        line_end: usize,
    },
    Comment {
        executable: bool,
    },
    Quoted {
        quote: u8,
    },
    /// A client command line (ADR-078), where a statement could begin.
    Command,
    StatementEnd,
}

impl<'a> MysqlScanner<'a> {
    fn new(script: &'a str, quoting: Quoting) -> Self {
        Self {
            script,
            quoting,
            i: 0,
            start: 0,
            has_code: false,
            delimiter: String::from(";"),
            delimiter_start: b';',
            ended: false,
        }
    }

    /// What starts at the position, other than code, and the byte after it. In the order the
    /// `mysql` client reads them: a `DELIMITER` line where a statement would start, a comment, a
    /// quote, and the delimiter only on a byte that is not white space.
    #[inline]
    fn opening(&self) -> Option<(Opening, usize)> {
        let script = self.script;
        let bytes = script.as_bytes();
        let i = self.i;
        // The word first: the look back to the line's start costs more.
        if !self.has_code && starts_with_ci(&bytes[i..], b"delimiter") && at_line_start(bytes, i) {
            let after = i + "delimiter".len();
            if bytes.get(after).is_some_and(|b| *b == b' ' || *b == b'\t') {
                let line_end = bytes[after..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(bytes.len(), |p| after + p);
                return Some((Opening::Delimiter { line_end }, line_end));
            }
        }
        if !self.has_code
            && let Some(end) = crate::scanner::command_line(script, i)
        {
            return Some((Opening::Command, end));
        }
        if let Some(end) = comment_end(bytes, i) {
            let executable = is_executable_comment(&bytes[i..]);
            return Some((Opening::Comment { executable }, end));
        }
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => Some((
                Opening::Quoted { quote },
                skip_quoted(bytes, i, quote, self.quoting),
            )),
            _ if crate::is_white_space_at(script, i) => None,
            _ if bytes[i..].starts_with(self.delimiter.as_bytes()) => {
                Some((Opening::StatementEnd, i + self.delimiter.len()))
            }
            _ => None,
        }
    }

    /// Reads the region that `opening` starts at the position, up to `end`.
    #[inline]
    fn take(&mut self, opening: Opening, end: usize) -> Region {
        let range = self.i..end;
        let kind = match opening {
            Opening::Delimiter { line_end } => {
                let new = self.script[range.start + "delimiter".len()..line_end].trim();
                if !new.is_empty() {
                    self.delimiter = new.to_string();
                    self.delimiter_start = new.as_bytes()[0];
                }
                // The next statement starts after the line's newline, which is code of no
                // statement.
                self.start = (line_end + 1).min(self.script.len());
                RegionKind::MetaLine { statement: None }
            }
            Opening::Command => {
                // A statement of its own, which the session refuses wherever nothing resolves
                // it; its newline is white space of what follows.
                let statement = crate::trimmed(self.script, range.start, range.end);
                self.start = end;
                RegionKind::MetaLine { statement }
            }
            Opening::Comment { executable } => {
                // The server runs the body of an executable comment.
                self.has_code |= executable;
                if executable {
                    RegionKind::ExecutableComment
                } else {
                    RegionKind::Comment
                }
            }
            Opening::Quoted { quote } => {
                self.has_code = true;
                let string =
                    quote == b'\'' || (quote == b'"' && self.quoting.double_quoted_strings);
                if string {
                    RegionKind::String
                } else {
                    RegionKind::QuotedName
                }
            }
            Opening::StatementEnd => {
                let statement =
                    crate::trimmed(self.script, self.start, range.start).filter(|_| self.has_code);
                self.start = end;
                self.has_code = false;
                RegionKind::StatementEnd { statement }
            }
        };
        self.i = end;
        Region { kind, range }
    }

    /// Whether something other than code may start at ASCII byte `b`: a quote, a comment's
    /// first byte, the delimiter's, or the `d` of a `DELIMITER` line where a statement would
    /// start.
    #[inline]
    fn may_open(&self, b: u8) -> bool {
        opens_in_code(b)
            || b == self.delimiter_start
            || (!self.has_code && (b.eq_ignore_ascii_case(&b'd') || b == b'@'))
    }

    /// Reads one byte of code, as the `mysql` client does: white space leaves the statement as
    /// it was, anything else is code.
    #[inline]
    fn code_step(&mut self) {
        if !crate::is_white_space_at(self.script, self.i) {
            self.has_code = true;
        }
        self.i += 1;
    }
}

impl Iterator for MysqlScanner<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        let len = self.script.len();
        let bytes = self.script.as_bytes();
        let code = self.i;
        while self.i < len {
            // The fast path: an ASCII byte that opens nothing here (no quote, comment marker,
            // delimiter or `DELIMITER` line can start at it) is code, read exactly as
            // `opening` and `code_step` would read it.
            let b = bytes[self.i];
            if b.is_ascii() && !self.may_open(b) {
                // ASCII white space, as is_white_space_at reads it (char::is_whitespace).
                if !matches!(b, 0x09..=0x0d | 0x20) {
                    self.has_code = true;
                }
                self.i += 1;
                // Once the statement holds code, white space and a `d` change nothing: only a
                // quote, a comment's first byte or the delimiter's may open something.
                if self.has_code {
                    while self.i < len {
                        let c = bytes[self.i];
                        if !c.is_ascii() || opens_in_code(c) || c == self.delimiter_start {
                            break;
                        }
                        self.i += 1;
                    }
                }
                continue;
            }
            if let Some((opening, end)) = self.opening() {
                if code < self.i {
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

/// The statements of `script`, read by the scanner.
fn split(script: &str, quoting: Quoting) -> Vec<StatementSpan> {
    MysqlScanner::new(script, quoting)
        .filter_map(|region| region.kind.statement())
        .collect()
}

/// What lies outside strings, names and comments, read by the scanner: an executable comment
/// counts as a comment here too (nothing in it is looked for), and a `DELIMITER` line is code as
/// a whole, as the splitter reads it.
fn code_ranges(script: &str, quoting: Quoting) -> Vec<std::ops::Range<usize>> {
    let mut ranges: Vec<std::ops::Range<usize>> = Vec::new();
    for region in MysqlScanner::new(script, quoting) {
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

/// Whether a quote or a comment may start at ASCII byte `b` (`'`, `"`, `` ` ``, `#`, `--`, `/*`).
#[inline]
fn opens_in_code(b: u8) -> bool {
    matches!(b, b'\'' | b'"' | b'`' | b'#' | b'-' | b'/')
}

/// `/*!` (MySQL and MariaDB, optionally with a version) or `/*M!` (MariaDB; `m` too): a
/// comment whose body the server may run.
fn is_executable_comment(bytes: &[u8]) -> bool {
    bytes.starts_with(b"/*!")
        || (bytes.len() >= 4
            && bytes.starts_with(b"/*")
            && bytes[2].eq_ignore_ascii_case(&b'm')
            && bytes[3] == b'!')
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

/// Byte after the closing quote. A doubled quote is an escape; so is a backslash inside a
/// string (`'…'`, and `"…"` unless `ANSI_QUOTES` makes it a name), unless the session's
/// `sql_mode` has `NO_BACKSLASH_ESCAPES`. Names (`` `…` ``) never take one.
#[inline]
fn skip_quoted(bytes: &[u8], open: usize, quote: u8, quoting: Quoting) -> usize {
    let string = quote == b'\'' || (quote == b'"' && quoting.double_quoted_strings);
    let backslash = string && quoting.backslash_escapes;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if backslash => i += 2,
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
        split(script, Quoting::DEFAULT)
            .iter()
            .map(|s| s.text(script))
            .collect()
    }

    fn parts_in<'a>(sql_mode: &str, script: &'a str) -> Vec<&'a str> {
        MysqlDialect
            .with_sql_mode(sql_mode)
            .unwrap()
            .split_statements(script)
            .iter()
            .map(|s| s.text(script))
            .collect()
    }

    #[test]
    fn quotes_are_read_as_the_sessions_sql_mode_reads_them() {
        let script = r"insert into t values ('C:\'); select 1;";
        // Default: `\'` is an escaped quote, the string goes on.
        assert_eq!(parts_in("STRICT_TRANS_TABLES", script), [script]);
        // NO_BACKSLASH_ESCAPES: `\` is an ordinary character, the string ends at `'`.
        for mode in [
            "NO_BACKSLASH_ESCAPES",
            "STRICT_TRANS_TABLES, no_backslash_escapes ,NO_ENGINE_SUBSTITUTION",
        ] {
            assert_eq!(
                parts_in(mode, script),
                [r"insert into t values ('C:\')", "select 1"],
                "{mode}"
            );
        }
        // ANSI_QUOTES: `"…"` is a name, where `\` escapes nothing; `'…'` still takes it.
        let names = r#"select "a\" from t; select 'b\';' x; select 2"#;
        assert_eq!(
            parts_in("ANSI_QUOTES", names),
            [r#"select "a\" from t"#, r"select 'b\';' x", "select 2"]
        );
        assert_eq!(parts_in("ANSI", names), parts_in("ANSI_QUOTES", names));
        // By default `"…"` is a string and `\"` escapes its quote: it never closes.
        assert_eq!(parts_in("", names), [names]);
        // Both: nothing escapes with a backslash.
        assert_eq!(
            parts_in(
                "ANSI_QUOTES,NO_BACKSLASH_ESCAPES",
                r"select 'a\'; select `b\`; select 3"
            ),
            [r"select 'a\'", r"select `b\`", "select 3"]
        );
        // The default mode gives back the plain dialect.
        assert_eq!(parts_in("", script), parts(script));
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
    fn statements_written_as_executable_comments_are_statements() {
        assert_eq!(
            parts("/*!40101 SET @saved_cs_client = @@character_set_client */;"),
            ["/*!40101 SET @saved_cs_client = @@character_set_client */"]
        );
        // mysqldump's view: three executable comments, one statement.
        let view = "/*!50001 CREATE ALGORITHM=UNDEFINED */\n\
                    /*!50013 DEFINER=`root`@`localhost` SQL SECURITY DEFINER */\n\
                    /*!50001 VIEW `v` AS select 1 AS `a` */";
        assert_eq!(parts(&format!("{view};\nselect 2")), [view, "select 2"]);
        // Its trigger, under `DELIMITER ;;`: the `;` inside the body does not split it.
        let trigger = "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER `t_bi` BEFORE INSERT ON `t` FOR EACH ROW BEGIN SET NEW.a = 1; END */";
        assert_eq!(
            parts(&format!(
                "DELIMITER ;;\n{trigger};;\nDELIMITER ;\nselect 3;"
            )),
            [trigger, "select 3"]
        );
        // MariaDB's marker, either case.
        assert_eq!(
            parts("/*M!100616 SET @x = 1 */; /*m!100616 SET @y = 2 */;"),
            ["/*M!100616 SET @x = 1 */", "/*m!100616 SET @y = 2 */"]
        );
        // Plain comments alone are still nothing; `/* M! */` is a plain comment.
        assert!(parts("/* not ! code */; /* M!100000 x */; -- /*!40101 x */\n;").is_empty());
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

    /// ADR-065: `code_ranges` reads the scanner the splitter reads. A `DELIMITER` line is code as
    /// a whole, so a quote in it opens no string that would hide the statements after it; and a
    /// custom delimiter is the delimiter, so `//` before a `*` opens no comment. Before the
    /// scanner, `code_ranges` read both as the start of a string or a comment, hiding `:x` and
    /// `:y` from the placeholder scanner while the splitter sent them.
    #[test]
    fn code_ranges_read_delimiter_lines_and_custom_delimiters_as_the_splitter_does() {
        let code = |script: &str, needle: &str| {
            let at = script.find(needle).unwrap();
            code_ranges(script, Quoting::DEFAULT)
                .iter()
                .any(|r| r.contains(&at))
        };
        // The delimiter is now `'`, so the `;` is part of the statement.
        let quoted_line = "DELIMITER '\nselect :x;";
        assert_eq!(parts(quoted_line), ["select :x;"]);
        assert!(code(quoted_line, ":x"), "the splitter sends `:x` as code");
        let custom = "DELIMITER //\nselect 1//* :y */";
        assert_eq!(parts(custom), ["select 1", "* :y */"]);
        assert!(
            code(custom, ":y"),
            "`//` is the delimiter, not half of `/*`"
        );
        // Without a DELIMITER line, quotes and comments hide what they always hid.
        assert!(!code("select '/* :z */'; -- :w\n", ":z"));
        assert!(!code("select '/* :z */'; -- :w\n", ":w"));
        // The line itself is code as a whole: no comment opens in it either.
        assert!(code("DELIMITER $$ -- :v\nselect 1;", ":v"));
    }

    /// The scanner's fast path reads ASCII white space as `is_white_space_at` does: if the two
    /// rules ever part, an ordinary byte would make a statement out of nothing, or not.
    #[test]
    fn the_fast_path_reads_white_space_as_the_shared_rule() {
        for b in 0u8..0x80 {
            let text = (b as char).to_string();
            assert_eq!(
                matches!(b, 0x09..=0x0d | 0x20),
                crate::is_white_space_at(&text, 0),
                "byte {b:#x}"
            );
        }
    }

    #[test]
    fn identifiers_are_quoted_with_backticks() {
        assert_eq!(MysqlDialect.quote_identifier("we`ird"), "`we``ird`");
    }
}
