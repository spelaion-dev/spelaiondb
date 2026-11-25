//! Where each table of a diagram goes and how its relations are drawn (ADR-046).
//!
//! A layered layout, computed once per model and display options:
//! 1. Each table becomes a box whose size follows what it shows (its columns, types and
//!    comments) in the UI's monospace font, from the character width the UI measured.
//! 2. Tables joined by keys form groups. In each group, a referenced table sits to the left of
//!    the tables that reference it (the longest chain of keys decides its column; key cycles are
//!    broken where they close), and the order inside each column is improved a few times by the
//!    average position of each table's neighbours, which takes most crossings out.
//! 3. A column taller than the group would be square is wrapped into several, and each table is
//!    placed as close as it can to the height of the tables it is joined to.
//! 4. Groups, largest first, and then the tables with no keys, are packed in rows.
//! 5. Each relation leaves the referencing table at the row of its first column and enters the
//!    referenced table at the row of the column it points to (the box's side when columns are
//!    hidden), with a straight stub at each end for the crow's foot marks and a curve between.
//!
//! A saved diagram's arrangement (ADR-084) changes steps 2 to 4: tables the user placed stay
//! where they were put, the others (new ones) are laid out the same way among themselves and
//! go below them, and tables that are gone are drawn as a box with their name where they were.
//! Its notes are wrapped to their width and its groups framed around their tables here too, so
//! the screen and the image exports agree.
//!
//! Everything is in pixels; the UI draws it as it comes and pans or zooms with a transform.

use std::collections::HashMap;

use super::saved::{ErArrangement, ErColor, MAX_COORDINATE, MAX_NOTE_WIDTH, MIN_NOTE_WIDTH};
use super::{Cardinality, ErModel, ErTable, TableName};

/// Longest column name, type, comment and table name a box shows; longer ones end in "…".
pub const MAX_COLUMN_CHARS: usize = 40;
pub const MAX_TYPE_CHARS: usize = 28;
pub const MAX_COMMENT_CHARS: usize = 40;
pub const MAX_TITLE_CHARS: usize = 48;

/// Inner padding of a box, horizontal and vertical.
const PAD_X: f64 = 10.0;
const PAD_Y: f64 = 6.0;
/// Space between a row's key mark, name, type and comment, in characters.
const TEXT_GAP_CHARS: f64 = 2.0;
const MIN_BOX_WIDTH: f64 = 120.0;
/// Space between columns of boxes (room for the curves) and between boxes of a column.
const GAP_X: f64 = 96.0;
const GAP_Y: f64 = 32.0;
/// Space between groups of tables, and around the whole diagram.
const GROUP_GAP: f64 = 72.0;
const MARGIN: f64 = 24.0;
/// The straight part of a relation at each end, where crow's foot marks are drawn.
const STUB: f64 = 18.0;
/// How far a relation that leaves and enters on the same side bulges out.
const BULGE: f64 = 48.0;
/// Rounds of crossing reduction (each one forwards and backwards).
const ORDER_ROUNDS: usize = 4;
/// Space between a group's frame and its tables.
const GROUP_PAD: f64 = 16.0;
/// Most lines a note shows; the rest of its text ends in "…".
const MAX_NOTE_LINES: usize = 60;

/// Which columns a box lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErColumns {
    All,
    /// Primary key, foreign key and unique columns only.
    Keys,
    /// Table names only.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErLayoutOptions {
    pub columns: ErColumns,
    pub types: bool,
    pub comments: bool,
}

/// The UI's measures of the diagram's monospace font, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErMetrics {
    pub char_width: f64,
    pub line_height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErPoint {
    pub x: f64,
    pub y: f64,
}

/// The side of a box a relation leaves or enters by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErSide {
    Left,
    Right,
}

impl ErSide {
    fn sign(self) -> f64 {
        match self {
            ErSide::Left => -1.0,
            ErSide::Right => 1.0,
        }
    }
}

/// A column's key roles, as a box marks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ErKeyMark {
    pub primary: bool,
    pub foreign: bool,
    pub unique: bool,
}

impl ErKeyMark {
    /// "PK", "FK", "UK" or a combination such as "PK FK"; empty for no key.
    pub fn text(self) -> String {
        [
            (self.primary, "PK"),
            (self.foreign, "FK"),
            (self.unique && !self.primary, "UK"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, mark)| *mark)
        .collect::<Vec<_>>()
        .join(" ")
    }
}

/// One listed column, its texts already shortened to what the box has room for.
#[derive(Debug, Clone, PartialEq)]
pub struct ErRow {
    /// Its position in the table's columns.
    pub column: usize,
    pub key: ErKeyMark,
    pub name: String,
    /// Empty when types are hidden.
    pub native_type: String,
    pub comment: Option<String>,
    pub nullable: bool,
}

/// A table's box. `x`, `y` are its top left corner; the other positions are relative to it.
#[derive(Debug, Clone, PartialEq)]
pub struct ErNode {
    /// Its position in the model's tables (nodes come in the same order).
    pub table: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// The table's name (qualified when the diagram spans several schemas), shortened.
    pub title: String,
    /// The table's comment, when comments are shown.
    pub comment: Option<String>,
    /// Where the first row starts, below the title (and comment).
    pub rows_top: f64,
    pub rows: Vec<ErRow>,
    /// Where each part of a row starts.
    pub key_x: f64,
    pub name_x: f64,
    pub type_x: f64,
    pub comment_x: f64,
    /// Not placed by the saved arrangement: laid out by the core below the placed tables.
    pub new: bool,
}

