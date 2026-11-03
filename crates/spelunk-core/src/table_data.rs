//! A table's rows a page at a time, as SQLyog's "Table Data" tab: the user types a `WHERE`
//! and an `ORDER BY`, the core writes `SELECT * FROM t WHERE (…) ORDER BY … LIMIT n OFFSET m`
//! (in SQL Server, `… ORDER BY … OFFSET m ROWS FETCH NEXT n ROWS ONLY`) with the engine's
//! quoting, and checks that what it wrote is still a single plain SELECT of that one table
//! with that page. So a `;`, a comment or a `UNION` typed in the filter
//! cannot run something else, read another table or drop the limit and load everything.

use spelunk_driver_api::Dialect;
use sqlparser::ast::{Fetch, GroupByExpr, LimitClause, SetExpr, Statement, TableFactor};
use sqlparser::parser::Parser;

use crate::Session;
use crate::generate::{qualified, scalar_count};
use crate::guard::sql_dialect;

/// Most rows a page may ask for.
pub const MAX_PAGE_ROWS: u64 = 100_000;

/// One page of a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePage {
    pub schema: String,
    pub name: String,
    /// What goes after `WHERE`; empty for every row.
    pub filter: String,
    /// What goes after `ORDER BY`; empty for the server's order.
    pub order_by: String,
    pub limit: u64,
    pub offset: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PageError {
    #[error("a page holds from 1 to {MAX_PAGE_ROWS} rows")]
    BadLimit,
    #[error("the filter or the order could not be read: {0}")]
    Unreadable(String),
    #[error(
        "the filter or the order must not change the query: only a condition after WHERE and expressions after ORDER BY fit"
    )]
    NotAPlainSelect,
}

/// The page's SELECT, checked.
pub fn table_page_sql(dialect: &dyn Dialect, page: &TablePage) -> Result<String, PageError> {
    if page.limit == 0 || page.limit > MAX_PAGE_ROWS {
        return Err(PageError::BadLimit);
    }
    let mut sql = format!(
        "SELECT *\nFROM {}",
        qualified(dialect, &page.schema, &page.name)
    );
    if dialect.name() == "sqlserver" {
        // T-SQL pages need an ORDER BY; without one, the server's order.
        let order_by = if page.order_by.trim().is_empty() {
            "(SELECT NULL)"
        } else {
            &page.order_by
        };
        push_clauses(&mut sql, &page.filter, order_by);
        sql.push_str(&format!(
            "\nOFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
            page.offset, page.limit
        ));
    } else {
        push_clauses(&mut sql, &page.filter, &page.order_by);
        sql.push_str(&format!("\nLIMIT {} OFFSET {}", page.limit, page.offset));
    }
    check(
        dialect,
        &sql,
        Some((page.limit, page.offset)),
        !page.filter.trim().is_empty(),
    )?;
    Ok(sql)
}

/// `SELECT COUNT(*)` of the rows the filter keeps, checked the same way.
pub fn table_count_sql(
    dialect: &dyn Dialect,
    schema: &str,
    name: &str,
    filter: &str,
) -> Result<String, PageError> {
    let mut sql = format!("SELECT COUNT(*)\nFROM {}", qualified(dialect, schema, name));
    push_clauses(&mut sql, filter, "");
    check(dialect, &sql, None, !filter.trim().is_empty())?;
    Ok(sql)
}

/// The rows the filter keeps, counted on `session` (the "Count" button).
pub async fn count_table_rows(
    session: &Session,
    schema: &str,
    name: &str,
    filter: &str,
) -> Result<i64, String> {
    let sql = table_count_sql(session.dialect().as_ref(), schema, name, filter)
        .map_err(|e| e.to_string())?;
    scalar_count(session, &sql).await
}

/// Each clause on its own lines, the filter in parentheses: a `--` comment typed in it ends
/// at its line and `a OR b` keeps its meaning.
fn push_clauses(sql: &mut String, filter: &str, order_by: &str) {
    let filter = filter.trim();
    if !filter.is_empty() {
        sql.push_str(&format!("\nWHERE (\n{filter}\n)"));
    }
    let order_by = order_by.trim();
    if !order_by.is_empty() {
        sql.push_str(&format!("\nORDER BY\n{order_by}\n"));
    }
}

