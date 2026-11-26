//! SQL notebooks (ADR-087): Markdown files with named SQL cells, read and written here.
//!
//! - [`format`]: the file format, version 1, and its round trip.
//! - [`references`]: a cell's `{{name}}` references, expanded into CTEs of the referenced
//!   cells' SQL before the guard sees anything.
//! - [`markdown`]: Markdown cells read into a small block model that the page and the HTML
//!   export draw, never as HTML.
//! - [`html`]: the HTML export, with each cell's results read from the stored results.
//!
//! Nothing here runs a statement: a cell runs as an editor script on its notebook tab's
//! session, through the same checks (ADR-087, points 10 and 12). Results never go into the
//! file (point 15).

pub mod format;
pub mod html;
pub mod markdown;
pub mod references;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::sql_file::{FileError, FileFormat, SaveCheck, read_sql_file, write_sql_file};

pub use format::{FORMAT_VERSION, is_cell_name};

/// Most cells a notebook holds.
pub const MAX_CELLS: usize = 500;
/// The longest cell name, in characters.
pub const MAX_CELL_NAME: usize = 63;

/// A notebook as the page edits it: cells in order and the connection it was last run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
pub struct Notebook {
    /// The id of the connection it was last run on: a suggestion when it opens, never a
    /// connection opened by reading it (ADR-087, point 11).
    pub connection: Option<String>,
    pub cells: Vec<Cell>,
}

/// One cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Cell {
    /// SQL, run as a script on the notebook's session; `name` is what references use.
    Sql { name: String, sql: String },
    /// Markdown text, drawn from [`markdown::read`]'s blocks.
    Markdown { text: String },
}

impl Cell {
    /// The SQL cell's name.
    pub fn name(&self) -> Option<&str> {
        match self {
            Cell::Sql { name, .. } => Some(name),
            Cell::Markdown { .. } => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotebookError {
    #[error(
        "this is not a SpelaionDB notebook: it does not start with a front matter block that says `spelaion-notebook: 1`"
    )]
    NotANotebook,
    #[error(
        "the notebook was written by a newer SpelaionDB (format version {version}); update the app to open it"
    )]
    NewerVersion { version: u32 },
    #[error("line {line} of the front matter: {reason}")]
    FrontMatter { line: usize, reason: String },
    #[error("the SQL cell `{name}` that starts at line {line} has no closing fence")]
    UnclosedCell { name: String, line: usize },
    #[error("{}`{name}` cannot name a cell: a letter or _, then letters, digits or _, at most {MAX_CELL_NAME}", line_prefix(*line))]
    BadCellName { name: String, line: Option<usize> },
    #[error("{}two cells are named `{name}` (names are unique without regard to case)", line_prefix(*line))]
    DuplicateName { name: String, line: Option<usize> },
    #[error("a notebook holds at most {MAX_CELLS} cells")]
    TooManyCells,
    #[error("the notebook's connection is not a connection id")]
    BadConnection,
    #[error(
        "Markdown cell {cell} holds a line that would read back as the start of a SQL cell (```sql cell=…) or as a cell separator (<!-- cell -->); change that line to save"
    )]
    MarkdownHoldsMarker { cell: usize },
    #[error("there is no cell {0}")]
    NoSuchCell(usize),
    #[error("{0}")]
    File(#[from] FileError),
    #[error("{0}")]
    Image(String),
}

fn line_prefix(line: Option<usize>) -> String {
    line.map(|l| format!("line {l}: ")).unwrap_or_default()
}

/// A notebook file as a notebook tab sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookFile {
    pub notebook: Notebook,
    pub format: FileFormat,
    /// SHA-256 of the file's bytes, to notice a change on disk (ADR-036).
    pub hash: String,
}

/// Reads a notebook from its text: [`format::parse`].
pub fn parse_notebook(text: &str) -> Result<Notebook, NotebookError> {
    format::parse(text)
}

/// The notebook's text: [`format::write`].
pub fn notebook_text(notebook: &Notebook) -> Result<String, NotebookError> {
    format::write(notebook)
}