/// A table the arrangement placed that the catalog no longer has in the diagram's scope: a box
/// with its name where it was.
#[derive(Debug, Clone, PartialEq)]
pub struct ErGoneNode {
    pub schema: String,
    pub name: String,
    pub title: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A note, its text wrapped to its width.
#[derive(Debug, Clone, PartialEq)]
pub struct ErNoteBox {
    /// Its position in the arrangement's notes.
    pub note: usize,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub lines: Vec<String>,
    pub color: ErColor,
}

/// A group's frame around its tables, its name above them.
#[derive(Debug, Clone, PartialEq)]
pub struct ErGroupFrame {
    /// Its position in the arrangement's groups.
    pub group: usize,
    pub label: String,
    pub color: ErColor,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A relation's line: from the referencing table's box to the referenced one's.
#[derive(Debug, Clone, PartialEq)]
pub struct ErEdge {
    /// Its position in the model's relations.
    pub relation: usize,
    pub from: usize,
    pub to: usize,
    pub name: String,
    pub from_cardinality: Cardinality,
    pub to_cardinality: Cardinality,
    pub from_side: ErSide,
    pub to_side: ErSide,
    /// Six points: the start on the referencing box, the end of its stub, two curve controls,
    /// the start of the referenced end's stub and the end on the referenced box. Drawn as
    /// `M p0 L p1 C p2 p3 p4 L p5`.
    pub points: Vec<ErPoint>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErLayout {
    /// The top left corner of everything drawn (0, 0 for an automatic layout; a table placed
    /// by hand may be left of or above it).
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub line_height: f64,
    pub nodes: Vec<ErNode>,
    pub edges: Vec<ErEdge>,
    pub gone: Vec<ErGoneNode>,
    pub notes: Vec<ErNoteBox>,
    pub groups: Vec<ErGroupFrame>,
}

/// Columns a character takes in a monospace font: two for the wide ones of East Asian scripts
/// and emoji, one for the rest.
fn char_cells(c: char) -> usize {
    let wide = matches!(c as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD);
    if wide { 2 } else { 1 }
}

fn cells(text: &str) -> usize {
    text.chars().map(char_cells).sum()
}

/// `text` on one line in at most `max` cells, ending in "…" when it was cut.
pub(crate) fn shorten(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if cells(flat) <= max {
        return flat.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in flat.chars() {
        let w = char_cells(c);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// The name a box (and the text exports) gives a table.
pub(crate) fn display_name(model: &ErModel, table: &ErTable) -> String {
    if model.single_schema() || table.schema.is_empty() {
        table.name.clone()
    } else {
        format!("{}.{}", table.schema, table.name)
    }
}

fn key_mark(table: &ErTable, column: &str) -> ErKeyMark {
    ErKeyMark {
        primary: table.primary_key.iter().any(|c| c == column),
        foreign: table.foreign_key_columns.iter().any(|c| c == column),
        unique: table
            .unique
            .iter()
            .any(|key| key.iter().any(|c| c == column)),
    }
}

fn node_for(model: &ErModel, index: usize, options: &ErLayoutOptions, cw: f64, lh: f64) -> ErNode {
    let table = &model.tables[index];
    let title = shorten(&display_name(model, table), MAX_TITLE_CHARS);
    let comment = options
        .comments
        .then(|| {
            table
                .comment
                .as_deref()
                .map(|c| shorten(c, MAX_COMMENT_CHARS))
        })
        .flatten()
        .filter(|c| !c.is_empty());
    let rows: Vec<ErRow> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c, key_mark(table, &c.name)))
        .filter(|(_, _, key)| match options.columns {
            ErColumns::All => true,
            ErColumns::Keys => key.primary || key.foreign || key.unique,
            ErColumns::None => false,
        })
        .map(|(i, c, key)| ErRow {
            column: i,
            key,
            name: shorten(&c.name, MAX_COLUMN_CHARS),
            native_type: if options.types {
                shorten(&c.native_type, MAX_TYPE_CHARS)
            } else {
                String::new()
            },
            comment: options
                .comments
                .then(|| c.comment.as_deref().map(|t| shorten(t, MAX_COMMENT_CHARS)))
                .flatten()
                .filter(|t| !t.is_empty()),
            nullable: c.nullable,
        })
        .collect();

    let widest = |f: &dyn Fn(&ErRow) -> usize| rows.iter().map(f).max().unwrap_or(0) as f64;
    let key_w = widest(&|r| r.key.text().len());
    let name_w = widest(&|r| cells(&r.name));
    let type_w = widest(&|r| cells(&r.native_type));
    let comment_w = widest(&|r| r.comment.as_deref().map_or(0, cells));
    let gap = TEXT_GAP_CHARS * cw;
    let key_x = PAD_X;
    let name_x = key_x + if key_w > 0.0 { key_w * cw + gap } else { 0.0 };
    let type_x = name_x + name_w * cw + gap;
    let comment_x = type_x + if type_w > 0.0 { type_w * cw + gap } else { 0.0 };
    let rows_right = comment_x + comment_w * cw;
    let title_right = PAD_X + cells(&title) as f64 * cw;
    let comment_right = PAD_X + comment.as_deref().map_or(0, cells) as f64 * cw;
    let width = (rows_right.max(title_right).max(comment_right) + PAD_X).max(MIN_BOX_WIDTH);
    let rows_top = PAD_Y * 2.0 + lh * if comment.is_some() { 2.0 } else { 1.0 };
    let height = rows_top + rows.len() as f64 * lh + if rows.is_empty() { 0.0 } else { PAD_Y };
    ErNode {
        table: index,
        x: 0.0,
        y: 0.0,
        width: width.ceil(),
        height: height.ceil(),
        title,
        comment,
        rows_top,
        rows,
        key_x,
        name_x,
        type_x,
        comment_x,
        new: false,
    }
}

/// A finite measure in a sensible range, or `fallback`.
fn measure(value: f64, fallback: f64, max: f64) -> f64 {
    if value.is_finite() && value >= 1.0 {
        value.min(max)
    } else {
        fallback
    }
}

/// Places `model`'s tables and routes its relations.
pub fn layout(model: &ErModel, options: &ErLayoutOptions, metrics: &ErMetrics) -> ErLayout {
    arrange(model, options, metrics, &ErArrangement::default())
}

/// A position from an arrangement within bounds (the caller checked it; this keeps the layout
/// finite whatever it is given).
fn bounded(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(-MAX_COORDINATE, MAX_COORDINATE).round()
    } else {
        0.0
    }
}

/// Places `model`'s tables as `arrangement` says (ADR-084) and routes its relations: with no
/// positions, the automatic layout; with some, placed tables where they were put, new ones laid
/// out among themselves below them, and gone ones where they were.
pub fn arrange(
    model: &ErModel,
    options: &ErLayoutOptions,
    metrics: &ErMetrics,
    arrangement: &ErArrangement,
) -> ErLayout {
    let cw = measure(metrics.char_width, 7.0, 64.0);
    // Whole pixels, so rows line up with the relations that point at them.
    let lh = measure(metrics.line_height, 18.0, 128.0).round();
    let mut nodes: Vec<ErNode> = (0..model.tables.len())
        .map(|i| node_for(model, i, options, cw, lh))
        .collect();
    let links: Vec<(usize, usize)> = model.relations.iter().map(|r| (r.from, r.to)).collect();
    let mut gone: Vec<ErGoneNode> = Vec::new();
    if arrangement.positions.is_empty() {
        place(&mut nodes, &links);
    } else {
        let present: HashMap<TableName, usize> = model
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| (t.key(), i))
            .collect();
        let mut placed = vec![false; nodes.len()];
        for p in &arrangement.positions {
            let (x, y) = (bounded(p.x), bounded(p.y));
            match present.get(&p.table) {
                Some(&i) => {
                    nodes[i].x = x;
                    nodes[i].y = y;
                    placed[i] = true;
                }
                None => gone.push(gone_node(model, &p.table, x, y, cw, lh)),
            }
        }
        let new: Vec<usize> = (0..nodes.len()).filter(|&i| !placed[i]).collect();
        if !new.is_empty() {
            let position: HashMap<usize, usize> =
                new.iter().enumerate().map(|(k, &i)| (i, k)).collect();
            let mut block: Vec<ErNode> = new.iter().map(|&i| nodes[i].clone()).collect();
            let block_links: Vec<(usize, usize)> = links
                .iter()
                .filter_map(|(a, b)| Some((*position.get(a)?, *position.get(b)?)))
                .collect();
            place(&mut block, &block_links);
            // Below everything placed, from its left edge.
            let mut left = f64::INFINITY;
            let mut bottom = f64::NEG_INFINITY;
            let placed_boxes = (0..nodes.len())
                .filter(|&i| placed[i])
                .map(|i| (nodes[i].x, nodes[i].y + nodes[i].height))
                .chain(gone.iter().map(|g| (g.x, g.y + g.height)));
            for (x, y) in placed_boxes {
                left = left.min(x);
                bottom = bottom.max(y);
            }
            let (dx, dy) = if left.is_finite() {
                (left - MARGIN, bottom + GROUP_GAP - MARGIN)
            } else {
                (0.0, 0.0)
            };
            for (k, &i) in new.iter().enumerate() {
                nodes[i].x = (block[k].x + dx).round();
                nodes[i].y = (block[k].y + dy).round();
                nodes[i].new = true;
            }
        }
    }
    let edges = route(model, &nodes, lh);
    let notes = note_boxes(arrangement, cw, lh);
    let groups = group_frames(model, arrangement, &nodes, &gone, lh);

