//! Pages of a query from the editor (ADR-054): only a single plain SELECT of one table whose
//! primary key the catalog reports, with every key column in the result, is paged, and its
//! pages are keyset pages: ordered by the user's ORDER BY and then the key, each page asking
//! for the rows after the last one shown. Any other statement runs once, as written.

use std::ops::ControlFlow;

use futures_util::StreamExt;
use spelaion_driver_api::{Dialect, TableInfo, Value};
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, Ident, ObjectName, OrderBy, OrderByExpr, OrderByKind,
    OrderByOptions, OrderBySort, Query, SelectItem, SetExpr, Statement, TableFactor,
    visit_expressions,
};
use sqlparser::parser::Parser;

use crate::classify::{ReadKind, classify_read_only};
use crate::export::sql_literal;
use crate::generate::scalar_count;
use crate::guard::sql_dialect;
use crate::objects::active_schema;
use crate::{CoreEvent, MAX_PAGE_ROWS, ResultId, Session};

/// The query, if it has a shape the core pages: one plain SELECT without its own limit,
/// grouping, functions or subqueries. Pages also need [`Keyset`].
fn pageable(dialect: &dyn Dialect, sql: &str) -> Option<Query> {
    if dialect.split_statements(sql).len() != 1
        || classify_read_only(dialect.name(), sql).ok()? != ReadKind::Query
    {
        return None;
    }
    let statements = Parser::parse_sql(sql_dialect(dialect.name()).as_ref(), sql).ok()?;
    let [Statement::Query(query)] = statements.as_slice() else {
        return None;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let plain_projection = select.projection.iter().all(|item| match item {
        SelectItem::Wildcard(options) | SelectItem::QualifiedWildcard(_, options) => {
            options.opt_ilike.is_none()
                && options.opt_exclude.is_none()
                && options.opt_except.is_none()
                && options.opt_replace.is_none()
                && options.opt_rename.is_none()
                && options.opt_alias.is_none()
        }
        SelectItem::UnnamedExpr(Expr::Identifier(_) | Expr::CompoundIdentifier(_))
        | SelectItem::ExprWithAlias {
            expr: Expr::Identifier(_) | Expr::CompoundIdentifier(_),
            ..
        } => true,
        _ => false,
    });
    let table = |relation: &TableFactor| {
        matches!(
            relation,
            TableFactor::Table {
                args: None,
                version: None,
                ..
            }
        )
    };
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
        || select.top.is_some()
        || select.into.is_some()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.exclude.is_some()
        || select.prewhere.is_some()
        || select.having.is_some()
        || select.qualify.is_some()
        || select.value_table_mode.is_some()
        || !select.lateral_views.is_empty()
        || !select.connect_by.is_empty()
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || !select.named_window.is_empty()
        || !matches!(&select.group_by, GroupByExpr::Expressions(items, _) if items.is_empty())
        || select.from.is_empty()
        || !plain_projection
        || !select.from.iter().all(|from| {
            table(&from.relation) && from.joins.iter().all(|join| table(&join.relation))
        })
    {
        return None;
    }
    if visit_expressions(query, |expr| {
        if matches!(
            expr,
            Expr::Function(_) | Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
        ) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
    {
        return None;
    }
    Some((**query).clone())
}

/// The one table a pageable query reads: its schema as written (if any), its name as the
/// catalog knows it, and how its columns are qualified in the query (its alias, or its name as
/// written). `None` for a join, several tables or a name with more than two parts.
fn target_table(engine: &str, query: &Query) -> Option<(Option<String>, String, String)> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let [from] = select.from.as_slice() else {
        return None;
    };
    if !from.joins.is_empty() {
        return None;
    }
    let TableFactor::Table { name, alias, .. } = &from.relation else {
        return None;
    };
    if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
        return None;
    }
    let parts: Vec<&Ident> = name.0.iter().map(|p| p.as_ident()).collect::<Option<_>>()?;
    let (schema, table) = match parts.as_slice() {
        [table] => (None, catalog_name(engine, table)),
        [schema, table] => (
            Some(catalog_name(engine, schema)),
            catalog_name(engine, table),
        ),
        _ => return None,
    };
    let qualifier = match alias {
        Some(alias) => alias.name.to_string(),
        None => ObjectName(name.0.clone()).to_string(),
    };
    Some((schema, table, qualifier))
}

/// A name as the catalog stores it: PostgreSQL folds unquoted names to lower case.
fn catalog_name(engine: &str, ident: &Ident) -> String {
    if engine == "postgres" && ident.quote_style.is_none() {
        ident.value.to_lowercase()
    } else {
        ident.value.clone()
    }
}

/// Two catalog names of the same column: exact on PostgreSQL, ignoring ASCII case elsewhere.
fn same_name(engine: &str, a: &str, b: &str) -> bool {
    if engine == "postgres" {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

/// Column types whose values the core writes back as literals that compare exactly as the
/// stored value does: integers, decimals, text, UUIDs and dates (ADR-054). The type as the
/// catalog writes it, its size and `unsigned` left out.
fn exact_literal_type(native_type: &str) -> bool {
    let lower = native_type.to_ascii_lowercase();
    let base = lower.split('(').next().unwrap_or_default();
    let base = base.trim().trim_end_matches(" unsigned").trim();
    matches!(
        base,
        "smallint"
            | "integer"
            | "int"
            | "bigint"
            | "tinyint"
            | "mediumint"
            | "int2"
            | "int4"
            | "int8"
            | "serial"
            | "smallserial"
            | "bigserial"
            | "numeric"
            | "decimal"
            | "text"
            | "varchar"
            | "character varying"
            | "char"
            | "character"
            | "bpchar"
            | "nvarchar"
            | "nchar"
            | "uuid"
            | "uniqueidentifier"
            | "date"
    )
}

/// One column the pages are ordered by.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderColumn {
    /// The column's name in the catalog.
    column: String,
    /// Its position in the result, and the name the result gives it there.
    position: usize,
    result_name: String,
    descending: bool,
}

/// How a query's pages are fetched: its parsed text, how its table's columns are qualified,
/// and the columns that order it totally (the user's ORDER BY, then the primary key).
#[derive(Debug, Clone)]
struct Keyset {
    query: Query,
    qualifier: String,
    order: Vec<OrderColumn>,
}

/// The keyset of `query` over `table` (its catalog description), or `None` when its pages
/// could not be fetched exactly: no primary key, a key column missing from the result, an
/// ORDER BY on something other than a `NOT NULL` column of the result, or a type whose
/// literals might not compare exactly.
fn keyset(engine: &str, query: Query, qualifier: String, table: &TableInfo) -> Option<Keyset> {
    if table.primary_key.is_empty() {
        return None;
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let column_of = |name: &str| {
        table
            .columns
            .iter()
            .find(|c| same_name(engine, &c.name, name))
    };
    // Where each column of the table is in the result, and the name the result gives it.
    let mut in_result: Vec<(String, usize, String)> = Vec::new();
    let mut aliases: Vec<String> = Vec::new();
    match select.projection.as_slice() {
        [SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)] => {
            for (position, column) in table.columns.iter().enumerate() {
                in_result.push((column.name.clone(), position, column.name.clone()));
            }
        }
        items => {
            for (position, item) in items.iter().enumerate() {
                let (ident, result_name) = match item {
                    SelectItem::UnnamedExpr(Expr::Identifier(ident)) => {
                        (ident, catalog_name(engine, ident))
                    }
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                        let last = parts.last()?;
                        (last, catalog_name(engine, last))
                    }
                    SelectItem::ExprWithAlias {
                        expr: Expr::Identifier(ident),
                        alias,
                    } => {
                        aliases.push(catalog_name(engine, alias));
                        (ident, catalog_name(engine, alias))
                    }
                    SelectItem::ExprWithAlias {
                        expr: Expr::CompoundIdentifier(parts),
                        alias,
                    } => {
                        aliases.push(catalog_name(engine, alias));
                        (parts.last()?, catalog_name(engine, alias))
                    }
                    _ => return None,
                };
                let column = column_of(&catalog_name(engine, ident))?;
                in_result.push((column.name.clone(), position, result_name));
            }
        }
    }
    // Two result columns with one name could not be told apart.
    for (i, (_, _, a)) in in_result.iter().enumerate() {
        if in_result[..i]
            .iter()
            .any(|(_, _, b)| a.eq_ignore_ascii_case(b))
        {
            return None;
        }
    }
    let position_of = |column: &str| {
        in_result
            .iter()
            .find(|(c, _, _)| same_name(engine, c, column))
            .map(|(_, position, result_name)| (*position, result_name.clone()))
    };
    // A primary key column is never NULL, except on SQLite, where only an `INTEGER PRIMARY
    // KEY` (the row id) is sure not to be: its other keys may hold NULLs.
    let never_null = |info: &spelaion_driver_api::ColumnInfo| {
        let key = table
            .primary_key
            .iter()
            .any(|k| same_name(engine, k, &info.name));
        !info.nullable
            || (key
                && (engine != "sqlite"
                    || (table.primary_key.len() == 1
                        && info.native_type.eq_ignore_ascii_case("integer"))))
    };
    let mut order: Vec<OrderColumn> = Vec::new();
    let push = |column: &str, descending: bool, order: &mut Vec<OrderColumn>| -> Option<()> {
        let info = column_of(column)?;
        if !never_null(info) || !exact_literal_type(&info.native_type) {
            return None;
        }
        if order
            .iter()
            .any(|o| same_name(engine, &o.column, &info.name))
        {
            return Some(());
        }
        let (position, result_name) = position_of(&info.name)?;
        order.push(OrderColumn {
            column: info.name.clone(),
            position,
            result_name,
            descending,
        });
        Some(())
    };
    if let Some(order_by) = &query.order_by {
        let OrderByKind::Expressions(items) = &order_by.kind else {
            return None;
        };
        if order_by.interpolate.is_some() {
            return None;
        }
        for item in items {
            let descending = match item.options.sort {
                None | Some(OrderBySort::Asc) => false,
                Some(OrderBySort::Desc) => true,
                Some(OrderBySort::Using(_)) => return None,
            };
            if item.options.nulls_first.is_some() || item.with_fill.is_some() {
                return None;
            }
            let ident = match &item.expr {
                // A bare name may be a result column's alias, which ORDER BY prefers.
                Expr::Identifier(ident)
                    if !aliases
                        .iter()
                        .any(|a| same_name(engine, a, &catalog_name(engine, ident))) =>
                {
                    ident
                }
                Expr::CompoundIdentifier(parts) => parts.last()?,
                _ => return None,
            };
            push(&catalog_name(engine, ident), descending, &mut order)?;
        }
    }
    for key in &table.primary_key {
        push(key, false, &mut order)?;
    }
    Some(Keyset {
        query,
        qualifier,
        order,
    })
}

impl Keyset {
    /// The page's SELECT: the user's query ordered by the keyset, the rows after `after` (the
    /// order columns' values in the last row shown) when given, at most `limit` rows.
    fn page_sql(
        &self,
        dialect: &dyn Dialect,
        limit: u64,
        after: Option<&[Value]>,
    ) -> Option<String> {
        let engine = dialect.name();
        let parser = sql_dialect(engine);
        let parse = |text: &str| {
            Parser::new(parser.as_ref())
                .try_with_sql(text)
                .ok()?
                .parse_expr()
                .ok()
        };
        let column =
            |c: &OrderColumn| format!("{}.{}", self.qualifier, dialect.quote_identifier(&c.column));
        let mut query = self.query.clone();
        let order = self
            .order
            .iter()
            .map(|c| {
                Some(OrderByExpr {
                    expr: parse(&column(c))?,
                    options: OrderByOptions {
                        sort: c.descending.then_some(OrderBySort::Desc),
                        nulls_first: None,
                    },
                    with_fill: None,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        query.order_by = Some(OrderBy {
            kind: OrderByKind::Expressions(order),
            interpolate: None,
        });
        if let Some(values) = after {
            if values.len() != self.order.len() || values.iter().any(|v| matches!(v, Value::Null)) {
                return None;
            }
            // (a > x) OR (a = x AND b > y) OR …, with < for a descending column.
            let literals: Vec<String> = values.iter().map(|v| sql_literal(v, engine)).collect();
            let terms: Vec<String> = (0..self.order.len())
                .map(|i| {
                    let mut parts: Vec<String> = (0..i)
                        .map(|j| format!("{} = {}", column(&self.order[j]), literals[j]))
                        .collect();
                    let op = if self.order[i].descending { "<" } else { ">" };
                    parts.push(format!("{} {op} {}", column(&self.order[i]), literals[i]));
                    format!("({})", parts.join(" AND "))
                })
                .collect();
            let condition = parse(&terms.join(" OR "))?;
            let SetExpr::Select(select) = query.body.as_mut() else {
                return None;
            };
            select.selection = Some(match select.selection.take() {
                Some(filter) => Expr::BinaryOp {
                    left: Box::new(Expr::Nested(Box::new(filter))),
                    op: BinaryOperator::And,
                    right: Box::new(Expr::Nested(Box::new(condition))),
                },
                None => condition,
            });
        }
        Some(if engine == "sqlserver" {
            format!("{query}\nOFFSET 0 ROWS FETCH NEXT {limit} ROWS ONLY")
        } else {
            format!("{query}\nLIMIT {limit}")
        })
    }
}

/// The keyset of `sql` on `session`, with its table's key read from the catalog now.
async fn resolve(session: &Session, sql: &str) -> Option<Keyset> {
    let dialect = session.dialect();
    let engine = dialect.name();
    let query = pageable(dialect.as_ref(), sql)?;
    let (schema, table, qualifier) = target_table(engine, &query)?;
    let schema = match schema {
        Some(schema) => schema,
        None => active_schema(session).await.ok()??,
    };
    let info = session
        .connection()
        .describe_table(&schema, &table)
        .await
        .ok()?;
    keyset(engine, query, qualifier, &info)
}

/// Whether the editor pages `sql` (ADR-054). When it does not, the app runs it as written.
pub async fn query_pages_exactly(session: &Session, sql: &str) -> bool {
    resolve(session, sql).await.is_some()
}

/// The first page of `sql`, or `None` when it is not paged and must run as written.
pub async fn first_query_page_sql(session: &Session, sql: &str, limit: u64) -> Option<String> {
    if limit == 0 || limit > MAX_PAGE_ROWS {
        return None;
    }
    resolve(session, sql)
        .await?
        .page_sql(session.dialect().as_ref(), limit, None)
}

/// Counts the original query, without its ordering, so Count always means all matching rows.
pub fn query_count_sql(dialect: &dyn Dialect, sql: &str) -> Option<String> {
    let mut query = pageable(dialect, sql)?;
    query.order_by = None;
    // The selected columns do not affect the number of rows. Replace them with one value:
    // `SELECT *` from a join can have duplicate column names, which MySQL rejects in a subquery.
    let template = Parser::parse_sql(sql_dialect(dialect.name()).as_ref(), "SELECT 1").ok()?;
    let [Statement::Query(template)] = template.as_slice() else {
        return None;
    };
    let SetExpr::Select(template) = template.body.as_ref() else {
        return None;
    };
    let SetExpr::Select(select) = query.body.as_mut() else {
        return None;
    };
    select.projection.clone_from(&template.projection);
    Some(format!(
        "SELECT COUNT(*) FROM ({query}) AS spelaion_counted"
    ))
}

pub async fn count_query_rows(session: &Session, sql: &str) -> Result<i64, String> {
    let sql =
        query_count_sql(session.dialect().as_ref(), sql).ok_or("this query cannot be counted")?;
    scalar_count(session, &sql).await
}

/// Fetches the page after the last row of `target` (which holds `offset` rows) into a
/// temporary result, then appends it atomically. The last row's order values come from the
/// stored result, read in the core (a masked value's original too: it never leaves the core).
/// A failed page never alters the rows already shown; `offset` also refuses a fetch for a
/// result that changed meanwhile.
pub async fn append_query_page(
    session: &Session,
    target: ResultId,
    sql: &str,
    limit: u64,
    offset: u64,
) -> Result<usize, String> {
    if limit == 0 || limit > MAX_PAGE_ROWS {
        return Err(format!("a page holds from 1 to {MAX_PAGE_ROWS} rows"));
    }
    let keyset = resolve(session, sql)
        .await
        .ok_or("this query cannot be paged")?;
    let after = {
        let stored = session.results().get(target).ok_or("result was freed")?;
        let stored = stored.read().map_err(|_| "result lock poisoned")?;
        if stored.rows() as u64 != offset || offset == 0 {
            return Err("the result changed while fetching more rows".into());
        }
        let last = stored.rows() - 1;
        keyset
            .order
            .iter()
            .map(|c| {
                let meta = stored
                    .schema()
                    .columns
                    .get(c.position)
                    .ok_or("the result's columns changed")?;
                if !meta.name.eq_ignore_ascii_case(&c.result_name) {
                    return Err("the result's columns changed".to_string());
                }
                if stored.is_masked(c.position) {
                    stored
                        .reveal(last, c.position)
                        .map(|cell| cell.value)
                        .map_err(|e| e.to_string())
                } else {
                    stored
                        .value_at(last, c.position)
                        .ok_or_else(|| "no last row".to_string())
                }
            })
            .collect::<Result<Vec<Value>, String>>()?
    };
    let paged = keyset
        .page_sql(session.dialect().as_ref(), limit, Some(&after))
        .ok_or("the next page could not be written")?;
    let mut events = session.run_generated(&paged);
    let mut results = Vec::new();
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => results.push(result),
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
    let outcome = if let Some(error) = failure {
        Err(error)
    } else if let [source] = results.as_slice() {
        session
            .results()
            .append_page(target, *source, offset as usize)
    } else {
        Err("the query did not return one result".into())
    };
    for result in results {
        session.results().remove(result);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_api::{ColumnInfo, ObjectInfo, ObjectKind};

    fn column(name: &str, native_type: &str, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            native_type: native_type.into(),
            nullable,
            default: None,
            comment: None,
        }
    }

    fn orders() -> TableInfo {
        TableInfo {
            object: ObjectInfo {
                schema: "shop".into(),
                name: "orders".into(),
                kind: ObjectKind::Table,
                estimated_rows: None,
                comment: None,
            },
            columns: vec![
                column("id", "integer", false),
                column("customer", "character varying(80)", false),
                column("total", "double precision", false),
                column("note", "text", true),
            ],
            primary_key: vec!["id".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        }
    }

    fn plan(dialect: &dyn Dialect, sql: &str, table: &TableInfo) -> Option<Keyset> {
        let query = pageable(dialect, sql)?;
        let (_, _, qualifier) = target_table(dialect.name(), &query)?;
        keyset(dialect.name(), query, qualifier, table)
    }

    #[test]
    fn keyset_pages_follow_the_order_then_the_key() {
        let pg = spelaion_driver_postgres::PostgresDialect;
        let k = plan(
            &pg,
            "select * from orders where customer <> 'x' order by customer desc",
            &orders(),
        )
        .unwrap();
        assert_eq!(
            k.page_sql(&pg, 100, None).unwrap(),
            "SELECT * FROM orders WHERE customer <> 'x' ORDER BY orders.customer DESC, orders.id\nLIMIT 100"
        );
        assert_eq!(
            k.page_sql(&pg, 100, Some(&[Value::Text("o'b".into()), Value::Int(7)]))
                .unwrap(),
            "SELECT * FROM orders WHERE (customer <> 'x') AND ((orders.customer < 'o''b') OR (orders.customer = 'o''b' AND orders.id > 7)) ORDER BY orders.customer DESC, orders.id\nLIMIT 100"
        );
        // An alias qualifies the columns; SQL Server fetches with OFFSET 0 … FETCH.
        let ms = spelaion_driver_sqlserver::SqlServerDialect;
        let k = plan(
            &ms,
            "select o.id, o.customer as who from shop.orders o",
            &orders(),
        )
        .unwrap();
        assert_eq!(
            k.page_sql(&ms, 50, Some(&[Value::Int(3)])).unwrap(),
            "SELECT o.id, o.customer AS who FROM shop.orders o WHERE (o.id > 3) ORDER BY o.id\nOFFSET 0 ROWS FETCH NEXT 50 ROWS ONLY"
        );
        assert_eq!(k.order[0].position, 0);
    }

    #[test]
    fn only_queries_that_page_exactly_are_paged() {
        let pg = spelaion_driver_postgres::PostgresDialect;
        let table = orders();
        for sql in [
            // The key is not in the result.
            "select customer from orders",
            // Ordered by a column that can be NULL, or whose values might not round-trip.
            "select * from orders order by note",
            "select * from orders order by total",
            // Ordered by an expression, an alias or with NULLS options.
            "select * from orders order by lower(customer)",
            "select id, customer as id2 from orders order by id2",
            "select customer as id, id as k from orders order by id",
            "select * from orders order by customer nulls first",
            // Two result columns with one name.
            "select id, id from orders",
            // Joins, own limits, grouping: not even pageable.
            "select * from orders join items on items.order_id = orders.id",
            "select * from orders limit 5",
            "select count(*) from orders",
            "select * from orders; delete from orders",
            "delete from orders",
        ] {
            assert!(plan(&pg, sql, &table).is_none(), "{sql}");
        }
        let mut keyless = table.clone();
        keyless.primary_key.clear();
        assert!(plan(&pg, "select * from orders", &keyless).is_none());
        // Ordered by the key itself, or by a NOT NULL text column and then the key.
        assert!(plan(&pg, "select * from orders order by id desc", &table).is_some());
        assert!(
            plan(
                &pg,
                "select id, customer from orders order by customer",
                &table
            )
            .is_some()
        );
        // A quoted name keeps its case on PostgreSQL: "ID" is not the key column.
        assert!(plan(&pg, "select \"ID\" from orders", &table).is_none());
    }

    #[test]
    fn counts_keep_working_for_pageable_queries() {
        let d = spelaion_driver_sqlite::SqliteDialect;
        assert_eq!(
            query_count_sql(&d, "select id from orders order by id;"),
            Some("SELECT COUNT(*) FROM (SELECT 1 FROM orders) AS spelaion_counted".into())
        );
    }
}
