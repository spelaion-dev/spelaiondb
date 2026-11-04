//! Pages of a single, conservatively checked SELECT. Statements with their own limit,
//! scripts, expressions or unsupported syntax continue to run unchanged.

use std::ops::ControlFlow;

use futures_util::StreamExt;
use spelunk_driver_api::Dialect;
use sqlparser::ast::{
    Expr, GroupByExpr, Query, SelectItem, SetExpr, Statement, TableFactor, visit_expressions,
};
use sqlparser::parser::Parser;

use crate::classify::{ReadKind, classify_read_only};
use crate::generate::scalar_count;
use crate::guard::sql_dialect;
use crate::{CoreEvent, MAX_PAGE_ROWS, ResultId, Session};

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

/// Returns a paged SQL statement, or `None` when the original must run unchanged.
pub fn query_page_sql(dialect: &dyn Dialect, sql: &str, limit: u64, offset: u64) -> Option<String> {
    if limit == 0 || limit > MAX_PAGE_ROWS {
        return None;
    }
    let query = pageable(dialect, sql)?;
    let spans = dialect.split_statements(sql);
    let original = spans.first()?.text(sql);
    if dialect.name() == "sqlserver" {
        let order = if query.order_by.is_none() {
            "\nORDER BY (SELECT NULL)"
        } else {
            ""
        };
        Some(format!(
            "{original}{order}\nOFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
        ))
    } else {
        Some(format!("{original}\nLIMIT {limit} OFFSET {offset}"))
    }
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
    Some(format!("SELECT COUNT(*) FROM ({query}) AS spelunk_counted"))
}

pub async fn count_query_rows(session: &Session, sql: &str) -> Result<i64, String> {
    let sql =
        query_count_sql(session.dialect().as_ref(), sql).ok_or("this query cannot be counted")?;
    scalar_count(session, &sql).await
}

/// Fetch the next page into a temporary result, then append it atomically. Failed pages never
/// alter the rows already displayed; the offset also prevents duplicate concurrent fetches.
pub async fn append_query_page(
    session: &Session,
    target: ResultId,
    sql: &str,
    limit: u64,
    offset: u64,
) -> Result<usize, String> {
    let paged = query_page_sql(session.dialect().as_ref(), sql, limit, offset)
        .ok_or("this query cannot be paged")?;
    let mut events = session.run_script(&paged);
    let mut results = Vec::new();
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => results.push(result),
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            _ => {}
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
    use spelunk_driver_sqlite::SqliteDialect;

    #[test]
    fn pages_only_safe_unlimited_queries() {
        let d = SqliteDialect;
        assert_eq!(
            query_page_sql(&d, "select * from orders order by id;", 100, 200),
            Some("select * from orders order by id\nLIMIT 100 OFFSET 200".into())
        );
        assert_eq!(
            query_count_sql(&d, "select id from orders order by id;"),
            Some("SELECT COUNT(*) FROM (SELECT 1 FROM orders) AS spelunk_counted".into())
        );
        for sql in [
            "select * from orders limit 5",
            "select random() from orders",
            "select * from orders; delete from orders",
            "select * from orders where id in (select id from other)",
            "delete from orders",
        ] {
            assert!(query_page_sql(&d, sql, 100, 0).is_none(), "{sql}");
        }
        assert!(query_page_sql(&d, "select * from orders", 0, 0).is_none());

        let mysql = spelunk_driver_mysql::MysqlDialect;
        assert_eq!(
            query_page_sql(
                &mysql,
                "select * from `shop`.`orders` order by `id`;",
                100,
                200
            ),
            Some("select * from `shop`.`orders` order by `id`\nLIMIT 100 OFFSET 200".into())
        );
        let sqlserver = spelunk_driver_sqlserver::SqlServerDialect;
        assert_eq!(
            query_page_sql(&sqlserver, "select * from dbo.orders;", 100, 200),
            Some("select * from dbo.orders\nORDER BY (SELECT NULL)\nOFFSET 200 ROWS FETCH NEXT 100 ROWS ONLY".into())
        );
    }
}
