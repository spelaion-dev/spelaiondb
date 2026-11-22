use spelaion_driver_api::{Dialect, StatementSpan};

use crate::{is_white_space_at, trimmed, white_space_before};

/// PostgreSQL dialect.
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script).0
    }

    /// The data of a `COPY … FROM STDIN` (SDB-162): from the line after its `;` up to the line
    /// `\.`, as `psql` reads it.
    fn copy_data(&self, script: &str, span: &StatementSpan) -> Option<std::ops::Range<usize>> {
        if copy_stdio(span.text(script)) != Some(CopyStdio::FromStdin) {
            return None;
        }
        let rest = &script[span.end..];
        let semicolon = span.end + (rest.len() - rest.trim_start().len());
        if script.as_bytes().get(semicolon) != Some(&b';') {
            return None;
        }
        Some(copy_block(script, semicolon + 1).data)
    }

    /// A psql meta-command (`\connect`, `\i`, `\set`…) is psql's, not SQL: never sent.
    fn refuse_statement(&self, statement: &str) -> Option<spelaion_driver_api::RequestError> {
        statement
            .starts_with('\\')
            .then(|| spelaion_driver_api::RequestError {
                position: 0,
                message: "this is a psql command, not SQL, and SpelaionDB only runs SQL".into(),
            })
    }

    /// What lies outside strings, quoted names and comments, and outside the data of every
    /// `COPY … FROM STDIN` (SDB-162). The splitter's psql lines are read as it reads them: a
    /// meta-command line is code as a whole, with no strings or comments in it (SDB-436), and
    /// COPY data is no code at all. Each piece of SQL between them is read on its own, so a
    /// quote in a psql line never changes how the SQL after it is read.
    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        let (_, apart) = split(script);
        let mut ranges = Vec::new();
        let mut start = 0;
        let pieces = apart.iter().map(|piece| match piece {
            Apart::Meta(line) => (line.clone(), true),
            Apart::Copy(block) => (block.data.start..block.resume, false),
        });
        // The end of the script closes the last piece of SQL.
        for (skipped, code) in pieces.chain(std::iter::once((script.len()..script.len(), false))) {
            for range in
                spelaion_driver_api::code_ranges(&script[start..skipped.start], skip_non_code)
            {
                push_code(&mut ranges, range.start + start..range.end + start);
            }
            if code {
                push_code(&mut ranges, skipped.clone());
            }
            start = skipped.end;
        }
        ranges
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

/// The byte after the word that starts at `i`: at the first byte no name holds, or at white
/// space (SDB-434).
fn word_end(script: &str, i: usize) -> usize {
    let b = script.as_bytes();
    (i..b.len())
        .find(|&j| {
            !is_ident_byte(b[j]) || (script.is_char_boundary(j) && is_white_space_at(script, j))
        })
        .unwrap_or(b.len())
}

/// `CREATE [OR REPLACE] FUNCTION | PROCEDURE`: its SQL-standard body (`BEGIN ATOMIC … END`,
/// PostgreSQL 14) is not quoted and holds semicolons.
fn is_routine(words: &[String]) -> bool {
    let kind = match words {
        [create, or, replace, kind, ..]
            if create == "create" && or == "or" && replace == "replace" =>
        {
            kind
        }
        [create, kind, ..] if create == "create" => kind,
        _ => return false,
    };
    kind == "function" || kind == "procedure"
}

/// Whether byte `i` is the first one on its line but for blanks.
fn at_line_start(b: &[u8], i: usize) -> bool {
    b[..i]
        .iter()
        .rev()
        .take_while(|&&c| c != b'\n')
        .all(|&c| c == b' ' || c == b'\t' || c == b'\r')
}

/// `\restrict <key>` or `\unrestrict <key>`, the lines a plain pg_dump file starts and ends with.
fn is_restrict(line: &str) -> bool {
    let command = line[1..].split_ascii_whitespace().next().unwrap_or("");
    command == "restrict" || command == "unrestrict"
}

/// The data of a `COPY … FROM STDIN` in a script (SDB-162).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyBlock {
    /// The data lines, without the terminating `\.` line.
    data: std::ops::Range<usize>,
    /// The byte after the terminating line (or the end of the script): where SQL goes on.
    resume: usize,
}

