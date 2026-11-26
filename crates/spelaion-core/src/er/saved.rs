//! Saved ER diagrams (ADR-084): an arrangement over the catalog, kept as local data.
//!
//! A saved diagram is its name, its scope, its display options and its arrangement: where the
//! user put each table, its notes and its colored groups. Never a column, a type or a key:
//! those are read from the catalog again whenever it opens. The store keeps the document as
//! text (`spelaion-store`'s `er_diagrams.rs`), so it is text someone or something else may have
//! written: it is checked here when it is saved and **again every time it is read back**, and
//! one that does not pass is listed as unreadable and never applied in part.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use spelaion_config::check_plain_text;
use spelaion_driver_api::NameFilter;
use spelaion_store::{ErDiagramSave, Store, StoreError, StoredErDiagram, StoredErDiagramDocument};

use super::layout::{ErColumns, ErLayoutOptions};
use super::{DiagramScope, MAX_DIAGRAM_TABLES, TableName};
use crate::history::{HistoryError, QueryHistory};

/// The document's version this build writes, and the newest it reads.
const DOCUMENT_VERSION: u32 = 1;
/// Longest name of a saved diagram or of a group, in characters.
pub const MAX_DIAGRAM_NAME: usize = 100;
/// Most tables an arrangement places: twice what a diagram reads, so tables that are gone
/// still fit beside the ones that are there.
pub const MAX_PLACED_TABLES: usize = 2 * MAX_DIAGRAM_TABLES;
pub const MAX_NOTES: usize = 200;
pub const MAX_NOTE_CHARS: usize = 2_000;
pub const MAX_GROUPS: usize = 100;
/// Farthest a table or a note may be from the diagram's origin, in pixels.
pub const MAX_COORDINATE: f64 = 1_000_000.0;
/// A note's narrowest and widest width, in pixels.
pub const MIN_NOTE_WIDTH: f64 = 120.0;
pub const MAX_NOTE_WIDTH: f64 = 800.0;
/// Longest schema, table name or filter pattern kept, in characters.
const MAX_IDENTIFIER_CHARS: usize = 512;
/// Most patterns a kept table filter holds on each side.
const MAX_FILTER_PATTERNS: usize = 100;

/// The palette a note or a group is colored from, kept by name; the UI draws each with the
/// theme's `--er-color-*` tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErColor {
    Gray,
    Blue,
    Green,
    Amber,
    Red,
    Purple,
}

impl ErColor {
    pub const ALL: [ErColor; 6] = [
        ErColor::Gray,
        ErColor::Blue,
        ErColor::Green,
        ErColor::Amber,
        ErColor::Red,
        ErColor::Purple,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ErColor::Gray => "gray",
            ErColor::Blue => "blue",
            ErColor::Green => "green",
            ErColor::Amber => "amber",
            ErColor::Red => "red",
            ErColor::Purple => "purple",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }
}

/// Where the user put a table: its box's top left corner, in the layout's pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct ErPlacement {
    pub table: TableName,
    pub x: f64,
    pub y: f64,
}

/// A note on the diagram: plain text at a place, wrapped to its width.
#[derive(Debug, Clone, PartialEq)]
pub struct ErNote {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub text: String,
    pub color: ErColor,
}

/// Tables framed together under a name, in a color. A table is in one group at most.
#[derive(Debug, Clone, PartialEq)]
pub struct ErGroup {
    pub name: String,
    pub color: ErColor,
    pub tables: Vec<TableName>,
}

/// What the user arranged on a diagram. With no positions, the tables follow the core's
/// automatic layout; with some, every placed table stays where it was put and the others are
/// new (ADR-084, point 4).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ErArrangement {
    pub positions: Vec<ErPlacement>,
    pub notes: Vec<ErNote>,
    pub groups: Vec<ErGroup>,
}

/// What a saved diagram holds besides its name.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedDiagramContent {
    pub scope: DiagramScope,
    pub options: ErLayoutOptions,
    pub arrangement: ErArrangement,
}

/// A saved diagram as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedDiagramSummary {
    pub id: i64,
    pub workspace: String,
    pub connection: String,
    pub name: String,
    pub updated_ms: i64,
    /// Why its document cannot be used, when it cannot: it then opens nothing.
    pub unreadable: Option<String>,
}

