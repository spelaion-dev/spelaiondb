//! References between cells (ADR-087, points 7 to 10): `{{name}}` in a cell's code names
//! another SQL cell, and the core expands the cell into one statement that brings each
//! referenced cell's SQL along as a CTE. The database runs the referenced SQL again: what is
//! referenced is a query, never the rows a cell showed.
//!
//! Only a read may be referenced, so a reference can never repeat a write: each referenced
//! cell must be exactly one statement that [`classify_read_only`] reads as a query. The
//! expanded statement is then checked by the guard and the read-only barrier like any editor
//! run; this module only writes it.

use std::collections::HashMap;
use std::ops::Range;

use spelaion_driver_api::Dialect;

use super::{Cell, Notebook, is_cell_name};
use crate::classify::{ReadKind, classify_read_only};

/// Most referenced cells one expansion brings along.
pub const MAX_REFERENCED_CELLS: usize = 50;
/// The longest expanded statement, in bytes.
pub const MAX_EXPANDED_BYTES: usize = 16 * 1024 * 1024;

/// A cell ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedCell {
    /// What runs: the cell's own SQL when it references nothing, else one statement with a
    /// CTE per referenced cell.
    pub sql: String,
    /// The referenced cells, in the order their CTEs are written (each after what it needs).
    pub references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceError {
    #[error("there is no cell {0}")]
    NoSuchCell(usize),
    #[error("cell {0} is Markdown, which does not run")]
    NotSql(usize),
    #[error("there is no SQL cell named `{0}`")]
    UnknownName(String),
    #[error("the cell `{0}` references itself")]
    SelfReference(String),
    #[error("the cells reference each other in a circle: {}", .0.join(" → "))]
    Cycle(Vec<String>),
    #[error(
        "the cell `{0}` references other cells, so it must hold exactly one statement (it holds {1})"
    )]
    NotOneStatement(String, usize),
    #[error("the referenced cell `{0}` must hold exactly one statement, a query (it holds {1})")]
    ReferencedNotOneStatement(String, usize),
    #[error("the referenced cell `{name}` is not a query, so it cannot be referenced: {reason}")]
    NotARead { name: String, reason: String },
    #[error("a cell may bring along at most {MAX_REFERENCED_CELLS} referenced cells")]
    TooMany,
    #[error("the statement with its references would be too large to run")]
    TooLarge,
    #[error("references between cells need a SQL engine; {0} is not one")]
    EngineWithoutSql(String),
}

/// A `{{name}}` reference in a cell: where it is and the name it gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub range: Range<usize>,
    pub name: String,
}

/// The `{{name}}` references in `sql`'s code (outside its strings, quoted names and comments,
/// as `dialect` reads them). Spaces inside the braces are allowed; braces around anything
/// that cannot be a cell's name are not a reference.
pub fn references(dialect: &dyn Dialect, sql: &str) -> Vec<Reference> {
    if dialect.is_sql() {
        references_in(sql, dialect.code_ranges(sql))
    } else {
        // A language that is not SQL has no CTE to expand into: any `{{name}}` in it is a
        // reference, refused by `expand_cell`, rather than text sent as it is.
        references_in(sql, std::iter::once(0..sql.len()).collect())
    }
}

fn references_in(sql: &str, codes: Vec<Range<usize>>) -> Vec<Reference> {
    let mut found = Vec::new();
    for code in codes {
        let text = &sql[code.clone()];
        let mut from = 0;
        while let Some(open) = text[from..].find("{{").map(|i| from + i) {
            let inner_start = open + 2;
            let Some(close) = text[inner_start..].find("}}").map(|i| inner_start + i) else {
                break;
            };
            let name = text[inner_start..close].trim();
            if is_cell_name(name) && !text[inner_start..close].contains('\n') {
                found.push(Reference {
                    range: code.start + open..code.start + close + 2,
                    name: name.to_string(),
                });
                from = close + 2;
            } else {
                from = open + 1;
            }
        }
    }
    found
}

/// `sql` with each reference replaced by the referenced cell's name, quoted by `dialect`.
fn replaced(dialect: &dyn Dialect, sql: &str, refs: &[Reference], names: &Names) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut at = 0;
    for reference in refs {
        out.push_str(&sql[at..reference.range.start]);
        let name = names.canonical(&reference.name).unwrap_or(&reference.name);
        out.push_str(&dialect.quote_identifier(name));
        at = reference.range.end;
    }
    out.push_str(&sql[at..]);
    out
}

