//! Which statement of a script the editor's cursor is on (Ctrl+Enter). It uses the same
//! dialect splitter that runs scripts, so what is highlighted is exactly what runs.

use spelaion_driver_api::{Dialect, StatementSpan};

/// The statement at byte `offset` of `script`. Between statements, the cursor belongs to
/// the previous one while it stays on that statement's last line (just after its `;`);
/// on a later line it belongs to the next one, if there is a next one.
pub fn statement_at(dialect: &dyn Dialect, script: &str, offset: usize) -> Option<StatementSpan> {
    let spans = dialect.split_statements(script);
    let offset = offset.min(script.len());
    let next = spans.iter().position(|s| s.end >= offset);
    match next {
        None => spans.last().copied(),
        Some(i) => {
            let candidate = spans[i];
            if candidate.start <= offset || i == 0 {
                return Some(candidate);
            }
            let previous = spans[i - 1];
            let gap = script.get(previous.end..offset).unwrap_or("");
            Some(if gap.contains('\n') {
                candidate
            } else {
                previous
            })
        }
    }
}

/// What the editor's Run will send, for its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    /// The statement under the cursor.
    Statement,
    /// The selected text.
    Selection,
    /// The whole script: there is no dialect to find a statement with.
    Script,
}

/// What Run will send and the statements in it, as the engine's splitter reads that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTarget {
    pub kind: RunKind,
    /// The text Run sends, in bytes of the script; `None` when there is nothing to run.
    pub span: Option<StatementSpan>,
    /// The statements of that text, in bytes of the script. A selection is split as the run
    /// will split it: alone, whatever the script holds around it.
    pub statements: Vec<StatementSpan>,
}

/// What Run will send for a cursor at byte `cursor` and a selection (`None` or empty: none).
/// A selection wins over the cursor; without a dialect (a tab with no connection) only the
/// whole script can be sent.
pub fn run_target(
    dialect: Option<&dyn Dialect>,
    script: &str,
    cursor: usize,
    selection: Option<std::ops::Range<usize>>,
) -> RunTarget {
    let selection = selection
        .map(|s| floor_boundary(script, s.start)..floor_boundary(script, s.end))
        .filter(|s| s.start < s.end);
    if let Some(selection) = selection {
        let statements = dialect
            .map(|d| d.split_statements(&script[selection.clone()]))
            .unwrap_or_default()
            .into_iter()
            .map(|s| StatementSpan {
                start: s.start + selection.start,
                end: s.end + selection.start,
            })
            .collect();
        return RunTarget {
            kind: RunKind::Selection,
            span: Some(StatementSpan {
                start: selection.start,
                end: selection.end,
            }),
            statements,
        };
    }
    let Some(dialect) = dialect else {
        return RunTarget {
            kind: RunKind::Script,
            span: (!script.trim().is_empty()).then_some(StatementSpan {
                start: 0,
                end: script.len(),
            }),
            statements: Vec::new(),
        };
    };
    let span = statement_at(dialect, script, cursor);
    RunTarget {
        kind: RunKind::Statement,
        span,
        statements: span.into_iter().collect(),
    }
}

/// Some statements of a script, as text that runs exactly them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedText {
    /// The text to run. Every character of the script it covers is still at its place (a
    /// statement left out is written over with spaces), so a position in it is one in the
    /// script.
    pub text: String,
    /// Where `text` starts in the script, in bytes.
    pub start: usize,
    /// Where it ends in the script, in bytes.
    pub end: usize,
    /// How many statements it runs.
    pub statements: usize,
}

/// Why some statements of a script cannot be run on their own.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunPickError {
    #[error("no statement was chosen")]
    Nothing,
    #[error("the script has no statement {0}")]
    NoStatement(usize),
    #[error("these statements cannot be run apart from the rest of the script")]
    Inseparable,
}

