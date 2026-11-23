//! Pages of a query from the editor (ADR-054): only a single plain SELECT of one table whose
//! primary key the catalog reports, with every key column in the result, is paged, and its
//! pages are keyset pages: ordered by the user's ORDER BY and then the key, each page asking
//! for the rows after the last one shown. Any other statement runs once, as written.
//!
//! The grid's sort of a paged query (its header clicks) is read again from the server the same
//! way: the columns clicked order the pages before the query's own ORDER BY and the key. Such
//! a column may hold NULLs: the core then writes where they go itself (last, in both
//! directions, as the grid sorts stored rows), because each engine puts them elsewhere, and
//! the comparison that fetches the next page follows the same rule.

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
use crate::sort::{MAX_SORT_KEYS, SortKey};
use crate::{CoreEvent, MAX_PAGE_ROWS, ResultEnd, ResultId, Session};

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

/// SQL Server's non-Unicode text types. A text literal is written `N'…'` (it keeps every
/// character), and comparing such a column with it converts the column to `nvarchar`: with a
/// SQL collation the comparison then follows Unicode rules while its ORDER BY follows the
/// collation's own, and the two disagree on hyphens and apostrophes (SDB-404).
fn sqlserver_non_unicode(engine: &str, native_type: &str) -> bool {
    let lower = native_type.to_ascii_lowercase();
    let base = lower.split('(').next().unwrap_or_default().trim();
    engine == "sqlserver" && matches!(base, "varchar" | "char" | "text")
}