/// Reads a notebook file with ADR-036's rules (size, encodings, line ending, hash).
pub fn read_notebook_file(path: &Path) -> Result<NotebookFile, NotebookError> {
    let file = read_sql_file(path)?;
    Ok(NotebookFile {
        notebook: format::parse(&file.text)?,
        format: file.format,
        hash: file.hash,
    })
}

/// Writes a notebook file atomically in `format`, after `check`; returns the new hash.
pub fn write_notebook_file(
    path: &Path,
    notebook: &Notebook,
    format: FileFormat,
    check: &SaveCheck,
) -> Result<String, NotebookError> {
    let text = format::write(notebook)?;
    Ok(write_sql_file(path, &text, format, check)?)
}

/// The name a new SQL cell gets: the first `cell_<n>` no cell has.
pub fn new_cell_name(notebook: &Notebook) -> String {
    let taken: std::collections::HashSet<String> = notebook
        .cells
        .iter()
        .filter_map(Cell::name)
        .map(str::to_ascii_lowercase)
        .collect();
    (1..)
        .map(|n| format!("cell_{n}"))
        .find(|name| !taken.contains(name))
        .unwrap_or_else(|| "cell".into())
}

/// Whether the SQL cell at `index` may be renamed to `name`: valid, and no other cell's.
pub fn check_cell_name(notebook: &Notebook, index: usize, name: &str) -> Result<(), NotebookError> {
    if !is_cell_name(name) {
        return Err(NotebookError::BadCellName {
            name: name.to_string(),
            line: None,
        });
    }
    let taken =
        notebook.cells.iter().enumerate().any(|(i, cell)| {
            i != index && cell.name().is_some_and(|n| n.eq_ignore_ascii_case(name))
        });
    if taken {
        return Err(NotebookError::DuplicateName {
            name: name.to_string(),
            line: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sql(name: &str) -> Cell {
        Cell::Sql {
            name: name.into(),
            sql: String::new(),
        }
    }

    #[test]
    fn new_cells_get_the_first_free_name() {
        let mut notebook = Notebook::default();
        assert_eq!(new_cell_name(&notebook), "cell_1");
        notebook.cells = vec![sql("Cell_1"), sql("cell_3")];
        assert_eq!(new_cell_name(&notebook), "cell_2");
    }

    #[test]
    fn renaming_refuses_taken_and_invalid_names() {
        let notebook = Notebook {
            connection: None,
            cells: vec![sql("a"), sql("b")],
        };
        assert!(check_cell_name(&notebook, 0, "A").is_ok());
        assert!(matches!(
            check_cell_name(&notebook, 0, "B"),
            Err(NotebookError::DuplicateName { .. })
        ));
        assert!(matches!(
            check_cell_name(&notebook, 0, "b c"),
            Err(NotebookError::BadCellName { .. })
        ));
    }

    #[test]
    fn a_notebook_file_round_trips_through_every_encoding() {
        use crate::sql_file::{FileEncoding, LineEnding};
        let dir = std::env::temp_dir().join(format!("spelaion-notebook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let notebook = Notebook {
            connection: Some("~/local".into()),
            cells: vec![
                Cell::Markdown {
                    text: "Café ±".into(),
                },
                Cell::Sql {
                    name: "a".into(),
                    sql: "select 'é'\nfrom t".into(),
                },
            ],
        };
        for encoding in [
            FileEncoding::Utf8,
            FileEncoding::Utf8Bom,
            FileEncoding::Utf16Le,
            FileEncoding::Utf16Be,
            FileEncoding::Windows1252,
        ] {
            for line_ending in [LineEnding::Lf, LineEnding::CrLf, LineEnding::Cr] {
                let path = dir.join("n.md");
                let format = FileFormat {
                    encoding,
                    line_ending,
                };
                let hash =
                    write_notebook_file(&path, &notebook, format, &SaveCheck::Overwrite).unwrap();
                let read = read_notebook_file(&path).unwrap();
                assert_eq!(read.notebook, notebook);
                assert_eq!(read.hash, hash);
                assert_eq!(read.format, format);
                // A save that expected other bytes is refused.
                assert_eq!(
                    write_notebook_file(
                        &path,
                        &notebook,
                        format,
                        &SaveCheck::Unchanged("other".into())
                    ),
                    Err(NotebookError::File(FileError::ChangedOnDisk))
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
