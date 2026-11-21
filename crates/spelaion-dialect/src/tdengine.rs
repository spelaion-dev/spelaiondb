//! TDengine's SQL, as the core reads it (ADR-056): a language shaped like SQL whose statements
//! are sent as they are written, but which `sqlparser` does not read (super tables,
//! `INSERT … USING … TAGS`, `SHOW`, cluster commands). Its splitter, quoting and guard lists are its
//! own, here.
//!
//! What is read:
//! - Statements end at `;`, outside `'…'` and `"…"` strings (a backslash escapes the next
//!   character), `` `…` `` names, `--` line comments and `/* */` comments.
//! - The guard reads each statement's first words ([`statement_class`]): `SELECT`, `SHOW`,
//!   `DESCRIBE` and `EXPLAIN` read; `INSERT`, `CREATE` and `ALTER` write; a `DELETE` without a
//!   `WHERE` deletes every row of its table; `DROP`, `ALTER … DROP`, `KILL` and the cluster's own
//!   commands (`COMPACT`, `BALANCE`, `REDISTRIBUTE`, `ALTER DNODE`…) drop; users, permissions and
//!   anything else are unknown, never a read.
//! - `USE` is refused before it is sent ([`refuse_statement`]): the tab's database is chosen above
//!   the editor, and the plugin runs every statement in it.

use spelaion_driver_api::{Dialect, RequestClass, RequestError, StatementSpan, code_ranges};

/// TDengine's SQL.
#[derive(Debug, Clone, Copy, Default)]
pub struct TdengineDialect;

impl Dialect for TdengineDialect {
    fn name(&self) -> &'static str {
        "tdengine"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    /// A name in backticks, a backtick doubled.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
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

/// How many rows opening a table shows: never the whole of it.
pub const BROWSE_LIMIT: u32 = 200;

/// The statement that shows a table's (or a super table's) first rows, in its own database.
pub fn browse_statement(dialect: &TdengineDialect, database: &str, table: &str) -> String {
    format!(
        "SELECT * FROM {}.{} LIMIT {BROWSE_LIMIT}",
        dialect.quote_identifier(database),
        dialect.quote_identifier(table)
    )
}

// ------------------------------------------------------------------------------ splitting

/// Where a string, a quoted name or a comment that starts at `i` ends, or `None`.
fn skip(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match b[i] {
        quote @ (b'\'' | b'"') => {
            let mut j = i + 1;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 2,
                    c if c == quote => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(b.len())
        }
        b'`' => {
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == b'`' {
                    if b.get(j + 1) == Some(&b'`') {
                        j += 2;
                        continue;
                    }
                    return Some(j + 1);
                }
                j += 1;
            }
            Some(b.len())
        }
        b'-' if b.get(i + 1) == Some(&b'-') => {
            Some(script[i..].find('\n').map_or(b.len(), |end| i + end))
        }
        b'/' if b.get(i + 1) == Some(&b'*') => Some(
            script[i + 2..]
                .find("*/")
                .map_or(b.len(), |end| i + 2 + end + 2),
        ),
        _ => None,
    }
}

/// The words of `text`'s code, uppercased, in order.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for range in code_ranges(text, skip) {
        let mut word = String::new();
        for c in text[range].chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                word.push(c.to_ascii_uppercase());
            } else if !word.is_empty() {
                out.push(std::mem::take(&mut word));
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
    // A statement starts at its first code: TDengine refuses an `INSERT` with a comment before
    // it (its insert parser reads `INSERT` first), so leading comments are left out of what is
    // sent.
    let push = |from: usize, to: usize, spans: &mut Vec<StatementSpan>| {
        if words(&script[from..to]).is_empty() {
            return;
        }
        let mut start = from;
        loop {
            // White space by the one rule of every splitter (SDB-434).
            while start < to && crate::is_white_space_at(script, start) {
                start += 1;
            }
            let comment = matches!(&b[start..to], [b'-', b'-', ..] | [b'/', b'*', ..]);
            match skip(script, start).filter(|_| comment) {
                Some(end) => start = end.min(to),
                None => break,
            }
        }
        let body = script[start..to].trim_end();
        spans.push(StatementSpan {
            start,
            end: start + body.len(),
        });
    };
    while i < b.len() {
        if let Some(end) = skip(script, i) {
            i = end.max(i + 1);
            continue;
        }
        if b[i] == b';' {
            push(start, i, &mut spans);
            start = i + 1;
        }
        i += 1;
    }
    push(start, b.len(), &mut spans);
    spans
}

// ------------------------------------------------------------------------------ the guard

/// The cluster's own commands: they move, compact or reconfigure what holds the data.
const CLUSTER: &[&str] = &[
    "COMPACT",
    "BALANCE",
    "REDISTRIBUTE",
    "SPLIT",
    "MERGE",
    "TRIM",
    "RESTORE",
];

