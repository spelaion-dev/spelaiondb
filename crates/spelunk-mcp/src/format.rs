//! Compact text responses for the agent: Markdown tables with size limits.

use spelunk_core::ReadResult;
use spelunk_driver_api::Value;

/// Maximum characters per cell; the rest is cut with `…`.
const MAX_CELL: usize = 200;
/// Approximate maximum size of a response: the agent's context is finite.
const MAX_RESPONSE: usize = 60_000;

fn cell(value: &Value) -> String {
    let text = value
        .to_string()
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ");
    if text.chars().count() > MAX_CELL {
        format!("{}…", text.chars().take(MAX_CELL).collect::<String>())
    } else {
        text
    }
}

/// Markdown table of a result, with a notice if it was cut.
pub fn result_table(result: &ReadResult) -> String {
    if result.columns.is_empty() {
        return format!("No columns ({} ms).", result.elapsed.as_millis());
    }
    let mut out = String::new();
    let names: Vec<String> = result
        .columns
        .iter()
        .map(|c| format!("{} ({})", c.name, c.native_type))
        .collect();
    out.push_str(&format!("| {} |\n", names.join(" | ")));
    out.push_str(&format!("|{}\n", "---|".repeat(names.len())));
    let mut shown = 0;
    for row in &result.rows {
        let line = format!(
            "| {} |\n",
            row.iter().map(cell).collect::<Vec<_>>().join(" | ")
        );
        if out.len() + line.len() > MAX_RESPONSE {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    out.push('\n');
    if shown < result.rows.len() || result.truncated {
        out.push_str(&format!(
            "Showing {shown} rows; there are more. Narrow the query (WHERE, LIMIT, specific columns) if you need others.\n"
        ));
    } else {
        out.push_str(&format!(
            "{shown} {}.\n",
            if shown == 1 { "row" } else { "rows" }
        ));
    }
    out.push_str(&format!("({} ms)", result.elapsed.as_millis()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelunk_core::ReadKind;
    use spelunk_driver_api::{ColumnMeta, LogicalType};
    use std::time::Duration;

    fn result(rows: Vec<Vec<Value>>, truncated: bool) -> ReadResult {
        ReadResult {
            columns: vec![
                ColumnMeta {
                    name: "id".into(),
                    logical: LogicalType::Int,
                    native_type: "int4".into(),
                    native_id: None,
                },
                ColumnMeta {
                    name: "note".into(),
                    logical: LogicalType::Text,
                    native_type: "text".into(),
                    native_id: None,
                },
            ],
            rows,
            truncated,
            kind: ReadKind::Query,
            elapsed: Duration::from_millis(7),
        }
    }

    #[test]
    fn renders_markdown_with_escaping_and_limits() {
        let long = "x".repeat(500);
        let text = result_table(&result(
            vec![
                vec![Value::Int(1), Value::Text("a|b\nc".into())],
                vec![Value::Int(2), Value::Text(long)],
                vec![Value::Null, Value::Null],
            ],
            false,
        ));
        assert!(
            text.starts_with("| id (int4) | note (text) |\n|---|---|\n"),
            "{text}"
        );
        assert!(text.contains("| 1 | a\\|b c |"), "{text}");
        assert!(text.contains(&format!("{}…", "x".repeat(200))));
        assert!(text.contains("| NULL | NULL |"));
        assert!(text.contains("3 rows."));
    }

    #[test]
    fn warns_when_truncated() {
        let text = result_table(&result(vec![vec![Value::Int(1), Value::Null]], true));
        assert!(text.contains("there are more"), "{text}");
    }
}
