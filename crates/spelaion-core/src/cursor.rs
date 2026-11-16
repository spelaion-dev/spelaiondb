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
    // From the first statement chosen to the last one; then the whole script, which also
    // keeps what the splitter reads before them (a `DELIMITER` line) and after them.
    for (start, end) in [(first.start, last.end), (0, script.len())] {
        let mut text = String::with_capacity(end - start);
        let mut at = start;
        for (span, keep) in spans.iter().zip(&keep) {
            if span.end <= start || span.start >= end {
                continue;
            }
            text.push_str(&script[at..span.start]);
            if *keep {
                text.push_str(span.text(script));
            } else {
                blank(span.text(script), &mut text);
            }
            at = span.end;
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
    fn nothing_to_run() {
        assert_eq!(at("|", "|"), None);
        assert_eq!(at("  -- only a comment|\n", "|"), None);
        assert_eq!(
            statement_at(&SqliteDialect, "select 1", 999).map(|s| s.end),
            Some(8)
        );
    }
}
