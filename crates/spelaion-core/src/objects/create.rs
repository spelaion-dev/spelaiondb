//! The create table form (ADR-047): columns, a primary key, foreign keys and indexes become one
//! script, written here in the engine's syntax, shown in full and applied only as it was shown.

use spelaion_config::Safeguards;
use spelaion_driver_api::Dialect;

use super::alter::{AlterationPlan, Step, run_plan};
use super::ddl::{self, Flavor};
use super::{ObjectError, qualified_name};
use crate::session::Session;

/// A column of the new table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

/// A foreign key of the new table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewForeignKey {
    /// Empty: named after the table and its columns.
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
}

/// An index of the new table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewIndex {
    /// Empty: named after the table and its columns.
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// What the create table form asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<NewColumn>,
    /// The primary key's columns, in order; empty for none.
    pub primary_key: Vec<String>,
    pub foreign_keys: Vec<NewForeignKey>,
    pub indexes: Vec<NewIndex>,
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn nstring(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// The script that creates the table, its indexes and its comments.
pub fn create_table_plan(
    dialect: &dyn Dialect,
    table: &NewTable,
) -> Result<AlterationPlan, ObjectError> {
    let engine = dialect.name();
    let flavor = if engine == "mysql" {
        Flavor::Mysql
    } else {
        Flavor::Sqlite
    };
    let quote = |name: &str| dialect.quote_identifier(name);
    let invalid = |what: String| Err(ObjectError::Unsupported(what));
    let name = table.name.trim();
    if name.is_empty() {
        return Err(ObjectError::NoName);
    }
    if table.columns.is_empty() {
        return invalid("add a column or more".into());
    }
    let target = qualified_name(dialect, &table.schema, &[name.to_string()]);
    let mut names: Vec<&str> = Vec::new();
    let mut elements = Vec::new();
    for column in &table.columns {
        let column_name = column.name.trim();
        if column_name.is_empty() {
            return invalid("every column needs a name".into());
        }
        if names.iter().any(|n| n.eq_ignore_ascii_case(column_name)) {
            return invalid(format!("{column_name} is there twice"));
        }
        names.push(column_name);
        let ty = column.data_type.trim();
        if !ddl::is_single_expression(ty, flavor) {
            return invalid(format!(
                "the type of {column_name} must be one type, such as varchar(80)"
            ));
        }
        let mut element = format!("{} {ty}", quote(column_name));
        if !column.nullable {
            element.push_str(" NOT NULL");
        }
        if let Some(default) = column
            .default
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            if !ddl::is_single_expression(default, flavor) {
                return invalid(format!(
                    "the default of {column_name} must be one SQL expression, such as 0 or 'text'"
                ));
            }
            element.push_str(&format!(" DEFAULT {default}"));
        }
        let comment = column
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());
        if comment.is_some() && engine == "sqlite" {
            return invalid("SQLite columns have no comments".into());
        }
        if let (Some(comment), "mysql") = (comment, engine) {
            element.push_str(&format!(
                " COMMENT {}",
                ddl::mysql_string(comment, crate::sql_mode::backslash_escapes(dialect))
            ));
        }
        elements.push(element);
    }
    let known = |columns: &[String]| -> Result<String, ObjectError> {
        if columns.is_empty() {
            return Err(ObjectError::NoColumns);
        }
        for column in columns {
            if !names.iter().any(|n| n == column) {
                return Err(ObjectError::Unsupported(format!(
                    "{column} is not a column of the new table"
                )));
            }
        }
        Ok(columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(", "))
    };
    let named = |given: &str, columns: &[String], suffix: &str| {
        let given = given.trim();
        if given.is_empty() {
            format!("{name}_{}_{suffix}", columns.join("_"))
        } else {
            given.to_string()
        }
    };
    if !table.primary_key.is_empty() {
        let list = known(&table.primary_key)?;
        // MySQL and MariaDB call every primary key PRIMARY: a name would be ignored.
        if engine == "mysql" {
            elements.push(format!("PRIMARY KEY ({list})"));
        } else {
            elements.push(format!(
                "CONSTRAINT {} PRIMARY KEY ({list})",
                quote(&format!("{name}_pkey"))
            ));
        }
    }
    for key in &table.foreign_keys {
        let list = known(&key.columns)?;
        if key.columns.len() != key.referenced_columns.len() {
            return invalid("choose the same number of columns and referenced columns".into());
        }
        let referenced_table = key.referenced_table.trim();
        if referenced_table.is_empty() || key.referenced_columns.iter().any(|c| c.trim().is_empty())
        {
            return invalid("choose the referenced table and its columns".into());
        }
        // SQLite's keys point to a table of the same database: it takes no schema there.
        let referenced = if engine == "sqlite" {
            quote(referenced_table)
        } else {
            qualified_name(
                dialect,
                key.referenced_schema.trim(),
                &[referenced_table.to_string()],
            )
        };
        let columns = key
            .referenced_columns
            .iter()
            .map(|c| quote(c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        elements.push(format!(
            "CONSTRAINT {} FOREIGN KEY ({list}) REFERENCES {referenced} ({columns})",
            quote(&named(&key.name, &key.columns, "fkey"))
        ));
    }
    let mut steps = vec![Step::Run(format!(
        "CREATE TABLE {target} (\n  {}\n)",
        elements.join(",\n  ")
    ))];
    for index in &table.indexes {
        let list = known(&index.columns)?;
        let index_name = named(
            &index.name,
            &index.columns,
            if index.unique { "key" } else { "idx" },
        );
        let unique = if index.unique { "UNIQUE " } else { "" };
        // SQLite names the index's schema, never the table's.
        let (index_name, on) = if engine == "sqlite" {
            (
                qualified_name(dialect, &table.schema, &[index_name]),
                quote(name),
            )
        } else {
            (quote(&index_name), target.clone())
        };
        steps.push(Step::Run(format!(
            "CREATE {unique}INDEX {index_name} ON {on} ({list})"
        )));
    }
    for column in &table.columns {
        let Some(comment) = column
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        match engine {
            "postgres" | "duckdb" => steps.push(Step::Run(format!(
                "COMMENT ON COLUMN {} IS {}",
                qualified_name(
                    dialect,
                    &table.schema,
                    &[name.to_string(), column.name.trim().to_string()]
                ),
                literal(comment)
            ))),
            "sqlserver" => steps.push(Step::Run(format!(
                "EXEC sp_addextendedproperty @name = N'MS_Description', @value = {}, @level0type = N'SCHEMA', @level0name = {}, @level1type = N'TABLE', @level1name = {}, @level2type = N'COLUMN', @level2name = {}",
                nstring(comment),
                nstring(&table.schema),
                nstring(name),
                nstring(column.name.trim())
            ))),
            // MySQL writes it in the column; SQLite has none (refused above).
            _ => {}
        }
    }
    let begin = match engine {
        "mysql" => None,
        _ if steps.len() < 2 => None,
        "sqlserver" => Some("BEGIN TRANSACTION"),
        _ => Some("BEGIN"),
    };
    Ok(AlterationPlan {
        before: Vec::new(),
        steps,
        after: Vec::new(),
        begin,
        note: None,
        rebuild: false,
    })
}

/// Creates the table of a form, never on a read-only connection, and only as the script the
/// user was shown; statements the connection's type asks about run only confirmed.
pub async fn apply_create_table(
    session: &Session,
    table: &NewTable,
    shown: &str,
    read_only: bool,
    safeguards: &Safeguards,
    confirmed: bool,
) -> Result<usize, ObjectError> {
    if read_only {
        return Err(ObjectError::ReadOnly);
    }
    let dialect = session.dialect();
    let plan = create_table_plan(dialect.as_ref(), table)?;
    if plan.script() != shown {
        return Err(ObjectError::Changed);
    }
    if !confirmed && !plan.risks(dialect.as_ref(), safeguards).is_empty() {
        return Err(ObjectError::NeedsConfirmation);
    }
    run_plan(session, &plan).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_dialect::MysqlDialect;
    use spelaion_dialect::PostgresDialect;
    use spelaion_dialect::SqliteDialect;

    fn table(columns: &[(&str, &str)]) -> NewTable {
        NewTable {
            schema: "s".into(),
            name: "t".into(),
            columns: columns
                .iter()
                .map(|(name, ty)| NewColumn {
                    name: (*name).into(),
                    data_type: (*ty).into(),
                    nullable: true,
                    default: None,
                    comment: None,
                })
                .collect(),
            primary_key: Vec::new(),
            foreign_keys: Vec::new(),
            indexes: Vec::new(),
        }
    }

    #[test]
    fn one_statement_needs_no_transaction_and_names_come_from_the_table() {
        let mut t = table(&[("id", "int"), ("Code", "varchar(8)")]);
        t.primary_key = vec!["id".into()];
        t.indexes = vec![NewIndex {
            name: String::new(),
            columns: vec!["Code".into()],
            unique: true,
        }];
        assert_eq!(
            create_table_plan(&PostgresDialect, &t).unwrap().script(),
            "BEGIN;\nCREATE TABLE s.t (\n  id int,\n  \"Code\" varchar(8),\n  CONSTRAINT t_pkey PRIMARY KEY (id)\n);\nCREATE UNIQUE INDEX \"t_Code_key\" ON s.t (\"Code\");\nCOMMIT;\n"
        );
        // MySQL names every primary key PRIMARY, and runs each statement on its own.
        t.indexes.clear();
        assert_eq!(
            create_table_plan(&MysqlDialect, &t).unwrap().script(),
            "CREATE TABLE `s`.`t` (\n  `id` int,\n  `Code` varchar(8),\n  PRIMARY KEY (`id`)\n);\n"
        );
    }

    #[test]
    fn sqlite_keys_point_to_a_table_of_the_same_database() {
        let mut t = table(&[("parent_id", "INTEGER")]);
        t.schema = "main".into();
        t.foreign_keys = vec![NewForeignKey {
            name: "fk".into(),
            columns: vec!["parent_id".into()],
            referenced_schema: "main".into(),
            referenced_table: "parent".into(),
            referenced_columns: vec!["id".into()],
        }];
        assert!(
            create_table_plan(&SqliteDialect, &t)
                .unwrap()
                .script()
                .contains("CONSTRAINT fk FOREIGN KEY (parent_id) REFERENCES parent (id)")
        );
    }

    #[test]
    fn what_cannot_make_a_table_says_why() {
        let bad = |t: NewTable| create_table_plan(&PostgresDialect, &t).is_err();
        assert!(bad(table(&[])));
        assert!(bad(table(&[("a", "int"), ("A", "text")])));
        assert!(bad(table(&[("a", "int; DROP TABLE x")])));
        assert!(bad(table(&[("", "int")])));
        let mut t = table(&[("a", "int")]);
        t.name = " ".into();
        assert_eq!(
            create_table_plan(&PostgresDialect, &t),
            Err(ObjectError::NoName)
        );
        let mut t = table(&[("a", "int")]);
        t.primary_key = vec!["b".into()];
        assert!(bad(t));
        let mut t = table(&[("a", "int")]);
        t.columns[0].default = Some("1); DROP TABLE x; --".into());
        assert!(bad(t));
        let mut t = table(&[("a", "int")]);
        t.foreign_keys = vec![NewForeignKey {
            name: String::new(),
            columns: vec!["a".into()],
            referenced_schema: "s".into(),
            referenced_table: "p".into(),
            referenced_columns: vec!["x".into(), "y".into()],
        }];
        assert!(bad(t));
    }
}
