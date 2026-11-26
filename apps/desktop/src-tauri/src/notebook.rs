//! SQL notebooks (ADR-087): a thin adapter over `spelaion-core`'s `notebook` module.
//!
//! A notebook file is read and written as a `.sql` file is (`files.rs`, ADR-036): its text is
//! what the core's format writes, so saving, the changed-on-disk check and kept tabs are the
//! editor's. These commands carry the rest: reading and writing that text, Markdown blocks,
//! expanding a cell's references, running a cell into its own result tabs, and the HTML export.

use serde::{Deserialize, Serialize};
use specta::Type;
use spelaion_core::notebook::html::{
    CellOutput, HtmlExport, MAX_EXPORT_ROWS, notebook_html, now_text, result_table,
    write_notebook_html,
};
use spelaion_core::notebook::markdown::{self, Block};
use spelaion_core::notebook::references::expand_cell;
use spelaion_core::notebook::{self, Notebook};
use spelaion_core::{ResultId, cell_results_key};
use spelaion_desktop_macros::command;
use std::path::Path;
use tauri::State;
use tauri::ipc::Channel;

use crate::dto::*;
use crate::{AppState, CmdResult};

/// Reads a notebook from its text (a file the page opened as a `.sql` file is, or a kept tab).
#[command]
pub(crate) async fn parse_notebook(text: String) -> CmdResult<Notebook> {
    notebook::parse_notebook(&text).map_err(|e| e.to_string())
}

/// The notebook's text, as its file holds it: what a save writes and a kept tab keeps.
#[command]
pub(crate) async fn notebook_text(notebook: Notebook) -> CmdResult<String> {
    notebook::notebook_text(&notebook).map_err(|e| e.to_string())
}

/// A Markdown cell's blocks, drawn by the page as text (ADR-087, point 18).
#[command]
pub(crate) async fn notebook_markdown(text: String) -> CmdResult<Vec<Block>> {
    Ok(markdown::read(&text))
}

/// The name a new SQL cell gets.
#[command]
pub(crate) async fn new_notebook_cell_name(notebook: Notebook) -> CmdResult<String> {
    Ok(notebook::new_cell_name(&notebook))
}

/// Whether the SQL cell at `index` may be called `name`.
#[command]
pub(crate) async fn check_notebook_cell_name(
    notebook: Notebook,
    index: u32,
    name: String,
) -> CmdResult<()> {
    notebook::check_cell_name(&notebook, index as usize, &name).map_err(|e| e.to_string())
}

/// A cell ready to run on the notebook tab's session: its references expanded into CTEs by the
/// session's own dialect (ADR-087, points 8 and 9).
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ExpandedCellItem {
    /// What runs, and what the guard and the history see.
    pub sql: String,
    /// The referenced cells, in the order their CTEs are written.
    pub references: Vec<String>,
}

#[command]
pub(crate) async fn expand_notebook_cell(
    session: String,
    notebook: Notebook,
    index: u32,
    state: State<'_, AppState>,
) -> CmdResult<ExpandedCellItem> {
    let open = state.session(&session).await?;
    let expanded = expand_cell(&notebook, index as usize, open.session.dialect().as_ref())
        .map_err(|e| e.to_string())?;
    Ok(ExpandedCellItem {
        sql: expanded.sql,
        references: expanded.references,
    })
}

/// Runs a cell's expanded SQL on the notebook tab's session, as the editor runs a script: the
/// same `check_run` (the read-only barrier, then the guard's confirmations), the history, the
/// tab's run options. Its results go to the cell's own result tabs (`cell_results_key`),
/// replacing the cell's previous ones.
#[command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_notebook_cell(
    session: String,
    cell: String,
    sql: String,
    confirmed: bool,
    options: RunOptionsItem,
    on_event: Channel<ScriptEvent>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let results = cell_results_key(&session, &cell);
    crate::start_script(
        &state,
        &session,
        sql,
        confirmed,
        options,
        ResultPlacementItem::Replace,
        on_event,
        None,
        Some(results),
    )
    .await
}

/// Frees one cell's results (the cell was deleted, or its results cleared).
#[command]
pub(crate) async fn release_notebook_cell(
    session: String,
    cell: String,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    state
        .result_tabs
        .close_editor(&cell_results_key(&session, &cell));
    Ok(())
}

/// A shown result of a cell, as the export reads it.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CellResultItem {
    pub result: u32,
    /// The chart the page shows for it, as SVG; the core checks it before embedding it.
    pub chart_svg: Option<String>,
}

/// What a SQL cell shows when the notebook is exported. The rows are read from the stored
/// results here, never sent by the page.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CellOutputItem {
    NotRun,
    Ran {
        messages: Vec<String>,
        results: Vec<CellResultItem>,
    },
    Failed {
        error: String,
    },
}

/// Writes the notebook as one self-contained HTML file to `path` (ADR-087, point 17).
#[command]
pub(crate) async fn export_notebook_html(
    path: String,
    title: String,
    connection: Option<String>,
    notebook: Notebook,
    outputs: Vec<CellOutputItem>,
    state: State<'_, AppState>,
) -> CmdResult<()> {
    let mut read = Vec::with_capacity(outputs.len());
    for output in outputs {
        read.push(match output {
            CellOutputItem::NotRun => CellOutput::NotRun,
            CellOutputItem::Failed { error } => CellOutput::Failed { error },
            CellOutputItem::Ran { messages, results } => {
                let mut tables = Vec::with_capacity(results.len());
                for shown in results {
                    let stored = state
                        .results
                        .stored(ResultId(shown.result.into()))
                        .map_err(|e| e.to_string())?;
                    let mut table = tokio::task::spawn_blocking(move || {
                        stored
                            .read()
                            .map(|r| result_table(&r, MAX_EXPORT_ROWS))
                            .map_err(|_| "result lock poisoned".to_string())
                    })
                    .await
                    .map_err(|e| e.to_string())??;
                    table.chart_svg = shown.chart_svg;
                    tables.push(table);
                }
                CellOutput::Ran {
                    messages,
                    results: tables,
                }
            }
        });
    }
    let export = HtmlExport {
        title,
        connection,
        written_at: now_text(),
        outputs: read,
    };
    tokio::task::spawn_blocking(move || {
        let html = notebook_html(&notebook, &export)?;
        write_notebook_html(Path::new(&path), &html)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}
