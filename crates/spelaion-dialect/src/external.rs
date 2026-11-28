use std::ops::Range;
use std::sync::Arc;

use spelaion_driver_api::{Dialect, ObjectInfo, RequestClass, RequestError, StatementSpan};

/// The name every engine of a plugin that is not part of SpelaionDB is read under (ADR-041,
/// ADR-055): `engine = "external"`. No barrier of the core answers to it.
pub const EXTERNAL: &str = "external";

/// How the core reads a script of an engine served by someone else's plugin. Its manifest names
/// one of the core's dialects, or none (ADR-041, point 5):
///
/// - **None:** standard SQL's lexical rules, as for the Flight SQL engines (ADR-079).
/// - **One of the core's:** that dialect's splitter, quoting and readers, so a script is split
///   as the plugin's server expects. Only the reading is borrowed, never the name: this dialect
///   is always called `external`, and the core chooses its barriers, its server families and
///   the SQL it writes by a dialect's name. A plugin that says it speaks PostgreSQL's SQL gets
///   PostgreSQL's splitter and nothing PostgreSQL's barriers would vouch for.
pub struct ExternalDialect {
    reads_as: Arc<dyn Dialect>,
}

impl ExternalDialect {
    /// The dialect of an engine whose manifest names `reads_as` (one of the core's dialects,
    /// by the engine name [`crate::for_engine`] knows it by), or standard SQL for none. `None`
    /// for a name the core has no dialect for, or for `external` itself.
    pub fn reading_as(reads_as: Option<&str>) -> Option<Self> {
        let reads_as: Arc<dyn Dialect> = match reads_as {
            None => Arc::new(crate::StandardDialect::named(EXTERNAL)),
            Some(EXTERNAL) => return None,
            Some(name) => crate::for_engine(name)?,
        };
        Some(Self { reads_as })
    }
}

impl Dialect for ExternalDialect {
    fn name(&self) -> &'static str {
        EXTERNAL
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        self.reads_as.split_statements(script)
    }

    fn copy_data(&self, script: &str, span: &StatementSpan) -> Option<Range<usize>> {
        self.reads_as.copy_data(script, span)
    }

    fn quote_identifier(&self, ident: &str) -> String {
        self.reads_as.quote_identifier(ident)
    }

    // `with_sql_mode` is left out on purpose: it would give back the borrowed dialect under its
    // own name.

    fn code_ranges(&self, script: &str) -> Vec<Range<usize>> {
        self.reads_as.code_ranges(script)
    }

    fn first_word(&self, statement: &str) -> Option<String> {
        self.reads_as.first_word(statement)
    }

    fn reads_requests(&self) -> bool {
        self.reads_as.reads_requests()
    }

    fn to_request(
        &self,
        statement: &str,
        database: &str,
    ) -> Option<Result<serde_json::Value, RequestError>> {
        self.reads_as.to_request(statement, database)
    }

    fn browse_statement(&self, object: &ObjectInfo) -> Option<String> {
        self.reads_as.browse_statement(object)
    }

    fn request_class(&self, request: &serde_json::Value) -> RequestClass {
        self.reads_as.request_class(request)
    }

    fn is_sql(&self) -> bool {
        self.reads_as.is_sql()
    }

    fn statement_class(&self, statement: &str) -> Option<RequestClass> {
        self.reads_as.statement_class(statement)
    }

    /// A client command line (ADR-078) is resolved only by the editor's run: never sent,
    /// whatever the borrowed dialect says.
    fn refuse_statement(&self, statement: &str) -> Option<RequestError> {
        crate::refuse_client_command(statement)
            .or_else(|| self.reads_as.refuse_statement(statement))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_borrows_the_reading_and_never_the_name() {
        let postgres = ExternalDialect::reading_as(Some("postgres")).unwrap();
        assert_eq!(postgres.name(), EXTERNAL);
        // PostgreSQL's splitter: a dollar-quoted body is one statement.
        let script = "do $$ begin; end $$; select 1";
        let parts: Vec<&str> = postgres
            .split_statements(script)
            .iter()
            .map(|span| span.text(script))
            .collect();
        assert_eq!(parts, ["do $$ begin; end $$", "select 1"]);
        // The MySQL modes would give MySQL's dialect back under its own name.
        let mysql = ExternalDialect::reading_as(Some("mysql")).unwrap();
        assert!(mysql.with_sql_mode("ANSI_QUOTES").is_none());
        assert_eq!(mysql.quote_identifier("a"), "`a`");
    }

    #[test]
    fn none_is_standard_sql_and_an_unknown_name_is_no_dialect() {
        let standard = ExternalDialect::reading_as(None).unwrap();
        assert_eq!(standard.name(), EXTERNAL);
        assert_eq!(standard.quote_identifier("a\"b"), "\"a\"\"b\"");
        for unknown in ["external", "foodb", "", "POSTGRES"] {
            assert!(
                ExternalDialect::reading_as(Some(unknown)).is_none(),
                "{unknown}"
            );
        }
    }

    #[test]
    fn a_client_command_is_never_sent() {
        let dialect = ExternalDialect::reading_as(Some("postgres")).unwrap();
        assert!(dialect.refuse_statement("@set a = 1").is_some());
    }
}
