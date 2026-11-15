//! Cassandra's language, CQL, as the core reads it (ADR-056): a language shaped like SQL whose
//! statements are sent as they are written, as SQL's are, but which `sqlparser` does not read.
//! So its splitter, quoting and guard lists are its own, here. ScyllaDB speaks the same.
//!
//! What is read:
//! - Statements end at `;`, outside `'…'` strings (a quote doubled inside), `"…"` names (the
//!   same), `$$…$$` bodies, `--` and `//` line comments and `/* */` comments. A batch,
//!   `BEGIN [UNLOGGED | COUNTER] BATCH … APPLY BATCH`, is one statement with its `;` inside.
//! - The guard reads each statement's first words ([`statement_class`]): `SELECT` and the
//!   `LIST` and `DESCRIBE` commands read; `INSERT`, `UPDATE`, `DELETE`, `CREATE` and `ALTER`
//!   write; `TRUNCATE` deletes everything; `DROP` (and `ALTER … DROP`) drops; a batch is its
//!   most serious statement; roles, permissions and anything else are unknown, never a read.
//! - `USE` is refused before it is sent ([`refuse_statement`]): the tab's keyspace is chosen
//!   above the editor, and the core keeps it as the session knows it.

use spelaion_driver_api::{Dialect, RequestClass, RequestError, StatementSpan, code_ranges};

/// Cassandra's language.
#[derive(Debug, Clone, Copy, Default)]
pub struct CqlDialect;

impl Dialect for CqlDialect {
    fn name(&self) -> &'static str {
        "cassandra"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    /// A name as CQL needs it: bare when it is lowercase letters, digits and `_` (CQL folds a
    /// bare name to lowercase), otherwise in double quotes with a quote doubled.
    fn quote_identifier(&self, ident: &str) -> String {
        let bare = ident.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && ident
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !RESERVED.contains(&ident.to_ascii_uppercase().as_str());
        if bare {
            ident.to_string()
        } else {
            format!("\"{}\"", ident.replace('"', "\"\""))
        }
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        code_ranges(script, skip)
    }

    fn is_sql(&self) -> bool {
        false
    }

    fn statement_class(&self, statement: &str) -> Option<RequestClass> {
        Some(statement_class(statement))
    }

    fn refuse_statement(&self, statement: &str) -> Option<RequestError> {
        refuse_statement(statement)
    }

    fn browse_statement(&self, object: &spelaion_driver_api::ObjectInfo) -> Option<String> {
        Some(browse_statement(self, &object.schema, &object.name))
    }
}

/// How many rows opening a table shows: never the whole of it, which may be spread over many
/// nodes.
pub const BROWSE_LIMIT: u32 = 200;

/// The statement that shows a table's first rows, in its own keyspace.
pub fn browse_statement(dialect: &CqlDialect, keyspace: &str, table: &str) -> String {
    format!(
        "SELECT * FROM {}.{} LIMIT {BROWSE_LIMIT}",
        dialect.quote_identifier(keyspace),
        dialect.quote_identifier(table)
    )
}

/// CQL's reserved words: a name that is one is always quoted.
const RESERVED: &[&str] = &[
    "ADD",
    "ALLOW",
    "ALTER",
    "AND",
    "APPLY",
    "ASC",
    "AUTHORIZE",
    "BATCH",
    "BEGIN",
    "BY",
    "COLUMNFAMILY",
    "CREATE",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DROP",
    "ENTRIES",
    "EXECUTE",
    "FROM",
    "FULL",
    "GRANT",
    "IF",
    "IN",
    "INDEX",
    "INFINITY",
    "INSERT",
    "INTO",
    "IS",
    "KEYSPACE",
    "LIMIT",
    "MATERIALIZED",
    "MODIFY",
    "NAN",
    "NORECURSIVE",
    "NOT",
    "NULL",
    "OF",
    "ON",
    "OR",
    "ORDER",
    "PRIMARY",
    "RENAME",
    "REPLACE",
    "REVOKE",
    "SCHEMA",
    "SELECT",
    "SET",
    "TABLE",
    "TO",
    "TOKEN",
    "TRUNCATE",
    "UNLOGGED",
    "UPDATE",
    "USE",
    "USING",
    "VIEW",
    "WHERE",
    "WITH",
];

