//! SQL written for a table from the explorer: preview, SELECT (all rows, by key, or by a list of
//! keys), INSERT, an upsert and MERGE where the engine has them, UPDATE and DELETE, with the
//! engine's identifier quoting. Values are `:named`
//! placeholders to replace before running; UPDATE and DELETE always carry a WHERE on the key
//! that identifies a row: the primary key, else a unique constraint, else every column.

use futures_util::StreamExt;
use spelaion_driver_api::{ConstraintKind, Dialect, TableInfo, Value};

use crate::session::{CoreEvent, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlTemplate {
    /// `SELECT * … LIMIT 100`, what a double-click opens.
    Preview,
    Select,
    /// `SELECT … WHERE key = :key`: one row by its key.
    SelectByKey,
    /// `SELECT … WHERE key IN (…)`: several rows by their keys.
    SelectIn,
    Insert,
    /// Insert, or update the row with the same key: `INSERT … ON CONFLICT … DO UPDATE`
    /// (PostgreSQL, SQLite, DuckDB) or `INSERT … ON DUPLICATE KEY UPDATE` (MySQL, MariaDB).
    Upsert,
    /// `MERGE INTO … USING (SELECT :values) …` (PostgreSQL, SQL Server, DuckDB).
    Merge,
    Update,
    /// `UPDATE … FROM (SELECT :values) AS source WHERE key matches`, in each engine's form.
    UpdateFrom,
    Delete,
    /// `DELETE … USING (SELECT :keys) AS source WHERE key matches` (not SQLite).
    DeleteUsing,
}

/// What the explorer offers for a table on this engine (a view only gets `Select`).
pub fn table_templates(dialect: &dyn Dialect) -> Vec<SqlTemplate> {
    let mut templates = vec![
        SqlTemplate::Select,
        SqlTemplate::SelectByKey,
        SqlTemplate::SelectIn,
        SqlTemplate::Insert,
    ];
    if has_upsert(dialect) {
        templates.push(SqlTemplate::Upsert);
    }
    if has_merge(dialect) {
        templates.push(SqlTemplate::Merge);
    }
    templates.extend([
        SqlTemplate::Update,
        SqlTemplate::UpdateFrom,
        SqlTemplate::Delete,
    ]);
    if has_delete_using(dialect) {
        templates.push(SqlTemplate::DeleteUsing);
    }
    templates
}

/// SQLite's DELETE takes one table only.
fn has_delete_using(dialect: &dyn Dialect) -> bool {
    dialect.name() != "sqlite"
}

fn has_upsert(dialect: &dyn Dialect) -> bool {
    matches!(dialect.name(), "postgres" | "sqlite" | "duckdb" | "mysql")
}

fn has_merge(dialect: &dyn Dialect) -> bool {
    matches!(dialect.name(), "postgres" | "sqlserver" | "duckdb")
}

/// How many `:key_n` placeholders `SelectIn` writes.
const IN_LIST: usize = 3;

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
        SqlTemplate::SelectByKey => {
            let key = row_key(table);
            format!(
                "SELECT\n{}\nFROM {name}\n{}",
                list(columns.iter().map(|c| quote(c)), ",\n"),
                where_clause(dialect, &key)
            )
        }
        SqlTemplate::SelectIn => {
            let key = row_key(table);
            format!(
                "SELECT\n{}\nFROM {name}\n{}{};\n",
                list(columns.iter().map(|c| quote(c)), ",\n"),
                key.note(dialect),
                in_list(dialect, &key.columns)
            )
        }
        SqlTemplate::Insert => format!(
            "INSERT INTO {name} (\n{}\n) VALUES (\n{}\n);\n",
            list(columns.iter().map(|c| quote(c)), ",\n"),
            list(columns.iter().map(|c| placeholder(c)), ",\n")
        ),
        // SQL Server's upsert is MERGE; MySQL, MariaDB and SQLite have no MERGE.
        SqlTemplate::Upsert if !has_upsert(dialect) => merge(dialect, table),
        SqlTemplate::Merge if !has_merge(dialect) => upsert(dialect, table),
        SqlTemplate::Upsert => upsert(dialect, table),
        SqlTemplate::Merge => merge(dialect, table),
        SqlTemplate::Update => {
            let key = row_key(table);
            let set: Vec<&str> = columns
                .iter()
                .copied()
                .filter(|c| !key.columns.contains(c))
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
                where_clause(dialect, &key)
            )
        }
        SqlTemplate::Delete => format!(
            "DELETE FROM {name}\n{}",
            where_clause(dialect, &row_key(table))
        ),
        SqlTemplate::UpdateFrom => update_from(dialect, table),
        // Without DELETE … USING, a plain DELETE by key.
        SqlTemplate::DeleteUsing if !has_delete_using(dialect) => format!(
            "DELETE FROM {name}\n{}",
            where_clause(dialect, &row_key(table))
        ),
        SqlTemplate::DeleteUsing => delete_using(dialect, table),
    }
}

