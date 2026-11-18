//! "Duplicate table…" in the explorer (SDB-394): a copy of a table in the same connection, under
//! a new name and in the same schema or another of it, with its structure only or its rows too.
//!
//! The statements are written here (ADR-026) in each engine's own form, from the catalog read
//! again each time, and the app shows them, with what the copy keeps and what it does not, before
//! anything runs. They run as shown in an editor tab of their own, so the guard and the
//! connection type's confirmation ask what they ask of any run, and a read-only connection
//! refuses them; the rows are copied by the server (`INSERT … SELECT`, `SELECT … INTO`), never
//! through the app.
//!
//! - PostgreSQL: `CREATE TABLE … (LIKE … INCLUDING …)`, with or without its indexes and
//!   constraints; rows with `INSERT … OVERRIDING SYSTEM VALUE SELECT`, generated columns left to
//!   compute.
//! - MySQL, MariaDB: `CREATE TABLE … LIKE …`, which always keeps the keys, indexes and checks.
//! - SQL Server: `SELECT … INTO`, which keeps the columns, their nullability and identity only.
//! - SQLite, DuckDB: the `CREATE TABLE` text the engine keeps, with the new name, then each of
//!   the table's indexes from its kept text under a new name (`<new table>_<index>`).

use spelaion_driver_api::Value;

use super::alter::{catalog_rows, table_structure};
use super::ddl::{self, Flavor};
use super::{ObjectError, qualified_name, rebuild};
use crate::Session;
use crate::export::sql_literal;
use crate::generate::scalar_metadata;

/// A part of a table, for what a copy keeps and what it does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablePart {
    Columns,
    NotNull,
    Defaults,
    PrimaryKey,
    Unique,
    Checks,
    Indexes,
    ForeignKeys,
    Identity,
    /// Where an identity column goes on from: its next value restarts from its start.
    IdentityPosition,
    Comments,
    Triggers,
    Rows,
}

/// What to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateTable {
    pub schema: String,
    pub name: String,
    /// The copy's schema: the same one or another of the connection.
    pub new_schema: String,
    pub new_name: String,
    pub with_data: bool,
    /// Keep the indexes and constraints, where the engine's form lets them go
    /// ([`duplicate_options`]).
    pub with_indexes: bool,
}

/// What the duplicate form may offer on this engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateOptions {
    /// The indexes and constraints may be left out (PostgreSQL); elsewhere the engine's form
    /// decides and the plan says what it keeps.
    pub indexes_optional: bool,
}

pub fn duplicate_options(engine: &str) -> DuplicateOptions {
    DuplicateOptions {
        indexes_optional: engine == "postgres",
    }
}

/// What the app shows before the copy runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicatePlan {
    /// The statements, in order, as they run: each ends in `;` on its own line.
    pub sql: String,
    pub kept: Vec<TablePart>,
    pub not_kept: Vec<TablePart>,
}

const ALL_PARTS: [TablePart; 13] = [
    TablePart::Columns,
    TablePart::NotNull,
    TablePart::Defaults,
    TablePart::PrimaryKey,
    TablePart::Unique,
    TablePart::Checks,
    TablePart::Indexes,
    TablePart::ForeignKeys,
    TablePart::Identity,
    TablePart::IdentityPosition,
    TablePart::Comments,
    TablePart::Triggers,
    TablePart::Rows,
];

fn failed(message: String) -> ObjectError {
    ObjectError::Failed {
        index: 0,
        total: 1,
        message,
    }
}