/// What the guard makes of one TDengine statement, from its first words.
pub fn statement_class(statement: &str) -> RequestClass {
    let words = words(statement);
    let word = |n: usize| words.get(n).map(String::as_str).unwrap_or("");
    match word(0) {
        "SELECT" | "SHOW" | "DESCRIBE" | "DESC" | "EXPLAIN" => RequestClass::Read,
        "INSERT" => RequestClass::Write,
        // `DELETE FROM t` without a `WHERE` empties the table (all its child tables, for a super
        // table).
        "DELETE" => {
            if words.iter().any(|w| w == "WHERE") {
                RequestClass::Write
            } else {
                RequestClass::DeleteAll
            }
        }
        "TRUNCATE" => RequestClass::DeleteAll,
        "DROP" | "KILL" => RequestClass::Drop,
        w if CLUSTER.contains(&w) => RequestClass::Drop,
        "ALTER" => match word(1) {
            "USER" => RequestClass::Unknown,
            // A node's or the whole cluster's configuration.
            "DNODE" | "ALL" | "LOCAL" | "CLUSTER" => RequestClass::Drop,
            _ if words.iter().any(|w| w == "DROP") => RequestClass::Drop,
            _ => RequestClass::Write,
        },
        "CREATE" => match word(1) {
            "USER" => RequestClass::Unknown,
            // `CREATE OR REPLACE FUNCTION` replaces one whole.
            "OR" => RequestClass::Drop,
            _ => RequestClass::Write,
        },
        "FLUSH" | "RESET" => RequestClass::Write,
        // `GRANT`, `REVOKE`, `USE` (refused anyway) and anything else.
        _ => RequestClass::Unknown,
    }
}

/// What the core will not send: `USE`, which would move the session's database behind its back.
pub fn refuse_statement(statement: &str) -> Option<RequestError> {
    if words(statement).first().is_some_and(|w| w == "USE") {
        let position = statement.len() - statement.trim_start().len();
        return Some(RequestError {
            position,
            message: "`USE` is not run here: choose the database above the editor".into(),
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
    fn statements_end_at_semicolons_outside_strings_names_and_comments() {
        let script = "SELECT * FROM t WHERE a = 'x;y\\'z';\n\
                      -- a comment; with a semicolon\n\
                      INSERT INTO `d;1` USING meters TAGS (\"SF;\", 2) VALUES (now, 1.5);\n\
                      /* block; */ SHOW DATABASES";
        // A statement starts at its first code: the comments before it are not sent (TDengine
        // refuses an `INSERT` after one).
        assert_eq!(
            texts(script),
            [
                "SELECT * FROM t WHERE a = 'x;y\\'z'",
                "INSERT INTO `d;1` USING meters TAGS (\"SF;\", 2) VALUES (now, 1.5)",
                "SHOW DATABASES",
            ]
        );
        assert_eq!(
            texts("-- one\n/* two */ -- three\n  INSERT INTO t VALUES (now, 1)"),
            ["INSERT INTO t VALUES (now, 1)"]
        );
        assert!(texts(";; -- only\n;").is_empty());
    }

    #[test]
    fn the_guard_reads_the_first_words() {
        let class = statement_class;
        for read in [
            "SELECT * FROM meters",
            "show databases",
            "DESCRIBE meters",
            "EXPLAIN SELECT 1",
        ] {
            assert_eq!(class(read), RequestClass::Read, "{read}");
        }
        for write in [
            "INSERT INTO d1001 USING meters TAGS ('SF', 2) VALUES (now, 1.5, 220)",
            "CREATE STABLE meters (ts TIMESTAMP, v DOUBLE) TAGS (loc VARCHAR(20))",
            "CREATE DATABASE power",
            "ALTER STABLE meters ADD TAG groupid INT",
            "DELETE FROM d1001 WHERE ts < '2024-01-01'",
            "FLUSH DATABASE power",
        ] {
            assert_eq!(class(write), RequestClass::Write, "{write}");
        }
        assert_eq!(class("DELETE FROM meters"), RequestClass::DeleteAll);
        for drop in [
            "DROP DATABASE power",
            "drop stable meters",
            "ALTER STABLE meters DROP TAG groupid",
            "KILL QUERY '3:4'",
            "COMPACT DATABASE power",
            "BALANCE VGROUP",
            "ALTER DNODE 1 'debugFlag' '143'",
            "ALTER ALL DNODES 'monitor' '1'",
        ] {
            assert_eq!(class(drop), RequestClass::Drop, "{drop}");
        }
        for unknown in [
            "GRANT ALL ON power.* TO u",
            "CREATE USER u PASS 'x'",
            "ALTER USER u PASS 'y'",
            "NOSUCH thing",
            "",
        ] {
            assert_eq!(class(unknown), RequestClass::Unknown, "{unknown}");
        }
        // A word in a string or a comment is not the statement's.
        assert_eq!(
            class("/* DROP */ SELECT 'DROP DATABASE x' FROM t"),
            RequestClass::Read
        );
        assert_eq!(class("DELETE FROM t -- WHERE x"), RequestClass::DeleteAll);
    }

    #[test]
    fn use_is_refused_before_it_is_sent() {
        let refused = refuse_statement(" use power").unwrap();
        assert_eq!(refused.position, 1);
        assert!(
            refused
                .message
                .contains("choose the database above the editor")
        );
        assert!(refuse_statement("SELECT * FROM used").is_none());
    }

    #[test]
    fn names_are_in_backticks() {
        let d = TdengineDialect;
        assert_eq!(d.quote_identifier("meters"), "`meters`");
        assert_eq!(d.quote_identifier("a`b"), "`a``b`");
        assert_eq!(
            browse_statement(&d, "power", "meters"),
            "SELECT * FROM `power`.`meters` LIMIT 200"
        );
        assert!(!d.is_sql());
    }
}
