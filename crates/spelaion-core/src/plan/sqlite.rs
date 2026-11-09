//! SQLite's `EXPLAIN QUERY PLAN` into the shared tree. The engine answers with rows
//! (`id`, `parent`, `notused`, `detail`): the core keeps them as a JSON array, the same one
//! `sqlite3 -json` prints, and also reads the `sqlite3` shell's `QUERY PLAN` text tree from a
//! file. SQLite reports no costs, row counts or timings: only the steps and their text.
use super::*;
use serde_json::Value;

const NOTE: &str = "SQLite's EXPLAIN QUERY PLAN lists the steps the query will take, in nested-loop order: each SCAN or SEARCH is one table (named by its alias in the query) visited inside the steps before it, so joins are not separate steps. It has no costs, row estimates or timings, and SQLite has no ANALYZE that executes the query, so every metric stays unavailable; the engine's own text is kept on each step. SCAN without an index visits the whole table, SEARCH uses an index or the rowid, and a temporary B-tree sorts, groups or removes duplicates. Findings need metrics, so SQLite plans have none.";

/// The longest engine text used as a step's type when it is not one this module knows.
const MAX_TYPE: usize = 80;

/// A JSON array of `EXPLAIN QUERY PLAN` rows: objects with `id`, `parent` and `detail`.
pub(super) fn is_json(json: &Value) -> bool {
    json.as_array().is_some_and(|rows| {
        rows.first().is_some_and(|row| {
            row.get("detail").is_some() && row.get("parent").is_some() && row.get("id").is_some()
        })
    })
}

/// The `sqlite3` shell's tree starts with a `QUERY PLAN` line.
pub(super) fn is_tree(text: &str) -> bool {
    text.trim_start()
        .lines()
        .next()
        .is_some_and(|line| line.trim_end() == "QUERY PLAN")
}

/// One `EXPLAIN QUERY PLAN` row.
pub(crate) struct Row {
    pub id: i64,
    pub parent: i64,
    pub detail: String,
}

/// The rows as the JSON array this module reads back (and a saved plan keeps).
pub(crate) fn raw_json(rows: &[Row]) -> String {
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "parent": row.parent,
                "notused": 0,
                "detail": row.detail,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).expect("plan rows serialize")
}

pub(super) fn parse_json(raw: &str, json: &Value, name: &str) -> Result<PlanDocument, PlanError> {
    let items = json
        .as_array()
        .ok_or_else(|| PlanError::Invalid("expected SQLite plan rows".into()))?;
    if items.len() >= MAX_PLAN_NODES {
        return Err(PlanError::Limit("tree has more than 10,000 nodes".into()));
    }
    let integer = |row: &Value, key: &str| -> Result<i64, PlanError> {
        match row.get(key) {
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        }
        .ok_or_else(|| PlanError::Invalid(format!("each SQLite plan row needs an integer {key}")))
    };
    let rows = items
        .iter()
        .map(|row| {
            Ok(Row {
                id: integer(row, "id")?,
                parent: integer(row, "parent")?,
                detail: row
                    .get("detail")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        PlanError::Invalid("each SQLite plan row needs a text detail".into())
                    })?
                    .to_owned(),
            })
        })
        .collect::<Result<Vec<_>, PlanError>>()?;
    build(raw, name, PlanRawFormat::Json, &rows)
}