    let mut bounds = Bounds::default();
    for n in &nodes {
        bounds.add(n.x, n.y, n.width, n.height);
    }
    for g in &gone {
        bounds.add(g.x, g.y, g.width, g.height);
    }
    for n in &notes {
        bounds.add(n.x, n.y, n.width, n.height);
    }
    for g in &groups {
        bounds.add(g.x, g.y, g.width, g.height);
    }
    for p in edges.iter().flat_map(|e| e.points.iter()) {
        bounds.add(p.x, p.y, 0.0, 0.0);
    }
    let (x, y, width, height) = bounds.with_margin();
    ErLayout {
        x,
        y,
        width,
        height,
        line_height: lh,
        nodes,
        edges,
        gone,
        notes,
        groups,
    }
}

/// The smallest rectangle around what is drawn.
struct Bounds {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            left: f64::INFINITY,
            top: f64::INFINITY,
            right: f64::NEG_INFINITY,
            bottom: f64::NEG_INFINITY,
        }
    }
}

impl Bounds {
    fn add(&mut self, x: f64, y: f64, width: f64, height: f64) {
        self.left = self.left.min(x);
        self.top = self.top.min(y);
        self.right = self.right.max(x + width);
        self.bottom = self.bottom.max(y + height);
    }

    /// Its corner and size with [`MARGIN`] around it, in whole pixels; an empty diagram is a
    /// margin's square at the origin.
    fn with_margin(&self) -> (f64, f64, f64, f64) {
        if !self.left.is_finite() {
            return (0.0, 0.0, 2.0 * MARGIN, 2.0 * MARGIN);
        }
        let x = (self.left - MARGIN).floor();
        let y = (self.top - MARGIN).floor();
        (
            x,
            y,
            (self.right + MARGIN - x).ceil(),
            (self.bottom + MARGIN - y).ceil(),
        )
    }
}

/// The box of a table that is gone: its name only, outlined where it was.
fn gone_node(model: &ErModel, table: &TableName, x: f64, y: f64, cw: f64, lh: f64) -> ErGoneNode {
    let same_schema = model.single_schema()
        && model
            .tables
            .first()
            .is_none_or(|t| t.schema == table.schema);
    let name = if same_schema || table.schema.is_empty() {
        table.name.clone()
    } else {
        format!("{}.{}", table.schema, table.name)
    };
    let title = shorten(&name, MAX_TITLE_CHARS);
    let width = (PAD_X * 2.0 + cells(&title) as f64 * cw).max(MIN_BOX_WIDTH);
    ErGoneNode {
        schema: table.schema.clone(),
        name: table.name.clone(),
        title,
        x,
        y,
        width: width.ceil(),
        height: (PAD_Y * 2.0 + lh).ceil(),
    }
}