// ------------------------------------------------------------------------------ splitting

/// Where a string, a quoted name, a `$$` body or a comment that starts at `i` ends, or `None`.
fn skip(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match b[i] {
        quote @ (b'\'' | b'"') => {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == quote {
                    if b.get(j + 1) == Some(&quote) {
                        j += 2;
                        continue;
                    }
                    return Some(j + 1);
                }
                j += 1;
            }
            Some(b.len())
        }
        b'$' if b.get(i + 1) == Some(&b'$') => Some(
            script[i + 2..]
                .find("$$")
                .map_or(b.len(), |end| i + 2 + end + 2),
        ),
        b'-' if b.get(i + 1) == Some(&b'-') => Some(line_end(script, i)),
        b'/' if b.get(i + 1) == Some(&b'/') => Some(line_end(script, i)),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(
            script[i + 2..]
                .find("*/")
                .map_or(b.len(), |end| i + 2 + end + 2),
        ),
        _ => None,
    }
}

fn line_end(script: &str, i: usize) -> usize {
    script[i..].find('\n').map_or(script.len(), |end| i + end)
}

/// The words of `text`'s code, uppercased, in order (names, numbers and keywords; strings and
/// comments left out).
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for range in code_ranges(text, skip) {
        let mut word = String::new();
        for c in text[range].chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                word.push(c.to_ascii_uppercase());
            } else {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
                if c == ';' {
                    out.push(";".into());
                }
            }
        }
        if !word.is_empty() {
            out.push(word);
        }
    }
    out
}

fn split(script: &str) -> Vec<StatementSpan> {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let push = |from: usize, to: usize, spans: &mut Vec<StatementSpan>| {
        let text = &script[from..to];
        if words(text).iter().any(|w| w != ";") {
            let lead = text.len() - text.trim_start().len();
            let body = text.trim();
            spans.push(StatementSpan {
                start: from + lead,
                end: from + lead + body.len(),
            });
        }
    };
    while i < b.len() {
        if let Some(end) = skip(script, i) {
            i = end.max(i + 1);
            continue;
        }
        if b[i] == b';' {
            let so_far = words(&script[start..i]);
            let in_batch = so_far.first().is_some_and(|w| w == "BEGIN")
                && so_far.iter().take(3).any(|w| w == "BATCH")
                && !so_far
                    .windows(2)
                    .any(|w| w[0] == "APPLY" && w[1] == "BATCH");
            if !in_batch {
                push(start, i, &mut spans);
                start = i + 1;
            }
        }
        i += 1;
    }
    push(start, b.len(), &mut spans);
    spans
}

// ------------------------------------------------------------------------------ the guard

/// What the guard makes of one CQL statement, from its first words.
pub fn statement_class(statement: &str) -> RequestClass {
    let words = words(statement);
    let first = words.first().map(String::as_str).unwrap_or("");
    match first {
        "SELECT" => RequestClass::Read,
        "LIST" | "DESCRIBE" | "DESC" => {
            // `LIST ROLES`, `LIST USERS`, `LIST ALL PERMISSIONS`, `DESCRIBE …`: they read.
            RequestClass::Read
        }
        "INSERT" | "UPDATE" | "DELETE" => RequestClass::Write,
        "TRUNCATE" => RequestClass::DeleteAll,
        "DROP" => RequestClass::Drop,
        "ALTER" => {
            // `ALTER ROLE`/`USER` changes who may do what: the core cannot tell.
            match words.get(1).map(String::as_str) {
                Some("ROLE" | "USER") => RequestClass::Unknown,
                _ if words.iter().any(|w| w == "DROP" || w == "RENAME") => RequestClass::Drop,
                _ => RequestClass::Write,
            }
        }
        "CREATE" => match words.get(1).map(String::as_str) {
            Some("ROLE" | "USER") => RequestClass::Unknown,
            // `CREATE OR REPLACE FUNCTION` replaces one whole.
            Some("OR") => RequestClass::Drop,
            _ => RequestClass::Write,
        },
        "BEGIN" => batch_class(statement),
        // `GRANT`, `REVOKE`, `USE` (refused anyway) and anything else.
        _ => RequestClass::Unknown,
    }
}