/// `QUERY PLAN` / `|--SCAN o` / `|  `--SEARCH c …`: three characters per level.
pub(super) fn parse_tree(raw: &str, name: &str) -> Result<PlanDocument, PlanError> {
    let mut rows = Vec::new();
    // The synthetic id of the last step seen at each depth.
    let mut open: Vec<i64> = Vec::new();
    for (number, line) in raw
        .trim_start_matches('\u{feff}')
        .trim_start()
        .lines()
        .skip(1)
        .enumerate()
    {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        let branch = line
            .find("|--")
            .into_iter()
            .chain(line.find("`--"))
            .min()
            .filter(|at| at % 3 == 0)
            .ok_or_else(|| {
                PlanError::Invalid(format!("line {} is not a QUERY PLAN step", number + 2))
            })?;
        let prefix = &line[..branch];
        if !prefix
            .as_bytes()
            .chunks(3)
            .all(|c| c == b"|  " || c == b"   ")
        {
            return Err(PlanError::Invalid(format!(
                "line {} is not a QUERY PLAN step",
                number + 2
            )));
        }
        let depth = branch / 3;
        if depth > open.len() {
            return Err(PlanError::Invalid(format!(
                "line {} is deeper than the step before it",
                number + 2
            )));
        }
        open.truncate(depth);
        let id = rows.len() as i64 + 1;
        rows.push(Row {
            id,
            parent: open.last().copied().unwrap_or(0),
            detail: line[branch + 3..].to_owned(),
        });
        open.push(id);
        if rows.len() >= MAX_PLAN_NODES {
            return Err(PlanError::Limit("tree has more than 10,000 nodes".into()));
        }
    }
    build(raw, name, PlanRawFormat::Text, &rows)
}

/// The rows under one synthetic root, in preorder. SQLite writes a parent before its
/// children; a row whose parent is not an earlier row is refused.
fn build(
    raw: &str,
    name: &str,
    raw_format: PlanRawFormat,
    rows: &[Row],
) -> Result<PlanDocument, PlanError> {
    if rows.is_empty() {
        return Err(PlanError::Invalid("the plan has no steps".into()));
    }
    let mut position = std::collections::HashMap::<i64, usize>::new();
    // Children of each row (by position), and of the root.
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
    let mut top = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if row.parent == 0 {
            top.push(i);
        } else {
            let parent = *position.get(&row.parent).ok_or_else(|| {
                PlanError::Invalid(format!(
                    "step {} refers to a parent step {} that does not come before it",
                    row.id, row.parent
                ))
            })?;
            children[parent].push(i);
        }
        if position.insert(row.id, i).is_some() {
            return Err(PlanError::Invalid(format!(
                "step id {} is repeated",
                row.id
            )));
        }
    }
    let mut nodes = vec![PlanNode::blank(
        0,
        None,
        0,
        "Query plan".into(),
        PlanOperation::Other,
    )];
    // Depth-first, so the vector is preorder.
    let mut stack: Vec<(usize, u32)> = top.iter().rev().map(|i| (*i, 0)).collect();
    while let Some((i, parent)) = stack.pop() {
        let depth = nodes[parent as usize].depth + 1;
        if depth > MAX_PLAN_DEPTH {
            return Err(PlanError::Limit("tree is deeper than 60 levels".into()));
        }
        let id = nodes.len() as u32;
        let mut node =
            PlanNode::blank(id, Some(parent), depth, String::new(), PlanOperation::Other);
        describe(&mut node, &rows[i].detail);
        nodes.push(node);
        nodes[parent as usize].children.push(id);
        stack.extend(children[i].iter().rev().map(|child| (*child, id)));
    }
    Ok(document(raw, name, "sqlite", raw_format, nodes, None, NOTE))
}

/// The text inside the last parentheses: `(a=? AND b>?)` → `a=? AND b>?`.
fn condition(text: &str) -> (&str, Option<String>) {
    match (text.find(" ("), text.ends_with(')')) {
        (Some(at), true) => (&text[..at], Some(text[at + 2..text.len() - 1].to_owned())),
        _ => (text, None),
    }
}