/// Words an alias must not be (they mean something where an alias goes).
const NOT_ALIASES: &[&str] = &[
    "as", "at", "by", "do", "go", "if", "in", "is", "no", "of", "on", "or", "to", "and", "end",
    "for", "not", "set", "all", "any", "asc", "key", "use", "add", "top",
];

/// Short aliases from the tables' names: the initials of their words (`order_lines` → `ol`),
/// numbered when two would be the same.
fn aliases(tables: &[TableInfo]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, table) in tables.iter().enumerate() {
        let initials: String = table
            .object
            .name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter_map(|word| word.chars().next())
            .filter(char::is_ascii_alphabetic)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        let base = if initials.is_empty() || NOT_ALIASES.contains(&initials.as_str()) {
            format!("t{}", i + 1)
        } else {
            initials
        };
        let mut alias = base.clone();
        let mut n = 2;
        while out.contains(&alias) {
            alias = format!("{base}{n}");
            n += 1;
        }
        out.push(alias);
    }
    out
}

/// `b.x = a.y AND …` for the foreign keys between table `a` and table `b`, either way.
fn links(
    dialect: &dyn Dialect,
    tables: &[TableInfo],
    aliases: &[String],
    a: usize,
    b: usize,
) -> Option<String> {
    let quote = |c: &str| dialect.quote_identifier(c);
    let refers = |from: usize, to: usize| -> Vec<String> {
        let target = &tables[to].object;
        tables[from]
            .foreign_keys
            .iter()
            .filter(|fk| {
                fk.referenced_table == target.name
                    && (fk.referenced_schema.is_empty() || fk.referenced_schema == target.schema)
            })
            .flat_map(|fk| {
                fk.columns
                    .iter()
                    .zip(&fk.referenced_columns)
                    .map(|(column, referenced)| {
                        format!(
                            "{}.{} = {}.{}",
                            aliases[to],
                            quote(referenced),
                            aliases[from],
                            quote(column)
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    let mut conditions = refers(a, b);
    if conditions.is_empty() {
        conditions = refers(b, a);
    }
    (!conditions.is_empty()).then(|| conditions.join(" AND "))
}

/// A SELECT that joins the tables chosen in the explorer on the foreign keys between them:
/// `INNER JOIN … ON …`, or the simple form, `FROM a, b WHERE …`. A table no foreign key links
/// to the others is still joined (CROSS JOIN, or no condition), and the SQL says so.
pub fn join_sql(dialect: &dyn Dialect, tables: &[TableInfo], inner: bool) -> String {
    let aliases = aliases(tables);
    let names: Vec<String> = tables
        .iter()
        .map(|t| qualified(dialect, &t.object.schema, &t.object.name))
        .collect();
    // Each table after the first goes next to one already placed that it links to.
    let mut placed = vec![0];
    let mut joins: Vec<(usize, Option<String>)> = Vec::new();
    while placed.len() < tables.len() {
        let next = (0..tables.len())
            .filter(|i| !placed.contains(i))
            .find_map(|i| {
                placed
                    .iter()
                    .find_map(|&p| links(dialect, tables, &aliases, p, i))
                    .map(|on| (i, Some(on)))
            })
            .unwrap_or_else(|| {
                let i = (0..tables.len())
                    .find(|i| !placed.contains(i))
                    .expect("one left");
                (i, None)
            });
        placed.push(next.0);
        joins.push(next);
    }
    let notes: String = joins
        .iter()
        .filter(|(_, on)| on.is_none())
        .map(|(i, _)| {
            format!(
                "-- No foreign key links {} to the other tables: every combination of rows.\n",
                names[*i]
            )
        })
        .collect();
    if inner {
        let mut sql = format!("{notes}SELECT *\nFROM {} {}", names[0], aliases[0]);
        for (i, on) in &joins {
            match on {
                Some(on) => sql.push_str(&format!(
                    "\nINNER JOIN {} {} ON {on}",
                    names[*i], aliases[*i]
                )),
                None => sql.push_str(&format!("\nCROSS JOIN {} {}", names[*i], aliases[*i])),
            }
        }
        return format!("{sql};\n");
    }
    let from = std::iter::once(0)
        .chain(joins.iter().map(|(i, _)| *i))
        .map(|i| format!("{} {}", names[i], aliases[i]))
        .collect::<Vec<_>>()
        .join(",\n     ");
    let conditions: Vec<&String> = joins.iter().filter_map(|(_, on)| on.as_ref()).collect();
    let mut sql = format!("{notes}SELECT *\nFROM {from}");
    if !conditions.is_empty() {
        sql.push_str(&format!(
            "\nWHERE {}",
            conditions
                .iter()
                .map(|c| c.as_str())
                .collect::<Vec<_>>()
                .join("\n  AND ")
        ));
    }
    format!("{sql};\n")
}

/// `(SELECT :a AS a, …) AS source`: the values to replace, or a real table in their place.
fn source(dialect: &dyn Dialect, columns: &[&str]) -> String {
    format!(
        "(\n  SELECT\n{}\n) AS source",
        columns
            .iter()
            .map(|c| format!("    {} AS {}", placeholder(c), dialect.quote_identifier(c)))
            .collect::<Vec<_>>()
            .join(",\n")
    )
}

/// `target.key = source.key AND …`
fn key_match(dialect: &dyn Dialect, key: &[&str]) -> String {
    key.iter()
        .map(|c| {
            let c = dialect.quote_identifier(c);
            format!("target.{c} = source.{c}")
        })
        .collect::<Vec<_>>()
        .join("\n  AND ")
}

/// The other columns take the source's values where the keys match. MySQL and SQL Server put
/// the source next to the table, so every form keeps its WHERE.
fn update_from(dialect: &dyn Dialect, table: &TableInfo) -> String {
    let name = qualified(dialect, &table.object.schema, &table.object.name);
    let quote = |c: &str| dialect.quote_identifier(c);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let key = row_key(table);
    let rest: Vec<&str> = columns
        .iter()
        .copied()
        .filter(|c| !key.columns.contains(c))
        .collect();
    let set = if rest.is_empty() { &columns } else { &rest };
    let source = source(dialect, &columns);
    let on = key_match(dialect, &key.columns);
    let note = key.note(dialect);
    let assignments = |prefix: &str| {
        set.iter()
            .map(|c| format!("  {prefix}{} = source.{}", quote(c), quote(c)))
            .collect::<Vec<_>>()
            .join(",\n")
    };
    match dialect.name() {
        "sqlserver" => format!(
            "{note}UPDATE target\nSET\n{}\nFROM {name} AS target, {source}\nWHERE {on};\n",
            assignments("")
        ),
        // The keys again in WHERE: the guard reads this form, and sees it is not every row.
        "mysql" => format!(
            "{note}UPDATE {name} AS target\nJOIN {source}\n  ON {on}\nSET\n{}\nWHERE {on};\n",
            assignments("target.")
        ),
        _ => format!(
            "{note}UPDATE {name} AS target\nSET\n{}\nFROM {source}\nWHERE {on};\n",
            assignments("")
        ),
    }
}

/// The rows whose keys are in the source.
fn delete_using(dialect: &dyn Dialect, table: &TableInfo) -> String {
    let name = qualified(dialect, &table.object.schema, &table.object.name);
    let key = row_key(table);
    let source = source(dialect, &key.columns);
    let on = key_match(dialect, &key.columns);
    let note = key.note(dialect);
    match dialect.name() {
        "sqlserver" | "mysql" => {
            format!("{note}DELETE target\nFROM {name} AS target, {source}\nWHERE {on};\n")
        }
        _ => format!("{note}DELETE FROM {name} AS target\nUSING {source}\nWHERE {on};\n"),
    }
}

/// `INSERT … ON CONFLICT (key) DO UPDATE SET c = EXCLUDED.c`, or MySQL's
/// `ON DUPLICATE KEY UPDATE c = VALUES(c)` (which MariaDB reads too). Without a key there is no
/// conflict to update on: a plain INSERT that says so.
fn upsert(dialect: &dyn Dialect, table: &TableInfo) -> String {
    let name = qualified(dialect, &table.object.schema, &table.object.name);
    let quote = |c: &str| dialect.quote_identifier(c);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let insert = format!(
        "INSERT INTO {name} (\n{}\n) VALUES (\n{}\n)",
        list(columns.iter().map(|c| quote(c)), ",\n"),
        list(columns.iter().map(|c| placeholder(c)), ",\n")
    );
    let key = row_key(table);
    if matches!(key.source, KeySource::AllColumns) {
        return format!(
            "-- No primary or unique key: there is no conflict to update on.\n{insert};\n"
        );
    }
    let rest: Vec<&str> = columns
        .iter()
        .copied()
        .filter(|c| !key.columns.contains(c))
        .collect();
    let note = key.note(dialect);
    if dialect.name() == "mysql" {
        // Updating a key column to itself when there is nothing else keeps it a no-op.
        let set = if rest.is_empty() { &key.columns } else { &rest };
        return format!(
            "{note}{insert}\nON DUPLICATE KEY UPDATE\n{};\n",
            list(
                set.iter()
                    .map(|c| format!("{} = VALUES({})", quote(c), quote(c))),
                ",\n"
            )
        );
    }
    let target = key
        .columns
        .iter()
        .map(|c| quote(c))
        .collect::<Vec<_>>()
        .join(", ");
    if rest.is_empty() {
        return format!("{note}{insert}\nON CONFLICT ({target}) DO NOTHING;\n");
    }
    format!(
        "{note}{insert}\nON CONFLICT ({target}) DO UPDATE SET\n{};\n",
        list(
            rest.iter()
                .map(|c| format!("{} = EXCLUDED.{}", quote(c), quote(c))),
            ",\n"
        )
    )
}

/// `MERGE INTO t AS target USING (SELECT :values) AS source ON target.key = source.key`, then
/// UPDATE the other columns when it matches and INSERT the row when it does not.
fn merge(dialect: &dyn Dialect, table: &TableInfo) -> String {
    let name = qualified(dialect, &table.object.schema, &table.object.name);
    let quote = |c: &str| dialect.quote_identifier(c);
    let columns: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let key = row_key(table);
    let on = key
        .columns
        .iter()
        .map(|c| format!("target.{} = source.{}", quote(c), quote(c)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let rest: Vec<&str> = columns
        .iter()
        .copied()
        .filter(|c| !key.columns.contains(c))
        .collect();
    let matched = if rest.is_empty() {
        String::new()
    } else {
        format!(
            "WHEN MATCHED THEN\n  UPDATE SET\n{}\n",
            rest.iter()
                .map(|c| format!("    {} = source.{}", quote(c), quote(c)))
                .collect::<Vec<_>>()
                .join(",\n")
        )
    };
    format!(
        "{}MERGE INTO {name} AS target\nUSING (\n  SELECT\n{}\n) AS source\nON {on}\n{matched}\
         WHEN NOT MATCHED THEN\n  INSERT ({})\n  VALUES ({});\n",
        key.note(dialect),
        columns
            .iter()
            .map(|c| format!("    {} AS {}", placeholder(c), quote(c)))
            .collect::<Vec<_>>()
            .join(",\n"),
        columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(", "),
        columns
            .iter()
            .map(|c| format!("source.{}", quote(c)))
            .collect::<Vec<_>>()
            .join(", ")
    )
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
/// when it returns no result (`KILL`) or no rows. The result is freed, also on failure. It is
/// read unmasked (ADR-051): callers parse it (counts, catalog names) and never show a value
/// of a table's column.
pub(crate) async fn scalar_value(session: &Session, sql: &str) -> Result<Option<Value>, String> {
    scalar_result(session, session.run_parsed(sql)).await
}

/// The first cell of a fixed internal metadata read, excluded from the user's transaction
/// statement count. It uses the same session and result cleanup as generated user actions.
pub(crate) async fn scalar_metadata(session: &Session, sql: &str) -> Result<Option<Value>, String> {
    scalar_result(session, session.run_metadata(sql)).await
}

async fn scalar_result(
    session: &Session,
    mut events: futures_util::stream::BoxStream<'static, CoreEvent>,
) -> Result<Option<Value>, String> {
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
            | CoreEvent::ErrorPaused { .. }
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

/// Every row of a short read written by the core (a page of a few dozen rows, a catalog
/// list), as values with the result's columns; the result is freed. `metadata` runs it as an
/// internal metadata read ([`Session::run_metadata`]) instead of a generated statement.
pub(crate) async fn short_rows(
    session: &Session,
    sql: &str,
    metadata: bool,
) -> Result<(Vec<spelaion_driver_api::ColumnMeta>, Vec<Vec<Value>>), String> {
    let mut events = if metadata {
        session.run_metadata(sql)
    } else {
        session.run_generated(sql)
    };
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
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
    }
    let store = session.results();
    let read = results.first().and_then(|id| {
        let stored = store.get(*id)?;
        let stored = stored.read().ok()?;
        let columns = stored.schema().columns.clone();
        let mut rows = Vec::with_capacity(stored.rows());
        for batch in stored.window(0, stored.rows()) {
            for row in 0..batch.num_rows() {
                rows.push(
                    columns
                        .iter()
                        .enumerate()
                        .map(|(i, meta)| {
                            Value::from_array(batch.column(i).as_ref(), row, meta.logical)
                                .unwrap_or(Value::Null)
                        })
                        .collect(),
                );
            }
        }
        Some((columns, rows))
    });
    for id in results {
        store.remove(id);
    }
    match (failure, read) {
        (Some(message), _) => Err(message),
        (None, Some(read)) => Ok(read),
        (None, None) => Err("the read returned no result".into()),
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

/// The columns that identify a row, and where they come from.
struct RowKey<'a> {
    columns: Vec<&'a str>,
    source: KeySource,
}

enum KeySource {
    PrimaryKey,
    Unique,
    AllColumns,
}

impl RowKey<'_> {
    /// A comment line when the key is not the primary key.
    fn note(&self, dialect: &dyn Dialect) -> String {
        match self.source {
            KeySource::PrimaryKey => String::new(),
            KeySource::Unique => format!(
                "-- No primary key: the unique key ({}) identifies the row.\n",
                self.columns
                    .iter()
                    .map(|c| dialect.quote_identifier(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            KeySource::AllColumns => {
                "-- No primary or unique key: every column identifies the row.\n".to_string()
            }
        }
    }
}

/// The primary key; without one, the first unique constraint whose columns cannot be NULL
/// (NULLs are not equal to each other), then any unique constraint; else every column.
fn row_key(table: &TableInfo) -> RowKey<'_> {
    if !table.primary_key.is_empty() {
        return RowKey {
            columns: table.primary_key.iter().map(String::as_str).collect(),
            source: KeySource::PrimaryKey,
        };
    }
    let not_null = |column: &String| {
        table
            .columns
            .iter()
            .any(|c| &c.name == column && !c.nullable)
    };
    let unique = || {
        table
            .constraints
            .iter()
            .filter(|c| c.kind == ConstraintKind::Unique && !c.columns.is_empty())
    };
    if let Some(constraint) = unique()
        .find(|c| c.columns.iter().all(not_null))
        .or_else(|| unique().next())
    {
        return RowKey {
            columns: constraint.columns.iter().map(String::as_str).collect(),
            source: KeySource::Unique,
        };
    }
    RowKey {
        columns: table.columns.iter().map(|c| c.name.as_str()).collect(),
        source: KeySource::AllColumns,
    }
}

fn where_clause(dialect: &dyn Dialect, key: &RowKey) -> String {
    let conditions = key
        .columns
        .iter()
        .map(|k| format!("{} = {}", dialect.quote_identifier(k), placeholder(k)))
        .collect::<Vec<_>>()
        .join("\n  AND ");
    format!("{}WHERE {conditions};\n", key.note(dialect))
}

/// `WHERE key IN (:key_1, …)`; with a key of several columns, a list of rows, which T-SQL
/// does not have: there, one condition per row joined by OR.
fn in_list(dialect: &dyn Dialect, key: &[&str]) -> String {
    let quote = |c: &str| dialect.quote_identifier(c);
    let numbered = |c: &str, n: usize| format!("{}_{n}", placeholder(c));
    if let [column] = key {
        let values: Vec<String> = (1..=IN_LIST).map(|n| numbered(column, n)).collect();
        return format!("WHERE {} IN ({})", quote(column), values.join(", "));
    }
    if dialect.name() == "sqlserver" {
        let rows: Vec<String> = (1..=IN_LIST)
            .map(|n| {
                let row: Vec<String> = key
                    .iter()
                    .map(|c| format!("{} = {}", quote(c), numbered(c, n)))
                    .collect();
                format!("({})", row.join(" AND "))
            })
            .collect();
        return format!("WHERE {}", rows.join("\n   OR "));
    }
    let columns: Vec<String> = key.iter().map(|c| quote(c)).collect();
    let rows: Vec<String> = (1..=IN_LIST)
        .map(|n| {
            let row: Vec<String> = key.iter().map(|c| numbered(c, n)).collect();
            format!("({})", row.join(", "))
        })
        .collect();
    format!(
        "WHERE ({}) IN (\n  {}\n)",
        columns.join(", "),
        rows.join(",\n  ")
    )
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
    use spelaion_dialect::SqliteDialect;
    use spelaion_driver_api::{ColumnInfo, ObjectInfo};

    async fn metadata_session(safeguards: spelaion_config::Safeguards) -> std::sync::Arc<Session> {
        use spelaion_driver_api::ConnectParams;
        let driver = crate::open::test_driver(spelaion_config::Engine::Sqlite);
        let session = std::sync::Arc::new(Session::new(
            driver
                .connect(&ConnectParams {
                    database: ":memory:".into(),
                    ..Default::default()
                })
                .await
                .unwrap(),
            driver.dialect(),
            std::sync::Arc::new(crate::ResultStore::new()),
        ));
        session.follow_transactions(&safeguards).await;
        session
    }

    #[tokio::test]
    async fn metadata_reads_share_the_session_lock_and_clean_partial_failures() {
        let session = metadata_session(spelaion_config::Safeguards {
            smart_commit: true,
            ..Default::default()
        })
        .await;
        let mut user_run = session.run_script("CREATE TABLE metadata_items (id integer)");
        assert!(matches!(
            user_run.next().await,
            Some(CoreEvent::StatementStarted { .. })
        ));
        let (started, waiting) = tokio::sync::oneshot::channel();
        let source = session.clone();
        let mut metadata = tokio::spawn(async move {
            started.send(()).unwrap();
            scalar_metadata(&source, "SELECT 'main'").await
        });
        waiting.await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut metadata)
                .await
                .is_err(),
            "metadata waits for the user's session control lock"
        );
        assert_eq!(
            session.commit().await.unwrap_err(),
            crate::TransactionError::Busy
        );
        while let Some(event) = user_run.next().await {
            assert!(!matches!(event, CoreEvent::StatementFailed { .. }));
        }
        assert_eq!(
            metadata.await.unwrap().unwrap(),
            Some(Value::Text("main".into()))
        );
        assert_eq!(session.transaction_status().statements, 1);
        assert!(session.results().is_empty());

        // A generated user mutation remains counted and frees its RETURNING result.
        assert_eq!(
            scalar_value(
                &session,
                "INSERT INTO metadata_items VALUES (1) RETURNING id"
            )
            .await
            .unwrap(),
            Some(Value::Int(1))
        );
        assert_eq!(session.transaction_status().statements, 2);
        assert!(session.results().is_empty());
        assert!(
            scalar_metadata(&session, "SELECT 42; SELECT missing_metadata_column")
                .await
                .is_err()
        );
        assert!(
            session.results().is_empty(),
            "even partial results are freed"
        );
        assert_eq!(session.transaction_status().statements, 2);
        assert!(session.transaction_status().is_open());
        session.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_metadata_frees_results_without_opening_a_manual_transaction() {
        let session = metadata_session(spelaion_config::Safeguards {
            manual_commit: true,
            ..Default::default()
        })
        .await;
        let source = session.clone();
        let metadata = tokio::spawn(async move {
            scalar_metadata(
                &source,
                "WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM numbers) SELECT n FROM numbers",
            )
            .await
        });
        let running = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while session.results().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        session.cancel().await.unwrap();
        running.expect("the real SQLite metadata query started returning rows");
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), metadata)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(session.results().is_empty());
        let status = session.transaction_status();
        assert!(!status.auto_commit);
        assert_eq!(status.state, Some(crate::TransactionState::Idle));
        assert_eq!(status.statements, 0);
        assert_eq!(
            scalar_metadata(&session, "SELECT 'usable after cancel'")
                .await
                .unwrap(),
            Some(Value::Text("usable after cancel".into()))
        );
        assert_eq!(session.transaction_status(), status);
        assert!(session.results().is_empty());
    }

    /// Backtick quoting like MySQL's, without depending on that driver.
    struct Backticks;
    impl Dialect for Backticks {
        fn name(&self) -> &'static str {
            "mysql"
        }
        fn split_statements(&self, _: &str) -> Vec<spelaion_driver_api::StatementSpan> {
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
                kind: spelaion_driver_api::ObjectKind::Table,
                estimated_rows: None,
                comment: None,
            },
            columns: vec![column("id"), column("unit price"), column("qty")],
            primary_key: primary_key.iter().map(|k| k.to_string()).collect(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
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
    fn joins_follow_the_foreign_keys_between_the_tables() {
        let mut orders = table("shop", &["id"]);
        orders.object.name = "orders".into();
        orders.columns[2].name = "customer_id".into();
        orders.foreign_keys = vec![spelaion_driver_api::ForeignKeyInfo {
            name: "orders_customer".into(),
            columns: vec!["customer_id".into()],
            referenced_schema: "shop".into(),
            referenced_table: "customer".into(),
            referenced_columns: vec!["id".into()],
        }];
        let mut customer = table("shop", &["id"]);
        customer.object.name = "customer".into();
        let mut notes = table("shop", &["id"]);
        notes.object.name = "order notes".into();
        let tables = [orders, customer, notes];
        assert_eq!(
            join_sql(&Backticks, &tables, true),
            "-- No foreign key links `shop`.`order notes` to the other tables: every combination of rows.\n\
             SELECT *\nFROM `shop`.`orders` o\n\
             INNER JOIN `shop`.`customer` c ON c.`id` = o.`customer_id`\n\
             CROSS JOIN `shop`.`order notes` t3;\n"
        );
        assert_eq!(
            join_sql(&Backticks, &tables[..2], false),
            "SELECT *\nFROM `shop`.`orders` o,\n     `shop`.`customer` c\nWHERE c.`id` = o.`customer_id`;\n"
        );
        // The other way round: customer first, orders refers to it.
        let reversed = [tables[1].clone(), tables[0].clone()];
        assert!(
            join_sql(&Backticks, &reversed, true)
                .contains("INNER JOIN `shop`.`orders` o ON c.`id` = o.`customer_id`")
        );
    }

    #[test]
    fn rows_by_key_and_by_a_list_of_keys() {
        let t = table("shop", &["id"]);
        assert_eq!(
            table_sql(&Backticks, &t, SqlTemplate::SelectByKey),
            "SELECT\n  `id`,\n  `unit price`,\n  `qty`\nFROM `shop`.`order lines`\nWHERE `id` = :id;\n"
        );
        assert_eq!(
            table_sql(&Backticks, &t, SqlTemplate::SelectIn),
            "SELECT\n  `id`,\n  `unit price`,\n  `qty`\nFROM `shop`.`order lines`\nWHERE `id` IN (:id_1, :id_2, :id_3);\n"
        );
        let pair = table("shop", &["id", "qty"]);
        assert!(
            table_sql(&Backticks, &pair, SqlTemplate::SelectIn).ends_with(
                "WHERE (`id`, `qty`) IN (\n  (:id_1, :qty_1),\n  (:id_2, :qty_2),\n  (:id_3, :qty_3)\n);\n"
            )
        );
    }

    #[test]
    fn without_a_primary_key_a_unique_key_identifies_the_row() {
        let mut t = table("shop", &[]);
        t.constraints = vec![
            spelaion_driver_api::ConstraintInfo {
                name: Some("maybe_null".into()),
                kind: ConstraintKind::Unique,
                columns: vec!["qty".into()],
                definition: String::new(),
            },
            spelaion_driver_api::ConstraintInfo {
                name: Some("code".into()),
                kind: ConstraintKind::Unique,
                columns: vec!["unit price".into()],
                definition: String::new(),
            },
        ];
        // A unique key whose columns cannot be NULL comes first.
        t.columns[1].nullable = false;
        assert_eq!(
            table_sql(&Backticks, &t, SqlTemplate::Delete),
            "DELETE FROM `shop`.`order lines`\n-- No primary key: the unique key (`unit price`) identifies the row.\nWHERE `unit price` = :unit_price;\n"
        );
        assert_eq!(
            table_sql(&Backticks, &t, SqlTemplate::Update),
            "UPDATE `shop`.`order lines`\nSET\n  `id` = :id,\n  `qty` = :qty\n-- No primary key: the unique key (`unit price`) identifies the row.\nWHERE `unit price` = :unit_price;\n"
        );
    }

    #[test]
    fn without_a_primary_key_every_column_identifies_the_row() {
        let sql = table_sql(&Backticks, &table("shop", &[]), SqlTemplate::Delete);
        assert_eq!(
            sql,
            "DELETE FROM `shop`.`order lines`\n-- No primary or unique key: every column identifies the row.\nWHERE `id` = :id\n  AND `unit price` = :unit_price\n  AND `qty` = :qty;\n"
        );
        // Generated writes never trip the "without WHERE" guard.
        assert!(
            crate::script_risks(
                &SqliteDialect,
                &sql.replace('`', "\""),
                &spelaion_config::Safeguards::default(),
            )
            .is_empty()
        );
    }
}
