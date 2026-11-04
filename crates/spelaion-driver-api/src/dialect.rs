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
}