/// `text` wrapped in lines of at most `max` cells: by words, a word longer than a line cut,
/// the user's own line breaks kept, tabs and other control characters read as spaces, and no
/// blank lines at either end.
pub(crate) fn wrap(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let flat: String = paragraph
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut line = String::new();
        let mut used = 0;
        for word in flat.split(' ').filter(|w| !w.is_empty()) {
            let width = cells(word);
            if used > 0 && used + 1 + width <= max {
                line.push(' ');
                line.push_str(word);
                used += 1 + width;
                continue;
            }
            if used > 0 {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            for c in word.chars() {
                let w = char_cells(c);
                if used > 0 && used + w > max {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += w;
            }
        }
        lines.push(line);
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let first = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    lines.drain(..first);
    lines
}

fn note_boxes(arrangement: &ErArrangement, cw: f64, lh: f64) -> Vec<ErNoteBox> {
    arrangement
        .notes
        .iter()
        .enumerate()
        .map(|(i, note)| {
            let width = if note.width.is_finite() {
                note.width.clamp(MIN_NOTE_WIDTH, MAX_NOTE_WIDTH).round()
            } else {
                MIN_NOTE_WIDTH
            };
            let columns = ((width - 2.0 * PAD_X) / cw).floor().max(1.0) as usize;
            let mut lines = wrap(&note.text, columns);
            if lines.len() > MAX_NOTE_LINES {
                lines.truncate(MAX_NOTE_LINES);
                if let Some(last) = lines.last_mut() {
                    *last = shorten(&format!("{last} …"), columns);
                }
            }
            let shown = lines.len().max(1) as f64;
            ErNoteBox {
                note: i,
                x: bounded(note.x),
                y: bounded(note.y),
                width,
                height: (PAD_Y * 2.0 + shown * lh).ceil(),
                lines,
                color: note.color,
            }
        })
        .collect()
}

fn group_frames(
    model: &ErModel,
    arrangement: &ErArrangement,
    nodes: &[ErNode],
    gone: &[ErGoneNode],
    lh: f64,
) -> Vec<ErGroupFrame> {
    let present: HashMap<TableName, usize> = model
        .tables
        .iter()
        .enumerate()
        .map(|(i, t)| (t.key(), i))
        .collect();
    arrangement
        .groups
        .iter()
        .enumerate()
        .filter_map(|(i, group)| {
            let mut bounds = Bounds::default();
            for table in &group.tables {
                if let Some(&n) = present.get(table) {
                    let node = &nodes[n];
                    bounds.add(node.x, node.y, node.width, node.height);
                } else if let Some(g) = gone
                    .iter()
                    .find(|g| g.schema == table.schema && g.name == table.name)
                {
                    bounds.add(g.x, g.y, g.width, g.height);
                }
            }
            // A group none of whose tables is drawn has nothing to frame.
            bounds.left.is_finite().then(|| {
                let top = bounds.top - GROUP_PAD - lh;
                ErGroupFrame {
                    group: i,
                    label: shorten(&group.name, MAX_TITLE_CHARS),
                    color: group.color,
                    x: bounds.left - GROUP_PAD,
                    y: top,
                    width: bounds.right - bounds.left + 2.0 * GROUP_PAD,
                    height: bounds.bottom + GROUP_PAD - top,
                }
            })
        })
        .collect()
}

/// Lays out `nodes` (sized already) by `links` between them, from referencing to referenced:
/// groups of joined tables largest first, then the tables with no keys, packed in rows from
/// ([`MARGIN`], [`MARGIN`]).
fn place(nodes: &mut [ErNode], links: &[(usize, usize)]) {
    let n = nodes.len();
    let mut parents = vec![Vec::new(); n];
    let mut neighbours = vec![Vec::new(); n];
    for &(from, to) in links {
        if from != to {
            parents[from].push(to);
            neighbours[from].push(to);
            neighbours[to].push(from);
        }
    }
    for list in parents.iter_mut().chain(neighbours.iter_mut()) {
        list.sort_unstable();
        list.dedup();
    }

    // Groups of tables joined by keys, in table order.
    let mut group_of = vec![usize::MAX; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for start in 0..n {
        if group_of[start] != usize::MAX {
            continue;
        }
        let id = groups.len();
        let mut members = vec![start];
        group_of[start] = id;
        let mut next = 0;
        while next < members.len() {
            let v = members[next];
            next += 1;
            for &u in &neighbours[v] {
                if group_of[u] == usize::MAX {
                    group_of[u] = id;
                    members.push(u);
                }
            }
        }
        members.sort_unstable();
        groups.push(members);
    }
    let (mut joined, single): (Vec<_>, Vec<_>) = groups.into_iter().partition(|g| g.len() > 1);
    joined.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));

    // Each block (a group, or all the single tables) laid out at its own origin.
    let mut blocks: Vec<(Vec<usize>, f64, f64)> = Vec::new();
    for group in joined {
        let (w, h) = place_group(&group, &parents, &neighbours, nodes);
        blocks.push((group, w, h));
    }
    let singles: Vec<usize> = single.into_iter().flatten().collect();
    if !singles.is_empty() {
        let rects: Vec<(f64, f64)> = singles
            .iter()
            .map(|&i| (nodes[i].width, nodes[i].height))
            .collect();
        let (positions, w, h) = shelf_pack(&rects, GAP_Y, GAP_X / 2.0);
        for (&i, (x, y)) in singles.iter().zip(positions) {
            nodes[i].x = x;
            nodes[i].y = y;
        }
        blocks.push((singles, w, h));
    }
    let rects: Vec<(f64, f64)> = blocks.iter().map(|(_, w, h)| (*w, *h)).collect();
    let (positions, _, _) = shelf_pack(&rects, GROUP_GAP, GROUP_GAP);
    for ((members, _, _), (bx, by)) in blocks.iter().zip(positions) {
        for &i in members {
            nodes[i].x = (nodes[i].x + bx + MARGIN).round();
            nodes[i].y = (nodes[i].y + by + MARGIN).round();
        }
    }
}

/// Lays out one group of joined tables at the origin; returns its width and height.
fn place_group(
    group: &[usize],
    parents: &[Vec<usize>],
    neighbours: &[Vec<usize>],
    nodes: &mut [ErNode],
) -> (f64, f64) {
    let n = nodes.len();
    // Columns: the longest chain of keys to a table that references none, cycles broken where
    // a key leads back to a table still being followed.
    let mut state = vec![0u8; n];
    let mut rank = vec![0usize; n];
    let mut dag_children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &start in group {
        if state[start] != 0 {
            continue;
        }
        // An explicit stack: (table, next parent to look at).
        let mut stack = vec![(start, 0usize)];
        state[start] = 1;
        while let Some(top) = stack.last_mut() {
            let v = top.0;
            if let Some(&p) = parents[v].get(top.1) {
                top.1 += 1;
                // A parent already on the stack closes a cycle: the key is left out here.
                if state[p] == 0 {
                    state[p] = 1;
                    stack.push((p, 0));
                }
            } else {
                state[v] = 2;
                stack.pop();
                let mut r = 0;
                for &p in &parents[v] {
                    // Parents still being followed closed a cycle; the others are placed.
                    if state[p] == 2 {
                        r = r.max(rank[p] + 1);
                        dag_children[p].push(v);
                    }
                }
                rank[v] = r;
            }
        }
    }
    // A table that references nothing sits just left of the nearest table referencing it,
    // not in the first column far away from it.
    for &v in group {
        if rank[v] == 0
            && let Some(nearest) = dag_children[v].iter().map(|&c| rank[c]).min()
        {
            rank[v] = nearest.saturating_sub(1);
        }
    }

    let ranks = group.iter().map(|&v| rank[v]).max().unwrap_or(0) + 1;
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); ranks];
    for &v in group {
        layers[rank[v]].push(v);
    }
    let mut pos = vec![0.0f64; n];
    let set_positions = |layer: &[usize], pos: &mut [f64]| {
        let len = layer.len() as f64;
        for (i, &v) in layer.iter().enumerate() {
            pos[v] = (i as f64 + 0.5) / len;
        }
    };
    for layer in &layers {
        set_positions(layer, &mut pos);
    }
    for round in 0..ORDER_ROUNDS * 2 {
        let forwards = round % 2 == 0;
        let order: Vec<usize> = if forwards {
            (1..ranks).collect()
        } else {
            (0..ranks.saturating_sub(1)).rev().collect()
        };
        for r in order {
            let keys: HashMap<usize, f64> = layers[r]
                .iter()
                .map(|&v| {
                    let near: Vec<f64> = neighbours[v]
                        .iter()
                        .filter(|&&u| if forwards { rank[u] < r } else { rank[u] > r })
                        .map(|&u| pos[u])
                        .collect();
                    let key = if near.is_empty() {
                        pos[v]
                    } else {
                        near.iter().sum::<f64>() / near.len() as f64
                    };
                    (v, key)
                })
                .collect();
            layers[r].sort_by(|a, b| keys[a].total_cmp(&keys[b]));
            set_positions(&layers[r], &mut pos);
        }
    }

    // Columns taller than a square group would be are wrapped into several.
    let area: f64 = group
        .iter()
        .map(|&v| (nodes[v].width + GAP_X) * (nodes[v].height + GAP_Y))
        .sum();
    let tallest = group.iter().map(|&v| nodes[v].height).fold(0.0, f64::max);
    let target = tallest.max(area.sqrt() * 1.2);
    let mut columns: Vec<Vec<usize>> = Vec::new();
    for layer in &layers {
        let mut column = Vec::new();
        let mut used = 0.0;
        for &v in layer {
            let h = nodes[v].height + GAP_Y;
            if !column.is_empty() && used + h > target {
                columns.push(std::mem::take(&mut column));
                used = 0.0;
            }
            column.push(v);
            used += h;
        }
        if !column.is_empty() {
            columns.push(column);
        }
    }

    let mut placed = vec![false; n];
    let mut x = 0.0;
    for column in &columns {
        let column_width = column.iter().map(|&v| nodes[v].width).fold(0.0, f64::max);
        let mut bottom = f64::NEG_INFINITY;
        // How far below where it wants to be each table joined to a placed one ended up.
        let mut pushed = Vec::new();
        for &v in column {
            let near: Vec<f64> = neighbours[v]
                .iter()
                .filter(|&&u| placed[u])
                .map(|&u| nodes[u].y + nodes[u].height / 2.0)
                .collect();
            let wanted = (!near.is_empty())
                .then(|| near.iter().sum::<f64>() / near.len() as f64 - nodes[v].height / 2.0);
            let next = if bottom.is_finite() {
                bottom + GAP_Y
            } else {
                0.0
            };
            let top = match wanted {
                Some(wanted) if bottom.is_finite() => wanted.max(next),
                Some(wanted) => wanted,
                None => next,
            };
            if let Some(wanted) = wanted {
                pushed.push(top - wanted);
            }
            nodes[v].x = x + (column_width - nodes[v].width) / 2.0;
            nodes[v].y = top;
            bottom = top + nodes[v].height;
        }
        // The stack is centred on where its tables want to be (otherwise every column drifts
        // further down than the one before), and never above the group's top.
        let shift = if pushed.is_empty() {
            0.0
        } else {
            pushed.iter().sum::<f64>() / pushed.len() as f64
        };
        let top_most = column
            .iter()
            .map(|&v| nodes[v].y - shift)
            .fold(f64::INFINITY, f64::min);
        let shift = if top_most < 0.0 {
            shift + top_most
        } else {
            shift
        };
        for &v in column {
            nodes[v].y -= shift;
        }
        for &v in column {
            placed[v] = true;
        }
        x += column_width + GAP_X;
    }
    let width = group
        .iter()
        .map(|&v| nodes[v].x + nodes[v].width)
        .fold(0.0, f64::max);
    let height = group
        .iter()
        .map(|&v| nodes[v].y + nodes[v].height)
        .fold(0.0, f64::max);
    (width, height)
}

