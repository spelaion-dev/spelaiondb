//! A table's rows a page at a time, as SQLyog's "Table Data" tab: the user types a `WHERE`
//! and an `ORDER BY`, the core writes `SELECT * FROM t WHERE (…) ORDER BY … LIMIT n OFFSET m`
//! (in SQL Server, `… ORDER BY … OFFSET m ROWS FETCH NEXT n ROWS ONLY`) with the engine's
//! quoting, and checks that what it wrote is still a single plain SELECT of that one table
//! with that page. So a `;`, a comment or a `UNION` typed in the filter
//! cannot run something else, read another table or drop the limit and load everything.

use spelaion_driver_api::{Dialect, LogicalType, Value};
use sqlparser::ast::{
    BinaryOperator, Expr, Fetch, GroupByExpr, LimitClause, SetExpr, Statement, TableFactor,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

use crate::Session;
use crate::export::sql_literal;
use crate::filter::FilterOp;
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

/// Every row the filter keeps, in the tab's order, for a streaming export (ADR-034): the
/// page's SELECT without its paging, checked the same way, so the filter and the order cannot
/// add a limit, read another table or run anything else.
pub fn table_export_sql(
    dialect: &dyn Dialect,
    schema: &str,
    name: &str,
    filter: &str,
    order_by: &str,
) -> Result<String, PageError> {
    let mut sql = format!("SELECT *\nFROM {}", qualified(dialect, schema, name));
    push_clauses(&mut sql, filter, order_by);
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

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConditionError {
    #[error("NULL is not a value to compare with: filter by IS NULL or IS NOT NULL")]
    NullValue,
    #[error("the filter has a -- comment: remove it to add a condition after it")]
    LineComment,
    #[error("a key needs at least one column")]
    EmptyKey,
}

/// The typed value of a cell of a page, for "filter by this value". Never a masked cell's
/// (ADR-051): the page holds the mask, and the condition would show the value it stands for.
pub fn cell_filter_value(
    stored: &crate::StoredResult,
    row: usize,
    column: usize,
) -> Result<Value, String> {
    let meta = stored
        .schema()
        .columns
        .get(column)
        .ok_or("no such column")?;
    if stored.is_masked(column) {
        return Err(format!(
            "{} is masked: its values cannot be filtered by",
            meta.name
        ));
    }
    stored
        .value_at(row, column)
        .ok_or_else(|| "no such row".to_string())
}

/// "Filter by this value" in a table data tab: the condition added to its `WHERE`, written by
/// the core from the typed value (a cell of the page, or a value typed for the column), so
/// the UI never writes SQL. The column is quoted for the engine and the value is the engine's
/// literal for its type; "contains" matches the text ignoring case where the engine allows
/// (`ILIKE` in PostgreSQL and DuckDB) with its `%`, `_` and `!` taken literally. The page's
/// SELECT is still checked whole by [`table_page_sql`].
pub fn value_condition(
    dialect: &dyn Dialect,
    column: &str,
    logical: LogicalType,
    op: FilterOp,
    value: &Value,
) -> Result<String, ConditionError> {
    let name = dialect.quote_identifier(column);
    let engine = dialect.name();
    // Types without equality in some engines (PostgreSQL's json) compare as text.
    let as_text = || match logical {
        LogicalType::Text => name.clone(),
        LogicalType::Bool
        | LogicalType::Int
        | LogicalType::Float
        | LogicalType::Decimal
        | LogicalType::Bytes
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => text_cast(engine, &name),
    };
    let compared = match logical {
        LogicalType::Json | LogicalType::Array | LogicalType::Native => as_text(),
        LogicalType::Text
        | LogicalType::Bool
        | LogicalType::Int
        | LogicalType::Float
        | LogicalType::Decimal
        | LogicalType::Bytes
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Interval => name.clone(),
    };
    let literal = || -> Result<String, ConditionError> {
        match value {
            Value::Null => Err(ConditionError::NullValue),
            _ if compared != name => Ok(sql_literal(&Value::Text(value.to_string()), engine)),
            other => Ok(sql_literal(other, engine)),
        }
    };
    let compare = |operator: &str| -> Result<String, ConditionError> {
        Ok(format!("{compared} {operator} {}", literal()?))
    };
    let like = |negated: bool, pattern: String, escaped: bool| {
        let operator = match (engine, negated) {
            ("postgres" | "duckdb", false) => "ILIKE",
            ("postgres" | "duckdb", true) => "NOT ILIKE",
            (_, false) => "LIKE",
            (_, true) => "NOT LIKE",
        };
        let escape = if escaped { " ESCAPE '!'" } else { "" };
        format!(
            "{} {operator} {}{escape}",
            as_text(),
            sql_literal(&Value::Text(pattern), engine)
        )
    };
    let needle = || -> Result<String, ConditionError> {
        match value {
            Value::Null => Err(ConditionError::NullValue),
            other => Ok(other.to_string()),
        }
    };
    Ok(match op {
        FilterOp::IsNull => format!("{name} IS NULL"),
        FilterOp::IsNotNull => format!("{name} IS NOT NULL"),
        FilterOp::Eq => compare("=")?,
        FilterOp::NotEq => compare("<>")?,
        FilterOp::Gt => compare(">")?,
        FilterOp::Lt => compare("<")?,
        FilterOp::Ge => compare(">=")?,
        FilterOp::Le => compare("<=")?,
        FilterOp::Like => like(false, needle()?, false),
        FilterOp::NotLike => like(true, needle()?, false),
        FilterOp::Contains => like(false, containing(&needle()?), true),
        FilterOp::NotContains => like(true, containing(&needle()?), true),
    })
}

/// The filter of a followed foreign key (SDB-11): each column of the table it leads to equal
/// to the key's value in the row it starts from, joined with AND in the key's order. Each
/// condition is written as "filter by this value" writes one (the column quoted for the engine,
/// the engine's literal for the value's type), so the UI never writes it. A NULL in any column
/// means the row references nothing through that key (MATCH SIMPLE): refused.
pub fn key_filter(
    dialect: &dyn Dialect,
    columns: &[(String, LogicalType, Value)],
) -> Result<String, ConditionError> {
    if columns.is_empty() {
        return Err(ConditionError::EmptyKey);
    }
    columns
        .iter()
        .map(|(column, logical, value)| {
            value_condition(dialect, column, *logical, FilterOp::Eq, value)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|conditions| conditions.join(" AND "))
}

/// `%…%` around a text, its own `%`, `_`, `!` and `[` escaped with `!` (MySQL reads `\` in
/// strings, so `\` is not the escape anywhere).
fn containing(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('%');
    for c in text.chars() {
        // SQL Server also reads `[` as the start of a character class; escaping it is
        // harmless elsewhere.
        if matches!(c, '%' | '_' | '!' | '[') {
            out.push('!');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The column as text, for LIKE on a column of another type.
fn text_cast(engine: &str, name: &str) -> String {
    match engine {
        "postgres" => format!("CAST({name} AS text)"),
        "mysql" | "mariadb" => format!("CAST({name} AS CHAR)"),
        "sqlserver" => format!("CAST({name} AS nvarchar(max))"),
        "sqlite" => format!("CAST({name} AS TEXT)"),
        _ => format!("CAST({name} AS VARCHAR)"),
    }
}

/// A value typed for a column (the custom filter of a table data tab) as the value it is
/// compared as: a number for numeric columns when it reads as one, text otherwise.
pub fn typed_value(text: &str, logical: LogicalType) -> Value {
    let trimmed = text.trim();
    match logical {
        LogicalType::Int => trimmed
            .parse::<i64>()
            .map(Value::Int)
            .unwrap_or_else(|_| Value::Text(text.to_string())),
        LogicalType::Float | LogicalType::Decimal
            if !trimmed.is_empty()
                && trimmed.parse::<f64>().is_ok_and(f64::is_finite)
                && trimmed
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')) =>
        {
            Value::Decimal(trimmed.to_string())
        }
        LogicalType::Bool
        | LogicalType::Text
        | LogicalType::Float
        | LogicalType::Decimal
        | LogicalType::Bytes
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => Value::Text(text.to_string()),
    }
}

/// The table data tab's filter with one more condition (AND), on one line like the filter
/// box: the filter in parentheses when it is an `OR` (or cannot be read) so the condition
/// applies to all of it. A filter with a `--` comment is refused: the comment would swallow
/// what comes after it.
pub fn add_condition(
    dialect: &dyn Dialect,
    filter: &str,
    condition: &str,
) -> Result<String, ConditionError> {
    let filter = filter.trim();
    if filter.is_empty() {
        return Ok(condition.to_string());
    }
    let parser = sql_dialect(dialect.name());
    let tokens = Tokenizer::new(parser.as_ref(), filter).tokenize();
    if tokens.as_ref().is_ok_and(|tokens| {
        tokens
            .iter()
            .any(|t| matches!(t, Token::Whitespace(Whitespace::SingleLineComment { .. })))
    }) {
        return Err(ConditionError::LineComment);
    }
    let plain = Parser::new(parser.as_ref())
        .try_with_sql(filter)
        .and_then(|mut p| {
            let expr = p.parse_expr()?;
            Ok(p.peek_token().token == Token::EOF && !is_or(&expr))
        })
        .unwrap_or(false);
    Ok(if plain {
        format!("{filter} AND {condition}")
    } else {
        format!("({filter}) AND {condition}")
    })
}

fn is_or(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::BinaryOp {
            op: BinaryOperator::Or | BinaryOperator::Xor,
            ..
        }
    )
}

/// Each clause on its own lines, the filter in parentheses: a `--` comment typed in it ends
/// at its line and `a OR b` keeps its meaning.
pub(crate) fn push_clauses(sql: &mut String, filter: &str, order_by: &str) {
    let filter = filter.trim();
    if !filter.is_empty() {
        sql.push_str(&format!("\nWHERE (\n{filter}\n)"));
    }
    let order_by = order_by.trim();
    if !order_by.is_empty() {
        sql.push_str(&format!("\nORDER BY\n{order_by}\n"));
    }
}

pub(crate) fn check(
    dialect: &dyn Dialect,
    sql: &str,
    page: Option<(u64, u64)>,
    filtered: bool,
) -> Result<(), PageError> {
    check_projection(dialect, sql, page, filtered, 1)
}

/// [`check`] for a SELECT of `columns` expressions instead of `*` (the foreign key picker's
/// page names the columns it shows).
pub(crate) fn check_projection(
    dialect: &dyn Dialect,
    sql: &str,
    page: Option<(u64, u64)>,
    filtered: bool,
    columns: usize,
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
        && select.projection.len() == columns
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
    use spelaion_driver_sqlite::SqliteDialect;

    struct Named(&'static str);

    impl Dialect for Named {
        fn name(&self) -> &'static str {
            self.0
        }
        fn split_statements(&self, _script: &str) -> Vec<spelaion_driver_api::StatementSpan> {
            Vec::new()
        }
        fn quote_identifier(&self, ident: &str) -> String {
            match self.0 {
                "mysql" => format!("`{}`", ident.replace('`', "``")),
                "sqlserver" => format!("[{}]", ident.replace(']', "]]")),
                _ => format!("\"{}\"", ident.replace('"', "\"\"")),
            }
        }
    }

    fn condition(engine: &'static str, logical: LogicalType, op: FilterOp, value: Value) -> String {
        value_condition(&Named(engine), "my col", logical, op, &value).unwrap()
    }

    #[test]
    fn conditions_quote_the_column_and_write_the_engines_literal() {
        let text = |s: &str| Value::Text(s.into());
        assert_eq!(
            condition("postgres", LogicalType::Text, FilterOp::Eq, text("O'Brien")),
            "\"my col\" = 'O''Brien'"
        );
        assert_eq!(
            condition("mysql", LogicalType::Text, FilterOp::NotEq, text("a\\b")),
            "`my col` <> 'a\\\\b'"
        );
        assert_eq!(
            condition("sqlserver", LogicalType::Text, FilterOp::Gt, text("ñ")),
            "[my col] > N'ñ'"
        );
        assert_eq!(
            condition("sqlite", LogicalType::Int, FilterOp::Lt, Value::Int(10)),
            "\"my col\" < 10"
        );
        assert_eq!(
            condition(
                "sqlserver",
                LogicalType::Bool,
                FilterOp::Eq,
                Value::Bool(true)
            ),
            "[my col] = 1"
        );
        assert_eq!(
            condition("postgres", LogicalType::Int, FilterOp::IsNull, Value::Null),
            "\"my col\" IS NULL"
        );
        assert_eq!(
            condition(
                "duckdb",
                LogicalType::Int,
                FilterOp::IsNotNull,
                Value::Int(1)
            ),
            "\"my col\" IS NOT NULL"
        );
        // json has no equality in PostgreSQL: compared as text.
        assert_eq!(
            condition(
                "postgres",
                LogicalType::Json,
                FilterOp::Eq,
                Value::Json("{\"a\": 1}".into())
            ),
            "CAST(\"my col\" AS text) = '{\"a\": 1}'"
        );
        assert_eq!(
            value_condition(
                &Named("postgres"),
                "c",
                LogicalType::Int,
                FilterOp::Eq,
                &Value::Null
            ),
            Err(ConditionError::NullValue)
        );
    }

    #[test]
    fn contains_escapes_wildcards_and_ignores_case_where_it_can() {
        let text = |s: &str| Value::Text(s.into());
        assert_eq!(
            condition(
                "postgres",
                LogicalType::Text,
                FilterOp::Contains,
                text("50%_a!")
            ),
            "\"my col\" ILIKE '%50!%!_a!!%' ESCAPE '!'"
        );
        assert_eq!(
            condition(
                "sqlserver",
                LogicalType::Text,
                FilterOp::NotContains,
                text("[x]")
            ),
            "[my col] NOT LIKE N'%![x]%' ESCAPE '!'"
        );
        // Another type is read as text first.
        assert_eq!(
            condition(
                "mysql",
                LogicalType::Int,
                FilterOp::Contains,
                Value::Int(42)
            ),
            "CAST(`my col` AS CHAR) LIKE '%42%' ESCAPE '!'"
        );
        assert_eq!(
            condition("duckdb", LogicalType::Text, FilterOp::Like, text("a%")),
            "\"my col\" ILIKE 'a%'"
        );
    }

    #[test]
    fn key_filters_join_one_quoted_condition_per_column() {
        let columns = |values: Vec<Value>| -> Vec<(String, LogicalType, Value)> {
            ["order region", "number"]
                .into_iter()
                .zip(values)
                .map(|(name, value)| {
                    let logical = match value {
                        Value::Int(_) => LogicalType::Int,
                        _ => LogicalType::Text,
                    };
                    (name.to_string(), logical, value)
                })
                .collect()
        };
        assert_eq!(
            key_filter(
                &Named("postgres"),
                &columns(vec![Value::Text("E'U".into()), Value::Int(7)])
            ),
            Ok("\"order region\" = 'E''U' AND \"number\" = 7".into())
        );
        assert_eq!(
            key_filter(
                &Named("sqlserver"),
                &columns(vec![Value::Text("ñ".into()), Value::Int(7)])
            ),
            Ok("[order region] = N'ñ' AND [number] = 7".into())
        );
        // A NULL part references nothing; an empty key is not a key.
        assert_eq!(
            key_filter(
                &Named("mysql"),
                &columns(vec![Value::Text("EU".into()), Value::Null])
            ),
            Err(ConditionError::NullValue)
        );
        assert_eq!(
            key_filter(&Named("mysql"), &[]),
            Err(ConditionError::EmptyKey)
        );
        // A value that tries to close the condition stays a literal, and the page is checked.
        let filter = key_filter(
            &SqliteDialect,
            &columns(vec![Value::Text("x') OR 1=1 --".into()), Value::Int(1)]),
        )
        .unwrap();
        assert_eq!(filter, "\"order region\" = 'x'') OR 1=1 --' AND number = 1");
        assert!(table_page_sql(&SqliteDialect, &page(&filter, "")).is_ok());
    }

    #[test]
    fn typed_values_are_numbers_only_for_numeric_columns() {
        assert_eq!(typed_value(" 42 ", LogicalType::Int), Value::Int(42));
        assert_eq!(
            typed_value("4x", LogicalType::Int),
            Value::Text("4x".into())
        );
        assert_eq!(
            typed_value("1.5", LogicalType::Decimal),
            Value::Decimal("1.5".into())
        );
        assert_eq!(
            typed_value("1; DROP", LogicalType::Float),
            Value::Text("1; DROP".into())
        );
        assert_eq!(
            typed_value("inf", LogicalType::Float),
            Value::Text("inf".into())
        );
        assert_eq!(
            typed_value("42", LogicalType::Text),
            Value::Text("42".into())
        );
    }

    #[test]
    fn conditions_are_added_with_and_and_or_is_kept_whole() {
        let d = SqliteDialect;
        let add = |filter: &str| add_condition(&d, filter, "b = 2");
        assert_eq!(add("  "), Ok("b = 2".into()));
        assert_eq!(add("a = 1"), Ok("a = 1 AND b = 2".into()));
        assert_eq!(
            add("a = 1 AND c > 3"),
            Ok("a = 1 AND c > 3 AND b = 2".into())
        );
        assert_eq!(
            add("a = 1 OR c > 3"),
            Ok("(a = 1 OR c > 3) AND b = 2".into())
        );
        assert_eq!(add("NOT a = 1"), Ok("NOT a = 1 AND b = 2".into()));
        // A comment would swallow the condition: refused. Block comments are fine.
        assert_eq!(add("a = 1 -- note"), Err(ConditionError::LineComment));
        assert_eq!(
            add("a = 1 /* note */"),
            Ok("a = 1 /* note */ AND b = 2".into())
        );
        // `--` inside a string is not a comment.
        assert_eq!(add("a = '--'"), Ok("a = '--' AND b = 2".into()));
        // What cannot be read is kept whole in parentheses; the page's check judges it.
        assert_eq!(add("a = ("), Ok("(a = () AND b = 2".into()));
        // The result still makes a page that passes the check.
        let filter = add_condition(&d, "a = 1 OR c > 3", "\"total\" > 10").unwrap();
        assert!(table_page_sql(&d, &page(&filter, "")).is_ok());
        // An injection typed in the filter is still refused once a condition is added.
        let filter = add("1=1) UNION SELECT * FROM t WHERE (1=1").unwrap();
        assert!(table_page_sql(&d, &page(&filter, "")).is_err());
    }

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
        use spelaion_driver_sqlserver::SqlServerDialect;
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
        // The export's SELECT: every row, the same checks, and no limit may be slipped in.
        assert_eq!(
            table_export_sql(&SqliteDialect, "main", "orders", "total > 1", "id desc").unwrap(),
            "SELECT *\nFROM orders\nWHERE (\ntotal > 1\n)\nORDER BY\nid desc\n"
        );
        assert_eq!(
            table_export_sql(&SqlServerDialect, "dbo", "orders", "", "").unwrap(),
            "SELECT *\nFROM dbo.orders"
        );
        for (filter, order_by) in [
            ("1=1); drop table orders; select (1", ""),
            ("1=1) union select * from secrets where (1", ""),
            ("", "id limit 10"),
            ("", "id limit 10 offset 5"),
            ("1=1 /*", ""),
            ("", "1 for update"),
            ("1=1) group by (id", ""),
            ("", "id; delete from orders"),
        ] {
            assert!(
                table_export_sql(&SqliteDialect, "main", "orders", filter, order_by).is_err(),
                "export: {filter} / {order_by}"
            );
        }
        for (filter, order_by) in [
            ("", "id offset 0 rows fetch next 10 rows only"),
            ("", "id offset 5 rows"),
            ("", "id\ngo\ndelete from orders"),
            ("1=1) union select * from secrets where (1", ""),
        ] {
            assert!(
                table_export_sql(&SqlServerDialect, "dbo", "orders", filter, order_by).is_err(),
                "T-SQL export: {filter} / {order_by}"
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
