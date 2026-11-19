//! A relation's properties and dependencies (SDB-393), from SQLite's catalog. Each transport
//! (a local file, a server's `sqlite3`, a libSQL server) gives a function that runs one query
//! and returns its rows; the queries and what is made of their rows are the same everywhere.
//!
//! SQLite keeps no dependency catalog, so what is read is what it does keep:
//! - foreign keys, both ways (`pragma_foreign_key_list`);
//! - the triggers on a table or a view (`sqlite_schema.tbl_name`);
//! - the tables a view reads, from the program SQLite compiles for it (`EXPLAIN`): every table
//!   or index it opens, by its root page. A view over another view is compiled into the tables
//!   that one reads, so it is listed as using those tables, and which views use a view is not
//!   known.
//!
//! What a trigger's body touches is not recorded anywhere, so triggers are always among the
//! untracked kinds.

use std::collections::BTreeSet;
use std::future::Future;

use spelaion_driver_api::{
    Dependencies, DependencyInfo, DependencyKind, DriverError, ObjectProperty, Result, property,
};

use crate::catalog::{
    Row, foreign_keys_sql, integer, literal, optional_text, referencing_keys_sql, schema_table,
    text,
};

/// The most views whose programs are read to find the ones that use a table: past it, views
/// are among the untracked kinds.
pub const MAX_VIEWS: usize = 1_000;

/// What `name` is in `sqlite_schema`, and its text.
fn relation_sql(schema: &str, name: &str) -> String {
    format!(
        "select type, sql from {} where name = {} collate nocase and type in ('table', 'view')",
        schema_table(schema),
        literal(name)
    )
}

fn missing(schema: &str, name: &str) -> DriverError {
    DriverError::Other(format!("there is no table or view {schema}.{name}"))
}

/// A table's properties: a virtual table's module (`engine`), and the options after its
/// columns (`WITHOUT ROWID`, `STRICT`). A view has none.
pub async fn properties<F, Fut>(
    schema: &str,
    name: &str,
    mut query: F,
) -> Result<Vec<ObjectProperty>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<Row>>>,
{
    let rows = query(relation_sql(schema, name)).await?;
    let row = rows.first().ok_or_else(|| missing(schema, name))?;
    if text(row, 0) != "table" {
        return Ok(Vec::new());
    }
    Ok(table_properties(&text(row, 1)))
}

