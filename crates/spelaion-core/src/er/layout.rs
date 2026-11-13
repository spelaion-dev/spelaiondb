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
//! Everything is in pixels; the UI draws it as it comes and pans or zooms with a transform.

use std::collections::HashMap;

use super::{Cardinality, ErModel, ErTable};

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
    pub width: f64,
    pub height: f64,
    pub line_height: f64,
    pub nodes: Vec<ErNode>,
    pub edges: Vec<ErEdge>,
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
    let cw = measure(metrics.char_width, 7.0, 64.0);
    // Whole pixels, so rows line up with the relations that point at them.
    let lh = measure(metrics.line_height, 18.0, 128.0).round();
    let mut nodes: Vec<ErNode> = (0..model.tables.len())
        .map(|i| node_for(model, i, options, cw, lh))
        .collect();
    let n = nodes.len();

    let mut parents = vec![Vec::new(); n];
    let mut neighbours = vec![Vec::new(); n];
    for r in &model.relations {
        if r.from != r.to {
            parents[r.from].push(r.to);
            neighbours[r.from].push(r.to);
            neighbours[r.to].push(r.from);
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
        let (w, h) = place_group(&group, &parents, &neighbours, &mut nodes);
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
    let (positions, width, height) = shelf_pack(&rects, GROUP_GAP, GROUP_GAP);
    for ((members, _, _), (bx, by)) in blocks.iter().zip(positions) {
        for &i in members {
            nodes[i].x = (nodes[i].x + bx + MARGIN).round();
            nodes[i].y = (nodes[i].y + by + MARGIN).round();
        }
    }
    let edges = route(model, &nodes, lh);
    let right = edges
        .iter()
        .flat_map(|e| e.points.iter().map(|p| p.x))
        .fold(width + MARGIN, f64::max);
    ErLayout {
        width: (right + MARGIN).ceil(),
        height: (height + 2.0 * MARGIN).ceil(),
        line_height: lh,
        nodes,
        edges,
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
