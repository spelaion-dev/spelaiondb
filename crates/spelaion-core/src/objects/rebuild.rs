//! SQLite's table rebuild (ADR-047): what `ALTER TABLE` cannot do there (change a column's
//! type, nullability or default, drop a constraint) is done by creating the table again,
//! following SQLite's own procedure ("Making Other Kinds Of Table Schema Changes"):
//!
//! 1. foreign keys off, if they were on (outside the transaction, where the pragma works);
//! 2. in one transaction: the views and triggers that name the table are dropped, the new table
//!    is created from the old one's text with only the change made, the rows copied, the old
//!    table dropped and the new one renamed, then its indexes, its triggers and the dropped
//!    views and triggers are created again from the text SQLite kept;
//! 3. with foreign keys on, `foreign_key_check` must find nothing, or everything rolls back;
//! 4. foreign keys on again.
//!
//! The rows keep their values (each converted to the new column's affinity on insert) and
//! `AUTOINCREMENT` keeps its counter.

use std::collections::HashMap;

use spelaion_driver_api::Value;

use super::ObjectError;
use super::alter::{AlterationPlan, Current, DroppedConstraint, Step, catalog_rows, sql_literal};
use super::ddl::{self, ClauseKind, ColumnRewrite, Flavor};
use super::qualified_name;
use crate::generate::scalar_metadata;
use crate::session::Session;

const NOTE: &str = "SQLite cannot change this in place: the table is created again with the change, its rows copied and its indexes, triggers and dependent views created again.";

fn unreadable(e: ddl::DdlError) -> ObjectError {
    ObjectError::Unsupported(e.to_string())
}

