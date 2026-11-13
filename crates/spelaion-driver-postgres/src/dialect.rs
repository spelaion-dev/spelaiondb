use spelaion_driver_api::{Dialect, StatementSpan};

/// PostgreSQL dialect.
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        spelaion_driver_api::code_ranges(script, skip_non_code)
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

/// Splits at `;` outside quotes, comments and `$tag$` bodies. In a `CREATE FUNCTION` or
/// `PROCEDURE`, as `psql` does, an unquoted `BEGIN` or `CASE` opens a block and `END` closes
/// one, and a `;` inside a block does not split: a `BEGIN ATOMIC` body stays whole, and its
/// `END` (a synonym of `COMMIT` on its own) is never sent alone. Elsewhere `BEGIN;` and `END;`
/// are statements of their own.
fn split(script: &str) -> Vec<StatementSpan> {
    let b = script.as_bytes();
    let mut spans = Vec::new();
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
                if has_code {
                    spans.push(trimmed(script, start, i));
                }
                i += 1;
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
    spans
}

/// A `COPY` whose data goes through the connection, apart from the statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyStdio {
    /// `COPY … FROM STDIN`: the client sends the rows.
    FromStdin,
    /// `COPY … TO STDOUT`: the client reads them.
    ToStdout,
}

/// Whether `sql` is a `COPY … FROM STDIN` or `COPY … TO STDOUT`: its first word is `COPY` and,
/// outside quotes, comments, `$tag$` bodies and parentheses (a query or a column list),
/// `FROM STDIN` or `TO STDOUT` follow. `COPY … FROM` / `TO` a file or a `PROGRAM` is neither.
pub(crate) fn copy_stdio(sql: &str) -> Option<CopyStdio> {
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
}