/// What a table's `CREATE` text says besides its columns.
pub fn table_properties(sql: &str) -> Vec<ObjectProperty> {
    let mut out = Vec::new();
    let words: Vec<String> = sql
        .split_whitespace()
        .take(6)
        .map(str::to_ascii_uppercase)
        .collect();
    if words.get(1).map(String::as_str) == Some("VIRTUAL") {
        // CREATE VIRTUAL TABLE [IF NOT EXISTS] name USING module(...)
        let upper = sql.to_ascii_uppercase();
        if let Some(at) = upper.find(" USING ") {
            let module: String = sql[at + 7..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !module.is_empty() {
                out.push(ObjectProperty {
                    name: property::ENGINE.into(),
                    value: module,
                });
            }
        }
        return out;
    }
    let Some(close) = sql.rfind(')') else {
        return out;
    };
    let options: Vec<String> = sql[close + 1..]
        .split(',')
        .map(|option| {
            option
                .split_whitespace()
                .map(str::to_ascii_uppercase)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|option| option == "WITHOUT ROWID" || option == "STRICT")
        .collect();
    if !options.is_empty() {
        out.push(ObjectProperty {
            name: property::OPTIONS.into(),
            value: options.join(", "),
        });
    }
    out
}

/// Every table, index, view and trigger of a schema, with what a program opens it by.
fn catalog_sql(schema: &str) -> String {
    format!(
        "select name, type, tbl_name, rootpage from {} \
         where type in ('table', 'index', 'view', 'trigger') order by name",
        schema_table(schema)
    )
}

/// The position of a schema in the connection's list (`main` is 0), which a program names it by.
fn database_sql(schema: &str) -> String {
    format!(
        "select seq from pragma_database_list where name = {}",
        literal(schema)
    )
}

/// The program SQLite compiles to read a view.
fn explain_sql(schema: &str, view: &str) -> String {
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    format!("explain select * from {}.{}", quote(schema), quote(view))
}

struct Entry {
    name: String,
    kind: String,
    table: String,
    root: i64,
}

/// The tables (by name, as `sqlite_schema` writes them) a view's program opens in `database`.
fn tables_read(program: &[Row], database: i64, catalog: &[Entry]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for step in program {
        let opcode = text(step, 1);
        if !matches!(opcode.as_str(), "OpenRead" | "OpenWrite" | "ReopenIdx") {
            continue;
        }
        let (root, db) = (integer(step, 3), integer(step, 4));
        if db != database || root <= 0 {
            continue;
        }
        if let Some(entry) = catalog.iter().find(|e| e.root == root) {
            match entry.kind.as_str() {
                "table" => {
                    out.insert(entry.name.clone());
                }
                // An index is read for its table.
                "index" => {
                    out.insert(entry.table.clone());
                }
                _ => {}
            }
        }
    }
    out
}

/// One foreign key per id of `pragma_foreign_key_list`'s rows (`id`, `table`, `from`, `to`):
/// the referenced table and `(from) → (to)`.
fn keys(rows: &[Row]) -> Vec<(i64, String, Vec<String>, Vec<String>)> {
    let mut out: Vec<(i64, String, Vec<String>, Vec<String>)> = Vec::new();
    for row in rows {
        let id = integer(row, 0);
        if out.last().is_none_or(|key| key.0 != id) {
            out.push((id, text(row, 1), Vec::new(), Vec::new()));
        }
        let key = out.last_mut().expect("just added");
        key.2.push(text(row, 2));
        if let Some(to) = optional_text(row, 3) {
            key.3.push(to);
        }
    }
    out
}

fn arrow(from: &[String], to: &[String]) -> String {
    if to.is_empty() {
        format!("({})", from.join(", "))
    } else {
        format!("({}) → ({})", from.join(", "), to.join(", "))
    }
}

fn kind_order(kind: DependencyKind) -> u8 {
    match kind {
        DependencyKind::Table => 0,
        DependencyKind::View => 1,
        DependencyKind::MaterializedView => 2,
        DependencyKind::ForeignKey => 3,
        DependencyKind::Trigger => 4,
        DependencyKind::Function => 5,
        DependencyKind::Procedure => 6,
        DependencyKind::Sequence => 7,
        DependencyKind::Type => 8,
        DependencyKind::Index => 9,
        DependencyKind::Other => 10,
    }
}

fn sorted(mut list: Vec<DependencyInfo>) -> Vec<DependencyInfo> {
    list.sort_by(|a, b| {
        (kind_order(a.kind), &a.schema, &a.name, &a.detail).cmp(&(
            kind_order(b.kind),
            &b.schema,
            &b.name,
            &b.detail,
        ))
    });
    list.dedup();
    list
}

/// What the table or view `schema.name` uses and what uses it, as far as SQLite records it
/// (the module says how far).
pub async fn dependencies<F, Fut>(
    schema: &str,
    name: &str,
    mut query: F,
) -> Result<Option<Dependencies>>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Vec<Row>>>,
{
    let catalog: Vec<Entry> = query(catalog_sql(schema))
        .await?
        .iter()
        .map(|row| Entry {
            name: text(row, 0),
            kind: text(row, 1),
            table: text(row, 2),
            root: integer(row, 3),
        })
        .collect();
    let target = catalog
        .iter()
        .find(|e| e.name.eq_ignore_ascii_case(name) && matches!(e.kind.as_str(), "table" | "view"))
        .ok_or_else(|| missing(schema, name))?;
    let (target_name, is_view) = (target.name.clone(), target.kind == "view");
    let database = query(database_sql(schema))
        .await?
        .first()
        .map(|row| integer(row, 0))
        .ok_or_else(|| DriverError::Other(format!("there is no schema {schema}")))?;
    let info = |name: &str, kind, detail: Option<String>| DependencyInfo {
        schema: schema.to_string(),
        name: name.to_string(),
        kind,
        detail,
    };
    let mut uses = Vec::new();
    let mut used_by = Vec::new();
    let mut untracked = vec![DependencyKind::Trigger];

    if is_view {
        // A view that no longer compiles (its table was dropped) says why.
        let program = query(explain_sql(schema, &target_name)).await?;
        for table in tables_read(&program, database, &catalog) {
            uses.push(info(&table, DependencyKind::Table, None));
        }
        // Which views read this one is compiled away into the tables it reads.
        untracked.push(DependencyKind::View);
    } else {
        for (id, table, from, to) in keys(&query(foreign_keys_sql(schema, &target_name)).await?) {
            uses.push(info(
                &table,
                DependencyKind::Table,
                Some(format!("fk_{id} {}", arrow(&from, &to))),
            ));
        }
        // Rows: the referencing table, the key's id, its column and the referenced one.
        let mut referencing: Vec<(String, i64, Vec<String>)> = Vec::new();
        for row in query(referencing_keys_sql(schema, &target_name)).await? {
            let (table, id) = (text(&row, 0), integer(&row, 1));
            if referencing
                .last()
                .is_none_or(|(t, i, _)| *t != table || *i != id)
            {
                referencing.push((table, id, Vec::new()));
            }
            referencing
                .last_mut()
                .expect("just added")
                .2
                .push(text(&row, 2));
        }
        for (table, id, columns) in referencing {
            used_by.push(info(
                &table,
                DependencyKind::ForeignKey,
                Some(format!("fk_{id} ({})", columns.join(", "))),
            ));
        }
        let views: Vec<&Entry> = catalog.iter().filter(|e| e.kind == "view").collect();
        if views.len() > MAX_VIEWS {
            untracked.push(DependencyKind::View);
        } else {
            for view in views {
                // A view that does not compile reads nothing that can be told.
                let Ok(program) = query(explain_sql(schema, &view.name)).await else {
                    continue;
                };
                if tables_read(&program, database, &catalog)
                    .iter()
                    .any(|table| table.eq_ignore_ascii_case(&target_name))
                {
                    used_by.push(info(&view.name, DependencyKind::View, None));
                }
            }
        }
    }
    for trigger in catalog
        .iter()
        .filter(|e| e.kind == "trigger" && e.table.eq_ignore_ascii_case(&target_name))
    {
        used_by.push(info(
            &trigger.name,
            DependencyKind::Trigger,
            Some(format!("on {}", trigger.table)),
        ));
    }
    Ok(Some(Dependencies {
        uses: sorted(uses),
        used_by: sorted(used_by),
        untracked,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_options_and_modules_are_read_from_the_create_text() {
        let options = |sql: &str| {
            table_properties(sql)
                .into_iter()
                .map(|p| (p.name, p.value))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            options("CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT) WITHOUT ROWID, STRICT"),
            [("options".to_string(), "WITHOUT ROWID, STRICT".to_string())]
        );
        assert_eq!(
            options("create table t(a, check (a > (1)))  strict"),
            [("options".to_string(), "STRICT".to_string())]
        );
        assert!(options("CREATE TABLE t (a)").is_empty());
        assert_eq!(
            options("CREATE VIRTUAL TABLE docs USING fts5(title, body)"),
            [("engine".to_string(), "fts5".to_string())]
        );
    }

    #[test]
    fn a_view_program_reads_tables_by_their_root_pages() {
        let catalog = vec![
            Entry {
                name: "orders".into(),
                kind: "table".into(),
                table: "orders".into(),
                root: 2,
            },
            Entry {
                name: "orders_customer".into(),
                kind: "index".into(),
                table: "orders".into(),
                root: 5,
            },
            Entry {
                name: "customer".into(),
                kind: "table".into(),
                table: "customer".into(),
                root: 3,
            },
        ];
        let step = |opcode: &str, root: i64, db: i64| {
            vec![
                crate::SqlValue::Integer(0),
                crate::SqlValue::Text(opcode.into()),
                crate::SqlValue::Integer(0),
                crate::SqlValue::Integer(root),
                crate::SqlValue::Integer(db),
            ]
        };
        let program = vec![
            step("Init", 0, 0),
            step("OpenRead", 5, 0),
            step("OpenRead", 3, 1),
            step("Column", 2, 0),
        ];
        // The index is read for its table; another database's page 3 is not this customer.
        assert_eq!(
            tables_read(&program, 0, &catalog)
                .into_iter()
                .collect::<Vec<_>>(),
            ["orders"]
        );
    }
}
