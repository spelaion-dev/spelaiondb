//! `CREATE TABLE` for the explorer's "Generate DDL", composed from the catalog views (SQL
//! Server keeps no text for a table), and a view's own text (`OBJECT_DEFINITION`). Names are
//! quoted by our dialect; expressions (defaults, checks, computed columns, filters) are the
//! server's.

use spelaion_driver_api::{Dialect, DriverError, Result, terminated};

use crate::SqlServerConnection;
use crate::dialect::SqlServerDialect;
use crate::introspect::native_type;

fn text(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten().filter(|s| !s.is_empty())
}

fn flag(row: &[Option<String>], i: usize) -> bool {
    text(row, i) == "1"
}

fn number(row: &[Option<String>], i: usize) -> i64 {
    text(row, i).parse().unwrap_or(0)
}

fn quote(name: &str) -> String {
    SqlServerDialect.quote_identifier(name)
}

/// An N'…' literal.
fn literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// The object, by schema and name, as `object_id` reads it.
const OBJECT: &str = "object_id(quotename(@P1) + '.' + quotename(@P2))";

pub async fn relation_definition(
    conn: &SqlServerConnection,
    schema: &str,
    name: &str,
) -> Result<String> {
    let params = || vec![schema.to_string(), name.to_string()];
    let rows = conn
        .rows(
            &format!(
                "select o.type, object_definition(o.object_id) \
                 from sys.objects o where o.object_id = {OBJECT} and o.type in ('U', 'V')"
            ),
            params(),
        )
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;
    if text(row, 0).trim() == "V" {
        return opt(row, 1).map(|d| terminated(&d)).ok_or_else(|| {
            DriverError::Other(format!(
                "the definition of {schema}.{name} is encrypted or not visible to this user"
            ))
        });
    }
    let qualified = format!("{}.{}", quote(schema), quote(name));
    let mut lines = columns(conn, params()).await?;
    lines.extend(key_and_check_constraints(conn, params()).await?);
    lines.extend(foreign_keys(conn, params()).await?);
    let mut statements = vec![format!(
        "CREATE TABLE {qualified} (\n{}\n);",
        lines.join(",\n")
    )];
    statements.extend(indexes(conn, params(), &qualified).await?);
    statements.extend(descriptions(conn, params(), schema, name).await?);
    Ok(statements.join("\n\n"))
}

async fn columns(conn: &SqlServerConnection, params: Vec<String>) -> Result<Vec<String>> {
    let rows = conn
        .rows(
            &format!(
                "select c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                        c.is_identity, cast(ic.seed_value as nvarchar(40)), \
                        cast(ic.increment_value as nvarchar(40)), cc.definition, \
                        cc.is_persisted, dc.name, dc.definition \
                 from sys.columns c \
                 join sys.types t on t.user_type_id = c.user_type_id \
                 left join sys.identity_columns ic \
                   on ic.object_id = c.object_id and ic.column_id = c.column_id \
                 left join sys.computed_columns cc \
                   on cc.object_id = c.object_id and cc.column_id = c.column_id \
                 left join sys.default_constraints dc on dc.object_id = c.default_object_id \
                 where c.object_id = {OBJECT} order by c.column_id"
            ),
            params,
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let name = quote(&text(row, 0));
            if let Some(expression) = opt(row, 9) {
                let persisted = if flag(row, 10) { " PERSISTED" } else { "" };
                return format!("    {name} AS {expression}{persisted}");
            }
            let mut line = format!(
                "    {name} {}",
                native_type(
                    &text(row, 1),
                    number(row, 2),
                    number(row, 3),
                    number(row, 4)
                )
            );
            if flag(row, 6) {
                line.push_str(&format!(" IDENTITY({}, {})", text(row, 7), text(row, 8)));
            }
            if let (Some(default_name), Some(expression)) = (opt(row, 11), opt(row, 12)) {
                line.push_str(&format!(
                    " CONSTRAINT {} DEFAULT {expression}",
                    quote(&default_name)
                ));
            }
            line.push_str(if flag(row, 5) { " NULL" } else { " NOT NULL" });
            line
        })
        .collect())
}