/// A batch is as serious as its most serious statement; it holds only writes in CQL.
fn batch_class(statement: &str) -> RequestClass {
    let mut worst = RequestClass::Write;
    let ranges = code_ranges(statement, skip);
    let mut from = 0;
    let mut pieces = Vec::new();
    for range in ranges {
        for (offset, c) in statement[range.clone()].char_indices() {
            if c == ';' {
                pieces.push(&statement[from..range.start + offset]);
                from = range.start + offset + 1;
            }
        }
    }
    pieces.push(&statement[from..]);
    for piece in pieces {
        let words = words(piece);
        // Leave out the batch's own words.
        let start = words
            .iter()
            .position(|w| w == "BATCH")
            .filter(|_| words.first().is_some_and(|w| w == "BEGIN"))
            .map_or(0, |p| p + 1);
        let words = &words[start..];
        let words: Vec<&String> = words
            .iter()
            .take_while(|w| !(w.as_str() == "APPLY"))
            .collect();
        if words.is_empty() {
            continue;
        }
        let text: Vec<&str> = words.iter().map(|w| w.as_str()).collect();
        let class = statement_class(&text.join(" "));
        worst = more_serious(worst, class);
    }
    worst
}

fn rank(class: RequestClass) -> u8 {
    match class {
        RequestClass::Read => 0,
        RequestClass::Write => 1,
        RequestClass::Unknown => 2,
        RequestClass::UpdateAll => 3,
        RequestClass::DeleteAll => 4,
        RequestClass::Drop => 5,
    }
}

fn more_serious(a: RequestClass, b: RequestClass) -> RequestClass {
    if rank(b) > rank(a) { b } else { a }
}

