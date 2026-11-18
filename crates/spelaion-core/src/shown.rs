//! The databases a connection shows (SDB-396): a server with hundreds of databases, of which
//! the user works with a few. The connection file names them (`databases`); every list of
//! databases the session gives (the database picker's, and for MySQL and MariaDB, whose
//! databases are the explorer's schemas, every list of schemas) keeps only those, matched
//! without regard to case, and always the connection's own database.
//!
//! It is what is shown, never a permission: a statement may still name another database, as
//! the server allows. Applied in the session's [`crate::health::Tracked`] connection, so the
//! explorer, the palette, metadata search, the data search's table picker and agents' lists all
//! see the same databases.

use spelaion_driver_api::SchemaInfo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShownDatabases {
    names: Vec<String>,
    /// MySQL's and MariaDB's schemas are its databases: filtered too.
    schemas_are_databases: bool,
}

impl ShownDatabases {
    /// `names` as the file lists them; `own` is the connection's database, always shown.
    pub(crate) fn new(names: &[String], own: &str, schemas_are_databases: bool) -> Self {
        let mut names: Vec<String> = names.iter().map(|n| n.trim().to_string()).collect();
        let own = own.trim();
        if !own.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(own)) {
            names.push(own.to_string());
        }
        Self {
            names,
            schemas_are_databases,
        }
    }

    fn shows(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.eq_ignore_ascii_case(name))
    }

    pub(crate) fn databases(&self, mut all: Vec<String>) -> Vec<String> {
        all.retain(|name| self.shows(name));
        all
    }

    pub(crate) fn schemas(&self, mut all: Vec<SchemaInfo>) -> Vec<SchemaInfo> {
        if self.schemas_are_databases {
            all.retain(|schema| self.shows(&schema.name));
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str, system: bool) -> SchemaInfo {
        SchemaInfo {
            name: name.into(),
            system,
        }
    }

    #[test]
    fn only_the_named_databases_and_the_connection_s_own_are_kept() {
        let shown = ShownDatabases::new(&["Sales".into(), "hr".into()], "app", true);
        let all = ["app", "sales", "HR", "other", "information_schema"].map(String::from);
        assert_eq!(shown.databases(all.to_vec()), ["app", "sales", "HR"]);
        let schemas = vec![
            schema("information_schema", true),
            schema("sales", false),
            schema("other", false),
        ];
        assert_eq!(shown.schemas(schemas), [schema("sales", false)]);
    }

    #[test]
    fn schemas_that_are_not_databases_are_all_kept() {
        let shown = ShownDatabases::new(&["sales".into()], "", false);
        let schemas = vec![schema("dbo", false), schema("audit", false)];
        assert_eq!(shown.schemas(schemas.clone()), schemas);
        assert_eq!(
            shown.databases(vec!["master".into(), "sales".into()]),
            ["sales"]
        );
    }
}