/// The statements `picked` (positions in `dialect.split_statements(script)`, in any order;
/// they run in the script's order, each once) as text to run.
///
/// The text is a piece of the script with the statements left out written over with spaces
/// (line breaks kept), so what separates statements stays where it was: a `GO` line, a `/`
/// after a PL/SQL unit, MySQL's `DELIMITER` lines. It is only returned if the engine's
/// splitter reads in it exactly the statements chosen; otherwise nothing may run.
pub fn pick_statements(
    dialect: &dyn Dialect,
    script: &str,
    picked: &[usize],
) -> Result<PickedText, RunPickError> {
    let spans = dialect.split_statements(script);
    let mut keep = vec![false; spans.len()];
    for &index in picked {
        *keep
            .get_mut(index)
            .ok_or(RunPickError::NoStatement(index))? = true;
    }
    let kept: Vec<StatementSpan> = spans
        .iter()
        .zip(&keep)
        .filter_map(|(span, keep)| keep.then_some(*span))
        .collect();
    let (Some(first), Some(last)) = (kept.first(), kept.last()) else {
        return Err(RunPickError::Nothing);
    };
    let wanted: Vec<&str> = kept.iter().map(|s| s.text(script)).collect();
    // From the first statement chosen to the last one (with the data that follows it, a
    // `COPY … FROM STDIN`'s); then the whole script, which also keeps what the splitter reads
    // before them (a `DELIMITER` line) and after them.
    for (start, end) in [
        (first.start, extent(dialect, script, last)),
        (0, script.len()),
    ] {
        let mut text = String::with_capacity(end - start);
        let mut at = start;
        for (span, keep) in spans.iter().zip(&keep) {
            if span.end <= start || span.start >= end {
                continue;
            }
            text.push_str(&script[at..span.start]);
            if *keep {
                // Its data, if any, is in what follows it and is copied as it is.
                text.push_str(span.text(script));
                at = span.end;
            } else {
                // A statement left out goes with its data (SDB-162): left behind, the data
                // would be read as SQL.
                let until = extent(dialect, script, span).min(end);
                blank(&script[span.start..until], &mut text);
                at = until;
            }
        }
        text.push_str(&script[at..end]);
        let read = dialect.split_statements(&text);
        if read
            .iter()
            .map(|s| s.text(&text))
            .eq(wanted.iter().copied())
        {
            return Ok(PickedText {
                text,
                start,
                end,
                statements: kept.len(),
            });
        }
    }
    Err(RunPickError::Inseparable)
}

/// Where the statement at `span` ends with what belongs to it: the data of a `COPY … FROM
/// STDIN` and the `\.` line that ends it (SDB-162), else the statement itself.
fn extent(dialect: &dyn Dialect, script: &str, span: &StatementSpan) -> usize {
    match dialect.copy_data(script, span) {
        None => span.end,
        Some(data) => script[data.end..]
            .find('\n')
            .map_or(script.len(), |p| data.end + p + 1),
    }
}

/// The statement at byte `offset` (`statement_at`'s) as text to run, for Ctrl+Enter, with the
/// statement's own place in the script; `None` when there is no statement there. The text is
/// what `pick_statements` writes: the statement alone when the engine's splitter reads it alone
/// as that one statement, otherwise the whole script with the other statements blanked, so a
/// MySQL `DELIMITER` line before it still applies (SDB-403: a procedure body sent alone was
/// split at its `;`s and run as its pieces).
pub fn statement_to_run(
    dialect: &dyn Dialect,
    script: &str,
    offset: usize,
) -> Result<Option<(PickedText, StatementSpan)>, RunPickError> {
    let Some(under) = statement_at(dialect, script, offset) else {
        return Ok(None);
    };
    let spans = dialect.split_statements(script);
    let index = spans
        .iter()
        .position(|s| *s == under)
        .ok_or(RunPickError::Inseparable)?;
    pick_statements(dialect, script, &[index]).map(|picked| Some((picked, under)))
}

/// From the statement at byte `offset` (`statement_at`'s) to the script's last one, as text to
/// run; `None` when the script has no statement.
pub fn from_cursor(
    dialect: &dyn Dialect,
    script: &str,
    offset: usize,
) -> Result<Option<PickedText>, RunPickError> {
    let Some(under) = statement_at(dialect, script, offset) else {
        return Ok(None);
    };
    let spans = dialect.split_statements(script);
    let first = spans.iter().position(|s| *s == under).unwrap_or(0);
    let picked: Vec<usize> = (first..spans.len()).collect();
    pick_statements(dialect, script, &picked).map(Some)
}

