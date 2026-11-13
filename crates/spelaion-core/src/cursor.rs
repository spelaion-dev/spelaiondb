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
    fn nothing_to_run() {
        assert_eq!(at("|", "|"), None);
        assert_eq!(at("  -- only a comment|\n", "|"), None);
        assert_eq!(
            statement_at(&SqliteDialect, "select 1", 999).map(|s| s.end),
            Some(8)
        );
    }
}
