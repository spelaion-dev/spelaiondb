//! The notebook file format, version 1 (ADR-087, points 1 to 6): Markdown with a front matter
//! block, SQL cells as fenced blocks whose info string is `sql cell=<name>`, and Markdown cells
//! between them, two in a row separated by a line `<!-- cell -->`.
//!
//! The contract the tests hold: reading what [`write`] wrote gives the same cells, and writing
//! what [`parse`] read gives the same text.

use std::collections::HashSet;

use super::{Cell, MAX_CELL_NAME, MAX_CELLS, Notebook, NotebookError};

/// The format's version, written in the front matter.
pub const FORMAT_VERSION: u32 = 1;
/// The front matter's key that says a Markdown file is a notebook, and of which version.
const VERSION_KEY: &str = "spelaion-notebook";
/// The front matter's key of the connection it was last run on.
const CONNECTION_KEY: &str = "connection";
/// The line that separates two Markdown cells in a row.
const SEPARATOR: &str = "<!-- cell -->";
/// The longest connection id the front matter keeps.
const MAX_CONNECTION_ID: usize = 512;

/// Whether `name` can name a cell: a letter or `_`, then letters, digits or `_`, at most
/// [`MAX_CELL_NAME`] characters.
pub fn is_cell_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    name.len() <= MAX_CELL_NAME && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What a line that opens a fence says: `Some(Ok((ticks, name)))` for a SQL cell's opening
/// fence, `Some(Err(name))` for one that says `sql cell=` with a name that cannot be a cell's,
/// `None` for anything else.
fn opening_fence(line: &str) -> Option<Result<(usize, String), String>> {
    let ticks = line.bytes().take_while(|&b| b == b'`').count();
    if ticks < 3 {
        return None;
    }
    let mut words = line[ticks..].split_whitespace();
    if words.next() != Some("sql") {
        return None;
    }
    let name = words.next()?.strip_prefix("cell=")?;
    if words.next().is_some() || !is_cell_name(name) {
        return Some(Err(name.to_string()));
    }
    Some(Ok((ticks, name.to_string())))
}

/// Whether `line` closes a fence of `ticks` backticks.
fn closes(line: &str, ticks: usize) -> bool {
    let line = line.trim_end();
    line.len() == ticks && line.bytes().all(|b| b == b'`')
}

/// `text` with every line break as `\n`.
pub(crate) fn unix_lines(text: &str) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        text.to_string()
    }
}

/// `lines` without the blank lines at either end, joined with `\n`.
fn trimmed_markdown(lines: &[&str]) -> String {
    let blank = |line: &&str| line.trim().is_empty();
    let start = lines.iter().position(|l| !blank(l)).unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !blank(l))
        .map_or(start, |i| i + 1);
    lines[start..end].join("\n")
}