/// The notebook's SQL cells by name, without regard to case.
struct Names<'a> {
    by_name: HashMap<String, (usize, &'a str, &'a str)>,
}

impl<'a> Names<'a> {
    fn new(notebook: &'a Notebook) -> Self {
        let by_name = notebook
            .cells
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| match cell {
                Cell::Sql { name, sql } => Some((
                    name.to_ascii_lowercase(),
                    (index, name.as_str(), sql.as_str()),
                )),
                Cell::Markdown { .. } => None,
            })
            .collect();
        Self { by_name }
    }

    fn get(&self, name: &str) -> Option<(usize, &'a str, &'a str)> {
        self.by_name.get(&name.to_ascii_lowercase()).copied()
    }

    fn canonical(&self, name: &str) -> Option<&'a str> {
        self.get(name).map(|(_, name, _)| name)
    }
}

/// Expands the cell at `index` of `notebook` for `dialect`'s engine (ADR-087, points 8 and 9).
pub fn expand_cell(
    notebook: &Notebook,
    index: usize,
    dialect: &dyn Dialect,
) -> Result<ExpandedCell, ReferenceError> {
    let (name, sql) = match notebook.cells.get(index) {
        None => return Err(ReferenceError::NoSuchCell(index + 1)),
        Some(Cell::Markdown { .. }) => return Err(ReferenceError::NotSql(index + 1)),
        Some(Cell::Sql { name, sql }) => (name.as_str(), sql.as_str()),
    };
    let refs = references(dialect, sql);
    if refs.is_empty() {
        return Ok(ExpandedCell {
            sql: sql.to_string(),
            references: Vec::new(),
        });
    }
    if !dialect.is_sql() {
        return Err(ReferenceError::EngineWithoutSql(dialect.name().to_string()));
    }
    let statements = dialect.split_statements(sql).len();
    if statements != 1 {
        return Err(ReferenceError::NotOneStatement(
            name.to_string(),
            statements,
        ));
    }
    let names = Names::new(notebook);

    // The referenced cells, each after the ones it needs (a depth-first walk that refuses a
    // circle), with their bodies already written with quoted names.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut done: HashMap<String, ()> = HashMap::new();
    let mut path: Vec<String> = vec![name.to_string()];
    for reference in &refs {
        visit(
            dialect,
            &names,
            &reference.name,
            &mut path,
            &mut done,
            &mut order,
        )?;
    }

    let main = replaced(dialect, sql, &refs, &names);
    let ctes = order
        .iter()
        .map(|(name, body)| format!("{} AS (\n{body}\n)", dialect.quote_identifier(name)))
        .collect::<Vec<_>>()
        .join(",\n");
    let expanded = match leading_with(dialect, &main) {
        Some(at) => format!("{} {ctes},\n{}", &main[..at], main[at..].trim_start()),
        None => format!("WITH {ctes}\n{main}"),
    };
    if expanded.len() > MAX_EXPANDED_BYTES {
        return Err(ReferenceError::TooLarge);
    }
    Ok(ExpandedCell {
        sql: expanded,
        references: order.into_iter().map(|(name, _)| name).collect(),
    })
}

fn visit(
    dialect: &dyn Dialect,
    names: &Names,
    wanted: &str,
    path: &mut Vec<String>,
    done: &mut HashMap<String, ()>,
    order: &mut Vec<(String, String)>,
) -> Result<(), ReferenceError> {
    let Some((_, name, sql)) = names.get(wanted) else {
        return Err(ReferenceError::UnknownName(wanted.to_string()));
    };
    let key = name.to_ascii_lowercase();
    if done.contains_key(&key) {
        return Ok(());
    }
    if let Some(at) = path.iter().position(|p| p.eq_ignore_ascii_case(name)) {
        if at == path.len() - 1 {
            return Err(ReferenceError::SelfReference(name.to_string()));
        }
        let mut circle: Vec<String> = path[at..].to_vec();
        circle.push(name.to_string());
        return Err(ReferenceError::Cycle(circle));
    }
    path.push(name.to_string());
    let refs = references(dialect, sql);
    for reference in &refs {
        visit(dialect, names, &reference.name, path, done, order)?;
    }
    path.pop();

    let spans = dialect.split_statements(sql);
    let [span] = spans.as_slice() else {
        return Err(ReferenceError::ReferencedNotOneStatement(
            name.to_string(),
            spans.len(),
        ));
    };
    // The statement without the comments and `;` around it, references written as names.
    let body = replaced(
        dialect,
        span.text(sql),
        &references(dialect, span.text(sql)),
        names,
    );
    match classify_read_only(dialect.name(), &body) {
        Ok(ReadKind::Query) => {}
        Ok(ReadKind::Explain | ReadKind::Show) => {
            return Err(ReferenceError::NotARead {
                name: name.to_string(),
                reason: "only a query (SELECT, VALUES, WITH … SELECT) can be referenced".into(),
            });
        }
        Err(reason) => {
            return Err(ReferenceError::NotARead {
                name: name.to_string(),
                reason,
            });
        }
    }
    done.insert(key, ());
    order.push((name.to_string(), body));
    if order.len() > MAX_REFERENCED_CELLS {
        return Err(ReferenceError::TooMany);
    }
    Ok(())
}

