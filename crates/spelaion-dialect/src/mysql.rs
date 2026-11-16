use std::sync::Arc;

use spelaion_driver_api::{Dialect, StatementSpan};

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

/// First word of a statement, after comments (to recognise its kind).
pub fn first_word(sql: &str) -> &str {
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
/// the dashes, as in MySQL. An executable comment (`/*! … */`, `/*M! … */`) is skipped the
/// same way when looking for the delimiter: a `;` inside it does not end the statement.
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

/// Splits on the current delimiter (`;` by default), respecting quotes (`'…'`, `"…"`,
/// `` `…` ``, read as the session's `sql_mode` reads them) and comments. `DELIMITER $$` lines,
/// as in the `mysql` client, change the delimiter for procedures and triggers and are not
/// statements.
fn split(script: &str, quoting: Quoting) -> Vec<StatementSpan> {
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
            // The server runs the body of an executable comment: a statement written only
            // with them (as `mysqldump` writes views and triggers) is still a statement.
            has_code |= is_executable_comment(&bytes[i..]);
            i = end;
            continue;
        }
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => {
                has_code = true;
                i = skip_quoted(bytes, i, quote, quoting);
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

/// What lies outside strings, names and comments, read as the splitter reads them. An
/// executable comment counts as a comment here too: nothing in it is looked for.
fn code_ranges(script: &str, quoting: Quoting) -> Vec<std::ops::Range<usize>> {
    spelaion_driver_api::code_ranges(script, |script, i| {
        let bytes = script.as_bytes();
        if let Some(end) = comment_end(bytes, i) {
            return Some(end);
        }
        match bytes[i] {
            quote @ (b'\'' | b'"' | b'`') => Some(skip_quoted(bytes, i, quote, quoting)),
            _ => None,
        }
    })
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

    #[test]
    fn identifiers_are_quoted_with_backticks() {
        assert_eq!(MysqlDialect.quote_identifier("we`ird"), "`we``ird`");
    }
}