/// What the splitter reads apart from SQL, in the script's order: psql's lines, not the
/// server's.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Apart {
    /// A psql meta-command line, from its `\` to the end of its line (without the newline).
    Meta(std::ops::Range<usize>),
    /// The data of a `COPY … FROM STDIN`, with its terminating line.
    Copy(CopyBlock),
}

/// Adds `range` to `ranges`, joined to the last one when they touch: code ranges never touch.
fn push_code(ranges: &mut Vec<std::ops::Range<usize>>, range: std::ops::Range<usize>) {
    if range.is_empty() {
        return;
    }
    match ranges.last_mut() {
        Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
        _ => ranges.push(range),
    }
}

/// The data that follows a `COPY … FROM STDIN` whose `;` ends just before `after`. As `psql`
/// reads it, it starts on the next line (the rest of the statement's line may only hold blanks
/// or a `--` comment; anything else is data) and ends before a line that holds only `\.` (a
/// `\r` before its newline allowed), or at the end of the script.
fn copy_block(script: &str, after: usize) -> CopyBlock {
    let b = script.as_bytes();
    let line_end = b[after..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| after + p);
    let rest = script[after..line_end].trim();
    let start = if rest.is_empty() || rest.starts_with("--") {
        (line_end + 1).min(b.len())
    } else {
        after
    };
    let mut line = start;
    while line < b.len() {
        let end = b[line..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(b.len(), |p| line + p);
        let text = &script[line..end];
        if text.strip_suffix('\r').unwrap_or(text) == "\\." {
            return CopyBlock {
                data: start..line,
                resume: (end + 1).min(b.len()),
            };
        }
        line = end + 1;
    }
    CopyBlock {
        data: start..b.len(),
        resume: b.len(),
    }
}

/// Splits at `;` outside quotes, comments and `$tag$` bodies. In a `CREATE FUNCTION` or
/// `PROCEDURE`, as `psql` does, an unquoted `BEGIN` or `CASE` opens a block and `END` closes
/// one, and a `;` inside a block does not split: a `BEGIN ATOMIC` body stays whole, and its
/// `END` (a synonym of `COMMIT` on its own) is never sent alone. Elsewhere `BEGIN;` and `END;`
/// are statements of their own. The data after a `COPY … FROM STDIN` (SDB-162) and each psql
/// meta-command line are returned apart too: they are not SQL.
fn split(script: &str) -> (Vec<StatementSpan>, Vec<Apart>) {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let mut apart = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    // The first words of the current statement, lowercased, and its block depth.
    let mut words: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;

    while i < b.len() {
        match b[i] {
            // A psql meta-command, on a line of its own before a statement: not SQL. A plain
            // pg_dump file (PostgreSQL 17.6 and later) wraps itself in `\restrict <key>` and
            // `\unrestrict <key>`, which only keep psql from running other meta-commands, and
            // SpelaionDB runs none: they are skipped. Any other is a statement of its own,
            // which the session refuses (`refuse_statement`), never glued to the next one.
            b'\\' if !has_code && at_line_start(b, i) => {
                let end = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
                if !is_restrict(&script[i..end]) {
                    spans.extend(trimmed(script, i, end));
                }
                apart.push(Apart::Meta(i..end));
                i = (end + 1).min(b.len());
                start = i;
                words.clear();
            }
            b'\'' => {
                // E'…' supports backslash escapes.
                i = skip_quoted(b, i, b'\'', escape_string(b, i));
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
            b'/' if b.get(i + 1) == Some(&b'*') => i = block_comment_end(b, i),
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
            b';' if depth == 0 => {
                i += 1;
                if let Some(span) = trimmed(script, start, i - 1).filter(|_| has_code) {
                    spans.push(span);
                    // Its data follows: skipped, never read as SQL.
                    if words.first().map(String::as_str) == Some("copy")
                        && copy_stdio(span.text(script)) == Some(CopyStdio::FromStdin)
                    {
                        let block = copy_block(script, i);
                        i = block.resume;
                        apart.push(Apart::Copy(block));
                    }
                }
                start = i;
                has_code = false;
                words.clear();
            }
            // White space outside ASCII is no part of a word, and a word may follow it
            // (SDB-434).
            c if (c.is_ascii_alphabetic() || c == b'_' || c >= 0x80)
                && (i == 0 || !is_ident_byte(b[i - 1]) || white_space_before(script, i))
                && !is_white_space_at(script, i) =>
            {
                let end = word_end(script, i);
                let word = script[i..end].to_ascii_lowercase();
                if words.len() < 4 {
                    words.push(word.clone());
                }
                if is_routine(&words) {
                    match word.as_str() {
                        "begin" | "case" => depth += 1,
                        "end" => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                }
                has_code = true;
                i = end;
            }
            _ => {
                if !is_white_space_at(script, i) {
                    has_code = true;
                }
                i += 1;
            }
        }
    }
    if has_code {
        spans.extend(trimmed(script, start, b.len()));
    }
    (spans, apart)
}

/// A `COPY` whose data goes through the connection, apart from the statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyStdio {
    /// `COPY … FROM STDIN`: the client sends the rows.
    FromStdin,
    /// `COPY … TO STDOUT`: the client reads them.
    ToStdout,
}

/// Whether `sql` is a `COPY … FROM STDIN` or `COPY … TO STDOUT`: its first word is `COPY` and,
/// outside quotes, comments, `$tag$` bodies and parentheses (a query or a column list),
/// `FROM STDIN` or `TO STDOUT` follow. `COPY … FROM` / `TO` a file or a `PROGRAM` is neither,
/// and nor is psql's `\copy` (SDB-437): a meta-command, which the session refuses before
/// anything is sent, never a `COPY` with data of its own.
pub fn copy_stdio(sql: &str) -> Option<CopyStdio> {
    let b = sql.as_bytes();
    let mut words: Vec<String> = Vec::new();
    let mut parens = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if words.is_empty() => return None,
            b'\'' => {
                i = skip_quoted(b, i, b'\'', escape_string(b, i));
                words.push(String::new());
            }
            b'"' => {
                i = skip_quoted(b, i, b'"', false);
                words.push(String::new());
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p + 1);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
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
                    let tag = &sql[i..=tag_end];
                    i = sql[tag_end + 1..]
                        .find(tag)
                        .map_or(b.len(), |p| tag_end + 1 + p + tag.len());
                    words.push(String::new());
                }
                None => i += 1,
            },
            b'(' => {
                parens += 1;
                i += 1;
            }
            b')' => {
                parens = parens.saturating_sub(1);
                i += 1;
            }
            // Words as the splitter reads them (SDB-439): one may follow white space outside
            // ASCII, and ends at it, so both agree on a statement's first word.
            c if (c.is_ascii_alphabetic() || c == b'_' || c >= 0x80)
                && (i == 0 || !is_ident_byte(b[i - 1]) || white_space_before(sql, i))
                && !is_white_space_at(sql, i) =>
            {
                let end = word_end(sql, i);
                if words.is_empty() || parens == 0 {
                    words.push(sql[i..end].to_ascii_lowercase());
                }
                i = end;
            }
            b';' => break,
            _ => i += 1,
        }
    }
    if words.first().map(String::as_str) != Some("copy") {
        return None;
    }
    words
        .windows(2)
        .find_map(|pair| match (pair[0].as_str(), pair[1].as_str()) {
            ("from", "stdin") => Some(CopyStdio::FromStdin),
            ("to", "stdout") => Some(CopyStdio::ToStdout),
            _ => None,
        })
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

