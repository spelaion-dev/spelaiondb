use std::ops::Range;

/// Byte range of a statement within a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementSpan {
    pub start: usize,
    pub end: usize,
}

impl StatementSpan {
    pub fn text<'a>(&self, script: &'a str) -> &'a str {
        &script[self.start..self.end]
    }
}

/// What the core and the UI need to know about an engine's SQL.
pub trait Dialect: Send + Sync {
    fn name(&self) -> &'static str;

    /// Splits a script into statements, without the trailing `;` and skipping statements that
    /// are empty or contain only comments. Must respect quotes, comments and engine-specific
    /// blocks (for example `$$ … $$` in PostgreSQL).
    fn split_statements(&self, script: &str) -> Vec<StatementSpan>;

    /// The data that follows the statement at `span` in `script` and belongs to it, as its
    /// engine's client sends it apart from the statement: PostgreSQL's `COPY … FROM STDIN`,
    /// whose rows follow on the next lines up to a line `\.` (SDB-162). The splitter skips
    /// that data, so it is never a statement, never searched for placeholders
    /// ([`Dialect::code_ranges`] leaves it out) and never read by the guard. `None` for a
    /// statement without any (every statement of most engines).
    fn copy_data(&self, _script: &str, _span: &StatementSpan) -> Option<Range<usize>> {
        None
    }

    /// Quotes an identifier for use in generated SQL.
    fn quote_identifier(&self, ident: &str) -> String;

    /// This dialect for a session whose `sql_mode` (MySQL, MariaDB) changes how SQL text is
    /// read: under `NO_BACKSLASH_ESCAPES` a `\` is an ordinary character in a string, and under
    /// `ANSI_QUOTES` `"…"` is a name. `sql_mode` is the server's comma-separated list. `None`
    /// for a dialect the mode does not concern; a dialect it concerns answers for every mode,
    /// the default included.
    fn with_sql_mode(&self, _sql_mode: &str) -> Option<std::sync::Arc<dyn Dialect>> {
        None
    }

    /// The `sql_mode` this dialect reads text with, when [`Dialect::with_sql_mode`] made it for
    /// a mode that reads text otherwise than the default. `None`: the engine's default reading.
    fn sql_mode(&self) -> Option<String> {
        None
    }

    /// The byte ranges of `script` that are code: outside its strings, quoted names and
    /// comments, read as this engine's own splitter reads them (SQL Server's `[names]`,
    /// PostgreSQL's `E'…\'…'` strings, nested block comments, `$tag$` bodies…). What looks for
    /// something in the code, such as the editor's `:name` placeholders, looks only there.
    /// The default reads standard SQL: `'…'` and `"…"` with doubled quotes, `--` and `/* */`.
    fn code_ranges(&self, script: &str) -> Vec<Range<usize>> {
        code_ranges(script, standard_skip)
    }

    /// The first word of `statement` in lower case, read by this dialect's lexical scanner
    /// (ADR-065): after white space and comments, the letters its first code starts with; empty
    /// when a string, a quoted name or a sign comes first. The guard decides from it when the
    /// SQL parser cannot read a statement. `None` for a dialect whose readers have not moved
    /// onto a scanner: the guard then reads the statement through [`Dialect::code_ranges`].
    fn first_word(&self, _statement: &str) -> Option<String> {
        None
    }

    /// Whether this is a language that is not SQL (ADR-056), whose statements become requests
    /// ([`Dialect::to_request`]).
    fn reads_requests(&self) -> bool {
        false
    }

    /// For an engine whose language is not SQL (ADR-056): the canonical request `statement`
    /// is, run in `database`, which is what the core checks and what the plugin is sent
    /// ([`crate::Connection::execute_request`]). `None` for an engine queried with SQL.
    fn to_request(
        &self,
        _statement: &str,
        _database: &str,
    ) -> Option<Result<serde_json::Value, RequestError>> {
        None
    }

    /// The statement the explorer runs to show `object`, for a language that is not SQL
    /// (ADR-056, point 5): bounded, and visible in an editor tab as what it is. The object is
    /// as the driver listed it (its kind and comment may say how to read it: a Redis key's
    /// type). `None` for SQL, whose objects open in a table data tab.
    fn browse_statement(&self, _object: &crate::ObjectInfo) -> Option<String> {
        None
    }

    /// What the guard makes of a request [`Dialect::to_request`] wrote (ADR-056, point 4).
    /// Read from the request, not the text, so every way of writing it is judged alike.
    fn request_class(&self, _request: &serde_json::Value) -> RequestClass {
        RequestClass::Unknown
    }