/// Reads a notebook from its text (any line breaks; a byte order mark is ignored).
pub fn parse(text: &str) -> Result<Notebook, NotebookError> {
    let text = unix_lines(text.trim_start_matches('\u{feff}'));
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return Err(NotebookError::NotANotebook);
    }
    // A front matter block that never closes is not one: the file is not a notebook.
    let Some(close) = (1..lines.len()).find(|&i| lines[i].trim_end() == "---") else {
        return Err(NotebookError::NotANotebook);
    };
    let mut version = None;
    let mut connection = None;
    for (index, line) in lines.iter().enumerate().take(close).skip(1) {
        let number = index + 1;
        let Some((key, value)) = line.split_once(':') else {
            return Err(front_matter(number, "it is not a `key: value` line"));
        };
        let value = value.trim();
        match key.trim() {
            VERSION_KEY if version.is_none() => {
                let parsed: u32 = value.parse().map_err(|_| {
                    front_matter(number, "the version must be a whole number, such as 1")
                })?;
                version = Some((parsed, number));
            }
            CONNECTION_KEY if connection.is_none() => {
                if value.is_empty()
                    || value.len() > MAX_CONNECTION_ID
                    || value.chars().any(char::is_control)
                {
                    return Err(front_matter(
                        number,
                        "the connection is not a connection id",
                    ));
                }
                connection = Some(value.to_string());
            }
            key @ (VERSION_KEY | CONNECTION_KEY) => {
                return Err(front_matter(number, &format!("`{key}` is given twice")));
            }
            other => {
                return Err(front_matter(
                    number,
                    &format!("`{other}` is not a key a notebook has"),
                ));
            }
        }
    }
    let mut at = close + 1;
    match version {
        None => return Err(NotebookError::NotANotebook),
        Some((v, _)) if v > FORMAT_VERSION => {
            return Err(NotebookError::NewerVersion { version: v });
        }
        Some((v, line)) if v < FORMAT_VERSION => {
            return Err(front_matter(line, &format!("there is no version {v}")));
        }
        Some(_) => {}
    }

    let mut cells = Vec::new();
    let mut names = HashSet::new();
    let mut markdown: Vec<&str> = Vec::new();
    let flush = |markdown: &mut Vec<&str>, cells: &mut Vec<Cell>| {
        let text = trimmed_markdown(markdown);
        markdown.clear();
        if !text.is_empty() {
            cells.push(Cell::Markdown { text });
        }
    };
    while at < lines.len() {
        let line = lines[at];
        let number = at + 1;
        at += 1;
        match opening_fence(line) {
            Some(Ok((ticks, name))) => {
                flush(&mut markdown, &mut cells);
                if !names.insert(name.to_ascii_lowercase()) {
                    return Err(NotebookError::DuplicateName {
                        name,
                        line: Some(number),
                    });
                }
                let start = at;
                let Some(end) = (start..lines.len()).find(|&i| closes(lines[i], ticks)) else {
                    return Err(NotebookError::UnclosedCell { name, line: number });
                };
                cells.push(Cell::Sql {
                    name,
                    sql: lines[start..end].join("\n"),
                });
                at = end + 1;
            }
            Some(Err(name)) => {
                return Err(NotebookError::BadCellName {
                    name,
                    line: Some(number),
                });
            }
            None if line.trim_end() == SEPARATOR => flush(&mut markdown, &mut cells),
            None => markdown.push(line),
        }
        if cells.len() > MAX_CELLS {
            return Err(NotebookError::TooManyCells);
        }
    }
    flush(&mut markdown, &mut cells);
    if cells.len() > MAX_CELLS {
        return Err(NotebookError::TooManyCells);
    }
    Ok(Notebook { connection, cells })
}

fn front_matter(line: usize, reason: &str) -> NotebookError {
    NotebookError::FrontMatter {
        line,
        reason: reason.to_string(),
    }
}

/// The fence for `sql`: one backtick longer than the longest run of backticks that starts one
/// of its lines (after the three spaces of indentation CommonMark allows), three at least.
fn fence_for(sql: &str) -> String {
    let longest = sql
        .split('\n')
        .map(|line| {
            let indent = line.bytes().take_while(|&b| b == b' ').count();
            if indent > 3 {
                0
            } else {
                line[indent..].bytes().take_while(|&b| b == b'`').count()
            }
        })
        .max()
        .unwrap_or(0);
    "`".repeat((longest + 1).max(3))
}

/// The notebook as its cells will read back: line breaks as `\n`, Markdown cells without the
/// blank lines at either end, empty Markdown cells dropped (ADR-087, point 4).
pub fn normalize(notebook: &Notebook) -> Notebook {
    let cells = notebook
        .cells
        .iter()
        .filter_map(|cell| match cell {
            Cell::Sql { name, sql } => Some(Cell::Sql {
                name: name.clone(),
                sql: unix_lines(sql),
            }),
            Cell::Markdown { text } => {
                let text = unix_lines(text);
                let lines: Vec<&str> = text.split('\n').collect();
                let text = trimmed_markdown(&lines);
                (!text.is_empty()).then_some(Cell::Markdown { text })
            }
        })
        .collect();
    Notebook {
        connection: notebook.connection.clone(),
        cells,
    }
}

/// Checks what [`write`] needs of a notebook: names valid and unique, the connection id plain,
/// no more than [`MAX_CELLS`] cells, and no Markdown cell holding a line that would read back
/// as a SQL cell's fence or a separator.
pub fn check(notebook: &Notebook) -> Result<(), NotebookError> {
    if notebook.cells.len() > MAX_CELLS {
        return Err(NotebookError::TooManyCells);
    }
    if let Some(connection) = &notebook.connection
        && (connection.trim() != connection
            || connection.is_empty()
            || connection.len() > MAX_CONNECTION_ID
            || connection.chars().any(char::is_control))
    {
        return Err(NotebookError::BadConnection);
    }
    let mut names = HashSet::new();
    for (index, cell) in notebook.cells.iter().enumerate() {
        match cell {
            Cell::Sql { name, .. } => {
                if !is_cell_name(name) {
                    return Err(NotebookError::BadCellName {
                        name: name.clone(),
                        line: None,
                    });
                }
                if !names.insert(name.to_ascii_lowercase()) {
                    return Err(NotebookError::DuplicateName {
                        name: name.clone(),
                        line: None,
                    });
                }
            }
            Cell::Markdown { text } => {
                let marker = unix_lines(text)
                    .split('\n')
                    .any(|line| opening_fence(line).is_some() || line.trim_end() == SEPARATOR);
                if marker {
                    return Err(NotebookError::MarkdownHoldsMarker { cell: index + 1 });
                }
            }
        }
    }
    Ok(())
}