fn check(
    dialect: &dyn Dialect,
    sql: &str,
    page: Option<(u64, u64)>,
    filtered: bool,
) -> Result<(), PageError> {
    // The splitter that runs scripts must see one statement too.
    if dialect.split_statements(sql).len() != 1 {
        return Err(PageError::NotAPlainSelect);
    }
    let parser = sql_dialect(dialect.name());
    let statements = Parser::parse_sql(parser.as_ref(), sql)
        .map_err(|e| PageError::Unreadable(e.to_string()))?;
    let [Statement::Query(query)] = statements.as_slice() else {
        return Err(PageError::NotAPlainSelect);
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err(PageError::NotAPlainSelect);
    };
    let one_table = matches!(
        select.from.as_slice(),
        [from] if from.joins.is_empty() && matches!(from.relation, TableFactor::Table { .. })
    );
    let plain = query.with.is_none()
        && query.locks.is_empty()
        && select.top.is_none()
        && select.into.is_none()
        && select.distinct.is_none()
        && select.having.is_none()
        && select.projection.len() == 1
        && select.selection.is_some() == filtered
        && matches!(&select.group_by, GroupByExpr::Expressions(e, _) if e.is_empty());
    let limit_kept = match (page, &query.limit_clause, &query.fetch) {
        (None, None, None) => true,
        (
            Some((limit, offset)),
            Some(LimitClause::LimitOffset {
                limit: Some(l),
                offset: Some(o),
                limit_by,
            }),
            None,
        ) => {
            l.to_string() == limit.to_string()
                && o.value.to_string() == offset.to_string()
                && limit_by.is_empty()
        }
        // SQL Server: `OFFSET m ROWS FETCH NEXT n ROWS ONLY`, nothing else.
        (
            Some((limit, offset)),
            Some(LimitClause::LimitOffset {
                limit: None,
                offset: Some(o),
                limit_by,
            }),
            Some(Fetch {
                with_ties: false,
                percent: false,
                quantity: Some(q),
            }),
        ) if dialect.name() == "sqlserver" => {
            q.to_string() == limit.to_string()
                && o.value.to_string() == offset.to_string()
                && limit_by.is_empty()
        }
        _ => false,
    };
    if one_table && plain && limit_kept {
        Ok(())
    } else {
        Err(PageError::NotAPlainSelect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_driver_sqlite::SqliteDialect;

    fn page(filter: &str, order_by: &str) -> TablePage {
        TablePage {
            schema: "main".into(),
            name: "orders".into(),
            filter: filter.into(),
            order_by: order_by.into(),
            limit: 500,
            offset: 1000,
        }
    }

    #[test]
    fn pages_are_written_with_the_clauses_on_their_own_lines() {
        assert_eq!(
            table_page_sql(&SqliteDialect, &page("", "")).unwrap(),
            "SELECT *\nFROM orders\nLIMIT 500 OFFSET 1000"
        );
        assert_eq!(
            table_page_sql(
                &SqliteDialect,
                &page(" total > 10 or id = 1 -- big ", "total desc")
            )
            .unwrap(),
            "SELECT *\nFROM orders\nWHERE (\ntotal > 10 or id = 1 -- big\n)\nORDER BY\ntotal desc\n\nLIMIT 500 OFFSET 1000"
        );
        assert_eq!(
            table_count_sql(&SqliteDialect, "main", "order lines", "x in (select 1)").unwrap(),
            "SELECT COUNT(*)\nFROM \"order lines\"\nWHERE (\nx in (select 1)\n)"
        );
    }

    #[test]
    fn filters_that_change_the_query_are_refused() {
        for (filter, order_by) in [
            ("1=1); drop table orders; select (1", ""),
            ("1=1) union select * from secrets where (1", ""),
            ("1=1 /*", ""),
            ("", "id limit 1000000 --"),
            ("", "id; delete from orders"),
            ("1=1) group by (id", ""),
            ("", "1 for update"),
        ] {
            assert!(
                table_page_sql(&SqliteDialect, &page(filter, order_by)).is_err(),
                "{filter} / {order_by}"
            );
        }
        assert!(matches!(
            table_page_sql(&SqliteDialect, &page("total >", "")),
            Err(PageError::Unreadable(_))
        ));
        // In T-SQL too: no second page, no TOP, nothing after the order.
        use spelunk_driver_sqlserver::SqlServerDialect;
        assert_eq!(
            table_page_sql(&SqlServerDialect, &page("", "")).unwrap(),
            "SELECT *\nFROM main.orders\nORDER BY\n(SELECT NULL)\n\nOFFSET 1000 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        assert!(table_page_sql(&SqlServerDialect, &page("total > 1", "[total] desc")).is_ok());
        for (filter, order_by) in [
            ("1=1); drop table orders; select (1", ""),
            ("1=1) union select * from secrets where (1", ""),
            ("", "id offset 0 rows fetch next 1000000 rows only --"),
            ("", "id\ngo\ndelete from orders"),
            ("1=1) or (1=1", "id; delete from orders"),
            ("", "id offset 0 rows --"),
            ("1=1 /*", ""),
        ] {
            assert!(
                table_page_sql(&SqlServerDialect, &page(filter, order_by)).is_err(),
                "T-SQL: {filter} / {order_by}"
            );
        }
        let mut huge = page("", "");
        huge.limit = MAX_PAGE_ROWS + 1;
        assert_eq!(
            table_page_sql(&SqliteDialect, &huge),
            Err(PageError::BadLimit)
        );
    }
}