/// Packs rectangles in rows, left to right, wrapping at about the width that makes the whole
/// square; returns each one's top left corner and the total width and height.
fn shelf_pack(rects: &[(f64, f64)], gap_y: f64, gap_x: f64) -> (Vec<(f64, f64)>, f64, f64) {
    let area: f64 = rects.iter().map(|(w, h)| (w + gap_x) * (h + gap_y)).sum();
    let widest = rects.iter().map(|(w, _)| *w).fold(0.0, f64::max);
    let limit = widest.max(area.sqrt() * 1.4);
    let mut positions = Vec::with_capacity(rects.len());
    let (mut x, mut y, mut row_height, mut width) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for &(w, h) in rects {
        if x > 0.0 && x + w > limit {
            y += row_height + gap_y;
            x = 0.0;
            row_height = 0.0;
        }
        positions.push((x, y));
        width = width.max(x + w);
        row_height = row_height.max(h);
        x += w + gap_x;
    }
    (positions, width, y + row_height)
}

/// The middle of the row that lists `column`, if the box lists it.
fn row_y(model: &ErModel, node: &ErNode, column: Option<&String>) -> Option<f64> {
    let column = column?;
    let table = &model.tables[node.table];
    let index = node.rows.iter().position(|r| {
        table
            .columns
            .get(r.column)
            .is_some_and(|c| &c.name == column)
    })?;
    let lh = (node.height - node.rows_top - PAD_Y) / node.rows.len() as f64;
    Some(node.y + node.rows_top + lh * (index as f64 + 0.5))
}