    /// Whether this is SQL, which the core writes statements in (catalog queries, object
    /// actions, `SELECT current_schema()`). False for a language of requests and for one shaped
    /// like SQL that is not (CQL), which say so.
    fn is_sql(&self) -> bool {
        !self.reads_requests()
    }

    /// For a language shaped like SQL that `sqlparser` does not read (ADR-056: CQL, PartiQL,
    /// Cypher…), whose statements are sent as text: what the guard makes of `statement`, from
    /// lists of its own. `None` for SQL, which the core's classifier reads, and for a language
    /// of requests, which [`Dialect::request_class`] reads.
    fn statement_class(&self, _statement: &str) -> Option<RequestClass> {
        None
    }

    /// For such a language: why `statement` is not sent at all (it would change what the
    /// session knows of itself, as CQL's `USE` does), or `None` to send it.
    fn refuse_statement(&self, _statement: &str) -> Option<RequestError> {
        None
    }

    /// Whether this is the standard reading of an engine the core has no dialect for
    /// (ADR-079): standard SQL's lexical rules, which some servers leave. The guard then asks
    /// before a statement holding text such a server may read otherwise (point 6). False for
    /// every engine with a dialect of its own.
    fn standard_reading(&self) -> bool {
        false
    }
}

/// Why a statement of a language that is not SQL could not be read (ADR-056): nothing is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    /// Byte offset in the statement where reading stopped.
    pub position: usize,
    pub message: String,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What a request is to the guard (ADR-056, point 4). Lists compiled into the core decide it;
/// whatever they do not know is [`RequestClass::Unknown`], never a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// Reads and changes nothing.
    Read,
    /// Changes data, with a filter.
    Write,
    /// Deletes every document (or row) of a collection: an empty filter.
    DeleteAll,
    /// Changes every document of a collection: an empty filter.
    UpdateAll,
    /// Drops, or replaces whole, a collection, an index or a database.
    Drop,
    /// The core cannot tell (a command it does not list, code that runs on the server).
    Unknown,
}

/// The ranges of `script` between what `skip` skips: `skip(script, i)` is the byte after a
/// string, a quoted name or a comment that starts at `i`, or `None` when none starts there.
/// Each driver builds [`Dialect::code_ranges`] from its splitter's own pieces with it.
pub fn code_ranges(script: &str, skip: impl Fn(&str, usize) -> Option<usize>) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < script.len() {
        match skip(script, i) {
            Some(end) => {
                if start < i {
                    ranges.push(start..i);
                }
                // Always forward, and on a character boundary.
                let mut end = end.clamp(i + 1, script.len());
                while !script.is_char_boundary(end) {
                    end += 1;
                }
                i = end;
                start = end;
            }
            None => i += 1,
        }
    }
    if start < script.len() {
        ranges.push(start..script.len());
    }
    ranges
}

/// Standard SQL's strings, quoted names and comments.
fn standard_skip(script: &str, i: usize) -> Option<usize> {
    let b = script.as_bytes();
    let doubled = |quote: u8| {
        let mut j = i + 1;
        while j < b.len() {
            if b[j] == quote {
                if b.get(j + 1) == Some(&quote) {
                    j += 2;
                    continue;
                }
                return j + 1;
            }
            j += 1;
        }
        b.len()
    };
    match *b.get(i)? {
        q @ (b'\'' | b'"') => Some(doubled(q)),
        b'-' if b.get(i + 1) == Some(&b'-') => Some(
            b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |p| i + p + 1),
        ),
        b'/' if b.get(i + 1) == Some(&b'*') => Some(
            script[i + 2..]
                .find("*/")
                .map_or(b.len(), |p| i + 2 + p + 2),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Standard;
    impl Dialect for Standard {
        fn name(&self) -> &'static str {
            "standard"
        }
        fn split_statements(&self, _: &str) -> Vec<StatementSpan> {
            Vec::new()
        }
        fn quote_identifier(&self, ident: &str) -> String {
            ident.into()
        }
    }

    fn code(script: &str) -> Vec<&str> {
        Standard
            .code_ranges(script)
            .into_iter()
            .map(|r| &script[r])
            .collect()
    }

    #[test]
    fn code_is_what_lies_outside_strings_names_and_comments() {
        assert_eq!(
            code("select 'a''b', \"c\" -- d\nfrom /* e */ t"),
            ["select ", ", ", " ", "from ", " t"]
        );
        assert_eq!(code("x 'never closed"), ["x "]);
        assert_eq!(code("'ñ' é"), [" é"]);
        assert!(code("").is_empty());
    }

    #[test]
    fn a_skip_that_does_not_move_forward_still_ends() {
        let ranges = code_ranges("abc", |_, i| (i == 1).then_some(0));
        assert_eq!(ranges, [0..1, 2..3]);
    }
}