/// Whether the `'` at `i` opens an `E'…'` string, where backslashes escape.
fn escape_string(b: &[u8], i: usize) -> bool {
    i > 0 && matches!(b[i - 1], b'E' | b'e') && (i < 2 || !is_ident_byte(b[i - 2]))
}

/// The byte after the string, quoted name, comment or `$tag$` body that starts at `i`, read as
/// the splitter reads it; `None` when none starts there.
fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match *b.get(i)? {
        b'\'' => Some(skip_quoted(b, i, b'\'', escape_string(b, i))),
        b'"' => Some(skip_quoted(b, i, b'"', false)),
        b'-' if b.get(i + 1) == Some(&b'-') => Some(
            b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |p| i + p + 1),
        ),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(block_comment_end(b, i)),
        b'$' if i == 0 || !is_ident_byte(b[i - 1]) => {
            let tag_end = dollar_tag(b, i)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(script: &str) -> Vec<&str> {
        split(script).0.iter().map(|s| s.text(script)).collect()
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
    fn begin_atomic_bodies_are_one_statement() {
        let function = "CREATE OR REPLACE FUNCTION add_one(a int) RETURNS int\nLANGUAGE sql\nBEGIN ATOMIC\n  SELECT a + 1;\n  SELECT CASE WHEN a > 0 THEN a ELSE -a END;\nEND";
        assert_eq!(
            parts(&format!("{function};\nselect 1;")),
            [function, "select 1"]
        );
        let procedure =
            "create procedure p() language sql begin atomic insert into t values (1); end";
        assert_eq!(
            parts(&format!("{procedure}; call p()")),
            [procedure, "call p()"]
        );
        // A transaction's BEGIN and END are still statements of their own.
        assert_eq!(parts("begin; select 1; end;"), ["begin", "select 1", "end"]);
        assert_eq!(
            parts("BEGIN WORK; select case when true then 1 end; END WORK;"),
            ["BEGIN WORK", "select case when true then 1 end", "END WORK"]
        );
        // The SQL-standard RETURN form has no block; words in quotes, comments and $$ bodies
        // do not count.
        assert_eq!(
            parts("create function f() returns int language sql return 1; select 2"),
            [
                "create function f() returns int language sql return 1",
                "select 2"
            ]
        );
        assert_eq!(
            parts(
                "create function \"begin\"() returns text language sql /* begin */ return 'begin'; select 3"
            ),
            [
                "create function \"begin\"() returns text language sql /* begin */ return 'begin'",
                "select 3"
            ]
        );
        assert_eq!(
            parts(
                "create function g() returns int as $$ begin return 1; end $$ language plpgsql; select 4"
            ),
            [
                "create function g() returns int as $$ begin return 1; end $$ language plpgsql",
                "select 4"
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
    fn copy_through_the_connection_is_recognised() {
        for sql in [
            "COPY t FROM STDIN",
            "copy public.t (a, b) from stdin with (format csv);",
            "-- rows\n/* and /* nested */ */ CoPy t FROM stdin",
            "copy \"T\" from stdin",
        ] {
            assert_eq!(copy_stdio(sql), Some(CopyStdio::FromStdin), "{sql}");
        }
        for sql in [
            "COPY t TO STDOUT",
            "copy (select 'from stdin', $$ to $$) to stdout with csv header",
            "copy t (a) to stdout",
        ] {
            assert_eq!(copy_stdio(sql), Some(CopyStdio::ToStdout), "{sql}");
        }
        for sql in [
            "select 'copy t from stdin'",
            "COPY t FROM '/tmp/x'",
            "copy t from 'stdin'",
            "copy t to program 'cat'",
            "copy (select * from stdin) to '/tmp/x'",
            "copy (select 1 as stdout) to '/tmp/x'",
            "-- copy t from stdin\nselect 1",
            "do $$ begin copy t from stdin; end $$",
            "with x as (select 1) select * from x",
            // psql's meta-command, not SQL (SDB-437).
            "\\copy t from stdin",
            "\\copy t to stdout",
            "-- rows\n\\copy t from stdin",
        ] {
            assert_eq!(copy_stdio(sql), None, "{sql}");
        }
    }

    #[test]
    fn a_psql_copy_line_has_no_data_and_is_refused() {
        let d = PostgresDialect;
        // Followed by a `;` on the next line, as `COPY … FROM STDIN;` would be (SDB-437): the
        // text after it is SQL of its own, not the meta-command's data.
        for script in [
            "\\copy t from stdin;\n;\nselect 1",
            "\\copy t from stdin\n;1\t2\n\\.\n",
        ] {
            let spans = d.split_statements(script);
            assert!(spans[0].text(script).starts_with("\\copy"), "{script:?}");
            assert_eq!(d.copy_data(script, &spans[0]), None, "{script:?}");
            assert!(d.refuse_statement(spans[0].text(script)).is_some());
        }
    }

    #[test]
    fn copy_stdio_reads_the_first_word_as_the_splitter_does() {
        // After white space outside ASCII a word starts, and at it one ends (SDB-439): the first
        // word here is `a` or `go`, not the `copy` after it, so no text is taken as COPY data.
        let d = PostgresDialect;
        for script in [
            "$\u{85}a\u{b}copy t from stdin;\n'",
            "0\u{a0}GO`copy t from stdin;\n//",
        ] {
            let spans = d.split_statements(script);
            assert_eq!(copy_stdio(spans[0].text(script)), None, "{script:?}");
            for span in &spans {
                assert_eq!(d.copy_data(script, span), None, "{script:?}");
            }
        }
        assert_eq!(
            copy_stdio("\u{a0}copy t from stdin"),
            Some(CopyStdio::FromStdin)
        );
        assert_eq!(copy_stdio("x\u{3000}copy t from stdin"), None);
        assert_eq!(
            copy_stdio("copy\u{a0}t\u{a0}from\u{a0}stdin"),
            Some(CopyStdio::FromStdin)
        );
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

    /// What a plain `pg_dump` writes: the data after `COPY … FROM stdin;`, up to `\.`.
    const DUMP: &str = "SET client_encoding = 'UTF8';\n\
        COPY public.t (id, note) FROM stdin;\n\
        1\tit's; not -- SQL\n\
        2\t:name and ${var} and $$\n\
        3\t\\N\n\
        \\.\n\
        SELECT setval('public.t_id_seq', 3, true);\n";

    #[test]
    fn the_data_of_copy_from_stdin_is_skipped_and_given_apart() {
        let d = PostgresDialect;
        let spans = d.split_statements(DUMP);
        let texts: Vec<&str> = spans.iter().map(|s| s.text(DUMP)).collect();
        assert_eq!(
            texts,
            [
                "SET client_encoding = 'UTF8'",
                "COPY public.t (id, note) FROM stdin",
                "SELECT setval('public.t_id_seq', 3, true)",
            ]
        );
        let data = d.copy_data(DUMP, &spans[1]).expect("its data");
        assert_eq!(
            &DUMP[data],
            "1\tit's; not -- SQL\n2\t:name and ${var} and $$\n3\t\\N\n"
        );
        // Only that statement has data.
        assert_eq!(d.copy_data(DUMP, &spans[0]), None);
        assert_eq!(d.copy_data(DUMP, &spans[2]), None);
    }

    #[test]
    fn copy_data_ends_at_its_terminator_line_or_the_end_of_the_script() {
        let d = PostgresDialect;
        // Windows line endings, and a comment after the statement's `;`.
        let crlf = "copy t from stdin; -- rows\r\na\r\nb\r\n\\.\r\nselect 1;\r\n";
        let spans = d.split_statements(crlf);
        assert_eq!(spans.len(), 2, "{spans:?}");
        assert_eq!(&crlf[d.copy_data(crlf, &spans[0]).unwrap()], "a\r\nb\r\n");
        assert_eq!(spans[1].text(crlf), "select 1");
        // No terminator: the data runs to the end, and nothing after it is SQL.
        let open = "copy t from stdin;\n1\n2";
        let spans = d.split_statements(open);
        assert_eq!(spans.len(), 1);
        assert_eq!(&open[d.copy_data(open, &spans[0]).unwrap()], "1\n2");
        // No data at all.
        let empty = "copy t from stdin;\n\\.\n";
        let spans = d.split_statements(empty);
        assert_eq!(d.copy_data(empty, &spans[0]), Some(19..19));
        // A `\.` inside a line is data; only a line of its own ends it.
        let inner = "copy t from stdin;\na\\.b\n \\.\n\\.\nselect 2";
        let spans = d.split_statements(inner);
        assert_eq!(
            &inner[d.copy_data(inner, &spans[0]).unwrap()],
            "a\\.b\n \\.\n"
        );
        assert_eq!(spans[1].text(inner), "select 2");
    }

    #[test]
    fn other_copies_and_copy_words_have_no_data() {
        let d = PostgresDialect;
        for script in [
            "copy t to stdout;\nselect 1",
            "copy t from '/tmp/x';\nselect 1",
            "select 'copy t from stdin';\nselect 1",
            "do $$ begin copy t from stdin; end $$;\nselect 1",
            "copy t from stdin",
        ] {
            let spans = d.split_statements(script);
            assert!(
                spans.iter().all(|s| d.copy_data(script, s).is_none()),
                "{script}"
            );
        }
        // Without its `;`, nothing follows the statement as its data.
        assert_eq!(
            parts("copy t to stdout;\nselect 1"),
            ["copy t to stdout", "select 1"]
        );
    }

    #[test]
    fn psql_meta_commands_are_never_glued_to_a_statement() {
        let d = PostgresDialect;
        // How pg_dump 17.6 and later starts and ends a plain dump: skipped.
        let dump = "--\n-- PostgreSQL database dump\n--\n\n\\restrict AbC123\n\nSET a = 1;\nselect 2;\n\n\\unrestrict AbC123\n";
        assert_eq!(parts(dump), ["SET a = 1", "select 2"]);
        // Any other is a statement of its own, which the session refuses.
        let other = "select 1;\n  \\connect other\nselect 2;\n\\i /tmp/x.sql\n";
        assert_eq!(
            parts(other),
            ["select 1", "\\connect other", "select 2", "\\i /tmp/x.sql"]
        );
        assert!(d.refuse_statement("\\connect other").is_some());
        assert!(d.refuse_statement("select 1").is_none());
        // A backslash inside a statement or a string is not one.
        assert_eq!(
            parts("select 'a\n\\b';\nselect E'\\\\x'\n  , 1;"),
            ["select 'a\n\\b'", "select E'\\\\x'\n  , 1"]
        );
    }

    #[test]
    fn copy_data_is_not_code() {
        let d = PostgresDialect;
        let code: String = d.code_ranges(DUMP).into_iter().map(|r| &DUMP[r]).collect();
        // Neither the placeholder nor the variable in the data is code, and the quote in the
        // data does not make the SQL after it look like a string.
        assert!(
            !code.contains(":name") && !code.contains("${var}"),
            "{code}"
        );
        assert!(code.contains("SELECT setval("), "{code}");
        assert!(
            code.contains("COPY public.t (id, note) FROM stdin;"),
            "{code}"
        );
    }

    #[test]
    fn meta_command_lines_are_code_without_strings_or_comments() {
        let d = PostgresDialect;
        let code = |script: &str| -> String {
            d.code_ranges(script)
                .into_iter()
                .map(|r| &script[r])
                .collect()
        };
        // The quote in a meta-command line opens no string: the next statement's placeholder
        // stays code, as the splitter reads it (SDB-436).
        let script = "\\echo it's\nselect :x";
        assert_eq!(parts(script), ["\\echo it's", "select :x"]);
        assert_eq!(code(script), script);
        // Nor does a comment opener: the SQL after it stays code, and a real comment after it
        // stays a comment.
        let script = "\\echo /* x\nselect 1; -- c\n\\restrict k $$\nselect '$$'";
        assert_eq!(parts(script), ["\\echo /* x", "select 1", "select '$$'"]);
        assert_eq!(
            code(script),
            "\\echo /* x\nselect 1; \\restrict k $$\nselect "
        );
        // A backslash inside a statement is SQL, read as the splitter reads it.
        let script = "select 1\n\\echo 'a;\n';";
        assert_eq!(parts(script), ["select 1\n\\echo 'a;\n'"]);
        assert_eq!(code(script), "select 1\n\\echo ;");
        // Code ranges never touch, even around a meta-command line.
        let script = "select 1;\n\\i x\n\\i y\nselect 2";
        let ranges = d.code_ranges(script);
        assert_eq!(ranges.len(), 1, "{ranges:?}");
        assert_eq!((ranges[0].start, ranges[0].end), (0, script.len()));
    }
}
