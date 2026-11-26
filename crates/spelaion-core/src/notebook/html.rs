//! The notebook's HTML export (ADR-087, point 17): one self-contained file with the Markdown
//! cells drawn from [`super::markdown`]'s blocks, each SQL cell's text and its results as
//! tables of at most [`MAX_EXPORT_ROWS`] rows read from the stored results (so a masked column
//! stays masked), and a shown chart as a `data:` image. Every text is escaped; the file has no
//! script, loads nothing and declares a policy that forbids both.

use std::path::Path;

use base64::Engine as _;
use spelaion_driver_api::Value;

use super::markdown::{self, Align, Block, Inline};
use super::{Cell, Notebook, NotebookError};
use crate::chart::{MAX_CHART_IMAGE_BYTES, is_plain_svg};
use crate::export::{is_numeric, text};
use crate::results::StoredResult;
use crate::sql_file::replace_atomically;

/// Most rows of one result written into the export.
pub const MAX_EXPORT_ROWS: usize = 1_000;
/// The policy the file declares: nothing loads and nothing runs.
const POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:";

/// A result as the export writes it: its columns and the first rows as the grid's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultTable {
    pub columns: Vec<String>,
    /// Numbers are aligned right, as in the grid.
    pub numeric: Vec<bool>,
    /// `None` is NULL.
    pub rows: Vec<Vec<Option<String>>>,
    /// The rows the result holds (more than `rows` past [`MAX_EXPORT_ROWS`]).
    pub total_rows: usize,
    /// The chart the page shows for it, as SVG (checked when the export is written).
    pub chart_svg: Option<String>,
}

/// The first `max_rows` rows of `result`'s current view (its sort and filters, as the grid
/// shows it), as text.
pub fn result_table(result: &StoredResult, max_rows: usize) -> ResultTable {
    let schema = result.schema();
    let total_rows = result.rows();
    let mut rows = Vec::new();
    let wanted = total_rows.min(max_rows);
    let mut start = 0;
    while start < wanted {
        let len = (wanted - start).min(500);
        for batch in result.window(start, len) {
            for row in 0..batch.num_rows() {
                rows.push(
                    schema
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(c, meta)| {
                            text(
                                &Value::from_array(batch.column(c).as_ref(), row, meta.logical)
                                    .unwrap_or(Value::Null),
                            )
                        })
                        .collect(),
                );
            }
        }
        start += len;
    }
    ResultTable {
        columns: schema.columns.iter().map(|c| c.name.clone()).collect(),
        numeric: schema
            .columns
            .iter()
            .map(|c| is_numeric(c.logical))
            .collect(),
        rows,
        total_rows,
        chart_svg: None,
    }
}

/// What a SQL cell showed when the notebook was exported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CellOutput {
    /// Not run since the notebook was opened.
    #[default]
    NotRun,
    /// It ran: its messages (rows affected, notices) and its results.
    Ran {
        messages: Vec<String>,
        results: Vec<ResultTable>,
    },
    /// It failed with this error.
    Failed { error: String },
}

/// What the export says around the cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlExport {
    pub title: String,
    /// The connection's name as the app shows it.
    pub connection: Option<String>,
    /// When it was written, as the reader should see it.
    pub written_at: String,
    /// One per cell of the notebook, in order; Markdown cells' entries are not read.
    pub outputs: Vec<CellOutput>,
}

/// `text` escaped for HTML text and attribute values.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

const STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;max-width:1100px;margin:2rem auto;\
padding:0 1rem;color:#1f2328;background:#fff}header{border-bottom:1px solid #d0d7de;\
margin-bottom:1.5rem}header p{color:#59636e;margin:.25rem 0 1rem}\
section{margin:1.25rem 0}.sql{border:1px solid #d0d7de;border-radius:6px;overflow:hidden}\
.sql h2{font-size:13px;margin:0;padding:.4rem .75rem;background:#f6f8fa;\
border-bottom:1px solid #d0d7de;font-family:ui-monospace,monospace}\
pre{margin:0;padding:.75rem;overflow:auto;font:13px/1.45 ui-monospace,monospace;\
background:#f6f8fa}.sql pre{background:#fff}.out{padding:.5rem .75rem;border-top:1px solid #d0d7de}\
.note{color:#59636e;font-size:13px}.error{color:#b42318;white-space:pre-wrap}\
.scroll{overflow:auto;max-height:480px}table{border-collapse:collapse;font-size:13px}\
th,td{border:1px solid #d0d7de;padding:2px 8px;text-align:left;vertical-align:top;\
white-space:pre}th{background:#f6f8fa}td.n{text-align:right}td.null{color:#8c959f}\
.md table td,.md table th{white-space:normal}blockquote{margin:0;padding-left:1em;\
border-left:3px solid #d0d7de;color:#59636e}code{font-family:ui-monospace,monospace;\
font-size:90%;background:#f6f8fa;padding:0 .2em;border-radius:3px}pre code{background:none;\
padding:0}.addr{color:#59636e}img.chart{display:block;max-width:100%;margin:.5rem 0}";

/// The export's HTML. A chart that is not a plain SVG, or too large, is refused rather than
/// left out.
pub fn notebook_html(notebook: &Notebook, export: &HtmlExport) -> Result<String, NotebookError> {
    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(&format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{POLICY}\">\n"
    ));
    out.push_str("<meta name=\"generator\" content=\"SpelaionDB\">\n");
    out.push_str(&format!("<title>{}</title>\n", escape(&export.title)));
    out.push_str(&format!(
        "<style>{STYLE}</style>\n</head>\n<body>\n<header>\n"
    ));
    out.push_str(&format!("<h1>{}</h1>\n<p>", escape(&export.title)));
    if let Some(connection) = &export.connection {
        out.push_str(&format!("Connection: {} · ", escape(connection)));
    }
    out.push_str(&format!(
        "Exported {} · Results as they were when each cell last ran, at most {MAX_EXPORT_ROWS} rows each.</p>\n</header>\n",
        escape(&export.written_at)
    ));
    for (index, cell) in notebook.cells.iter().enumerate() {
        match cell {
            Cell::Markdown { text } => {
                out.push_str("<section class=\"md\">\n");
                write_blocks(&markdown::read(text), &mut out);
                out.push_str("</section>\n");
            }
            Cell::Sql { name, sql } => {
                out.push_str("<section class=\"sql\">\n");
                out.push_str(&format!("<h2>{}</h2>\n", escape(name)));
                out.push_str(&format!("<pre><code>{}</code></pre>\n", escape(sql)));
                let output = export.outputs.get(index).cloned().unwrap_or_default();
                write_output(&output, &mut out)?;
                out.push_str("</section>\n");
            }
        }
    }
    out.push_str("</body>\n</html>\n");
    Ok(out)
}

fn write_output(output: &CellOutput, out: &mut String) -> Result<(), NotebookError> {
    out.push_str("<div class=\"out\">\n");
    match output {
        CellOutput::NotRun => out.push_str("<p class=\"note\">Not run.</p>\n"),
        CellOutput::Failed { error } => {
            out.push_str(&format!("<p class=\"error\">{}</p>\n", escape(error)));
        }
        CellOutput::Ran { messages, results } => {
            for message in messages {
                out.push_str(&format!("<p class=\"note\">{}</p>\n", escape(message)));
            }
            for result in results {
                write_result(result, out)?;
            }
            if messages.is_empty() && results.is_empty() {
                out.push_str("<p class=\"note\">Ran, with nothing to show.</p>\n");
            }
        }
    }
    out.push_str("</div>\n");
    Ok(())
}

fn write_result(result: &ResultTable, out: &mut String) -> Result<(), NotebookError> {
    if let Some(svg) = &result.chart_svg {
        if svg.len() > MAX_CHART_IMAGE_BYTES || !is_plain_svg(svg) {
            return Err(NotebookError::Image(
                "a chart is not an SVG image the export can embed".into(),
            ));
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(svg.as_bytes());
        out.push_str(&format!(
            "<img class=\"chart\" alt=\"Chart\" src=\"data:image/svg+xml;base64,{encoded}\">\n"
        ));
    }
    out.push_str("<div class=\"scroll\"><table>\n<thead><tr>");
    for column in &result.columns {
        out.push_str(&format!("<th>{}</th>", escape(column)));
    }
    out.push_str("</tr></thead>\n<tbody>\n");
    for row in &result.rows {
        out.push_str("<tr>");
        for (c, value) in row.iter().enumerate() {
            let numeric = result.numeric.get(c).copied().unwrap_or(false);
            match value {
                None => out.push_str("<td class=\"null\">NULL</td>"),
                Some(v) if numeric => out.push_str(&format!("<td class=\"n\">{}</td>", escape(v))),
                Some(v) => out.push_str(&format!("<td>{}</td>", escape(v))),
            }
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody></table></div>\n");
    let shown = result.rows.len();
    let rows = |n: usize| {
        if n == 1 {
            "1 row".to_string()
        } else {
            format!("{n} rows")
        }
    };
    if shown < result.total_rows {
        out.push_str(&format!(
            "<p class=\"note\">The first {} of {}.</p>\n",
            rows(shown),
            rows(result.total_rows)
        ));
    } else {
        out.push_str(&format!("<p class=\"note\">{}.</p>\n", rows(shown)));
    }
    Ok(())
}

fn write_blocks(blocks: &[Block], out: &mut String) {
    for block in blocks {
        match block {
            Block::Heading { level, content } => {
                let level = (*level).clamp(1, 6);
                out.push_str(&format!("<h{level}>"));
                write_inlines(content, out);
                out.push_str(&format!("</h{level}>\n"));
            }
            Block::Paragraph { content } => {
                out.push_str("<p>");
                write_inlines(content, out);
                out.push_str("</p>\n");
            }
            Block::Quote { blocks } => {
                out.push_str("<blockquote>\n");
                write_blocks(blocks, out);
                out.push_str("</blockquote>\n");
            }
            Block::List {
                ordered,
                start,
                items,
            } => {
                if *ordered {
                    out.push_str(&format!("<ol start=\"{start}\">\n"));
                } else {
                    out.push_str("<ul>\n");
                }
                for item in items {
                    out.push_str("<li>");
                    write_inlines(item, out);
                    out.push_str("</li>\n");
                }
                out.push_str(if *ordered { "</ol>\n" } else { "</ul>\n" });
            }
            Block::Code { text, .. } => {
                out.push_str(&format!("<pre><code>{}</code></pre>\n", escape(text)));
            }
            Block::Rule => out.push_str("<hr>\n"),
            Block::Table {
                align,
                header,
                rows,
            } => {
                let style = |c: usize| match align.get(c).copied().unwrap_or(Align::None) {
                    Align::None => "",
                    Align::Left => " style=\"text-align:left\"",
                    Align::Center => " style=\"text-align:center\"",
                    Align::Right => " style=\"text-align:right\"",
                };
                out.push_str("<table>\n<thead><tr>");
                for (c, cell) in header.iter().enumerate() {
                    out.push_str(&format!("<th{}>", style(c)));
                    write_inlines(cell, out);
                    out.push_str("</th>");
                }
                out.push_str("</tr></thead>\n<tbody>\n");
                for row in rows {
                    out.push_str("<tr>");
                    for (c, cell) in row.iter().enumerate() {
                        out.push_str(&format!("<td{}>", style(c)));
                        write_inlines(cell, out);
                        out.push_str("</td>");
                    }
                    out.push_str("</tr>\n");
                }
                out.push_str("</tbody></table>\n");
            }
        }
    }
}

fn write_inlines(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text { text } => out.push_str(&escape(text)),
            Inline::Strong { content } => {
                out.push_str("<strong>");
                write_inlines(content, out);
                out.push_str("</strong>");
            }
            Inline::Emphasis { content } => {
                out.push_str("<em>");
                write_inlines(content, out);
                out.push_str("</em>");
            }
            Inline::Code { text } => out.push_str(&format!("<code>{}</code>", escape(text))),
            // The address is shown, never linked: nothing in a notebook navigates.
            Inline::Link { content, url } => {
                write_inlines(content, out);
                out.push_str(&format!(" <span class=\"addr\">({})</span>", escape(url)));
            }
            Inline::Image { alt, url } => out.push_str(&format!(
                "[{}] <span class=\"addr\">({})</span>",
                escape(alt),
                escape(url)
            )),
            Inline::LineBreak => out.push_str("<br>"),
        }
    }
}

/// The local date and time as the export's header says it (`2026-11-26 10:00`).
pub fn now_text() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

/// Writes the export to `path` atomically.
pub fn write_notebook_html(path: &Path, html: &str) -> Result<(), NotebookError> {
    replace_atomically(path, html.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notebook() -> Notebook {
        Notebook {
            connection: None,
            cells: vec![
                Cell::Markdown {
                    text: "# Lost <b>customers</b>\n\nSee [docs](javascript:alert(1)) & **more**"
                        .into(),
                },
                Cell::Sql {
                    name: "lost".into(),
                    sql: "select '<script>' as x".into(),
                },
                Cell::Sql {
                    name: "later".into(),
                    sql: "select 2".into(),
                },
                Cell::Sql {
                    name: "bad".into(),
                    sql: "select nope".into(),
                },
            ],
        }
    }

    fn export(outputs: Vec<CellOutput>) -> HtmlExport {
        HtmlExport {
            title: "Lost <customers>".into(),
            connection: Some("prod/orders".into()),
            written_at: "2026-11-26 10:00".into(),
            outputs,
        }
    }

    fn table(chart: Option<&str>) -> ResultTable {
        ResultTable {
            columns: vec!["x".into(), "n".into()],
            numeric: vec![false, true],
            rows: vec![
                vec![Some("<script>".into()), Some("1".into())],
                vec![Some("••••••".into()), None],
            ],
            total_rows: 1_500,
            chart_svg: chart.map(str::to_string),
        }
    }

    #[test]
    fn the_export_escapes_everything_and_links_nothing() {
        let html = notebook_html(
            &notebook(),
            &export(vec![
                CellOutput::NotRun,
                CellOutput::Ran {
                    messages: vec!["2 rows".into()],
                    results: vec![table(Some(
                        "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>",
                    ))],
                },
                CellOutput::NotRun,
                CellOutput::Failed {
                    error: "column \"nope\" does not exist <here>".into(),
                },
            ]),
        )
        .unwrap();
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(!html.contains("href"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("<h1>Lost &lt;b&gt;customers&lt;/b&gt;</h1>"));
        assert!(html.contains("docs <span class=\"addr\">(javascript:alert(1))</span>"));
        assert!(html.contains("<td class=\"null\">NULL</td>"));
        assert!(html.contains("<td class=\"n\">1</td>"));
        assert!(html.contains("<td>••••••</td>"));
        assert!(html.contains("The first 2 rows of 1500 rows."));
        assert!(html.contains("Not run."));
        assert!(html.contains("column &quot;nope&quot; does not exist &lt;here&gt;"));
        assert!(html.contains("src=\"data:image/svg+xml;base64,"));
        assert!(html.contains(&format!("content=\"{POLICY}\"")));
    }

    #[test]
    fn a_chart_with_script_is_refused() {
        for svg in [
            "<svg><script>alert(1)</script></svg>",
            "<svg><foreignObject><div/></foreignObject></svg>",
            "<div>not svg</div>",
        ] {
            let result = notebook_html(
                &notebook(),
                &export(vec![
                    CellOutput::NotRun,
                    CellOutput::Ran {
                        messages: vec![],
                        results: vec![table(Some(svg))],
                    },
                ]),
            );
            assert!(matches!(result, Err(NotebookError::Image(_))), "{svg}");
        }
    }
}
