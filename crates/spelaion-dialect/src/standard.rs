use spelaion_driver_api::{Dialect, StatementSpan};

/// Standard SQL's lexical rules, under an engine's own name: what the core reads a script of an
/// engine with as long as it knows nothing more of its SQL (the Flight SQL engines, which may be
/// any server). `'…'` strings and `"…"` names with doubled quotes, `--` and block comments: the
/// same splitter as Trino's.
///
/// The name is the engine's, never another's: the core's barriers are chosen by dialect name,
/// and an engine read this way has none (ADR-041).
pub struct StandardDialect {
    name: &'static str,
}

impl StandardDialect {
    pub const fn named(name: &'static str) -> Self {
        Self { name }
    }
}

impl Dialect for StandardDialect {
    fn name(&self) -> &'static str {
        self.name
    }

    /// A client command line (ADR-078) is resolved only by the editor's run: never sent.
    fn refuse_statement(&self, statement: &str) -> Option<spelaion_driver_api::RequestError> {
        crate::refuse_client_command(statement)
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        crate::trino::split(script)
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        crate::trino::code_ranges(script)
    }

    /// Always in double quotes, a `"` doubled.
    fn quote_identifier(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    /// The reading the guard knows servers may leave (ADR-079, point 6).
    fn standard_reading(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_splits_as_standard_sql_under_its_own_name() {
        let d = StandardDialect::named("flightsql");
        assert_eq!(d.name(), "flightsql");
        let script = "select 'a;b'; select \"c;d\" -- e;\n; select 3";
        let parts: Vec<&str> = d
            .split_statements(script)
            .iter()
            .map(|s| s.text(script))
            .collect();
        assert_eq!(parts, ["select 'a;b'", "select \"c;d\" -- e;", "select 3"]);
        assert_eq!(d.quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