/// Primary key and unique constraints (with their columns, in order) and checks.
async fn key_and_check_constraints(
    conn: &SqlServerConnection,
    params: Vec<String>,
) -> Result<Vec<String>> {
    let rows = conn
        .rows(
            &format!(
                "select kc.name, kc.type, i.type_desc, c.name, ic.is_descending_key, null, \
                        case kc.type when 'PK' then 0 else 1 end \
                 from sys.key_constraints kc \
                 join sys.indexes i \
                   on i.object_id = kc.parent_object_id and i.index_id = kc.unique_index_id \
                 join sys.index_columns ic \
                   on ic.object_id = i.object_id and ic.index_id = i.index_id \
                 join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
                 where kc.parent_object_id = {OBJECT} and ic.is_included_column = 0 \
                 union all \
                 select cc.name, 'C', null, null, null, cc.definition, 2 \
                 from sys.check_constraints cc where cc.parent_object_id = {OBJECT} \
                 order by 7, 1"
            ),
            params,
        )
        .await?;
    let mut out: Vec<(String, String)> = Vec::new();
    for row in &rows {
        let name = text(row, 0);
        if text(row, 1).trim() == "C" {
            out.push((
                name.clone(),
                format!("    CONSTRAINT {} CHECK {}", quote(&name), text(row, 5)),
            ));
            continue;
        }
        let column = format!(
            "{}{}",
            quote(&text(row, 3)),
            if flag(row, 4) { " DESC" } else { "" }
        );
        match out.last_mut() {
            Some((last, line)) if *last == name => {
                line.insert_str(line.len() - 1, &format!(", {column}"));
            }
            _ => {
                let kind = if text(row, 1).trim() == "PK" {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                };
                out.push((
                    name.clone(),
                    format!(
                        "    CONSTRAINT {} {kind} {} ({column})",
                        quote(&name),
                        text(row, 2)
                    ),
                ));
            }
        }
    }
    Ok(out.into_iter().map(|(_, line)| line).collect())
}

struct ForeignKey {
    name: String,
    columns: Vec<String>,
    table: String,
    referenced: Vec<String>,
    actions: String,
}

async fn foreign_keys(conn: &SqlServerConnection, params: Vec<String>) -> Result<Vec<String>> {
    let rows = conn
        .rows(
            &format!(
                "select fk.name, pc.name, schema_name(rt.schema_id), rt.name, rc.name, \
                        fk.delete_referential_action_desc, fk.update_referential_action_desc \
                 from sys.foreign_keys fk \
                 join sys.foreign_key_columns fkc on fkc.constraint_object_id = fk.object_id \
                 join sys.columns pc \
                   on pc.object_id = fkc.parent_object_id and pc.column_id = fkc.parent_column_id \
                 join sys.tables rt on rt.object_id = fkc.referenced_object_id \
                 join sys.columns rc on rc.object_id = fkc.referenced_object_id \
                  and rc.column_id = fkc.referenced_column_id \
                 where fk.parent_object_id = {OBJECT} \
                 order by fk.name, fkc.constraint_column_id"
            ),
            params,
        )
        .await?;
    let mut keys: Vec<ForeignKey> = Vec::new();
    for row in &rows {
        let name = text(row, 0);
        match keys.last_mut() {
            Some(key) if key.name == name => {
                key.columns.push(quote(&text(row, 1)));
                key.referenced.push(quote(&text(row, 4)));
            }
            _ => {
                let mut actions = String::new();
                for (event, i) in [("DELETE", 5), ("UPDATE", 6)] {
                    let action = text(row, i);
                    if action != "NO_ACTION" && !action.is_empty() {
                        actions.push_str(&format!(" ON {event} {}", action.replace('_', " ")));
                    }
                }
                keys.push(ForeignKey {
                    name,
                    columns: vec![quote(&text(row, 1))],
                    table: format!("{}.{}", quote(&text(row, 2)), quote(&text(row, 3))),
                    referenced: vec![quote(&text(row, 4))],
                    actions,
                });
            }
        }
    }
    Ok(keys
        .into_iter()
        .map(|key| {
            format!(
                "    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){}",
                quote(&key.name),
                key.columns.join(", "),
                key.table,
                key.referenced.join(", "),
                key.actions
            )
        })
        .collect())
}

