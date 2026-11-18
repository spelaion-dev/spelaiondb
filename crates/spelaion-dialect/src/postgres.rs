use spelaion_driver_api::{Dialect, StatementSpan};

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

    /// What lies outside strings, quoted names and comments, and outside the data of every
    /// `COPY … FROM STDIN` (SDB-162): each piece between two data blocks is read on its own, so
    /// a quote in the data never changes how the SQL after it is read.
    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        let (_, blocks) = split(script);
        let mut ranges = Vec::new();
        let mut start = 0;
        for block in blocks
            .iter()
            .map(|b| b.data.start..b.resume)
            .chain(std::iter::once(script.len()..script.len()))
        {
            let piece = &script[start..block.start];
            ranges.extend(
                spelaion_driver_api::code_ranges(piece, skip_non_code)
                    .into_iter()
                    .map(|r| r.start + start..r.end + start),
            );
            start = block.end;
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

/// The data of a `COPY … FROM STDIN` in a script (SDB-162).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyBlock {
    /// The data lines, without the terminating `\.` line.
    data: std::ops::Range<usize>,
    /// The byte after the terminating line (or the end of the script): where SQL goes on.
    resume: usize,
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
/// are statements of their own. The data after a `COPY … FROM STDIN` (SDB-162) is skipped and
/// returned apart: it is not SQL.
fn split(script: &str) -> (Vec<StatementSpan>, Vec<CopyBlock>) {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let mut blocks = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    // The first words of the current statement, lowercased, and its block depth.
    let mut words: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;

    while i < b.len() {
        match b[i] {
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
                if has_code {
                    let span = trimmed(script, start, i - 1);
                    spans.push(span);
                    // Its data follows: skipped, never read as SQL.
                    if words.first().map(String::as_str) == Some("copy")
                        && copy_stdio(span.text(script)) == Some(CopyStdio::FromStdin)
                    {
                        let block = copy_block(script, i);
                        i = block.resume;
                        blocks.push(block);
                    }
                }
                start = i;
                has_code = false;
                words.clear();
            }
            c if (c.is_ascii_alphabetic() || c == b'_' || c >= 0x80)
                && (i == 0 || !is_ident_byte(b[i - 1])) =>
            {
                let end = b[i..]
                    .iter()
                    .position(|&c| !is_ident_byte(c))
                    .map_or(b.len(), |p| i + p);
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
    (spans, blocks)
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
/// `FROM STDIN` or `TO STDOUT` follow. `COPY … FROM` / `TO` a file or a `PROGRAM` is neither.
pub fn copy_stdio(sql: &str) -> Option<CopyStdio> {
    let b = sql.as_bytes();
    let mut words: Vec<String> = Vec::new();
    let mut parens = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' => {
                let escapes =
                    i > 0 && matches!(b[i - 1], b'E' | b'e') && (i < 2 || !is_ident_byte(b[i - 2]));
                i = skip_quoted(b, i, b'\'', escapes);
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
            c if (c.is_ascii_alphabetic() || c == b'_' || c >= 0x80)
                && (i == 0 || !is_ident_byte(b[i - 1])) =>
            {
                let end = b[i..]
                    .iter()
                    .position(|&c| !is_ident_byte(c))
                    .map_or(b.len(), |p| i + p);
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
        ] {
            assert_eq!(copy_stdio(sql), None, "{sql}");
        }
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
}