/// What the core will not send: `USE`, which would move the session's keyspace behind its back.
pub fn refuse_statement(statement: &str) -> Option<RequestError> {
    let words = words(statement);
    if words.first().is_some_and(|w| w == "USE") {
        let position = statement.len() - statement.trim_start().len();
        return Some(RequestError {
            position,
            message: "`USE` is not run here: choose the keyspace above the editor".into(),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(script: &str) -> Vec<&str> {
        split(script).iter().map(|s| s.text(script)).collect()
    }

    #[test]
    fn statements_end_at_semicolons_outside_strings_names_bodies_and_comments() {
        let script = "SELECT * FROM t WHERE a = 'x;y';\n\
                      -- a comment; with a semicolon\n\
                      INSERT INTO \"T;1\" (a) VALUES ('it''s');\n\
                      // another; one\n\
                      CREATE FUNCTION f() RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return 1; $$;\n\
                      /* block; */ SELECT 1 FROM t";
        assert_eq!(
            texts(script),
            [
                "SELECT * FROM t WHERE a = 'x;y'",
                "-- a comment; with a semicolon\nINSERT INTO \"T;1\" (a) VALUES ('it''s')",
                "// another; one\nCREATE FUNCTION f() RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return 1; $$",
                "/* block; */ SELECT 1 FROM t",
            ]
        );
        assert!(texts(";;  -- only\n;").is_empty());
    }

    #[test]
    fn a_batch_is_one_statement() {
        let script = "BEGIN BATCH\n  INSERT INTO t (a) VALUES (1);\n  UPDATE t SET b = 2 WHERE a = 1;\nAPPLY BATCH;\nSELECT * FROM t;";
        assert_eq!(
            texts(script),
            [
                "BEGIN BATCH\n  INSERT INTO t (a) VALUES (1);\n  UPDATE t SET b = 2 WHERE a = 1;\nAPPLY BATCH",
                "SELECT * FROM t",
            ]
        );
        let unlogged =
            "begin unlogged batch insert into t (a) values (1); apply batch; select 1 from t";
        assert_eq!(texts(unlogged).len(), 2);
    }

    #[test]
    fn the_guard_reads_the_first_words() {
        let class = statement_class;
        for read in [
            "SELECT * FROM t",
            "select json * from ks.t",
            "LIST ROLES",
            "DESCRIBE TABLES",
            "DESC KEYSPACE ks",
        ] {
            assert_eq!(class(read), RequestClass::Read, "{read}");
        }
        for write in [
            "INSERT INTO t (a) VALUES (1)",
            "UPDATE t SET b = 1 WHERE a = 1",
            "DELETE FROM t WHERE a = 1",
            "CREATE TABLE t (a int PRIMARY KEY)",
            "ALTER TABLE t ADD c int",
            "CREATE INDEX ON t (b)",
        ] {
            assert_eq!(class(write), RequestClass::Write, "{write}");
        }
        assert_eq!(class("TRUNCATE t"), RequestClass::DeleteAll);
        assert_eq!(class("truncate table ks.t"), RequestClass::DeleteAll);
        for drop in [
            "DROP TABLE t",
            "drop keyspace ks",
            "ALTER TABLE t DROP c",
            "ALTER TABLE t RENAME a TO b",
            "CREATE OR REPLACE FUNCTION f() RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS $$ return 1; $$",
        ] {
            assert_eq!(class(drop), RequestClass::Drop, "{drop}");
        }
        for unknown in [
            "GRANT SELECT ON ks.t TO r",
            "REVOKE ALL ON KEYSPACE ks FROM r",
            "CREATE ROLE r WITH PASSWORD = 'x'",
            "ALTER ROLE r WITH PASSWORD = 'y'",
            "NOSUCH thing",
            "",
        ] {
            assert_eq!(class(unknown), RequestClass::Unknown, "{unknown}");
        }
        // A word in a string or a comment is not a statement's.
        assert_eq!(
            class("/* DROP TABLE t */ SELECT 'DROP' FROM t"),
            RequestClass::Read
        );
        // A batch is its most serious statement.
        assert_eq!(
            class("BEGIN BATCH INSERT INTO t (a) VALUES (1); APPLY BATCH"),
            RequestClass::Write
        );
        assert_eq!(
            class(
                "BEGIN BATCH INSERT INTO t (a) VALUES ('APPLY'); DELETE FROM t WHERE a = 2; APPLY BATCH"
            ),
            RequestClass::Write
        );
    }

    #[test]
    fn use_is_refused_before_it_is_sent() {
        let refused = refuse_statement("  use shop").unwrap();
        assert_eq!(refused.position, 2);
        assert!(
            refused
                .message
                .contains("choose the keyspace above the editor")
        );
        assert!(refuse_statement("SELECT * FROM used").is_none());
        assert!(refuse_statement("-- use x\nSELECT 1 FROM t").is_none());
    }

    #[test]
    fn names_are_quoted_as_cql_folds_them() {
        let d = CqlDialect;
        assert_eq!(d.quote_identifier("users"), "users");
        assert_eq!(d.quote_identifier("user_2"), "user_2");
        assert_eq!(d.quote_identifier("Users"), "\"Users\"");
        assert_eq!(d.quote_identifier("table"), "\"table\"");
        assert_eq!(d.quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(d.quote_identifier("2x"), "\"2x\"");
        assert_eq!(
            browse_statement(&d, "shop", "Orders"),
            "SELECT * FROM shop.\"Orders\" LIMIT 200"
        );
    }

    #[test]
    fn code_is_what_lies_outside_strings_names_bodies_and_comments() {
        let script = "SELECT 'a' FROM \"b\" -- c\n/* d */ WHERE $$e$$";
        let code: String = CqlDialect
            .code_ranges(script)
            .into_iter()
            .map(|r| &script[r])
            .collect();
        assert_eq!(code, "SELECT  FROM  \n WHERE ");
    }
}