/// One column the pages are ordered by.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrderColumn {
    /// SQL Server `varchar` or `char`: its literal is cast to `varchar`, so the comparison
    /// follows the column's collation as its ORDER BY does.
    non_unicode: bool,
    /// The column's name in the catalog.
    column: String,
    /// Its position in the result, and the name the result gives it there.
    position: usize,
    result_name: String,
    descending: bool,
    /// The column may hold NULLs (only a column of the grid's sort may): the pages put them
    /// last, whatever the direction and the engine.
    nullable: bool,
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
///
/// `sort` is the grid's sort, by position in the result: those columns order the pages first.
/// They may be nullable, but not `masked` (asked with the column's name in the catalog and
/// in the result): the order of a masked column would tell its values apart (ADR-051).
fn keyset(
    engine: &str,
    query: Query,
    qualifier: String,
    table: &TableInfo,
    sort: &[SortKey],
    masked: &dyn Fn(&str, &str) -> bool,
) -> Option<Keyset> {
    if table.primary_key.is_empty() || sort.len() > MAX_SORT_KEYS {
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
    let push = |column: &str,
                descending: bool,
                may_be_null: bool,
                order: &mut Vec<OrderColumn>|
     -> Option<()> {
        let info = column_of(column)?;
        let nullable = !never_null(info);
        if (nullable && !may_be_null) || !exact_literal_type(&info.native_type) {
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
            non_unicode: sqlserver_non_unicode(engine, &info.native_type),
            column: info.name.clone(),
            position,
            result_name,
            descending,
            nullable,
        });
        Some(())
    };
    // The grid's sort first: each key is a result column, which is a column of the table.
    for (i, key) in sort.iter().enumerate() {
        if sort[..i].iter().any(|k| k.column == key.column) {
            return None;
        }
        let (column, _, result_name) = in_result
            .iter()
            .find(|(_, position, _)| *position == key.column)?;
        if masked(column, result_name) {
            return None;
        }
        push(column, key.descending, true, &mut order)?;
    }
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
            push(&catalog_name(engine, ident), descending, false, &mut order)?;
        }
    }
    for key in &table.primary_key {
        push(key, false, false, &mut order)?;
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
        let mut order = Vec::new();
        for c in &self.order {
            if c.nullable {
                // NULLs last in both directions, written out: PostgreSQL and DuckDB put them
                // after every value, MySQL, SQL Server and SQLite before, and `NULLS LAST`
                // is not SQL everywhere.
                order.push(OrderByExpr {
                    expr: parse(&format!(
                        "CASE WHEN {} IS NULL THEN 1 ELSE 0 END",
                        column(c)
                    ))?,
                    options: OrderByOptions {
                        sort: None,
                        nulls_first: None,
                    },
                    with_fill: None,
                });
            }
            order.push(OrderByExpr {
                expr: parse(&column(c))?,
                options: OrderByOptions {
                    sort: c.descending.then_some(OrderBySort::Desc),
                    nulls_first: None,
                },
                with_fill: None,
            });
        }
        query.order_by = Some(OrderBy {
            kind: OrderByKind::Expressions(order),
            interpolate: None,
        });
        if let Some(values) = after {
            if values.len() != self.order.len()
                || values
                    .iter()
                    .zip(&self.order)
                    .any(|(v, c)| matches!(v, Value::Null) && !c.nullable)
            {
                return None;
            }
            // (a > x) OR (a = x AND b > y) OR …, with < for a descending column. Where a
            // column may be NULL, its NULLs come after every value: a row with one is after
            // any value (`a > x OR a IS NULL`), nothing is after a NULL in that column (no
            // term for it), and two NULLs are equal there (`a IS NULL`).
            // A value read from a SQL Server `varchar` is written back as `varchar`: converted
            // with the database's code page, which a column's value always fits unless the
            // column has a collation of another code page (a known limit, in ADR-054).
            let literals: Vec<String> = values
                .iter()
                .zip(&self.order)
                .map(|(v, c)| {
                    let literal = sql_literal(v, engine);
                    if c.non_unicode && matches!(v, Value::Text(_)) {
                        format!("CAST({literal} AS varchar(max))")
                    } else {
                        literal
                    }
                })
                .collect();
            let null = |i: usize| matches!(values[i], Value::Null);
            let terms: Vec<String> = (0..self.order.len())
                .filter(|&i| !null(i))
                .map(|i| {
                    let mut parts: Vec<String> = (0..i)
                        .map(|j| {
                            if null(j) {
                                format!("{} IS NULL", column(&self.order[j]))
                            } else {
                                format!("{} = {}", column(&self.order[j]), literals[j])
                            }
                        })
                        .collect();
                    let c = &self.order[i];
                    let op = if c.descending { "<" } else { ">" };
                    parts.push(if c.nullable {
                        format!(
                            "({name} {op} {} OR {name} IS NULL)",
                            literals[i],
                            name = column(c)
                        )
                    } else {
                        format!("{} {op} {}", column(c), literals[i])
                    });
                    format!("({})", parts.join(" AND "))
                })
                .collect();
            // The key's columns are never NULL, so the last term is always there.
            if terms.is_empty() {
                return None;
            }
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

/// Whether the primary keys an engine's catalog reports are enforced unique (SDB-405), by the
/// name of its dialect (MariaDB reads as `mysql`, libSQL as `sqlite`) and, for PostgreSQL and
/// MySQL, the server the session reached. ClickHouse's primary key is a sorting key, Trino's,
/// Presto's, Hive's and Flight SQL's come from connectors or servers that need not enforce
/// one, Databend and TDengine enforce none here: their queries are not paged, they run as
/// written.
fn unique_keys(engine: &str, family: Option<&crate::ServerFamily>) -> bool {
    match engine {
        "sqlite" | "sqlserver" | "duckdb" | "oracle" | "spanner" => true,
        // The server is asked when the session opens (`open_connection` always sets it for
        // these two, `Unchecked` when it cannot tell): a server not known to enforce them is
        // not paged. A session made without one is the engine itself, as for the read-only
        // checks (`Session::read_only_transaction_refusal`).
        "postgres" | "mysql" => family.is_none_or(crate::ServerFamily::enforces_unique_keys),
        _ => false,
    }
}

/// The keyset of `sql` on `session` ordered by the grid's `sort` first, with its table's key
/// read from the catalog now.
async fn resolve(session: &Session, sql: &str, sort: &[SortKey]) -> Option<Keyset> {
    resolve_order(session, sql, sort, false)
        .await
        .map(|(keyset, _)| keyset)
}

/// Resolves the first page, retrying an ineligible grid sort against the same catalog read.
async fn resolve_order(
    session: &Session,
    sql: &str,
    sort: &[SortKey],
    use_query_order_if_needed: bool,
) -> Option<(Keyset, bool)> {
    let dialect = session.dialect();
    let engine = dialect.name();
    if !unique_keys(engine, session.server_family()) {
        return None;
    }
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
    let masking = session.masking();
    let masked = |column: &str, result_name: &str| {
        masking.masks_table_column(&table, column) || masking.masks(result_name)
    };
    if use_query_order_if_needed && !sort.is_empty() {
        if let Some(keyset) = keyset(
            engine,
            query.clone(),
            qualifier.clone(),
            &info,
            sort,
            &masked,
        ) {
            return Some((keyset, true));
        }
        return keyset(engine, query, qualifier, &info, &[], &masked).map(|keyset| (keyset, false));
    }
    keyset(engine, query, qualifier, &info, sort, &masked).map(|keyset| (keyset, !sort.is_empty()))
}

/// Whether the editor pages `sql` (ADR-054) ordered by the grid's `sort` first (result
/// columns by position, the most significant first; none for the query's own order). When it
/// does not, the app runs it as written, or sorts only the rows it has loaded.
pub async fn query_pages_exactly(session: &Session, sql: &str, sort: &[SortKey]) -> bool {
    resolve(session, sql, sort).await.is_some()
}

/// The first page of `sql` ordered by the grid's `sort` first, or `None` when it is not paged
/// that way and must run as written.
pub async fn first_query_page_sql(
    session: &Session,
    sql: &str,
    limit: u64,
    sort: &[SortKey],
) -> Option<String> {
    if limit == 0 || limit > MAX_PAGE_ROWS {
        return None;
    }
    resolve(session, sql, sort)
        .await?
        .page_sql(session.dialect().as_ref(), limit, None)
}

/// Prepares the first editor page with one catalog read. The boolean says whether the
/// requested grid sort was used. Without a page, the editor runs the original SQL.
pub async fn editor_query_page_sql(
    session: &Session,
    sql: &str,
    limit: u64,
    sort: &[SortKey],
) -> Option<(String, bool)> {
    if limit == 0 || limit > MAX_PAGE_ROWS {
        return None;
    }
    let (keyset, used_sort) = resolve_order(session, sql, sort, true).await?;
    let page = keyset.page_sql(session.dialect().as_ref(), limit, None)?;
    Some((page, used_sort))
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
/// result that changed meanwhile. `sort` is the grid's sort the first page was read with.
pub async fn append_query_page(
    session: &Session,
    target: ResultId,
    sql: &str,
    limit: u64,
    offset: u64,
    sort: &[SortKey],
) -> Result<usize, String> {
    if limit == 0 || limit > MAX_PAGE_ROWS {
        return Err(format!("a page holds from 1 to {MAX_PAGE_ROWS} rows"));
    }
    let keyset = resolve(session, sql, sort)
        .await
        .ok_or("this query cannot be paged")?;
    let after = {
        let stored = session
            .results()
            .stored(target)
            .map_err(|e| e.to_string())?;
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
    // Only a whole page is appended (SDB-367): a page cut short would leave a gap before the
    // next one.
    let mut whole = true;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => results.push(result),
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            CoreEvent::ResultEnded { end, .. } => whole &= end == ResultEnd::Whole,
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
    } else if !whole {
        Err("the page was cut short: nothing was added".into())
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
        keyset(dialect.name(), query, qualifier, table, &[], &|_, _| false)
    }

    /// [`plan`] with the grid's sort (result position, descending) and the masked columns.
    fn sorted_plan(
        dialect: &dyn Dialect,
        sql: &str,
        table: &TableInfo,
        sort: &[(usize, bool)],
        masked: &[&str],
    ) -> Option<Keyset> {
        let query = pageable(dialect, sql)?;
        let (_, _, qualifier) = target_table(dialect.name(), &query)?;
        let sort: Vec<SortKey> = sort
            .iter()
            .map(|&(column, descending)| SortKey { column, descending })
            .collect();
        keyset(
            dialect.name(),
            query,
            qualifier,
            table,
            &sort,
            &|column, _| masked.contains(&column),
        )
    }

    #[test]
    fn the_grids_sort_orders_the_pages_first_with_nulls_last() {
        let pg = spelaion_dialect::PostgresDialect;
        let table = orders();
        // `note` may be NULL, `customer` may not: only the first gets the CASE.
        let k = sorted_plan(
            &pg,
            "select * from orders order by customer",
            &table,
            &[(3, true), (1, false)],
            &[],
        )
        .unwrap();
        assert_eq!(
            k.page_sql(&pg, 10, None).unwrap(),
            "SELECT * FROM orders ORDER BY CASE WHEN orders.note IS NULL THEN 1 ELSE 0 END, orders.note DESC, orders.customer, orders.id\nLIMIT 10"
        );
        // After a row with a note: smaller notes and every NULL, then ties by the rest.
        let after = [
            Value::Text("m".into()),
            Value::Text("ada".into()),
            Value::Int(7),
        ];
        assert_eq!(
            k.page_sql(&pg, 10, Some(&after)).unwrap(),
            "SELECT * FROM orders WHERE ((orders.note < 'm' OR orders.note IS NULL)) OR (orders.note = 'm' AND orders.customer > 'ada') OR (orders.note = 'm' AND orders.customer = 'ada' AND orders.id > 7) ORDER BY CASE WHEN orders.note IS NULL THEN 1 ELSE 0 END, orders.note DESC, orders.customer, orders.id\nLIMIT 10"
        );
        // After a row without one: nothing is after a NULL, only its ties.
        let after = [Value::Null, Value::Text("ada".into()), Value::Int(7)];
        assert_eq!(
            k.page_sql(&pg, 10, Some(&after)).unwrap(),
            "SELECT * FROM orders WHERE (orders.note IS NULL AND orders.customer > 'ada') OR (orders.note IS NULL AND orders.customer = 'ada' AND orders.id > 7) ORDER BY CASE WHEN orders.note IS NULL THEN 1 ELSE 0 END, orders.note DESC, orders.customer, orders.id\nLIMIT 10"
        );
        // A NULL where the column cannot hold one is not a row of this table.
        let after = [Value::Text("m".into()), Value::Null, Value::Int(7)];
        assert!(k.page_sql(&pg, 10, Some(&after)).is_none());
        // The positions are the result's: with named columns, 1 is `customer` as `who`.
        let k = sorted_plan(
            &pg,
            "select o.id, o.customer as who, o.note from orders o where o.id > 5",
            &table,
            &[(1, true)],
            &[],
        )
        .unwrap();
        assert_eq!(
            k.page_sql(&pg, 10, Some(&[Value::Text("x".into()), Value::Int(9)]))
                .unwrap(),
            "SELECT o.id, o.customer AS who, o.note FROM orders o WHERE (o.id > 5) AND ((o.customer < 'x') OR (o.customer = 'x' AND o.id > 9)) ORDER BY o.customer DESC, o.id\nLIMIT 10"
        );
        assert_eq!(k.order[0].result_name, "who");
        // The key itself, clicked descending, is the whole order.
        let k = sorted_plan(&pg, "select * from orders", &table, &[(0, true)], &[]).unwrap();
        assert_eq!(
            k.page_sql(&pg, 10, None).unwrap(),
            "SELECT * FROM orders ORDER BY orders.id DESC\nLIMIT 10"
        );
    }

    #[test]
    fn a_sort_the_pages_cannot_follow_exactly_is_not_paged() {
        let pg = spelaion_dialect::PostgresDialect;
        let table = orders();
        let sql = "select * from orders";
        // A type whose literals might not compare exactly, a column that is not there, the
        // same column twice, too many keys, and a masked column.
        assert!(sorted_plan(&pg, sql, &table, &[(2, false)], &[]).is_none());
        assert!(sorted_plan(&pg, sql, &table, &[(4, false)], &[]).is_none());
        assert!(sorted_plan(&pg, sql, &table, &[(1, false), (1, true)], &[]).is_none());
        let many: Vec<(usize, bool)> = (0..=MAX_SORT_KEYS).map(|_| (1, false)).collect();
        assert!(sorted_plan(&pg, sql, &table, &many, &[]).is_none());
        assert!(sorted_plan(&pg, sql, &table, &[(3, false)], &["note"]).is_none());
        assert!(sorted_plan(&pg, sql, &table, &[(3, false)], &["customer"]).is_some());
        // The query's own ORDER BY still may not name a nullable column.
        assert!(
            sorted_plan(
                &pg,
                "select * from orders order by note",
                &table,
                &[(1, false)],
                &[]
            )
            .is_none()
        );
        // A query that is not paged at all is not paged with a sort either.
        assert!(
            sorted_plan(
                &pg,
                "select customer from orders",
                &table,
                &[(0, false)],
                &[]
            )
            .is_none()
        );
    }

    #[test]
    fn keyset_pages_follow_the_order_then_the_key() {
        let pg = spelaion_dialect::PostgresDialect;
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
        let ms = spelaion_dialect::SqlServerDialect;
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
        let pg = spelaion_dialect::PostgresDialect;
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
        let d = spelaion_dialect::SqliteDialect;
        assert_eq!(
            query_count_sql(&d, "select id from orders order by id;"),
            Some("SELECT COUNT(*) FROM (SELECT 1 FROM orders) AS spelaion_counted".into())
        );
    }

    #[test]
    fn only_keys_the_server_enforces_unique_page_a_query() {
        use crate::ServerFamily;
        // Engines that enforce their primary keys.
        for engine in ["sqlite", "sqlserver", "duckdb", "oracle", "spanner"] {
            assert!(unique_keys(engine, None), "{engine}");
        }
        // PostgreSQL and MySQL only for the servers known to enforce them; an unchecked one
        // never.
        assert!(unique_keys("postgres", Some(&ServerFamily::PostgreSql)));
        assert!(unique_keys("postgres", Some(&ServerFamily::CockroachDb)));
        assert!(unique_keys("mysql", Some(&ServerFamily::MariaDb)));
        assert!(unique_keys("mysql", Some(&ServerFamily::TiDb)));
        assert!(unique_keys("postgres", None));
        assert!(unique_keys("mysql", None));
        assert!(!unique_keys(
            "postgres",
            Some(&ServerFamily::Unchecked(String::new()))
        ));
        for family in [
            ServerFamily::Materialize,
            ServerFamily::Unchecked("Redshift".into()),
        ] {
            assert!(!unique_keys("postgres", Some(&family)), "{family:?}");
        }
        for family in [
            ServerFamily::Doris,
            ServerFamily::GreptimeDb,
            ServerFamily::Databend,
            ServerFamily::SingleStore,
        ] {
            assert!(!unique_keys("mysql", Some(&family)), "{family:?}");
        }
        // Engines whose keys are sorting keys, a connector's, or not enforced.
        for engine in [
            "clickhouse",
            "trino",
            "databend",
            "tdengine",
            "flightsql",
            "dremio",
            "gizmosql",
            "hive",
            "spark",
            "kyuubi",
            "impala",
        ] {
            assert!(!unique_keys(engine, None), "{engine}");
        }
    }
}