/// The plan of a copy: refused on a read-only connection before anything is read, and when the
/// new name is empty or the table's own.
pub async fn duplicate_plan(
    session: &Session,
    request: &DuplicateTable,
    read_only: bool,
) -> Result<DuplicatePlan, ObjectError> {
    use TablePart::*;
    if read_only {
        return Err(ObjectError::ReadOnly);
    }
    let new_name = request.new_name.trim();
    if new_name.is_empty() {
        return Err(ObjectError::NoName);
    }
    if new_name == request.name && request.new_schema == request.schema {
        return Err(ObjectError::SameName);
    }
    let dialect = session.dialect();
    let engine = dialect.name().to_string();
    let quote = |name: &str| dialect.quote_identifier(name);
    let old = qualified_name(
        dialect.as_ref(),
        &request.schema,
        std::slice::from_ref(&request.name),
    );
    let new = qualified_name(
        dialect.as_ref(),
        &request.new_schema,
        &[new_name.to_string()],
    );
    let mut statements: Vec<String> = Vec::new();
    let mut kept: Vec<TablePart> = vec![Columns, NotNull];
    match engine.as_str() {
        "postgres" => {
            let including = if request.with_indexes {
                kept.extend([
                    Defaults, PrimaryKey, Unique, Checks, Indexes, Identity, Comments,
                ]);
                "INCLUDING ALL".to_string()
            } else {
                kept.extend([Defaults, Identity, Comments]);
                "INCLUDING DEFAULTS INCLUDING IDENTITY INCLUDING GENERATED INCLUDING COMMENTS"
                    .to_string()
            };
            statements.push(format!("CREATE TABLE {new} (LIKE {old} {including})"));
            if request.with_data {
                let columns = postgres_columns(session, &request.schema, &request.name).await?;
                statements.push(format!(
                    "INSERT INTO {new} ({columns}) OVERRIDING SYSTEM VALUE SELECT {columns} FROM {old}"
                ));
            }
        }
        "mysql" => {
            kept.extend([
                Defaults, PrimaryKey, Unique, Checks, Indexes, Identity, Comments,
            ]);
            statements.push(format!("CREATE TABLE {new} LIKE {old}"));
            if request.with_data {
                // MySQL moves AUTO_INCREMENT on past the values copied.
                kept.push(IdentityPosition);
                let columns = plain_columns(session, &request.schema, &request.name).await?;
                statements.push(format!(
                    "INSERT INTO {new} ({columns}) SELECT {columns} FROM {old}"
                ));
            }
        }
        "sqlserver" => {
            kept.push(Identity);
            if request.with_data {
                kept.push(IdentityPosition);
                statements.push(format!("SELECT * INTO {new} FROM {old}"));
            } else {
                statements.push(format!("SELECT * INTO {new} FROM {old} WHERE 1 = 0"));
            }
        }
        "sqlite" | "duckdb" => {
            let (table_sql, indexes) = if engine == "sqlite" {
                let table_sql = rebuild::table_sql(session, &request.schema, &request.name).await?;
                let sql = format!(
                    "SELECT sql FROM {}.sqlite_master WHERE type = 'index' AND tbl_name = {} AND sql IS NOT NULL ORDER BY name",
                    quote(&request.schema),
                    sql_literal(&Value::Text(request.name.clone()), "sqlite")
                );
                (table_sql, kept_texts(session, &sql).await?)
            } else {
                let filter = format!(
                    "schema_name = {} AND table_name = {}",
                    sql_literal(&Value::Text(request.schema.clone()), "duckdb"),
                    sql_literal(&Value::Text(request.name.clone()), "duckdb")
                );
                let table_sql = kept_texts(
                    session,
                    &format!("SELECT sql FROM duckdb_tables() WHERE {filter}"),
                )
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| ObjectError::Gone(request.name.clone()))?;
                let indexes = kept_texts(
                    session,
                    &format!("SELECT sql FROM duckdb_indexes() WHERE {filter} AND sql IS NOT NULL ORDER BY index_name"),
                )
                .await?;
                (table_sql, indexes)
            };
            kept.extend([Defaults, PrimaryKey, Unique, Checks, ForeignKeys, Indexes]);
            if engine == "sqlite" {
                kept.push(Identity);
            }
            let unreadable = |e: ddl::DdlError| ObjectError::Unsupported(e.to_string());
            statements.push(
                ddl::rename_created(&table_sql, Flavor::Sqlite, &new, None).map_err(unreadable)?,
            );
            for index in &indexes {
                let index_name = index_name(index).ok_or_else(|| {
                    ObjectError::Unsupported("an index of the table could not be read".into())
                })?;
                let copy = format!("{new_name}_{index_name}");
                // SQLite names the index's schema and its table bare; DuckDB the other way.
                let (name, table) = if engine == "sqlite" {
                    (
                        qualified_name(dialect.as_ref(), &request.new_schema, &[copy]),
                        quote(new_name),
                    )
                } else {
                    (quote(&copy), new.clone())
                };
                statements.push(
                    ddl::rename_created(index, Flavor::Sqlite, &name, Some(&table))
                        .map_err(unreadable)?,
                );
            }
            if request.with_data {
                if engine == "sqlite" {
                    kept.push(IdentityPosition);
                }
                let columns = plain_columns(session, &request.schema, &request.name).await?;
                statements.push(format!(
                    "INSERT INTO {new} ({columns}) SELECT {columns} FROM {old}"
                ));
            }
        }
        _ => {
            return Err(ObjectError::Unsupported(
                "this engine cannot duplicate a table here".into(),
            ));
        }
    }
    if request.with_data {
        kept.push(Rows);
    }
    let not_kept = ALL_PARTS
        .iter()
        .copied()
        .filter(|part| !kept.contains(part))
        .filter(|part| request.with_data || !matches!(part, Rows | IdentityPosition))
        .collect();
    Ok(DuplicatePlan {
        sql: statements
            .iter()
            .map(|s| format!("{};\n", s.trim().trim_end_matches(';').trim_end()))
            .collect(),
        kept,
        not_kept,
    })
}

