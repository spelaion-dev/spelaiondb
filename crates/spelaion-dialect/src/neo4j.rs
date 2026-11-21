//! Neo4j's Cypher, read by the core (ADR-056): a language shaped like SQL, with statements
//! separated by `;`, strings in `'…'` or `"…"` with backslash escapes, names in backticks
//! (`` ` `` doubled inside), and `//` and `/* */` comments.
//!
//! The canonical request is the statement as typed, with the database it runs in:
//! `{"database": "neo4j", "statement": "MATCH (n) RETURN n LIMIT 10"}`. The guard reads the
//! statement's words outside strings, names and comments: `DELETE` and `DETACH DELETE` remove
//! nodes and relationships, `DROP` drops an index, a constraint or a database, the clauses that
//! create or change data write, and a procedure call (`CALL db.…`, APOC) is unknown, since the
//! core cannot see what it does.

use serde_json::{Value, json};
use spelaion_driver_api::{
    Dialect, ObjectInfo, ObjectKind, RequestClass, RequestError, StatementSpan,
};

use crate::{is_white_space_at, trimmed};

pub struct Neo4jDialect;

impl Dialect for Neo4jDialect {
    fn name(&self) -> &'static str {
        "neo4j"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    /// Always in backticks, doubled inside: label, type and property names may hold anything.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        spelaion_driver_api::code_ranges(script, skip_non_code)
    }

    fn reads_requests(&self) -> bool {
        true
    }

    fn to_request(&self, statement: &str, database: &str) -> Option<Result<Value, RequestError>> {
        Some(to_request(statement, database))
    }

    fn request_class(&self, request: &Value) -> RequestClass {
        request_class(request)
    }

    fn browse_statement(&self, object: &ObjectInfo) -> Option<String> {
        browse_statement(object)
    }
}

/// The request a statement runs as: its text, trimmed, and the database.
pub fn to_request(statement: &str, database: &str) -> Result<Value, RequestError> {
    let statement = statement.trim();
    if statement.is_empty() {
        return Err(RequestError {
            position: 0,
            message: "there is no statement".into(),
        });
    }
    Ok(json!({ "database": database, "statement": statement }))
}

/// The bounded statement the explorer runs to show a label's nodes or a type's relationships.
pub fn browse_statement(object: &ObjectInfo) -> Option<String> {
    let name = format!("`{}`", object.name.replace('`', "``"));
    match object.kind {
        ObjectKind::NodeLabel => Some(format!("MATCH (n:{name}) RETURN n LIMIT 200")),
        ObjectKind::RelationshipType => Some(format!("MATCH ()-[r:{name}]->() RETURN r LIMIT 200")),
        ObjectKind::Table
        | ObjectKind::PartitionedTable
        | ObjectKind::View
        | ObjectKind::MaterializedView
        | ObjectKind::ForeignTable
        | ObjectKind::Collection => None,
    }
}

/// The words of a statement outside strings, names and comments, in upper case.
fn words(statement: &str) -> Vec<String> {
    let b = statement.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(end) = skip_non_code(statement, i) {
            i = end.max(i + 1);
            continue;
        }
        if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            out.push(statement[start..i].to_ascii_uppercase());
            continue;
        }
        i += 1;
    }
    out
}

/// What the guard makes of a request (ADR-056, point 4). Unknown is never a read.
pub fn request_class(request: &Value) -> RequestClass {
    let Some(statement) = request["statement"].as_str() else {
        return RequestClass::Unknown;
    };
    let words = words(statement);
    let has = |w: &str| words.iter().any(|x| x == w);
    if has("DELETE") {
        return RequestClass::DeleteAll;
    }
    if has("DROP") {
        return RequestClass::Drop;
    }
    // A procedure can do anything (write, drop, call out): the core cannot tell.
    if has("CALL")
        && words
            .windows(2)
            .any(|w| w[0] == "CALL" && w[1] != "{" && !w[1].is_empty() && w[1].contains('.'))
    {
        return RequestClass::Unknown;
    }
    if [
        "CREATE",
        "MERGE",
        "SET",
        "REMOVE",
        "LOAD",
        "FOREACH",
        "ALTER",
        "GRANT",
        "REVOKE",
        "DENY",
        "START",
        "STOP",
        "TERMINATE",
    ]
    .iter()
    .any(|w| has(w))
    {
        return RequestClass::Write;
    }
    match words.first().map(String::as_str) {
        Some(
            "MATCH" | "OPTIONAL" | "RETURN" | "WITH" | "UNWIND" | "SHOW" | "EXPLAIN" | "PROFILE"
            | "USE",
        ) => RequestClass::Read,
        // A subquery block, a function call or anything else the core does not list.
        _ => RequestClass::Unknown,
    }
}