/// A saved diagram, checked just now.
#[derive(Debug, Clone, PartialEq)]
pub struct SavedDiagram {
    pub summary: SavedDiagramSummary,
    pub content: SavedDiagramContent,
}

#[derive(Debug, thiserror::Error)]
pub enum SavedDiagramError {
    /// What was to be saved does not pass the checks: nothing was saved.
    #[error("{0}")]
    Invalid(String),
    #[error("a saved diagram of this connection is already called \"{0}\"")]
    NameTaken(String),
    #[error("the saved diagram is not there any more")]
    Missing,
    #[error("there are too many saved diagrams: delete some first")]
    Full,
    /// The stored document did not pass the checks when read.
    #[error("the saved diagram cannot be read: {0}")]
    Unreadable(String),
    #[error(transparent)]
    Store(#[from] HistoryError),
}

/// One step of the store's actor on saved diagrams.
#[derive(Debug)]
pub(crate) enum DiagramStoreCall {
    List,
    Get(i64),
    Save {
        id: Option<i64>,
        workspace: String,
        connection: String,
        name: String,
        document: String,
    },
    Delete(i64),
}

/// What the store answered, as it is written there: not checked.
#[derive(Debug)]
pub(crate) enum DiagramStoreAnswer {
    List(Vec<StoredErDiagramDocument>),
    Get(Option<StoredErDiagramDocument>),
    Saved(ErDiagramSave),
    Deleted(bool),
}

/// The store actor's step.
pub(crate) fn stored_diagrams(
    store: &Store,
    call: DiagramStoreCall,
) -> Result<DiagramStoreAnswer, StoreError> {
    Ok(match call {
        DiagramStoreCall::List => {
            let listed = store.er_diagrams()?;
            let mut documents = Vec::with_capacity(listed.len());
            for diagram in listed {
                // Each one read whole, so its listing can say whether it opens.
                if let Some(document) = store.er_diagram(diagram.id)? {
                    documents.push(document);
                }
            }
            DiagramStoreAnswer::List(documents)
        }
        DiagramStoreCall::Get(id) => DiagramStoreAnswer::Get(store.er_diagram(id)?),
        DiagramStoreCall::Save {
            id,
            workspace,
            connection,
            name,
            document,
        } => DiagramStoreAnswer::Saved(store.save_er_diagram(
            id,
            &workspace,
            &connection,
            &name,
            &document,
        )?),
        DiagramStoreCall::Delete(id) => DiagramStoreAnswer::Deleted(store.delete_er_diagram(id)?),
    })
}

fn unexpected() -> SavedDiagramError {
    SavedDiagramError::Store(HistoryError::Closed)
}

/// A one-line name: trimmed, 1 to [`MAX_DIAGRAM_NAME`] characters of plain text.
pub fn check_diagram_name(name: &str, what: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if name.contains(['\n', '\r', '\t']) || check_plain_text(name, MAX_DIAGRAM_NAME).is_err() {
        return Err(format!(
            "{what} is one line of at most {MAX_DIAGRAM_NAME} characters, without hidden characters"
        ));
    }
    Ok(name.to_string())
}

fn check_identifier(text: &str, what: &str) -> Result<(), String> {
    if text.chars().count() > MAX_IDENTIFIER_CHARS || text.contains('\0') {
        return Err(format!("{what} is too long or holds a NUL character"));
    }
    Ok(())
}

fn check_table(table: &TableName) -> Result<(), String> {
    check_identifier(&table.schema, "a schema name")?;
    check_identifier(&table.name, "a table name")?;
    if table.name.is_empty() {
        return Err("a table has no name".into());
    }
    Ok(())
}

fn check_coordinate(value: f64) -> Result<(), String> {
    if value.is_finite() && value.abs() <= MAX_COORDINATE {
        Ok(())
    } else {
        Err(format!(
            "a position is not a number within {MAX_COORDINATE} pixels of the origin"
        ))
    }
}

/// Checks what a diagram's scope names.
pub fn check_scope(scope: &DiagramScope) -> Result<(), String> {
    match scope {
        DiagramScope::Schema { schema, filter } => {
            check_identifier(schema, "the schema name")?;
            if let Some(filter) = filter {
                for (side, patterns) in [("include", &filter.include), ("exclude", &filter.exclude)]
                {
                    if patterns.len() > MAX_FILTER_PATTERNS {
                        return Err(format!("the table filter has too many {side} patterns"));
                    }
                    for pattern in patterns {
                        check_identifier(pattern, "a table filter pattern")?;
                    }
                }
            }
            Ok(())
        }
        DiagramScope::Tables { tables } => {
            if tables.is_empty() || tables.len() > MAX_DIAGRAM_TABLES {
                return Err(format!(
                    "a diagram of chosen tables names 1 to {MAX_DIAGRAM_TABLES} tables"
                ));
            }
            tables.iter().try_for_each(check_table)
        }
        DiagramScope::Focus { table } => check_table(table),
    }
}

/// Checks an arrangement, whether it is to be laid out, saved or was just read back.
pub fn check_arrangement(arrangement: &ErArrangement) -> Result<(), String> {
    if arrangement.positions.len() > MAX_PLACED_TABLES {
        return Err(format!(
            "a diagram places at most {MAX_PLACED_TABLES} tables"
        ));
    }
    let mut placed = HashSet::new();
    for placement in &arrangement.positions {
        check_table(&placement.table)?;
        check_coordinate(placement.x)?;
        check_coordinate(placement.y)?;
        if !placed.insert(&placement.table) {
            return Err(format!(
                "the table {}.{} is placed twice",
                placement.table.schema, placement.table.name
            ));
        }
    }
    if arrangement.notes.len() > MAX_NOTES {
        return Err(format!("a diagram has at most {MAX_NOTES} notes"));
    }
    for note in &arrangement.notes {
        check_coordinate(note.x)?;
        check_coordinate(note.y)?;
        if !(MIN_NOTE_WIDTH..=MAX_NOTE_WIDTH).contains(&note.width) {
            return Err(format!(
                "a note is {MIN_NOTE_WIDTH} to {MAX_NOTE_WIDTH} pixels wide"
            ));
        }
        if note.text.trim().is_empty() {
            return Err("a note is empty".into());
        }
        if check_plain_text(&note.text, MAX_NOTE_CHARS).is_err() {
            return Err(format!(
                "a note is at most {MAX_NOTE_CHARS} characters of text, line breaks and tabs"
            ));
        }
    }
    if arrangement.groups.len() > MAX_GROUPS {
        return Err(format!("a diagram has at most {MAX_GROUPS} groups"));
    }
    let mut grouped = HashSet::new();
    for group in &arrangement.groups {
        check_diagram_name(&group.name, "a group's name")?;
        if group.tables.is_empty() || group.tables.len() > MAX_PLACED_TABLES {
            return Err(format!("a group holds 1 to {MAX_PLACED_TABLES} tables"));
        }
        for table in &group.tables {
            check_table(table)?;
            if !grouped.insert(table) {
                return Err(format!(
                    "the table {}.{} is in two groups",
                    table.schema, table.name
                ));
            }
        }
    }
    Ok(())
}

// The document as it is kept. Its own types, so a change to the core's never changes what
// earlier builds wrote; enums are kept by name and read back through `parse`.

#[derive(Serialize, Deserialize)]
struct Document {
    version: u32,
    scope: DocumentScope,
    options: DocumentOptions,
    #[serde(default)]
    positions: Vec<DocumentPlacement>,
    #[serde(default)]
    notes: Vec<DocumentNote>,
    #[serde(default)]
    groups: Vec<DocumentGroup>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DocumentScope {
    Schema {
        schema: String,
        filter: Option<DocumentFilter>,
    },
    Tables {
        tables: Vec<DocumentTable>,
    },
    Focus {
        table: DocumentTable,
    },
}

#[derive(Serialize, Deserialize)]
struct DocumentFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct DocumentTable {
    schema: String,
    name: String,
}

#[derive(Serialize, Deserialize)]
struct DocumentOptions {
    columns: String,
    types: bool,
    comments: bool,
}

#[derive(Serialize, Deserialize)]
struct DocumentPlacement {
    schema: String,
    name: String,
    x: f64,
    y: f64,
}

#[derive(Serialize, Deserialize)]
struct DocumentNote {
    x: f64,
    y: f64,
    width: f64,
    text: String,
    color: String,
}

#[derive(Serialize, Deserialize)]
struct DocumentGroup {
    name: String,
    color: String,
    tables: Vec<DocumentTable>,
}

fn columns_name(columns: ErColumns) -> &'static str {
    match columns {
        ErColumns::All => "all",
        ErColumns::Keys => "keys",
        ErColumns::None => "none",
    }
}

fn document_table(table: &TableName) -> DocumentTable {
    DocumentTable {
        schema: table.schema.clone(),
        name: table.name.clone(),
    }
}

fn table_name(table: DocumentTable) -> TableName {
    TableName {
        schema: table.schema,
        name: table.name,
    }
}

fn color(name: &str) -> Result<ErColor, String> {
    ErColor::parse(name).ok_or_else(|| format!("unknown color \"{name}\""))
}

/// The document of `content`, checked first.
pub fn encode_diagram(content: &SavedDiagramContent) -> Result<String, String> {
    check_scope(&content.scope)?;
    check_arrangement(&content.arrangement)?;
    let document = Document {
        version: DOCUMENT_VERSION,
        scope: match &content.scope {
            DiagramScope::Schema { schema, filter } => DocumentScope::Schema {
                schema: schema.clone(),
                filter: filter.as_ref().map(|f| DocumentFilter {
                    include: f.include.clone(),
                    exclude: f.exclude.clone(),
                }),
            },
            DiagramScope::Tables { tables } => DocumentScope::Tables {
                tables: tables.iter().map(document_table).collect(),
            },
            DiagramScope::Focus { table } => DocumentScope::Focus {
                table: document_table(table),
            },
        },
        options: DocumentOptions {
            columns: columns_name(content.options.columns).into(),
            types: content.options.types,
            comments: content.options.comments,
        },
        positions: content
            .arrangement
            .positions
            .iter()
            .map(|p| DocumentPlacement {
                schema: p.table.schema.clone(),
                name: p.table.name.clone(),
                x: p.x,
                y: p.y,
            })
            .collect(),
        notes: content
            .arrangement
            .notes
            .iter()
            .map(|n| DocumentNote {
                x: n.x,
                y: n.y,
                width: n.width,
                text: n.text.clone(),
                color: n.color.as_str().into(),
            })
            .collect(),
        groups: content
            .arrangement
            .groups
            .iter()
            .map(|g| DocumentGroup {
                name: g.name.clone(),
                color: g.color.as_str().into(),
                tables: g.tables.iter().map(document_table).collect(),
            })
            .collect(),
    };
    serde_json::to_string(&document).map_err(|e| e.to_string())
}

/// Reads a kept document back, checking all of it: an error leaves nothing to apply.
pub fn decode_diagram(text: &str) -> Result<SavedDiagramContent, String> {
    // Read only far enough to know whether this build can read the rest.
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let version: Version = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if version.version > DOCUMENT_VERSION {
        return Err("it was saved by a newer version of SpelaionDB".into());
    }
    let document: Document = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let scope = match document.scope {
        DocumentScope::Schema { schema, filter } => DiagramScope::Schema {
            schema,
            filter: filter.map(|f| NameFilter {
                include: f.include,
                exclude: f.exclude,
            }),
        },
        DocumentScope::Tables { tables } => DiagramScope::Tables {
            tables: tables.into_iter().map(table_name).collect(),
        },
        DocumentScope::Focus { table } => DiagramScope::Focus {
            table: table_name(table),
        },
    };
    let columns = match document.options.columns.as_str() {
        "all" => ErColumns::All,
        "keys" => ErColumns::Keys,
        "none" => ErColumns::None,
        other => return Err(format!("unknown columns option \"{other}\"")),
    };
    let arrangement = ErArrangement {
        positions: document
            .positions
            .into_iter()
            .map(|p| ErPlacement {
                table: TableName {
                    schema: p.schema,
                    name: p.name,
                },
                x: p.x,
                y: p.y,
            })
            .collect(),
        notes: document
            .notes
            .into_iter()
            .map(|n| {
                Ok(ErNote {
                    x: n.x,
                    y: n.y,
                    width: n.width,
                    text: n.text,
                    color: color(&n.color)?,
                })
            })
            .collect::<Result<_, String>>()?,
        groups: document
            .groups
            .into_iter()
            .map(|g| {
                Ok(ErGroup {
                    name: g.name,
                    color: color(&g.color)?,
                    tables: g.tables.into_iter().map(table_name).collect(),
                })
            })
            .collect::<Result<_, String>>()?,
    };
    check_scope(&scope)?;
    check_arrangement(&arrangement)?;
    // A stored name is a name too: a group's was checked above.
    Ok(SavedDiagramContent {
        scope,
        options: ErLayoutOptions {
            columns,
            types: document.options.types,
            comments: document.options.comments,
        },
        arrangement,
    })
}

fn summary_of(diagram: StoredErDiagram, unreadable: Option<String>) -> SavedDiagramSummary {
    SavedDiagramSummary {
        id: diagram.id,
        workspace: diagram.workspace,
        connection: diagram.connection,
        name: diagram.name,
        updated_ms: diagram.updated_ms,
        unreadable,
    }
}

/// A stored diagram checked: its content, or why it cannot be used. Its stored name is
/// checked like a new one.
fn checked(stored: StoredErDiagramDocument) -> (SavedDiagramSummary, Option<SavedDiagramContent>) {
    let decoded = check_diagram_name(&stored.diagram.name, "its name")
        .and_then(|_| decode_diagram(&stored.document));
    match decoded {
        Ok(content) => (summary_of(stored.diagram, None), Some(content)),
        Err(reason) => (summary_of(stored.diagram, Some(reason)), None),
    }
}

impl QueryHistory {
    /// Every saved diagram, each one checked: one that does not pass says why.
    pub async fn saved_diagrams(&self) -> Result<Vec<SavedDiagramSummary>, SavedDiagramError> {
        match self.stored_er_diagrams(DiagramStoreCall::List).await? {
            DiagramStoreAnswer::List(stored) => {
                Ok(stored.into_iter().map(|d| checked(d).0).collect())
            }
            _ => Err(unexpected()),
        }
    }