struct Index {
    name: String,
    header: String,
    columns: Vec<String>,
    included: Vec<String>,
    filter: Option<String>,
}

/// The indexes that are not behind a primary key or unique constraint, with their included
/// columns and filter.
async fn indexes(
    conn: &SqlServerConnection,
    params: Vec<String>,
    qualified: &str,
) -> Result<Vec<String>> {
    let rows = conn
        .rows(
            &format!(
                "select i.name, i.is_unique, i.type_desc, c.name, ic.is_descending_key, \
                        ic.is_included_column, i.filter_definition \
                 from sys.indexes i \
                 join sys.index_columns ic \
                   on ic.object_id = i.object_id and ic.index_id = i.index_id \
                 join sys.columns c on c.object_id = ic.object_id and c.column_id = ic.column_id \
                 where i.object_id = {OBJECT} and i.is_primary_key = 0 \
                   and i.is_unique_constraint = 0 and i.type > 0 \
                 order by i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
            ),
            params,
        )
        .await?;
    let mut out: Vec<Index> = Vec::new();
    for row in &rows {
        let name = text(row, 0);
        if out.last().is_none_or(|index| index.name != name) {
            out.push(Index {
                name: name.clone(),
                header: format!(
                    "CREATE {}{} INDEX {} ON {qualified}",
                    if flag(row, 1) { "UNIQUE " } else { "" },
                    text(row, 2),
                    quote(&name)
                ),
                columns: Vec::new(),
                included: Vec::new(),
                filter: opt(row, 6),
            });
        }
        let index = out.last_mut().expect("just added");
        let column = quote(&text(row, 3));
        if flag(row, 5) {
            index.included.push(column);
        } else {
            index.columns.push(format!(
                "{column}{}",
                if flag(row, 4) { " DESC" } else { "" }
            ));
        }
    }
    Ok(out
        .into_iter()
        .map(|index| {
            let mut sql = format!("{} ({})", index.header, index.columns.join(", "));
            if !index.included.is_empty() {
                sql.push_str(&format!(" INCLUDE ({})", index.included.join(", ")));
            }
            if let Some(filter) = index.filter {
                sql.push_str(&format!(" WHERE {filter}"));
            }
            format!("{sql};")
        })
        .collect())
}

/// The table's and its columns' `MS_Description`, which the explorer shows as comments.
async fn descriptions(
    conn: &SqlServerConnection,
    params: Vec<String>,
    schema: &str,
    name: &str,
) -> Result<Vec<String>> {
    let rows = conn
        .rows(
            &format!(
                "select c.name, cast(ep.value as nvarchar(4000)) \
                 from sys.extended_properties ep \
                 left join sys.columns c \
                   on c.object_id = ep.major_id and c.column_id = ep.minor_id \
                 where ep.class = 1 and ep.major_id = {OBJECT} and ep.name = 'MS_Description' \
                 order by ep.minor_id"
            ),
            params,
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let column = match opt(row, 0) {
                Some(column) => format!(
                    ", @level2type = N'COLUMN', @level2name = {}",
                    literal(&column)
                ),
                None => String::new(),
            };
            format!(
                "EXEC sys.sp_addextendedproperty @name = N'MS_Description', @value = {}, \
                 @level0type = N'SCHEMA', @level0name = {}, @level1type = N'TABLE', \
                 @level1name = {}{column};",
                literal(&text(row, 1)),
                literal(schema),
                literal(name)
            )
        })
        .collect())
}