/// The byte after the string, name or comment that starts at `i`; `None` when none starts
/// there.
pub fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    match *b.get(i)? {
        q @ (b'\'' | b'"') => {
            let mut j = i + 1;
            while j < b.len() {
                match b[j] {
                    b'\\' => j += 2,
                    c if c == q => return Some(j + 1),
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
        b'/' if b.get(i + 1) == Some(&b'/') => Some(
            b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |n| i + n),
        ),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(
            b[i + 2..]
                .windows(2)
                .position(|w| w == b"*/")
                .map_or(b.len(), |n| i + 2 + n + 2),
        ),
        _ => None,
    }
}

fn split(script: &str) -> Vec<StatementSpan> {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let (mut start, mut has_code, mut i) = (0, false, 0);
    while i < b.len() {
        if let Some(end) = skip_non_code(script, i) {
            if !matches!(b[i], b'/') {
                has_code = true;
            }
            i = end.max(i + 1);
            continue;
        }
        if b[i] == b';' {
            if has_code {
                spans.extend(trimmed(script, start, i));
            }
            i += 1;
            start = i;
            has_code = false;
            continue;
        }
        if !is_white_space_at(script, i) {
            has_code = true;
        }
        i += 1;
    }
    if has_code {
        spans.extend(trimmed(script, start, b.len()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(script: &str) -> Vec<&str> {
        Neo4jDialect
            .split_statements(script)
            .iter()
            .map(|s| s.text(script))
            .collect()
    }

    fn class(statement: &str) -> RequestClass {
        request_class(&to_request(statement, "neo4j").unwrap())
    }

    #[test]
    fn statements_end_at_semicolons_outside_strings_names_and_comments() {
        assert_eq!(
            statements(
                "MATCH (n {name: 'a;b'}) RETURN n; // c;\nMATCH (`x;y`) RETURN \"it\\\"s;\"; /* ; */"
            ),
            [
                "MATCH (n {name: 'a;b'}) RETURN n",
                "// c;\nMATCH (`x;y`) RETURN \"it\\\"s;\""
            ]
        );
        assert_eq!(statements(" ; // nothing\n"), Vec::<&str>::new());
    }

    #[test]
    fn the_request_is_the_statement_and_its_database() {
        assert_eq!(
            to_request("  MATCH (n) RETURN n  ", "movies").unwrap(),
            json!({"database": "movies", "statement": "MATCH (n) RETURN n"})
        );
        assert!(to_request("   ", "neo4j").is_err());
    }

    #[test]
    fn the_guard_reads_the_clauses() {
        assert_eq!(
            class("MATCH (n:Person) RETURN n.name LIMIT 10"),
            RequestClass::Read
        );
        assert_eq!(class("SHOW DATABASES"), RequestClass::Read);
        assert_eq!(class("RETURN 1"), RequestClass::Read);
        assert_eq!(class("MATCH (n) DETACH DELETE n"), RequestClass::DeleteAll);
        assert_eq!(
            class("MATCH (n:T) WHERE n.id = 1 DELETE n"),
            RequestClass::DeleteAll
        );
        assert_eq!(class("DROP INDEX person_name"), RequestClass::Drop);
        assert_eq!(class("DROP DATABASE other"), RequestClass::Drop);
        assert_eq!(
            class("CREATE (n:Person {name: 'Ana'})"),
            RequestClass::Write
        );
        assert_eq!(class("MATCH (n) SET n.seen = true"), RequestClass::Write);
        assert_eq!(class("MERGE (n:T {id: 1})"), RequestClass::Write);
        assert_eq!(
            class("LOAD CSV FROM 'file:///x.csv' AS row CREATE (:R)"),
            RequestClass::Write
        );
        assert_eq!(class("CALL db.labels()"), RequestClass::Unknown);
        assert_eq!(
            class("CALL apoc.periodic.iterate('a', 'b', {})"),
            RequestClass::Unknown
        );
        // Words in strings, names and comments are not clauses.
        assert_eq!(
            class("MATCH (n {note: 'delete me'}) RETURN n.`drop` // DETACH DELETE"),
            RequestClass::Read
        );
        // A subquery that deletes still deletes.
        assert_eq!(
            class("MATCH (n) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS"),
            RequestClass::DeleteAll
        );
    }

    #[test]
    fn labels_and_types_open_with_a_bounded_statement() {
        let object = |name: &str, kind| ObjectInfo {
            schema: "neo4j".into(),
            name: name.into(),
            kind,
            estimated_rows: None,
            comment: None,
        };
        assert_eq!(
            browse_statement(&object("Per`son", ObjectKind::NodeLabel)).unwrap(),
            "MATCH (n:`Per``son`) RETURN n LIMIT 200"
        );
        assert_eq!(
            browse_statement(&object("KNOWS", ObjectKind::RelationshipType)).unwrap(),
            "MATCH ()-[r:`KNOWS`]->() RETURN r LIMIT 200"
        );
        assert!(browse_statement(&object("t", ObjectKind::Table)).is_none());
    }

    #[test]
    fn names_are_quoted_with_backticks() {
        assert_eq!(Neo4jDialect.quote_identifier("a`b"), "`a``b`");
    }
}