/// Most statements an outline lists: a pasted dump has hundreds of thousands, and nobody reads
/// a panel that long.
pub const MAX_OUTLINE_ENTRIES: usize = 2000;

/// Most characters of a statement an outline shows.
pub const OUTLINE_SUMMARY_CHARS: usize = 80;

/// One statement of a script, for the editor's outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    /// The statement, in bytes of the script.
    pub span: StatementSpan,
    /// The line it starts on, from 1.
    pub line: usize,
    /// How it starts, on one line: from its first character outside comments, its white space
    /// collapsed, at most [`OUTLINE_SUMMARY_CHARS`] characters.
    pub summary: String,
}

/// The statements of a script, as the engine's splitter reads it (the one that runs it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outline {
    /// The first [`MAX_OUTLINE_ENTRIES`] statements, in the script's order.
    pub entries: Vec<OutlineEntry>,
    /// How many statements the script has.
    pub total: usize,
}

/// The outline of `script`: one entry per statement the engine's splitter finds in it.
pub fn outline(dialect: &dyn Dialect, script: &str) -> Outline {
    let spans = dialect.split_statements(script);
    let total = spans.len();
    let code = dialect.code_ranges(script);
    let mut entries = Vec::with_capacity(total.min(MAX_OUTLINE_ENTRIES));
    // Lines and code ranges are walked once, in step with the statements.
    let (mut line, mut counted, mut range) = (1, 0, 0);
    for span in spans.into_iter().take(MAX_OUTLINE_ENTRIES) {
        let start = span.start.min(script.len());
        let end = span.end.min(script.len()).max(start);
        line += script.as_bytes()[counted.min(start)..start]
            .iter()
            .filter(|b| **b == b'\n')
            .count();
        counted = counted.max(start);
        while range < code.len() && code[range].end <= start {
            range += 1;
        }
        // The first character of the statement that is code and not white space; a statement
        // that is only a comment (MySQL's executable comments) is shown from its start.
        let mut from = start;
        'found: for r in &code[range..] {
            if r.start >= end {
                break;
            }
            let (a, b) = (r.start.max(start), r.end.min(end));
            let Some(text) = script.get(a..b) else {
                continue;
            };
            if let Some((i, _)) = text.char_indices().find(|(_, c)| !c.is_whitespace()) {
                from = a + i;
                break 'found;
            }
        }
        let mut summary = String::new();
        let mut chars = 0;
        let mut space = false;
        let mut cut = false;
        for c in script.get(from..end).unwrap_or("").chars() {
            if c.is_whitespace() {
                space = !summary.is_empty();
                continue;
            }
            if chars + usize::from(space) >= OUTLINE_SUMMARY_CHARS {
                cut = true;
                break;
            }
            if space {
                summary.push(' ');
                chars += 1;
                space = false;
            }
            summary.push(c);
            chars += 1;
        }
        if cut {
            summary.push('…');
        }
        entries.push(OutlineEntry {
            span,
            line,
            summary,
        });
    }
    Outline { entries, total }
}

/// The statement after (`forward`) or before the one the cursor at byte `offset` is in or
/// next to, for "next / previous statement"; `None` at the script's last or first one.
pub fn neighbour_statement(
    dialect: &dyn Dialect,
    script: &str,
    offset: usize,
    forward: bool,
) -> Option<StatementSpan> {
    let spans = dialect.split_statements(script);
    let offset = offset.min(script.len());
    if forward {
        return spans.into_iter().find(|s| s.start > offset);
    }
    // Before the statement the cursor is inside (or at the end of); from between two
    // statements, the one before the cursor.
    let limit = spans
        .iter()
        .find(|s| s.start <= offset && offset <= s.end)
        .map_or(offset, |s| s.start);
    spans.into_iter().rev().find(|s| s.start < limit)
}