/// The kept texts a catalog query returns, one per row.
async fn kept_texts(session: &Session, sql: &str) -> Result<Vec<String>, ObjectError> {
    Ok(catalog_rows(session, sql)
        .await?
        .into_iter()
        .filter_map(|row| match row.into_iter().next() {
            Some(Value::Text(text)) => Some(text),
            _ => None,
        })
        .collect())
}

/// The name of the index a kept `CREATE INDEX` creates, unquoted and without its schema.
fn index_name(sql: &str) -> Option<String> {
    // The name is the last part of what `rename_created` replaces: read it back the same way.
    let marker = "\u{0}";
    let renamed = ddl::rename_created(sql, Flavor::Sqlite, marker, None).ok()?;
    let at = renamed.find(marker)?;
    let tail_len = renamed.len() - at - marker.len();
    let original = &sql[at..sql.len() - tail_len];
    let last = original.rsplit('.').next()?.trim();
    Some(ddl::unquote(last))
}

/// The table's columns that take a value, quoted and joined, as its structure reads them
/// (generated columns compute their own).
async fn plain_columns(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<String, ObjectError> {
    let structure = table_structure(session, schema, table).await?;
    let dialect = session.dialect();
    let columns: Vec<String> = structure
        .columns
        .iter()
        .filter(|c| !c.generated)
        .map(|c| dialect.quote_identifier(&c.name))
        .collect();
    if columns.is_empty() {
        return Err(ObjectError::Gone(table.to_string()));
    }
    Ok(columns.join(", "))
}

/// PostgreSQL's columns that take a value (not generated), quoted by PostgreSQL itself.
async fn postgres_columns(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<String, ObjectError> {
    let literal = |text: &str| sql_literal(&Value::Text(text.to_string()), "postgres");
    let sql = format!(
        "SELECT string_agg(quote_ident(attname), ', ' ORDER BY attnum) FROM pg_attribute \
         WHERE attrelid = to_regclass(quote_ident({}) || '.' || quote_ident({})) \
         AND attnum > 0 AND NOT attisdropped AND attgenerated = ''",
        literal(schema),
        literal(table)
    );
    match scalar_metadata(session, &sql).await.map_err(failed)? {
        Some(Value::Text(columns)) if !columns.is_empty() => Ok(columns),
        Some(_) | None => Err(ObjectError::Gone(table.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_is_named_from_its_kept_text() {
        assert_eq!(
            index_name("CREATE INDEX idx_v ON t (v)").as_deref(),
            Some("idx_v")
        );
        assert_eq!(
            index_name("CREATE UNIQUE INDEX IF NOT EXISTS \"main\".\"My Idx\" ON \"t\" (v)")
                .as_deref(),
            Some("My Idx")
        );
    }

    #[test]
    fn kept_texts_take_the_new_names() {
        assert_eq!(
            ddl::rename_created(
                "CREATE TABLE \"orders\" (id INTEGER PRIMARY KEY, v INT DEFAULT 5, CHECK (v > 0))",
                Flavor::Sqlite,
                "main.orders_copy",
                None
            )
            .unwrap(),
            "CREATE TABLE main.orders_copy (id INTEGER PRIMARY KEY, v INT DEFAULT 5, CHECK (v > 0))"
        );
        assert_eq!(
            ddl::rename_created(
                "CREATE INDEX idx_v ON main.orders(v);",
                Flavor::Sqlite,
                "orders_copy_idx_v",
                Some("main.orders_copy")
            )
            .unwrap(),
            "CREATE INDEX orders_copy_idx_v ON main.orders_copy(v);"
        );
    }
}