    /// One saved diagram, checked just now.
    pub async fn saved_diagram(&self, id: i64) -> Result<SavedDiagram, SavedDiagramError> {
        match self.stored_er_diagrams(DiagramStoreCall::Get(id)).await? {
            DiagramStoreAnswer::Get(Some(stored)) => match checked(stored) {
                (summary, Some(content)) => Ok(SavedDiagram { summary, content }),
                (summary, None) => Err(SavedDiagramError::Unreadable(
                    summary.unreadable.unwrap_or_default(),
                )),
            },
            DiagramStoreAnswer::Get(None) => Err(SavedDiagramError::Missing),
            _ => Err(unexpected()),
        }
    }

    /// Saves a diagram of `connection` under `name`: a new one, or over `id` (which keeps its
    /// workspace and connection). Everything is checked before the store sees it.
    pub async fn save_diagram(
        &self,
        id: Option<i64>,
        workspace: String,
        connection: String,
        name: &str,
        content: &SavedDiagramContent,
    ) -> Result<SavedDiagram, SavedDiagramError> {
        let name =
            check_diagram_name(name, "the diagram's name").map_err(SavedDiagramError::Invalid)?;
        let document = encode_diagram(content).map_err(SavedDiagramError::Invalid)?;
        let call = DiagramStoreCall::Save {
            id,
            workspace,
            connection,
            name: name.clone(),
            document,
        };
        match self.stored_er_diagrams(call).await? {
            DiagramStoreAnswer::Saved(ErDiagramSave::Saved(id)) => self.saved_diagram(id).await,
            DiagramStoreAnswer::Saved(ErDiagramSave::NameTaken) => {
                Err(SavedDiagramError::NameTaken(name))
            }
            DiagramStoreAnswer::Saved(ErDiagramSave::Missing) => Err(SavedDiagramError::Missing),
            DiagramStoreAnswer::Saved(ErDiagramSave::Full) => Err(SavedDiagramError::Full),
            _ => Err(unexpected()),
        }
    }