fn master(session: &Session, schema: &str) -> String {
    let quoted = session.dialect().quote_identifier(schema);
    // The temp schema keeps its catalog in its own table.
    if schema.eq_ignore_ascii_case("temp") {
        format!("{quoted}.sqlite_temp_master")
    } else {
        format!("{quoted}.sqlite_master")
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// The `CREATE TABLE` text SQLite keeps for a table.
pub(crate) async fn table_sql(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<String, ObjectError> {
    let sql = format!(
        "SELECT sql FROM {} WHERE type = 'table' AND name = {}",
        master(session, schema),
        sql_literal(table)
    );
    catalog_rows(session, &sql)
        .await?
        .first()
        .and_then(|row| row.first())
        .and_then(text)
        .ok_or_else(|| ObjectError::Gone(table.to_string()))
}

/// An object SQLite keeps the text of.
struct Kept {
    kind: String,
    name: String,
    table: String,
    sql: String,
}

/// The rebuild's statements for the columns rewritten and the constraints dropped.
pub(crate) async fn plan(
    session: &Session,
    schema: &str,
    table: &str,
    current: &Current,
    rewrites: &HashMap<String, ColumnRewrite>,
    drops: &[DroppedConstraint],
) -> Result<AlterationPlan, ObjectError> {
    let dialect = session.dialect();
    let dialect = dialect.as_ref();
    let quote = |name: &str| dialect.quote_identifier(name);
    let qualified = |name: &str| qualified_name(dialect, schema, &[name.to_string()]);
    let sql = current.ddl.as_deref().unwrap_or_default();
    let create = ddl::parse_create_table(sql, Flavor::Sqlite).map_err(unreadable)?;

    // What each dropped constraint is, from the catalog: SQLite has no names to find them by.
    let mut wanted: Vec<(ClauseKind, Vec<String>, Option<String>, String)> = Vec::new();
    for dropped in drops {
        match dropped {
            DroppedConstraint::Constraint { name } => {
                let constraint = current
                    .info
                    .constraints
                    .iter()
                    .find(|c| c.name.as_deref() == Some(name.as_str()))
                    .ok_or_else(|| ObjectError::Gone(name.clone()))?;
                if constraint.kind != spelaion_driver_api::ConstraintKind::Unique {
                    return Err(ObjectError::Unsupported(
                        "SQLite's rebuild drops unique constraints and foreign keys".into(),
                    ));
                }
                wanted.push((
                    ClauseKind::Unique,
                    constraint.columns.clone(),
                    None,
                    name.clone(),
                ));
            }
            DroppedConstraint::ForeignKey { name } => {
                let key = current
                    .info
                    .foreign_keys
                    .iter()
                    .find(|fk| &fk.name == name)
                    .ok_or_else(|| ObjectError::Gone(name.clone()))?;
                wanted.push((
                    ClauseKind::References,
                    key.columns.clone(),
                    Some(key.referenced_table.clone()),
                    name.clone(),
                ));
            }
        }
    }
    let same_columns = |a: &[String], b: &[String]| {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
    };
    let mut found = vec![false; wanted.len()];

    // The body again: changed columns rewritten, dropped table constraints left out.
    let mut elements: Vec<String> = Vec::new();
    let mut copied: Vec<String> = Vec::new();
    for range in &create.elements {
        let element = &sql[range.clone()];
        if ddl::is_table_constraint(element, Flavor::Sqlite) {
            let constraint = ddl::table_constraint(element, Flavor::Sqlite);
            let drop = constraint.as_ref().and_then(|c| {
                wanted.iter().position(|(kind, columns, referenced, _)| {
                    *kind == c.kind
                        && same_columns(columns, &c.columns)
                        && referenced.as_deref().is_none_or(|r| {
                            c.referenced
                                .as_deref()
                                .is_some_and(|t| t.eq_ignore_ascii_case(r))
                        })
                })
            });
            match drop {
                Some(i) if !found[i] => found[i] = true,
                _ => elements.push(element.to_string()),
            }
            continue;
        }
        let def = ddl::parse_column(element, Flavor::Sqlite).map_err(unreadable)?;
        if !def.generated() {
            copied.push(quote(&def.name));
        }
        // A column's own UNIQUE or REFERENCES clause, when that is the constraint dropped.
        let mut dropped_clauses = Vec::new();
        for (i, (kind, columns, referenced, _)) in wanted.iter().enumerate() {
            if found[i] || columns.len() != 1 || !columns[0].eq_ignore_ascii_case(&def.name) {
                continue;
            }
            let clause = def.clauses.iter().position(|clause| {
                clause.kind == *kind
                    && referenced.as_deref().is_none_or(|r| {
                        ddl::references_table(&element[clause.range.clone()], Flavor::Sqlite)
                            .is_some_and(|t| t.eq_ignore_ascii_case(r))
                    })
            });
            if let Some(clause) = clause {
                found[i] = true;
                dropped_clauses.push(clause);
            }
        }
        let rewrite = rewrites.get(&def.name);
        if rewrite.is_none() && dropped_clauses.is_empty() {
            elements.push(element.to_string());
            continue;
        }
        let mut def = def;
        let mut index = 0;
        def.clauses.retain(|_| {
            let keep = !dropped_clauses.contains(&index);
            index += 1;
            keep
        });
        elements.push(ddl::rewrite_column(
            element,
            &def,
            &rewrite.cloned().unwrap_or_default(),
        ));
    }
    if let Some(i) = found.iter().position(|f| !f) {
        return Err(ObjectError::Unsupported(format!(
            "{} was not found in the table's definition",
            wanted[i].3
        )));
    }
    if rewrites
        .keys()
        .any(|name| !current.columns.iter().any(|c| &c.name == name))
    {
        return Err(ObjectError::Unsupported(
            "a changed column is not in the table's definition".into(),
        ));
    }

    let kept: Vec<Kept> = catalog_rows(
        session,
        &format!(
            "SELECT type, name, tbl_name, sql FROM {} WHERE sql IS NOT NULL AND type IN ('index', 'trigger', 'view', 'table') ORDER BY rowid",
            master(session, schema)
        ),
    )
    .await?
    .into_iter()
    .filter_map(|row| {
        Some(Kept {
            kind: text(row.first()?)?,
            name: text(row.get(1)?)?,
            table: text(row.get(2)?)?,
            sql: text(row.get(3)?)?,
        })
    })
    .collect();
    let new_name = format!("_spelaion_rebuild_{table}");
    if kept.iter().any(|k| k.name.eq_ignore_ascii_case(&new_name)) {
        return Err(ObjectError::Unsupported(format!(
            "a table named {new_name} is in the way: rename or drop it first"
        )));
    }
    let is_table = |k: &Kept| k.table.eq_ignore_ascii_case(table);
    let indexes: Vec<&Kept> = kept
        .iter()
        .filter(|k| k.kind == "index" && is_table(k))
        .collect();
    let triggers: Vec<&Kept> = kept
        .iter()
        .filter(|k| k.kind == "trigger" && is_table(k))
        .collect();
    // Views and other tables' triggers that name the table, and those that name them in turn:
    // renaming the new table checks every one of them, so they go first and come back after.
    let mut names = vec![table.to_string()];
    let mut dependents: Vec<&Kept> = Vec::new();
    loop {
        let more: Vec<&Kept> = kept
            .iter()
            .filter(|k| {
                (k.kind == "view" || k.kind == "trigger" && !is_table(k))
                    && !dependents.iter().any(|d| d.name == k.name)
                    && names
                        .iter()
                        .any(|n| ddl::mentions(&k.sql, n, Flavor::Sqlite))
            })
            .collect();
        if more.is_empty() {
            break;
        }
        names.extend(
            more.iter()
                .filter(|k| k.kind == "view")
                .map(|k| k.name.clone()),
        );
        dependents.extend(more);
    }
    dependents.sort_by_key(|d| kept.iter().position(|k| k.name == d.name));

    let foreign_keys = matches!(
        scalar_metadata(session, "PRAGMA foreign_keys").await,
        Ok(Some(Value::Int(1)))
    );
    let sequence = if kept
        .iter()
        .any(|k| k.kind == "table" && k.name.eq_ignore_ascii_case("sqlite_sequence"))
    {
        let sql = format!(
            "SELECT seq FROM {}.sqlite_sequence WHERE name = {}",
            quote(schema),
            sql_literal(table)
        );
        match scalar_metadata(session, &sql).await {
            Ok(Some(Value::Int(seq))) => Some(seq),
            _ => None,
        }
    } else {
        None
    };

    let recreate = |k: &Kept| {
        if schema.eq_ignore_ascii_case("main") {
            k.sql.clone()
        } else {
            ddl::qualify_create(&k.sql, &quote(schema))
        }
    };
    let mut steps = Vec::new();
    for dependent in dependents.iter().rev() {
        let what = if dependent.kind == "view" {
            "VIEW"
        } else {
            "TRIGGER"
        };
        steps.push(Step::Run(format!(
            "DROP {what} {}",
            qualified(&dependent.name)
        )));
    }
    steps.push(Step::Run(format!(
        "CREATE TABLE {} (\n  {}\n){}",
        qualified(&new_name),
        elements.join(",\n  "),
        sql[create.body.end..].trim_end().trim_end_matches(';')
    )));
    let list = copied.join(", ");
    steps.push(Step::Run(format!(
        "INSERT INTO {} ({list}) SELECT {list} FROM {}",
        qualified(&new_name),
        qualified(table)
    )));
    steps.push(Step::Run(format!("DROP TABLE {}", qualified(table))));
    steps.push(Step::Run(format!(
        "ALTER TABLE {} RENAME TO {}",
        qualified(&new_name),
        quote(table)
    )));
    if let Some(seq) = sequence {
        steps.push(Step::Run(format!(
            "UPDATE {}.sqlite_sequence SET seq = {seq} WHERE name = {}",
            quote(schema),
            sql_literal(table)
        )));
    }
    for kept in indexes.iter().chain(&triggers).chain(&dependents) {
        steps.push(Step::Run(recreate(kept)));
    }
    let mut before = Vec::new();
    let mut after = Vec::new();
    if foreign_keys {
        before.push("PRAGMA foreign_keys = OFF".to_string());
        after.push("PRAGMA foreign_keys = ON".to_string());
        // The table's own keys, and those of the tables that point to it.
        let mut checked = vec![table.to_string()];
        if let Ok(keys) = session
            .connection()
            .list_referencing_keys(schema, table)
            .await
        {
            for key in keys {
                if !checked.iter().any(|t| t.eq_ignore_ascii_case(&key.table)) {
                    checked.push(key.table);
                }
            }
        }
        for checked in checked {
            steps.push(Step::ExpectZero {
                sql: format!(
                    "SELECT count(*) FROM pragma_foreign_key_check({}, {})",
                    sql_literal(&checked),
                    sql_literal(schema)
                ),
                problem: format!(
                    "the rebuilt table breaks foreign keys of {checked}: nothing was changed"
                ),
            });
        }
    }
    Ok(AlterationPlan {
        before,
        steps,
        after,
        begin: Some("BEGIN"),
        note: Some(NOTE),
        rebuild: true,
    })
}