fn route(model: &ErModel, nodes: &[ErNode], lh: f64) -> Vec<ErEdge> {
    struct End {
        side: ErSide,
        y: Option<f64>,
    }
    let ends: Vec<(End, End)> = model
        .relations
        .iter()
        .map(|r| {
            let (from, to) = (&nodes[r.from], &nodes[r.to]);
            let (from_side, to_side) = if r.from == r.to || from.x + from.width < to.x {
                if r.from == r.to {
                    (ErSide::Right, ErSide::Right)
                } else {
                    (ErSide::Right, ErSide::Left)
                }
            } else if from.x > to.x + to.width {
                (ErSide::Left, ErSide::Right)
            } else {
                // One above the other: both ends on the right, bulging out.
                (ErSide::Right, ErSide::Right)
            };
            let mut from_y = row_y(model, from, r.from_columns.first());
            let to_y = row_y(model, to, r.to_columns.first());
            // A key of a table to itself on the same row would draw nothing.
            if r.from == r.to && from_y.is_some() && from_y == to_y {
                from_y = Some(from_y.unwrap_or_default() + lh / 2.0);
            }
            (
                End {
                    side: from_side,
                    y: from_y,
                },
                End {
                    side: to_side,
                    y: to_y,
                },
            )
        })
        .collect();

    // Ends with no row to point at are spread along their side of the box.
    let mut spread: HashMap<(usize, bool), (usize, usize)> = HashMap::new();
    let side_key = |node: usize, side: ErSide| (node, side == ErSide::Right);
    for (r, (from, to)) in model.relations.iter().zip(&ends) {
        for (node, end) in [(r.from, from), (r.to, to)] {
            if end.y.is_none() {
                spread.entry(side_key(node, end.side)).or_default().1 += 1;
            }
        }
    }
    let mut point = |node: usize, end: &End| -> ErPoint {
        let box_ = &nodes[node];
        let x = match end.side {
            ErSide::Left => box_.x,
            ErSide::Right => box_.x + box_.width,
        };
        let y = end.y.unwrap_or_else(|| {
            let slot = spread.entry(side_key(node, end.side)).or_default();
            slot.0 += 1;
            box_.y + box_.height * slot.0 as f64 / (slot.1 as f64 + 1.0)
        });
        ErPoint { x, y }
    };

    model
        .relations
        .iter()
        .zip(&ends)
        .enumerate()
        .map(|(i, (r, (from, to)))| {
            let start = point(r.from, from);
            let end = point(r.to, to);
            let start_stub = ErPoint {
                x: start.x + from.side.sign() * STUB,
                y: start.y,
            };
            let end_stub = ErPoint {
                x: end.x + to.side.sign() * STUB,
                y: end.y,
            };
            let (c1, c2) = if from.side == to.side {
                let x = start_stub.x.max(end_stub.x) + from.side.sign() * BULGE;
                (ErPoint { x, y: start.y }, ErPoint { x, y: end.y })
            } else {
                let pull = ((end_stub.x - start_stub.x).abs() / 2.0).max(STUB * 2.0);
                (
                    ErPoint {
                        x: start_stub.x + from.side.sign() * pull,
                        y: start.y,
                    },
                    ErPoint {
                        x: end_stub.x + to.side.sign() * pull,
                        y: end.y,
                    },
                )
            };
            ErEdge {
                relation: i,
                from: r.from,
                to: r.to,
                name: r.name.clone(),
                from_cardinality: r.from_cardinality,
                to_cardinality: r.to_cardinality,
                from_side: from.side,
                to_side: to.side,
                points: vec![start, start_stub, c1, c2, end_stub, end],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::table;
    use super::*;

    const METRICS: ErMetrics = ErMetrics {
        char_width: 7.0,
        line_height: 18.0,
    };

    fn options(columns: ErColumns) -> ErLayoutOptions {
        ErLayoutOptions {
            columns,
            types: true,
            comments: true,
        }
    }

    fn overlaps(a: &ErNode, b: &ErNode) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    #[test]
    fn texts_are_shortened_by_cells() {
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(shorten("abcd", 3), "ab…");
        assert_eq!(shorten("a\nb", 5), "a b");
        assert_eq!(shorten("表表表", 4), "表…");
        assert_eq!(cells("表a"), 3);
    }

    #[test]
    fn referenced_tables_sit_left_and_ends_point_at_rows() {
        let model = ErModel::from_tables(
            vec![
                table(
                    "s",
                    "country",
                    &[("code", "char(2)", false)],
                    &["code"],
                    &[],
                ),
                table(
                    "s",
                    "customer",
                    &[("id", "int", false), ("country", "char(2)", true)],
                    &["id"],
                    &[("customer_country", &["country"], "country", &["code"])],
                ),
                table(
                    "s",
                    "orders",
                    &[
                        ("id", "int", false),
                        ("note", "text", true),
                        ("customer", "int", false),
                    ],
                    &["id"],
                    &[("orders_customer", &["customer"], "customer", &["id"])],
                ),
                table("s", "lonely", &[("id", "int", false)], &[], &[]),
            ],
            None,
        );
        let out = layout(&model, &options(ErColumns::All), &METRICS);
        let node = |name: &str| {
            let i = model.tables.iter().position(|t| t.name == name).unwrap();
            &out.nodes[i]
        };
        assert!(node("country").x < node("customer").x);
        assert!(node("customer").x < node("orders").x);
        for (i, a) in out.nodes.iter().enumerate() {
            for b in &out.nodes[i + 1..] {
                assert!(!overlaps(a, b), "{} overlaps {}", a.title, b.title);
            }
        }
        let edge = out
            .edges
            .iter()
            .find(|e| e.name == "orders_customer")
            .unwrap();
        let orders = node("orders");
        assert_eq!(edge.from_side, ErSide::Left);
        assert_eq!(edge.to_side, ErSide::Right);
        // The third row of orders ("customer"), and the first of customer ("id").
        let p = &edge.points;
        assert_eq!(p.len(), 6);
        assert_eq!(p[0].x, orders.x);
        assert_eq!(p[0].y, orders.y + orders.rows_top + 18.0 * 2.5);
        let customer = node("customer");
        assert_eq!(p[5].x, customer.x + customer.width);
        assert_eq!(p[5].y, customer.y + customer.rows_top + 9.0);
        assert!(out.width >= orders.x + orders.width && out.height >= orders.y + orders.height);

        // Names only: no rows, and the ends spread along the sides.
        let names = layout(&model, &options(ErColumns::None), &METRICS);
        assert!(names.nodes.iter().all(|n| n.rows.is_empty()));
        assert!(names.nodes[0].height < out.nodes[0].height || out.nodes[0].rows.len() == 1);
        // Keys only keeps orders' id and customer, not its note.
        let keys = layout(&model, &options(ErColumns::Keys), &METRICS);
        let i = model
            .tables
            .iter()
            .position(|t| t.name == "orders")
            .unwrap();
        let listed: Vec<_> = keys.nodes[i].rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(listed, ["id", "customer"]);
        assert_eq!(keys.nodes[i].rows[0].key.text(), "PK");
        assert_eq!(keys.nodes[i].rows[1].key.text(), "FK");
    }

    #[test]
    fn cycles_and_self_references_are_laid_out() {
        let model = ErModel::from_tables(
            vec![
                table(
                    "s",
                    "a",
                    &[
                        ("id", "int", false),
                        ("b", "int", true),
                        ("parent", "int", true),
                    ],
                    &["id"],
                    &[
                        ("a_b", &["b"], "b", &["id"]),
                        ("a_parent", &["parent"], "a", &["id"]),
                    ],
                ),
                table(
                    "s",
                    "b",
                    &[("id", "int", false), ("a", "int", true)],
                    &["id"],
                    &[("b_a", &["a"], "a", &["id"])],
                ),
            ],
            None,
        );
        let out = layout(&model, &options(ErColumns::All), &METRICS);
        assert_eq!(out.edges.len(), 3);
        assert!(!overlaps(&out.nodes[0], &out.nodes[1]));
        let own = out.edges.iter().find(|e| e.name == "a_parent").unwrap();
        assert_eq!((own.from_side, own.to_side), (ErSide::Right, ErSide::Right));
        assert!(own.points[2].x > out.nodes[0].x + out.nodes[0].width);
        assert_ne!(own.points[0].y, own.points[5].y);
    }

    fn name(table: &str) -> TableName {
        TableName {
            schema: "s".into(),
            name: table.into(),
        }
    }

    fn shop() -> ErModel {
        ErModel::from_tables(
            vec![
                table("s", "customer", &[("id", "int", false)], &["id"], &[]),
                table(
                    "s",
                    "orders",
                    &[("id", "int", false), ("customer", "int", false)],
                    &["id"],
                    &[("orders_customer", &["customer"], "customer", &["id"])],
                ),
                table(
                    "s",
                    "line",
                    &[("id", "int", false), ("orders", "int", false)],
                    &["id"],
                    &[("line_orders", &["orders"], "orders", &["id"])],
                ),
                table(
                    "s",
                    "fresh",
                    &[("id", "int", false), ("line", "int", false)],
                    &["id"],
                    &[("fresh_line", &["line"], "line", &["id"])],
                ),
            ],
            None,
        )
    }

    #[test]
    fn placed_tables_stay_new_ones_go_below_and_gone_ones_where_they_were() {
        use crate::er::saved::{ErGroup, ErNote, ErPlacement};
        let model = shop();
        let arrangement = ErArrangement {
            positions: vec![
                ErPlacement {
                    table: name("customer"),
                    x: -300.0,
                    y: -50.0,
                },
                ErPlacement {
                    table: name("orders"),
                    x: 400.0,
                    y: 10.0,
                },
                ErPlacement {
                    table: name("dropped"),
                    x: 100.0,
                    y: 300.0,
                },
            ],
            notes: vec![ErNote {
                x: 900.0,
                y: -200.0,
                width: 160.0,
                text: "Customers come from the CRM; a very long word: abcdefghijklmnopqrstuvwxyz\n\nKeep it."
                    .into(),
                color: ErColor::Amber,
            }],
            groups: vec![
                ErGroup {
                    name: "Sales".into(),
                    color: ErColor::Blue,
                    tables: vec![name("customer"), name("dropped")],
                },
                ErGroup {
                    name: "Nothing here".into(),
                    color: ErColor::Red,
                    tables: vec![name("elsewhere")],
                },
            ],
        };
        let out = arrange(&model, &options(ErColumns::All), &METRICS, &arrangement);
        let node = |n: &str| {
            let i = model.tables.iter().position(|t| t.name == n).unwrap();
            &out.nodes[i]
        };
        assert_eq!((node("customer").x, node("customer").y), (-300.0, -50.0));
        assert_eq!((node("orders").x, node("orders").y), (400.0, 10.0));
        assert!(!node("customer").new && !node("orders").new);
        // The new ones, laid out among themselves (line left of fresh, which references it),
        // below everything placed, the gone table included.
        let (line, fresh) = (node("line"), node("fresh"));
        assert!(line.new && fresh.new);
        assert!(line.x < fresh.x);
        assert!(line.y >= 300.0 + out.gone[0].height + GROUP_GAP - 1.0);
        assert_eq!(line.x, -300.0);
        assert_eq!(out.gone.len(), 1);
        assert_eq!(
            (out.gone[0].name.as_str(), out.gone[0].x, out.gone[0].y),
            ("dropped", 100.0, 300.0)
        );
        // Relations are routed between the boxes where they are: orders is right of customer.
        let edge = out
            .edges
            .iter()
            .find(|e| e.name == "orders_customer")
            .unwrap();
        assert_eq!(
            (edge.from_side, edge.to_side),
            (ErSide::Left, ErSide::Right)
        );
        assert_eq!(edge.points[0].x, 400.0);
        // The note is wrapped to its width: (160 - 20) / 7 = 20 cells a line.
        let note = &out.notes[0];
        assert!(
            note.lines.iter().all(|l| cells(l) <= 20),
            "{:?}",
            note.lines
        );
        assert!(note.lines.contains(&"abcdefghijklmnopqrst".to_string()));
        assert!(note.lines.contains(&String::new()));
        assert_eq!(note.lines.last().map(String::as_str), Some("Keep it."));
        assert_eq!(note.height, 12.0 + note.lines.len() as f64 * 18.0);
        // A group frames its tables there and gone; one with none drawn has no frame.
        assert_eq!(out.groups.len(), 1);
        let frame = &out.groups[0];
        assert_eq!((frame.group, frame.label.as_str()), (0, "Sales"));
        assert!(frame.x < -300.0 && frame.y < -50.0 - 18.0);
        assert!(frame.y + frame.height > 300.0 + out.gone[0].height);
        // The bounds hold everything, left of and above the origin too.
        assert!(out.x < -300.0 - 16.0 && out.y < -200.0);
        assert!(out.x + out.width >= 900.0 + 160.0);
        for n in &out.nodes {
            assert!(out.y + out.height >= n.y + n.height);
        }
    }

    #[test]
    fn without_positions_the_layout_is_automatic() {
        let model = shop();
        let plain = layout(&model, &options(ErColumns::All), &METRICS);
        let same = arrange(
            &model,
            &options(ErColumns::All),
            &METRICS,
            &ErArrangement::default(),
        );
        assert_eq!(plain, same);
        assert_eq!((plain.x, plain.y), (0.0, 0.0));
        assert!(plain.nodes.iter().all(|n| !n.new));
        assert!(plain.gone.is_empty() && plain.notes.is_empty() && plain.groups.is_empty());
        // Fixing every table where the automatic layout put it changes nothing.
        let fixed = ErArrangement {
            positions: model
                .tables
                .iter()
                .zip(&plain.nodes)
                .map(|(t, n)| crate::er::saved::ErPlacement {
                    table: t.key(),
                    x: n.x,
                    y: n.y,
                })
                .collect(),
            ..ErArrangement::default()
        };
        let again = arrange(&model, &options(ErColumns::All), &METRICS, &fixed);
        assert_eq!(again.nodes, plain.nodes);
        assert_eq!(again.edges, plain.edges);
    }

    #[test]
    fn wrapping_keeps_words_and_line_breaks() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("\n\nab\n\ncd\n\n", 5), ["ab", "", "cd"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
        assert_eq!(wrap("a\tb", 5), ["a b"]);
        assert_eq!(wrap("表表表", 4), ["表表", "表"]);
        assert!(wrap("   ", 4).is_empty());
    }

    #[test]
    fn bad_metrics_fall_back() {
        let model = ErModel::from_tables(
            vec![table("s", "t", &[("id", "int", false)], &["id"], &[])],
            None,
        );
        let out = layout(
            &model,
            &options(ErColumns::All),
            &ErMetrics {
                char_width: f64::NAN,
                line_height: -3.0,
            },
        );
        assert!(out.nodes[0].width.is_finite() && out.nodes[0].height > 0.0);
        assert_eq!(out.line_height, 18.0);
    }
}