    /// Renames a saved diagram, keeping what it holds (which must still be readable).
    pub async fn rename_saved_diagram(
        &self,
        id: i64,
        name: &str,
    ) -> Result<SavedDiagram, SavedDiagramError> {
        let saved = self.saved_diagram(id).await?;
        self.save_diagram(
            Some(id),
            saved.summary.workspace,
            saved.summary.connection,
            name,
            &saved.content,
        )
        .await
    }

    /// Deletes a saved diagram; whether it was there.
    pub async fn delete_saved_diagram(&self, id: i64) -> Result<bool, SavedDiagramError> {
        match self
            .stored_er_diagrams(DiagramStoreCall::Delete(id))
            .await?
        {
            DiagramStoreAnswer::Deleted(deleted) => Ok(deleted),
            _ => Err(unexpected()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str) -> TableName {
        TableName {
            schema: "public".into(),
            name: name.into(),
        }
    }

    fn content() -> SavedDiagramContent {
        SavedDiagramContent {
            scope: DiagramScope::Schema {
                schema: "public".into(),
                filter: Some(NameFilter {
                    include: vec!["order*".into()],
                    exclude: vec![],
                }),
            },
            options: ErLayoutOptions {
                columns: ErColumns::Keys,
                types: false,
                comments: true,
            },
            arrangement: ErArrangement {
                positions: vec![
                    ErPlacement {
                        table: table("orders"),
                        x: -40.5,
                        y: 120.0,
                    },
                    ErPlacement {
                        table: table("order_line"),
                        x: 400.0,
                        y: 120.0,
                    },
                ],
                notes: vec![ErNote {
                    x: 10.0,
                    y: 10.0,
                    width: 240.0,
                    text: "Orders are\tnever deleted.\nAsk billing.".into(),
                    color: ErColor::Amber,
                }],
                groups: vec![ErGroup {
                    name: "Sales".into(),
                    color: ErColor::Blue,
                    tables: vec![table("orders"), table("order_line")],
                }],
            },
        }
    }

    #[test]
    fn a_document_reads_back_as_it_was_written() {
        let content = content();
        let text = encode_diagram(&content).unwrap();
        assert!(text.contains("\"version\":1"));
        assert_eq!(decode_diagram(&text).unwrap(), content);
        // Other scopes too.
        for scope in [
            DiagramScope::Tables {
                tables: vec![table("a"), table("b")],
            },
            DiagramScope::Focus { table: table("a") },
        ] {
            let other = SavedDiagramContent {
                scope,
                ..content.clone()
            };
            assert_eq!(
                decode_diagram(&encode_diagram(&other).unwrap()).unwrap(),
                other
            );
        }
    }

    #[test]
    fn what_does_not_pass_is_refused_whole() {
        let refused = |change: &dyn Fn(&mut SavedDiagramContent)| {
            let mut content = content();
            change(&mut content);
            encode_diagram(&content).unwrap_err()
        };
        refused(&|c| c.arrangement.positions[0].x = f64::NAN);
        refused(&|c| c.arrangement.positions[0].y = MAX_COORDINATE * 2.0);
        refused(&|c| {
            let again = c.arrangement.positions[0].clone();
            c.arrangement.positions.push(again);
        });
        refused(&|c| c.arrangement.notes[0].text = "  ".into());
        refused(&|c| c.arrangement.notes[0].text = "x".repeat(MAX_NOTE_CHARS + 1));
        refused(&|c| c.arrangement.notes[0].text = "hidden\u{202e}text".into());
        refused(&|c| c.arrangement.notes[0].width = 20.0);
        refused(&|c| c.arrangement.groups[0].name = "two\nlines".into());
        refused(&|c| c.arrangement.groups[0].tables.clear());
        refused(&|c| {
            let mut again = c.arrangement.groups[0].clone();
            again.name = "Again".into();
            c.arrangement.groups.push(again);
        });
        refused(&|c| c.scope = DiagramScope::Tables { tables: vec![] });
        refused(&|c| c.arrangement.positions[0].table.name = String::new());
        assert!(check_diagram_name("  Orders  ", "x").unwrap() == "Orders");
        assert!(check_diagram_name("", "x").is_err());
        assert!(check_diagram_name(&"x".repeat(MAX_DIAGRAM_NAME + 1), "x").is_err());
    }

    #[test]
    fn a_stored_document_is_checked_when_read() {
        let good = encode_diagram(&content()).unwrap();
        // Written by something else: a color this build does not know, a position out of
        // bounds, a newer version, or not a document at all. Nothing is applied in part.
        for bad in [
            good.replace("\"amber\"", "\"pink\""),
            good.replace("-40.5", "1e300"),
            good.replace("\"version\":1", "\"version\":2"),
            good.replace("\"keys\"", "\"some\""),
            "not json".to_string(),
        ] {
            assert!(decode_diagram(&bad).is_err(), "{bad}");
        }
        let newer = decode_diagram(&good.replace("\"version\":1", "\"version\":2")).unwrap_err();
        assert!(newer.contains("newer"));
    }
}
