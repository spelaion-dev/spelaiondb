//! SQL written for a table from the explorer: preview, SELECT, INSERT, UPDATE and DELETE,
//! with the engine's identifier quoting. Values are `:named` placeholders to replace before
//! running; UPDATE and DELETE always carry a WHERE (on the primary key when there is one).

use futures_util::StreamExt;
use spelunk_driver_api::{Dialect, TableInfo, Value};

use crate::session::{CoreEvent, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlTemplate {
    /// `SELECT * … LIMIT 100`, what a double-click opens.
    Preview,
    Select,
    Insert,
    Update,
    Delete,
}

/// How many rows the preview asks for.
const PREVIEW_ROWS: u32 = 100;

pub fn table_sql(dialect: &dyn Dialect, table: &TableInfo, template: SqlTemplate) -> String {
    let name = qualified(dialect, &table.object.schema, &table.object.name);
    let quote = |c: &str| dialect.quote_identifier(c);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    // T-SQL has no LIMIT: `SELECT TOP n`.
    let sqlserver = dialect.name() == "sqlserver";
    match template {
        SqlTemplate::Preview if sqlserver => format!("SELECT TOP {PREVIEW_ROWS} *\nFROM {name};\n"),
        SqlTemplate::Preview => format!("SELECT *\nFROM {name}\nLIMIT {PREVIEW_ROWS};\n"),
        SqlTemplate::Select if sqlserver => format!(
            "SELECT TOP {PREVIEW_ROWS}\n{}\nFROM {name};\n",
            list(columns.iter().map(|c| quote(c)), ",\n")
        ),
        SqlTemplate::Select => format!(
            "SELECT\n{}\nFROM {name}\nLIMIT {PREVIEW_ROWS};\n",
            list(columns.iter().map(|c| quote(c)), ",\n")
        ),
        SqlTemplate::Insert => format!(
            "INSERT INTO {name} (\n{}\n) VALUES (\n{}\n);\n",
            list(columns.iter().map(|c| quote(c)), ",\n"),
            list(columns.iter().map(|c| placeholder(c)), ",\n")
        ),
        SqlTemplate::Update => {
            let keys = keys(table);
            let set: Vec<&str> = columns
                .iter()
                .copied()
                .filter(|c| !table.primary_key.iter().any(|k| k == c))
                .collect();
            // A table that is all key still gets a valid statement.
            let set = if set.is_empty() { columns.clone() } else { set };
            format!(
                "UPDATE {name}\nSET\n{}\n{}",
                list(
                    set.iter()
                        .map(|c| format!("{} = {}", quote(c), placeholder(c))),
                    ",\n"
                ),
                where_clause(dialect, &keys, table.primary_key.is_empty())
            )
        }
        SqlTemplate::Delete => format!(
            "DELETE FROM {name}\n{}",
            where_clause(dialect, &keys(table), table.primary_key.is_empty())
        ),
    }
}

/// The exact number of rows of a table or view (`SELECT COUNT(*)`), run on `session`; the
/// result is freed right away. Counting a big table can take a while: it can be cancelled.
pub async fn count_rows(session: &Session, schema: &str, name: &str) -> Result<i64, String> {
    let sql = format!(
        "SELECT COUNT(*) FROM {}",
        qualified(session.dialect().as_ref(), schema, name)
    );
    scalar_count(session, &sql).await
}

/// Runs one statement written by the core and returns the first cell of its result: `None`
/// when it returns no result (`KILL`) or no rows. The result is freed, also on failure.
pub(crate) async fn scalar_value(session: &Session, sql: &str) -> Result<Option<Value>, String> {
    let mut events = session.run_script(sql);
    let mut results = Vec::new();
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result: id, .. } => results.push(id),
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ScriptFinished { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    let store = session.results();
    let value = results.first().and_then(|id| {
        let stored = store.get(*id)?;
        let stored = stored.read().ok()?;
        let batch = stored.window(0, 1).into_iter().next()?;
        let logical = stored.schema().columns.first()?.logical;
        Value::from_array(batch.column(0).as_ref(), 0, logical)
    });
    for id in results {
        store.remove(id);
    }
    match failure {
        Some(message) => Err(message),
        None => Ok(value),
    }
}

/// Runs a `SELECT COUNT(*)` written by the core and returns the number; the result is freed.
pub(crate) async fn scalar_count(session: &Session, sql: &str) -> Result<i64, String> {
    match scalar_value(session, sql).await? {
        Some(Value::Int(n)) => Ok(n),
        Some(Value::Decimal(text) | Value::Text(text)) => text
            .parse()
            .map_err(|_| format!("unexpected count: {text}")),
        None => Err("the count returned no result".into()),
        other => Err(format!("unexpected count: {other:?}")),
    }
}

/// `schema.table`, leaving out the implicit `main` of SQLite and DuckDB.
pub(crate) fn qualified(dialect: &dyn Dialect, schema: &str, name: &str) -> String {
    if schema.is_empty() || (matches!(dialect.name(), "sqlite" | "duckdb") && schema == "main") {
        dialect.quote_identifier(name)
    } else {
        format!(
            "{}.{}",
            dialect.quote_identifier(schema),
            dialect.quote_identifier(name)
        )
    }
}

/// The columns that identify a row: the primary key, or every column without one.
fn keys(table: &TableInfo) -> Vec<&str> {
    if table.primary_key.is_empty() {
        table.columns.iter().map(|c| c.name.as_str()).collect()
    } else {
        table.primary_key.iter().map(String::as_str).collect()
    }
}

fn where_clause(dialect: &dyn Dialect, keys: &[&str], no_primary_key: bool) -> String {
    let conditions = keys
        .iter()
        .map(|k| format!("{} = {}", dialect.quote_identifier(k), placeholder(k)))
        .collect::<Vec<_>>()
        .join("\n  AND ");
    let note = if no_primary_key {
        "-- No primary key: every column identifies the row.\n"
    } else {
        ""
    };
    format!("{note}WHERE {conditions};\n")
}

/// `:name`, with anything but letters, digits and `_` turned into `_`.
fn placeholder(column: &str) -> String {
    let name: String = column
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(":{name}")
}

fn list(items: impl Iterator<Item = impl AsRef<str>>, separator: &str) -> String {
    items
        .map(|i| format!("  {}", i.as_ref()))
        .collect::<Vec<_>>()
        .join(separator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_driver_api::{ColumnInfo, ObjectInfo};
    use spelunk_driver_sqlite::SqliteDialect;

    /// Backtick quoting like MySQL's, without depending on that driver.
    struct Backticks;
    impl Dialect for Backticks {
        fn name(&self) -> &'static str {
            "mysql"
        }
        fn split_statements(&self, _: &str) -> Vec<spelunk_driver_api::StatementSpan> {
            Vec::new()
        }
        fn quote_identifier(&self, ident: &str) -> String {
            format!("`{}`", ident.replace('`', "``"))
        }
    }

    fn table(schema: &str, primary_key: &[&str]) -> TableInfo {
        let column = |name: &str| ColumnInfo {
            name: name.into(),
            native_type: "text".into(),
            nullable: true,
            default: None,
            comment: None,
        };
        TableInfo {
            object: ObjectInfo {
                schema: schema.into(),
                name: "order lines".into(),
                kind: spelunk_driver_api::ObjectKind::Table,
                estimated_rows: None,
                comment: None,
            },
            columns: vec![column("id"), column("unit price"), column("qty")],
            primary_key: primary_key.iter().map(|k| k.to_string()).collect(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    #[test]
    fn templates_quote_for_the_engine() {
        let t = table("shop", &["id"]);
        assert_eq!(
            table_sql(&Backticks, &t, SqlTemplate::Preview),
            "SELECT *\nFROM `shop`.`order lines`\nLIMIT 100;\n"
        );
        assert_eq!(
            table_sql(&Backticks, &t, SqlTemplate::Insert),
            "INSERT INTO `shop`.`order lines` (\n  `id`,\n  `unit price`,\n  `qty`\n) VALUES (\n  :id,\n  :unit_price,\n  :qty\n);\n"
        );
        assert_eq!(
            table_sql(&Backticks, &t, SqlTemplate::Update),
            "UPDATE `shop`.`order lines`\nSET\n  `unit price` = :unit_price,\n  `qty` = :qty\nWHERE `id` = :id;\n"
        );
        assert_eq!(
            table_sql(&SqliteDialect, &table("main", &["id"]), SqlTemplate::Select),
            "SELECT\n  id,\n  \"unit price\",\n  qty\nFROM \"order lines\"\nLIMIT 100;\n"
        );
    }

    #[test]
    fn without_a_primary_key_every_column_identifies_the_row() {
        let sql = table_sql(&Backticks, &table("shop", &[]), SqlTemplate::Delete);
        assert_eq!(
            sql,
            "DELETE FROM `shop`.`order lines`\n-- No primary key: every column identifies the row.\nWHERE `id` = :id\n  AND `unit price` = :unit_price\n  AND `qty` = :qty;\n"
        );
        // Generated writes never trip the "without WHERE" guard.
        assert!(crate::script_risks(&SqliteDialect, &sql.replace('`', "\""), false).is_empty());
    }
}
