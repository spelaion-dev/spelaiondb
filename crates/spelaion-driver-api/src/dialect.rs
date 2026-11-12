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