/// Where the cell's own CTEs start, when its code starts with `WITH` (or `WITH RECURSIVE`):
/// the byte just after that keyword, where the references' CTEs go first.
fn leading_with(dialect: &dyn Dialect, sql: &str) -> Option<usize> {
    let codes = dialect.code_ranges(sql);
    let mut words = codes.iter().flat_map(|code| words_in(sql, code.clone()));
    let first = words.next()?;
    if !sql[first.clone()].eq_ignore_ascii_case("with") {
        return None;
    }
    match words.next() {
        Some(next) if sql[next.clone()].eq_ignore_ascii_case("recursive") => Some(next.end),
        _ => Some(first.end),
    }
}

/// The words (letters, digits and `_`) of `sql[range]`, and a range for each other character
/// that is not white space, so the first two items are what the code starts with.
fn words_in(sql: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let text = &sql[range.clone()];
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let mut end = at + c.len_utf8();
        if c.is_alphanumeric() || c == '_' {
            while let Some(&(next, d)) = chars.peek() {
                if !(d.is_alphanumeric() || d == '_') {
                    break;
                }
                end = next + d.len_utf8();
                chars.next();
            }
        }
        out.push(range.start + at..range.start + end);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialect(engine: &str) -> std::sync::Arc<dyn Dialect> {
        spelaion_dialect::for_engine(engine).expect("a dialect")
    }

    fn notebook(cells: &[(&str, &str)]) -> Notebook {
        Notebook {
            connection: None,
            cells: cells
                .iter()
                .map(|(name, sql)| Cell::Sql {
                    name: (*name).into(),
                    sql: (*sql).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn references_are_found_only_in_code() {
        let pg = dialect("postgres");
        let sql =
            "select '{{a}}', \"{{b}}\" -- {{c}}\n from {{ d }} join {{e}} on {{1x}} /* {{f}} */";
        let names: Vec<String> = references(pg.as_ref(), sql)
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(names, ["d", "e"]);
        let found = references(pg.as_ref(), "x{{a}}y");
        assert_eq!(found[0].range, 1..6);
        let mysql = dialect("mysql");
        assert!(references(mysql.as_ref(), "select `{{a}}`").is_empty());
        let mssql = dialect("sqlserver");
        assert!(references(mssql.as_ref(), "select [{{a}}]").is_empty());
    }

    #[test]
    fn a_cell_without_references_runs_as_written() {
        let nb = notebook(&[("a", "select 1; select 2;")]);
        let expanded = expand_cell(&nb, 0, dialect("postgres").as_ref()).unwrap();
        assert_eq!(expanded.sql, "select 1; select 2;");
        assert!(expanded.references.is_empty());
    }

    #[test]
    fn references_become_ctes_in_the_order_they_are_needed() {
        let nb = notebook(&[
            ("base", "select id from customers; -- the customers"),
            ("Top", "select id from {{base}} where id < 10"),
            (
                "report",
                "select * from {{top}} t join {{BASE}} b using (id)",
            ),
        ]);
        let expanded = expand_cell(&nb, 2, dialect("postgres").as_ref()).unwrap();
        assert_eq!(expanded.references, ["base", "Top"]);
        // PostgreSQL quotes only the names that need it.
        assert_eq!(
            expanded.sql,
            "WITH base AS (\nselect id from customers\n),\n\"Top\" AS (\n\
             select id from base where id < 10\n)\n\
             select * from \"Top\" t join base b using (id)"
        );
        let mysql = expand_cell(&nb, 1, dialect("mysql").as_ref()).unwrap();
        assert_eq!(
            mysql.sql,
            "WITH `base` AS (\nselect id from customers\n)\nselect id from `base` where id < 10"
        );
    }

    #[test]
    fn a_cell_that_starts_with_with_gets_the_references_first() {
        let nb = notebook(&[
            ("a", "select 1 as n"),
            (
                "b",
                "/* note */ WITH x AS (select n from {{a}}) select * from x",
            ),
            (
                "c",
                "with recursive r(n) as (select n from {{a}} union all select n + 1 from r where n < 3) select * from r",
            ),
        ]);
        let pg = dialect("postgres");
        assert_eq!(
            expand_cell(&nb, 1, pg.as_ref()).unwrap().sql,
            "/* note */ WITH a AS (\nselect 1 as n\n),\nx AS (select n from a) select * from x"
        );
        assert_eq!(
            expand_cell(&nb, 2, pg.as_ref()).unwrap().sql,
            "with recursive a AS (\nselect 1 as n\n),\nr(n) as (select n from a union all select n + 1 from r where n < 3) select * from r"
        );
    }

    #[test]
    fn only_a_single_query_can_be_referenced() {
        let pg = dialect("postgres");
        for (body, kind) in [
            ("delete from t returning *", "not a read"),
            (
                "with x as (delete from t returning *) select * from x",
                "not a read",
            ),
            ("explain select 1", "not a read"),
            ("select 1; select 2", "statements"),
            ("insert into t values (1)", "not a read"),
        ] {
            let nb = notebook(&[("w", body), ("r", "select * from {{w}}")]);
            let error = expand_cell(&nb, 1, pg.as_ref()).unwrap_err();
            match kind {
                "statements" => assert_eq!(
                    error,
                    ReferenceError::ReferencedNotOneStatement("w".into(), 2)
                ),
                _ => assert!(
                    matches!(error, ReferenceError::NotARead { .. }),
                    "{body}: {error}"
                ),
            }
        }
    }

    #[test]
    fn unknown_names_circles_and_markdown_are_refused() {
        let pg = dialect("postgres");
        let nb = notebook(&[("a", "select * from {{nope}}")]);
        assert_eq!(
            expand_cell(&nb, 0, pg.as_ref()),
            Err(ReferenceError::UnknownName("nope".into()))
        );
        let nb = notebook(&[("a", "select * from {{a}}")]);
        assert_eq!(
            expand_cell(&nb, 0, pg.as_ref()),
            Err(ReferenceError::SelfReference("a".into()))
        );
        let nb = notebook(&[
            ("a", "select * from {{b}}"),
            ("b", "select * from {{c}}"),
            ("c", "select * from {{a}}"),
        ]);
        assert_eq!(
            expand_cell(&nb, 0, pg.as_ref()),
            Err(ReferenceError::Cycle(vec![
                "a".into(),
                "b".into(),
                "c".into(),
                "a".into()
            ]))
        );
        let nb = notebook(&[("a", "select 1"), ("b", "select * from {{a}}; select 2")]);
        assert_eq!(
            expand_cell(&nb, 1, pg.as_ref()),
            Err(ReferenceError::NotOneStatement("b".into(), 2))
        );
        let mut nb = notebook(&[("a", "select 1")]);
        nb.cells.push(Cell::Markdown { text: "x".into() });
        assert_eq!(
            expand_cell(&nb, 1, pg.as_ref()),
            Err(ReferenceError::NotSql(2))
        );
        assert_eq!(
            expand_cell(&nb, 9, pg.as_ref()),
            Err(ReferenceError::NoSuchCell(10))
        );
    }

    #[test]
    fn engines_without_sql_refuse_references_but_run_plain_cells() {
        let redis = dialect("redis");
        let nb = notebook(&[("a", "GET k"), ("b", "GET {{a}}")]);
        assert_eq!(expand_cell(&nb, 0, redis.as_ref()).unwrap().sql, "GET k");
        assert!(matches!(
            expand_cell(&nb, 1, redis.as_ref()),
            Err(ReferenceError::EngineWithoutSql(_))
        ));
    }

    #[test]
    fn too_many_referenced_cells_are_refused() {
        let mut cells: Vec<(String, String)> = (0..=MAX_REFERENCED_CELLS)
            .map(|i| (format!("c{i}"), "select 1 as n".to_string()))
            .collect();
        let all = (0..=MAX_REFERENCED_CELLS)
            .map(|i| format!("{{{{c{i}}}}}"))
            .collect::<Vec<_>>()
            .join(", ");
        cells.push(("last".into(), format!("select * from {all}")));
        let borrowed: Vec<(&str, &str)> = cells
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let nb = notebook(&borrowed);
        assert_eq!(
            expand_cell(&nb, MAX_REFERENCED_CELLS + 1, dialect("postgres").as_ref()),
            Err(ReferenceError::TooMany)
        );
    }
}