fn sentence(words: &str) -> String {
    let lower = words.to_ascii_lowercase();
    let mut chars = lower.chars();
    chars
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

/// Fills a step's type and fields from SQLite's text, which is kept whole as its detail.
fn describe(node: &mut PlanNode, detail: &str) {
    let text = detail.trim();
    node.detail = Some(text.to_owned());
    let access = text
        .strip_prefix("SCAN ")
        .map(|rest| (false, rest))
        .or_else(|| text.strip_prefix("SEARCH ").map(|rest| (true, rest)));
    if let Some((search, rest)) = access {
        // SQLite before 3.36 wrote `SCAN TABLE orders AS o`.
        let rest = rest.strip_prefix("TABLE ").unwrap_or(rest);
        let (target, using) = match rest.find(" USING ") {
            Some(at) => (&rest[..at], Some(&rest[at + 7..])),
            None => match rest.find(" VIRTUAL TABLE ") {
                Some(at) => (&rest[..at], Some(&rest[at + 1..])),
                None => (rest, None),
            },
        };
        let target = target
            .rsplit_once(" AS ")
            .map_or(target, |(_, alias)| alias);
        if target == "CONSTANT ROW" {
            node.node_type = "Constant row".into();
            return;
        }
        node.operation = PlanOperation::Scan;
        node.relation = Some(target.to_owned());
        let verb = if search { "search" } else { "scan" };
        let Some(using) = using else {
            node.node_type = if search { "Search" } else { "Scan" }.into();
            node.sequential_scan = !search;
            return;
        };
        let (using, cond) = condition(using);
        node.index_condition = cond;
        node.node_type = if let Some(index) = using
            .strip_prefix("COVERING INDEX ")
            .map(|i| (true, i))
            .or_else(|| using.strip_prefix("INDEX ").map(|i| (false, i)))
        {
            node.index = Some(index.1.to_owned());
            format!(
                "{} {verb}",
                if index.0 { "Covering index" } else { "Index" }
            )
        } else if using.starts_with("AUTOMATIC ") {
            format!("Automatic index {verb}")
        } else if using.starts_with("INTEGER PRIMARY KEY") {
            format!("Rowid {verb}")
        } else if using.starts_with("PRIMARY KEY") {
            format!("Primary key {verb}")
        } else if using.starts_with("VIRTUAL TABLE") {
            format!("Virtual table {verb}")
        } else {
            sentence(verb)
        };
        return;
    }
    if let Some(purpose) = text.strip_prefix("USE TEMP B-TREE FOR ") {
        node.node_type = "Temp B-tree".into();
        node.operation = if purpose.contains("ORDER BY") {
            node.sort_method = Some(format!("temporary B-tree for {purpose}"));
            PlanOperation::Sort
        } else if purpose.contains("GROUP BY") {
            PlanOperation::Aggregate
        } else {
            PlanOperation::Temporary
        };
        return;
    }
    let known: &[(&str, &str, PlanOperation)] = &[
        ("MATERIALIZE ", "Materialize", PlanOperation::Temporary),
        ("CO-ROUTINE ", "Co-routine", PlanOperation::Other),
        ("COMPOUND ", "Compound query", PlanOperation::Other),
        (
            "LEFT-MOST SUBQUERY",
            "Left-most subquery",
            PlanOperation::Other,
        ),
        ("UNION ALL", "Union all", PlanOperation::Other),
        ("UNION USING TEMP B-TREE", "Union", PlanOperation::Temporary),
        (
            "INTERSECT USING TEMP B-TREE",
            "Intersect",
            PlanOperation::Temporary,
        ),
        (
            "EXCEPT USING TEMP B-TREE",
            "Except",
            PlanOperation::Temporary,
        ),
        ("MULTI-INDEX OR", "Multi-index OR", PlanOperation::Other),
        ("RIGHT-JOIN ", "Right join", PlanOperation::Join),
        // SQLite 3.38 and later build a Bloom filter for a join's inner table.
        (
            "CREATE BLOOM FILTER",
            "Create Bloom filter",
            PlanOperation::Temporary,
        ),
    ];
    if let Some((_, label, operation)) = known.iter().find(|(p, _, _)| text.starts_with(p)) {
        node.node_type = (*label).into();
        node.operation = *operation;
    } else if let Some(rest) = text.strip_prefix("BLOOM FILTER ON ") {
        let (table, cond) = condition(rest);
        node.node_type = "Bloom filter".into();
        node.relation = Some(table.to_owned());
        node.index_condition = cond;
    } else if let Some(at) = text.find("SUBQUERY") {
        // `LIST SUBQUERY 1`, `CORRELATED SCALAR SUBQUERY 2`.
        node.node_type = sentence(&text[..at + 8]);
    } else if text
        .strip_prefix("INDEX ")
        .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
    {
        node.node_type = "Index".into();
    } else {
        let mut end = text.len().min(MAX_TYPE);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        node.node_type = if text.is_empty() {
            "Step".into()
        } else {
            text[..end].to_owned()
        };
    }
}