/// The notebook's text, with `\n` line breaks (the file's own are put in when it is written).
pub fn write(notebook: &Notebook) -> Result<String, NotebookError> {
    check(notebook)?;
    let notebook = normalize(notebook);
    let mut out = format!("---\n{VERSION_KEY}: {FORMAT_VERSION}\n");
    if let Some(connection) = &notebook.connection {
        out.push_str(&format!("{CONNECTION_KEY}: {connection}\n"));
    }
    out.push_str("---\n");
    let mut after_markdown = false;
    for cell in &notebook.cells {
        match cell {
            Cell::Sql { name, sql } => {
                let fence = fence_for(sql);
                out.push('\n');
                out.push_str(&format!("{fence}sql cell={name}\n"));
                if !sql.is_empty() {
                    out.push_str(sql);
                    out.push('\n');
                }
                out.push_str(&fence);
                out.push('\n');
                after_markdown = false;
            }
            Cell::Markdown { text } => {
                if after_markdown {
                    out.push('\n');
                    out.push_str(SEPARATOR);
                    out.push('\n');
                }
                out.push('\n');
                out.push_str(text);
                out.push('\n');
                after_markdown = true;
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(name: &str, sql: &str) -> Cell {
        Cell::Sql {
            name: name.into(),
            sql: sql.into(),
        }
    }

    fn md(text: &str) -> Cell {
        Cell::Markdown { text: text.into() }
    }

    fn notebook(cells: Vec<Cell>) -> Notebook {
        Notebook {
            connection: Some("prod/orders".into()),
            cells,
        }
    }

    /// The contract: what is written reads back as the same cells, and writes again the same.
    fn round_trips(notebook: &Notebook) -> String {
        let text = write(notebook).unwrap();
        let read = parse(&text).unwrap();
        assert_eq!(read, normalize(notebook), "{text}");
        assert_eq!(write(&read).unwrap(), text);
        text
    }

    #[test]
    fn a_notebook_is_written_as_markdown_with_named_sql_fences() {
        let text = round_trips(&notebook(vec![
            md("# Lost customers\n\nWho stopped ordering?"),
            sql("lost", "SELECT id FROM customers"),
            md("And what they bought:"),
            md("A second note."),
            sql("bought", "SELECT * FROM {{lost}}"),
        ]));
        assert_eq!(
            text,
            "---\nspelaion-notebook: 1\nconnection: prod/orders\n---\n\n# Lost customers\n\n\
             Who stopped ordering?\n\n```sql cell=lost\nSELECT id FROM customers\n```\n\n\
             And what they bought:\n\n<!-- cell -->\n\nA second note.\n\n\
             ```sql cell=bought\nSELECT * FROM {{lost}}\n```\n"
        );
    }

    #[test]
    fn sql_with_backticks_blank_lines_and_edges_round_trips() {
        for body in [
            "",
            "\n",
            "select 1\n",
            "\n\nselect 1\n\n",
            "select `a` from `t`",
            "```\nselect 1\n```",
            "  ````\n````` x",
            "select '\n```sql cell=x\n'",
            "-- <!-- cell -->\nselect 1",
        ] {
            round_trips(&notebook(vec![
                sql("c", body),
                md("between"),
                sql("d", body),
            ]));
        }
        let text = write(&notebook(vec![sql("c", "```\nx\n  ````")])).unwrap();
        assert!(text.contains("\n`````sql cell=c\n"), "{text}");
    }

    #[test]
    fn markdown_is_trimmed_and_empty_cells_are_not_kept() {
        let nb = notebook(vec![md("\n\n  \ntext\n\n"), md(""), md("   "), md("more")]);
        let text = round_trips(&nb);
        assert_eq!(
            parse(&text).unwrap().cells,
            vec![md("text"), md("more")],
            "{text}"
        );
        assert_eq!(
            round_trips(&notebook(vec![])),
            "---\nspelaion-notebook: 1\nconnection: prod/orders\n---\n"
        );
    }

    #[test]
    fn crlf_and_a_byte_order_mark_read_the_same() {
        let text = write(&notebook(vec![md("a\nb"), sql("c", "select 1\nfrom t")])).unwrap();
        let crlf = format!("\u{feff}{}", text.replace('\n', "\r\n"));
        assert_eq!(parse(&crlf).unwrap(), parse(&text).unwrap());
        let nb = notebook(vec![sql("c", "select 1\r\nfrom t\r\n")]);
        assert_eq!(
            parse(&write(&nb).unwrap()).unwrap().cells,
            vec![sql("c", "select 1\nfrom t\n")]
        );
    }

    #[test]
    fn a_markdown_cell_that_would_read_back_differently_is_refused() {
        for text in [
            "```sql cell=x\nselect 1\n```",
            "text\n<!-- cell -->\nmore",
            "````sql   cell=bad-name",
        ] {
            assert_eq!(
                write(&notebook(vec![sql("a", "1"), md(text)])),
                Err(NotebookError::MarkdownHoldsMarker { cell: 2 }),
                "{text}"
            );
        }
        // A fence of Markdown without `cell=` is Markdown.
        round_trips(&notebook(vec![md("```sql\nselect 1\n```")]));
    }

    #[test]
    fn names_are_checked_when_written_and_read() {
        assert!(is_cell_name("_a1") && is_cell_name("Top_10"));
        for bad in ["", "1a", "a-b", "a b", "ñ", &"a".repeat(64)] {
            assert!(!is_cell_name(bad), "{bad}");
            assert!(matches!(
                write(&notebook(vec![sql(bad, "1")])),
                Err(NotebookError::BadCellName { .. })
            ));
        }
        assert_eq!(
            write(&notebook(vec![sql("a", "1"), sql("A", "2")])),
            Err(NotebookError::DuplicateName {
                name: "A".into(),
                line: None
            })
        );
        let twice = "---\nspelaion-notebook: 1\n---\n```sql cell=a\n```\n```sql cell=A\n```\n";
        assert_eq!(
            parse(twice),
            Err(NotebookError::DuplicateName {
                name: "A".into(),
                line: Some(6)
            })
        );
        let bad = "---\nspelaion-notebook: 1\n---\n\n```sql cell=a-b\n```\n";
        assert_eq!(
            parse(bad),
            Err(NotebookError::BadCellName {
                name: "a-b".into(),
                line: Some(5)
            })
        );
    }

    #[test]
    fn the_front_matter_is_strict() {
        assert_eq!(parse("# just markdown\n"), Err(NotebookError::NotANotebook));
        assert_eq!(parse(""), Err(NotebookError::NotANotebook));
        assert_eq!(parse("---\ntitle: x"), Err(NotebookError::NotANotebook));
        assert_eq!(
            parse("---\nconnection: a\n---\n"),
            Err(NotebookError::NotANotebook)
        );
        assert_eq!(
            parse("---\nspelaion-notebook: 2\n---\n"),
            Err(NotebookError::NewerVersion { version: 2 })
        );
        for (text, line) in [
            ("---\nspelaion-notebook: 1\ntitle: x\n---\n", 3),
            ("---\nspelaion-notebook: one\n---\n", 2),
            ("---\nspelaion-notebook: 0\n---\n", 2),
            ("---\nspelaion-notebook: 1\nspelaion-notebook: 1\n---\n", 3),
            ("---\nspelaion-notebook: 1\nno colon\n---\n", 3),
            ("---\nspelaion-notebook: 1\nconnection:\n---\n", 3),
        ] {
            assert!(
                matches!(parse(text), Err(NotebookError::FrontMatter { line: l, .. }) if l == line),
                "{text}: {:?}",
                parse(text)
            );
        }
        let none = parse("---\nspelaion-notebook: 1\n---\n").unwrap();
        assert_eq!(none.connection, None);
        assert_eq!(write(&none).unwrap(), "---\nspelaion-notebook: 1\n---\n");
    }

    #[test]
    fn an_unclosed_cell_is_refused_with_its_line() {
        assert_eq!(
            parse("---\nspelaion-notebook: 1\n---\n\n````sql cell=a\nselect 1\n```\n"),
            Err(NotebookError::UnclosedCell {
                name: "a".into(),
                line: 5
            })
        );
    }

    #[test]
    fn too_many_cells_are_refused() {
        let cells = (0..=MAX_CELLS)
            .map(|i| sql(&format!("c{i}"), "1"))
            .collect();
        assert_eq!(write(&notebook(cells)), Err(NotebookError::TooManyCells));
        let mut text = String::from("---\nspelaion-notebook: 1\n---\n");
        for i in 0..=MAX_CELLS {
            text.push_str(&format!("```sql cell=c{i}\n```\n"));
        }
        assert_eq!(parse(&text), Err(NotebookError::TooManyCells));
    }
}