/// Writes spaces over `text`, keeping its line breaks. A character takes as many spaces as
/// it has UTF-16 units, so the editor's positions after it do not move.
fn blank(text: &str, into: &mut String) {
    for c in text.chars() {
        if c == '\n' || c == '\r' {
            into.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                into.push(' ');
            }
        }
    }
}

/// `offset` moved back to a character boundary of `text`, and never past its end.
fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_dialect::SqliteDialect;

    fn at(script: &str, marker: &str) -> Option<String> {
        let offset = script.find(marker).expect("marker");
        let script = script.replacen(marker, "", 1);
        statement_at(&SqliteDialect, &script, offset).map(|s| s.text(&script).to_string())
    }

    #[test]
    fn picks_the_statement_under_the_cursor() {
        let script = "select 1;\nselect 'a;|b';\n\n-- third\nselect 3";
        assert_eq!(at(script, "|").as_deref(), Some("select 'a;b'"));
        assert_eq!(at("se|lect 1;\nselect 2", "|").as_deref(), Some("select 1"));
        assert_eq!(at("select 1;\nselect 2|", "|").as_deref(), Some("select 2"));
        assert_eq!(
            at("select 1;\n-- third\nselect |3", "|").as_deref(),
            Some("-- third\nselect 3")
        );
    }

    #[test]
    fn between_statements_follows_the_line() {
        // Just after the `;`: still the statement that ends there.
        assert_eq!(
            at("select 1;|  \nselect 2", "|").as_deref(),
            Some("select 1")
        );
        // On a blank line before another statement: the next one.
        assert_eq!(
            at("select 1;\n|\nselect 2", "|").as_deref(),
            Some("select 2")
        );
        // After the last statement: the last one.
        assert_eq!(
            at("select 1;\nselect 2;\n\n|", "|").as_deref(),
            Some("select 2")
        );
        // Before the first: the first.
        assert_eq!(at("\n|\n  select 1;", "|").as_deref(), Some("select 1"));
    }

    #[test]
    fn run_says_what_it_will_send() {
        let script = "select 1;\nselect 2;\nselect 3";
        // No selection: the statement under the cursor.
        let target = run_target(Some(&SqliteDialect), script, 12, None);
        assert_eq!(target.kind, RunKind::Statement);
        assert_eq!(target.span.map(|s| s.text(script)), Some("select 2"));
        assert_eq!(target.statements.len(), 1);
        // An empty selection is no selection.
        let target = run_target(Some(&SqliteDialect), script, 12, Some(12..12));
        assert_eq!(target.kind, RunKind::Statement);
        // A selection wins, and is split alone: half a statement is a statement of it.
        let target = run_target(Some(&SqliteDialect), script, 0, Some(3..script.len()));
        assert_eq!(target.kind, RunKind::Selection);
        assert_eq!(
            target.span.map(|s| s.text(script)),
            Some("ect 1;\nselect 2;\nselect 3")
        );
        let texts: Vec<&str> = target.statements.iter().map(|s| s.text(script)).collect();
        assert_eq!(texts, ["ect 1", "select 2", "select 3"]);
        // A selection with nothing to run still is what Run would send.
        let target = run_target(Some(&SqliteDialect), script, 0, Some(9..10));
        assert_eq!(target.kind, RunKind::Selection);
        assert!(target.statements.is_empty());
        // Without a dialect there is no statement to find: the script.
        let target = run_target(None, script, 12, None);
        assert_eq!(target.kind, RunKind::Script);
        assert_eq!(target.span.map(|s| s.text(script)), Some(script));
        assert_eq!(run_target(None, " \n", 0, None).span, None);
        let target = run_target(None, script, 0, Some(0..8));
        assert_eq!(target.kind, RunKind::Selection);
        assert!(target.statements.is_empty());
        // Nothing under the cursor.
        let target = run_target(Some(&SqliteDialect), "-- nothing", 0, None);
        assert_eq!((target.kind, target.span), (RunKind::Statement, None));
    }

    #[test]
    fn a_selection_out_of_place_is_brought_back_into_the_script() {
        let script = "select 'é';select 2";
        let inside = script.find('é').unwrap() + 1;
        // Past the end, and in the middle of a character: never a panic.
        let target = run_target(Some(&SqliteDialect), script, 0, Some(inside..999));
        assert_eq!(
            target.span.map(|s| s.text(script)),
            Some("é';select 2"),
            "{target:?}"
        );
        assert_eq!(
            run_target(Some(&SqliteDialect), script, 999, Some(500..999)).kind,
            RunKind::Statement
        );
    }

    #[test]
    fn picks_statements_and_leaves_the_rest_blank() {
        let script = "select 1;\nselect 'é;';\n-- c\nselect 3;\nselect 4;";
        let picked = pick_statements(&SqliteDialect, script, &[2, 0]).unwrap();
        assert_eq!(picked.statements, 2);
        assert_eq!(
            (picked.start, picked.end),
            (0, script.find(";\nselect 4").unwrap())
        );
        // `select 'é;'` is eleven characters in the editor: eleven spaces.
        assert_eq!(
            picked.text,
            format!("select 1;\n{};\n-- c\nselect 3", " ".repeat(11))
        );
        // One statement is its own text.
        let one = pick_statements(&SqliteDialect, script, &[1, 1]).unwrap();
        assert_eq!(one.text, "select 'é;'");
        assert_eq!(one.statements, 1);
        assert_eq!(one.start, 10);
        assert_eq!(
            pick_statements(&SqliteDialect, script, &[]),
            Err(RunPickError::Nothing)
        );
        assert_eq!(
            pick_statements(&SqliteDialect, script, &[0, 4]),
            Err(RunPickError::NoStatement(4))
        );
    }

    #[test]
    fn from_the_cursor_to_the_end() {
        let script = "select 1;\nselect 2;\n\nselect 3;\n";
        let run = |marker: usize| {
            from_cursor(&SqliteDialect, script, marker)
                .unwrap()
                .unwrap()
        };
        // In the second statement, and just after its `;`.
        for offset in [13, 19] {
            let picked = run(offset);
            assert_eq!(picked.text, "select 2;\n\nselect 3");
            assert_eq!((picked.start, picked.statements), (10, 2));
        }
        // On the blank line: from the next one.
        assert_eq!(run(20).text, "select 3");
        // In the first: everything; after the last: the last.
        assert_eq!(run(0).statements, 3);
        assert_eq!(run(script.len()).text, "select 3");
        assert_eq!(from_cursor(&SqliteDialect, "-- nothing", 3), Ok(None));
    }

    /// A splitter whose reading of a statement depends on text far from it.
    struct Moody;

    impl Dialect for Moody {
        fn name(&self) -> &'static str {
            "moody"
        }
        fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
            let separator = if script.contains("raw") { '\n' } else { ';' };
            let mut spans = Vec::new();
            let mut start = 0;
            for piece in script.split(separator) {
                let trimmed = piece.trim();
                if !trimmed.is_empty() {
                    let at = start + (piece.len() - piece.trim_start().len());
                    spans.push(StatementSpan {
                        start: at,
                        end: at + trimmed.len(),
                    });
                }
                start += piece.len() + separator.len_utf8();
            }
            spans
        }
        fn quote_identifier(&self, ident: &str) -> String {
            ident.into()
        }
    }

    #[test]
    fn statements_that_read_otherwise_alone_are_not_run() {
        let script = "raw;a\nb;c";
        let texts: Vec<&str> = Moody
            .split_statements(script)
            .iter()
            .map(|s| s.text(script))
            .collect();
        assert_eq!(texts, ["raw;a", "b;c"]);
        // Alone, `b;c` would run as `b` and `c`: refused rather than run as something else.
        assert_eq!(
            pick_statements(&Moody, script, &[1]),
            Err(RunPickError::Inseparable)
        );
        assert_eq!(
            from_cursor(&Moody, script, 7),
            Err(RunPickError::Inseparable)
        );
        assert_eq!(
            pick_statements(&Moody, script, &[0, 1]).unwrap().text,
            script
        );
    }

    #[test]
    fn the_outline_lists_each_statement_with_its_line_and_how_it_starts() {
        let script = "-- setup\ncreate table t (\n  id int\n);\n\n/* two */ insert   into t\nvalues (';');\nselect 'ñ' ;";
        let o = outline(&SqliteDialect, script);
        assert_eq!(o.total, 3);
        let shown: Vec<(usize, &str)> = o
            .entries
            .iter()
            .map(|e| (e.line, e.summary.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                (1, "create table t ( id int )"),
                (6, "insert into t values (';')"),
                (8, "select 'ñ'"),
            ]
        );
        // Each entry is the splitter's statement: what "run" would send.
        let spans: Vec<StatementSpan> = o.entries.iter().map(|e| e.span).collect();
        assert_eq!(spans, SqliteDialect.split_statements(script));
        // A long statement is cut, by characters.
        let long = format!("select '{}'", "ñ".repeat(200));
        let o = outline(&SqliteDialect, &long);
        assert_eq!(
            o.entries[0].summary.chars().count(),
            OUTLINE_SUMMARY_CHARS + 1
        );
        assert!(o.entries[0].summary.ends_with('…'));
        assert!(outline(&SqliteDialect, "  -- nothing\n").entries.is_empty());
    }

    #[test]
    fn the_outline_follows_each_engines_splitter_and_is_bounded() {
        // T-SQL: `GO` separates batches and is not a statement.
        let script = "select 1\nGO\nselect 2\nGO\n";
        let o = outline(&spelaion_dialect::SqlServerDialect, script);
        let shown: Vec<(usize, &str)> = o
            .entries
            .iter()
            .map(|e| (e.line, e.summary.as_str()))
            .collect();
        assert_eq!(shown, [(1, "select 1"), (3, "select 2")]);
        // PostgreSQL: a `;` inside a dollar-quoted body does not end the statement.
        let script = "do $$ begin perform 1; end $$;\nselect 2";
        let o = outline(&spelaion_dialect::PostgresDialect, script);
        assert_eq!(o.total, 2);
        assert_eq!(o.entries[1].line, 2);
        // A dump: every statement is counted, only the first ones are listed.
        let dump = "select 1;\n".repeat(MAX_OUTLINE_ENTRIES + 5);
        let o = outline(&SqliteDialect, &dump);
        assert_eq!(o.total, MAX_OUTLINE_ENTRIES + 5);
        assert_eq!(o.entries.len(), MAX_OUTLINE_ENTRIES);
        assert_eq!(o.entries.last().unwrap().line, MAX_OUTLINE_ENTRIES);
    }

    #[test]
    fn next_and_previous_statement_from_the_cursor() {
        let script = "select 1;\n\nselect 2;\nselect 3";
        let go = |marker_at: usize, forward| {
            neighbour_statement(&SqliteDialect, script, marker_at, forward)
                .map(|s| s.text(script).to_string())
        };
        let second = script.find("select 2").unwrap();
        // From inside the first statement.
        assert_eq!(go(3, true).as_deref(), Some("select 2"));
        assert_eq!(go(3, false), None);
        // From the blank line between two statements.
        assert_eq!(go(10, true).as_deref(), Some("select 2"));
        assert_eq!(go(10, false).as_deref(), Some("select 1"));
        // From the start, the middle and the end of the second one.
        for offset in [second, second + 4, second + 8] {
            assert_eq!(go(offset, true).as_deref(), Some("select 3"), "{offset}");
            assert_eq!(go(offset, false).as_deref(), Some("select 1"), "{offset}");
        }
        // From the last one, and past the end of the script.
        assert_eq!(go(script.len(), true), None);
        assert_eq!(go(9999, false).as_deref(), Some("select 2"));
        assert_eq!(neighbour_statement(&SqliteDialect, "", 0, true), None);
    }

    #[test]
    fn nothing_to_run() {
        assert_eq!(at("|", "|"), None);
        assert_eq!(at("  -- only a comment|\n", "|"), None);
        assert_eq!(
            statement_at(&SqliteDialect, "select 1", 999).map(|s| s.end),
            Some(8)
        );
    }
}
