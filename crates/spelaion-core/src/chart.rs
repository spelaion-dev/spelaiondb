//! Charts from a result (ADR-040): the core reads the stored result and prepares the series a
//! chart draws, so the UI only ever receives points, never rows (ADR-010).
//!
//! - The chart reads the rows of the current view (the grid's sort and filters), as the calc
//!   panel and copies do.
//! - Rows that share an X value are combined with the chosen aggregate (sum by default), as a
//!   `GROUP BY x` would. One point per X value, never more than [`MAX_CHART_POINTS`].
//! - A continuous X (numbers, dates, times, timestamps) is drawn in order. With more than
//!   [`MAX_CHART_POINTS`] different values, its range is split into that many equal ranges, each
//!   combining its rows.
//! - Any other X (text, booleans…) keeps the order in which its values first appear. Past
//!   [`MAX_CHART_POINTS`] values, the rest are combined in a last "Other" point.
//! - A pie has one series and its largest [`MAX_PIE_SLICES`] slices (the last one "Other");
//!   values that are zero, negative or empty have no slice and are counted.
//! - What was combined, and how many rows had no number, travels with the points so the UI
//!   can say so.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use base64::Engine as _;
use spelaion_driver_api::{LogicalType, Value};

use crate::results::StoredResult;
use crate::selection_stats::StatsTicket;
use crate::sort::ViewError;
use crate::sql_file::replace_atomically;

/// Most points (X positions) a chart sends to the UI, whatever the number of rows.
pub const MAX_CHART_POINTS: usize = 2_000;

/// Most series (Y columns) in one chart: one per color of the theme's chart palette.
pub const MAX_CHART_SERIES: usize = 8;

/// Most slices of a pie, "Other" included.
pub const MAX_PIE_SLICES: usize = 8;

/// Most different X values a pie ranks before the rest go straight to "Other".
pub const MAX_PIE_GROUPS: usize = 100_000;

/// Longest label sent for an X value (longer ones end in "…").
pub const MAX_LABEL_CHARS: usize = 100;

/// Largest chart image the core writes to a file.
pub const MAX_CHART_IMAGE_BYTES: usize = 32 * 1024 * 1024;

/// How the drivers write ±infinity in dates and timestamps (`spelaion-driver-api`'s value
/// module): such values have no place on an axis.
const INFINITE_DATES: [i32; 2] = [i32::MAX, i32::MIN];
const INFINITE_MICROS: [i64; 2] = [i64::MAX, i64::MIN];

/// Rows read between two checks of whether the chart was replaced or cancelled.
const CHECK_EVERY: usize = 4_096;

/// The kind of chart. Bars and lines take up to [`MAX_CHART_SERIES`] Y columns; a pie takes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    Bar,
    Line,
    Pie,
}

/// How the Y values of the rows that share a point are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartAggregate {
    Sum,
    Avg,
    Min,
    Max,
    /// The Y cells that hold a number.
    Count,
}

/// What to draw: the X column and the Y columns, by position in the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartRequest {
    pub kind: ChartKind,
    pub x: usize,
    pub y: Vec<usize>,
    pub aggregate: ChartAggregate,
}

/// The label of a point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChartLabel {
    /// The X value as the grid shows it (cut at [`MAX_LABEL_CHARS`]).
    Value(String),
    Null,
    /// The rows of the values that were not drawn on their own.
    Other,
}

/// One Y column: a value per point (`None`: no row of that point had a number).
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSeries {
    pub column: usize,
    pub name: String,
    pub values: Vec<Option<f64>>,
}

/// How the rows became points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartReduction {
    /// One point per row: no two rows share an X value.
    Rows,
    /// Rows that share an X value were combined into one point.
    Grouped,
    /// The X range was split into equal ranges, each combining its rows.
    Ranges,
    /// Some values were combined in "Other" (the last point): past the first
    /// [`MAX_CHART_POINTS`] values, or past a pie's largest slices.
    Other,
}

/// The points of a chart, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    /// One label per point; at most [`MAX_CHART_POINTS`].
    pub labels: Vec<ChartLabel>,
    /// Where each point sits on a continuous X axis (numbers; days for dates, microseconds for
    /// times and timestamps), for lines. `None` for any other X and for pies.
    pub positions: Option<Vec<f64>>,
    pub series: Vec<ChartSeries>,
    pub reduction: ChartReduction,
    /// Rows of the current view that were read.
    pub rows: u64,
    /// Different X values seen; a lower bound when `groups_capped`.
    pub groups: u64,
    pub groups_capped: bool,
    /// Rows left out because their X is NULL or not a finite value on a continuous axis.
    pub skipped_rows: u64,
    /// Y cells that are NULL or not a finite number: no aggregate counts them.
    pub empty_values: u64,
    /// Pie values with no slice because they are zero, negative or empty.
    pub hidden_slices: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChartError {
    #[error(transparent)]
    View(#[from] ViewError),
    #[error("choose at least one column for the values")]
    NoSeries,
    #[error("a chart draws at most {MAX_CHART_SERIES} columns of values")]
    TooManySeries,
    #[error("a pie draws one column of values")]
    PieSeries,
    #[error("a pie adds its values up: choose Sum or Count")]
    PieAggregate,
    #[error("column \"{0}\" does not hold numbers")]
    NotNumeric(String),
    #[error("the chart image is not a {0} image")]
    NotAnImage(&'static str),
    #[error("the chart image is too large ({bytes} bytes; at most {MAX_CHART_IMAGE_BYTES})")]
    ImageTooLarge { bytes: usize },
    #[error("could not write {path}: {message}")]
    File { path: String, message: String },
}

/// The values a point combines, for every aggregate at once.
#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    sum: f64,
    count: u64,
    min: f64,
    max: f64,
}

impl Acc {
    fn add(&mut self, value: f64) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        } else {
            self.min = self.min.min(value);
            self.max = self.max.max(value);
        }
        self.sum += value;
        self.count += 1;
    }

    fn merge(&mut self, other: &Acc) {
        if other.count == 0 {
            return;
        }
        if self.count == 0 {
            *self = *other;
            return;
        }
        self.sum += other.sum;
        self.count += other.count;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    fn value(&self, aggregate: ChartAggregate) -> Option<f64> {
        let value = match aggregate {
            ChartAggregate::Count => return Some(self.count as f64),
            _ if self.count == 0 => return None,
            ChartAggregate::Sum => self.sum,
            ChartAggregate::Avg => self.sum / self.count as f64,
            ChartAggregate::Min => self.min,
            ChartAggregate::Max => self.max,
        };
        value.is_finite().then_some(value)
    }
}

/// A point being built: its label, its place on a continuous axis and one accumulator per
/// series.
#[derive(Debug, Clone)]
struct Group {
    label: ChartLabel,
    key: Option<f64>,
    accs: Vec<Acc>,
}

impl Group {
    fn new(label: ChartLabel, key: Option<f64>, series: usize) -> Self {
        Self {
            label,
            key,
            accs: vec![Acc::default(); series],
        }
    }

    fn merge(&mut self, other: &Group) {
        for (acc, theirs) in self.accs.iter_mut().zip(&other.accs) {
            acc.merge(theirs);
        }
    }
}

/// Whether X values of this type sit on a continuous, ordered axis.
fn is_continuous(logical: LogicalType) -> bool {
    match logical {
        LogicalType::Int
        | LogicalType::Float
        | LogicalType::Decimal
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz => true,
        LogicalType::Bool
        | LogicalType::Text
        | LogicalType::Bytes
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => false,
    }
}

/// Whether a Y column holds numbers.
fn is_numeric(logical: LogicalType) -> bool {
    match logical {
        LogicalType::Int | LogicalType::Float | LogicalType::Decimal => true,
        LogicalType::Bool
        | LogicalType::Text
        | LogicalType::Bytes
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => false,
    }
}

/// A Y cell as a finite number.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => f.is_finite().then_some(*f),
        Value::Decimal(text) => text.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        Value::Null
        | Value::Bool(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::TimestampTz(_)
        | Value::Uuid(_)
        | Value::Json(_)
        | Value::Interval(_)
        | Value::Array(_)
        | Value::Native(_) => None,
    }
}

/// Where an X value sits on a continuous axis; `None` for NULL and infinite values.
fn axis_key(value: &Value) -> Option<f64> {
    match value {
        Value::Date(d) => (!INFINITE_DATES.contains(d)).then_some(f64::from(*d)),
        Value::Time(t) => Some(*t as f64),
        Value::Timestamp(t) | Value::TimestampTz(t) => {
            (!INFINITE_MICROS.contains(t)).then_some(*t as f64)
        }
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Uuid(_)
        | Value::Json(_)
        | Value::Interval(_)
        | Value::Array(_)
        | Value::Native(_) => number(value),
    }
}

/// A label's text, cut at [`MAX_LABEL_CHARS`].
fn label(text: &str) -> ChartLabel {
    match text.char_indices().nth(MAX_LABEL_CHARS) {
        None => ChartLabel::Value(text.to_string()),
        Some((cut, _)) => ChartLabel::Value(format!("{}…", &text[..cut])),
    }
}

/// Reads the cells of the current view a chart needs.
struct Reader<'a> {
    result: &'a StoredResult,
    x: (usize, LogicalType),
    y: Vec<(usize, LogicalType)>,
    ticket: &'a StatsTicket,
}

impl Reader<'_> {
    /// Calls `f` with the X value and the Y values of every row of the view. `false`: the
    /// chart was replaced or cancelled.
    fn each(&self, mut f: impl FnMut(Value, &[Option<f64>])) -> Result<bool, ViewError> {
        let mut ys = vec![None; self.y.len()];
        for position in 0..self.result.rows() {
            if position % CHECK_EVERY == 0 && self.ticket.is_stale() {
                return Ok(false);
            }
            let (batch, row) = self.result.locate(self.result.original_row(position));
            for (slot, &(column, logical)) in ys.iter_mut().zip(&self.y) {
                let value = Value::from_array(batch.column(column).as_ref(), row, logical)
                    .ok_or(ViewError::UnexpectedArray(column))?;
                *slot = number(&value);
            }
            let (column, logical) = self.x;
            let x = Value::from_array(batch.column(column).as_ref(), row, logical)
                .ok_or(ViewError::UnexpectedArray(column))?;
            f(x, &ys);
        }
        Ok(!self.ticket.is_stale())
    }

    /// Only the X values, for a first pass.
    fn each_x(&self, mut f: impl FnMut(Value)) -> Result<bool, ViewError> {
        let (column, logical) = self.x;
        for position in 0..self.result.rows() {
            if position % CHECK_EVERY == 0 && self.ticket.is_stale() {
                return Ok(false);
            }
            let (batch, row) = self.result.locate(self.result.original_row(position));
            let x = Value::from_array(batch.column(column).as_ref(), row, logical)
                .ok_or(ViewError::UnexpectedArray(column))?;
            f(x);
        }
        Ok(!self.ticket.is_stale())
    }
}

/// Points before they become series, and what the reduction counted.
struct Points {
    groups: Vec<Group>,
    reduction: ChartReduction,
    distinct: u64,
    distinct_capped: bool,
    skipped_rows: u64,
    hidden_slices: u64,
}

/// Prepares the points of a chart from the current view of a stored result. `None`: a newer
/// chart on the same result, or a cancel, stopped this one.
pub(crate) fn prepare(
    result: &StoredResult,
    request: &ChartRequest,
    ticket: &StatsTicket,
) -> Result<Option<ChartData>, ChartError> {
    let schema = result.schema();
    let column = |index: usize| {
        schema
            .columns
            .get(index)
            .ok_or(ChartError::View(ViewError::NoSuchColumn(index)))
    };
    let x_meta = column(request.x)?;
    let mut y: Vec<(usize, LogicalType)> = Vec::with_capacity(request.y.len());
    for &index in &request.y {
        let meta = column(index)?;
        if !is_numeric(meta.logical) {
            return Err(ChartError::NotNumeric(meta.name.clone()));
        }
        if !y.iter().any(|&(c, _)| c == index) {
            y.push((index, meta.logical));
        }
    }
    if y.is_empty() {
        return Err(ChartError::NoSeries);
    }
    if y.len() > MAX_CHART_SERIES {
        return Err(ChartError::TooManySeries);
    }
    if request.kind == ChartKind::Pie {
        if y.len() != 1 {
            return Err(ChartError::PieSeries);
        }
        if !matches!(
            request.aggregate,
            ChartAggregate::Sum | ChartAggregate::Count
        ) {
            return Err(ChartError::PieAggregate);
        }
    }
    let reader = Reader {
        result,
        x: (request.x, x_meta.logical),
        y: y.clone(),
        ticket,
    };
    let mut empty_values = 0u64;
    let continuous = request.kind != ChartKind::Pie && is_continuous(x_meta.logical);
    let points = if continuous {
        continuous_points(&reader, &mut empty_values)?
    } else {
        categorical_points(&reader, request, &mut empty_values)?
    };
    let Some(points) = points else {
        return Ok(None);
    };
    let positions = continuous.then(|| {
        points
            .groups
            .iter()
            .map(|g| g.key.unwrap_or_default())
            .collect()
    });
    let series = y
        .iter()
        .enumerate()
        .map(|(slot, &(index, _))| ChartSeries {
            column: index,
            name: schema.columns[index].name.clone(),
            values: points
                .groups
                .iter()
                .map(|g| g.accs[slot].value(request.aggregate))
                .collect(),
        })
        .collect();
    Ok(Some(ChartData {
        labels: points.groups.into_iter().map(|g| g.label).collect(),
        positions,
        series,
        reduction: points.reduction,
        rows: result.rows() as u64,
        groups: points.distinct,
        groups_capped: points.distinct_capped,
        skipped_rows: points.skipped_rows,
        empty_values,
        hidden_slices: points.hidden_slices,
    }))
}

/// Adds a row's Y values to a point, counting the empty ones.
fn add_row(group: &mut Group, ys: &[Option<f64>], empty: &mut u64) {
    for (acc, y) in group.accs.iter_mut().zip(ys) {
        match y {
            Some(v) => acc.add(*v),
            None => *empty += 1,
        }
    }
}

/// Numbers, dates and times: one point per value in order, or equal ranges past
/// [`MAX_CHART_POINTS`] values.
fn continuous_points(reader: &Reader, empty: &mut u64) -> Result<Option<Points>, ChartError> {
    // First pass: the range, and whether the values fit one point each.
    let mut text = String::new();
    let mut seen: HashSet<Box<str>> = HashSet::new();
    let mut capped = false;
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut skipped = 0u64;
    let done = reader.each_x(|x| {
        let Some(key) = axis_key(&x) else {
            skipped += 1;
            return;
        };
        min = min.min(key);
        max = max.max(key);
        if !capped {
            text.clear();
            let _ = write!(text, "{x}");
            if !seen.contains(text.as_str()) {
                if seen.len() == MAX_CHART_POINTS {
                    capped = true;
                } else {
                    seen.insert(text.as_str().into());
                }
            }
        }
    })?;
    if !done {
        return Ok(None);
    }
    let distinct = if capped {
        MAX_CHART_POINTS as u64 + 1
    } else {
        seen.len() as u64
    };
    drop(seen);
    let series = reader.y.len();
    let mut plotted = 0u64;
    if !capped {
        // One point per value, in the axis's order.
        let mut index: HashMap<Box<str>, usize> = HashMap::new();
        let mut groups: Vec<Group> = Vec::new();
        let done = reader.each(|x, ys| {
            let Some(key) = axis_key(&x) else {
                return;
            };
            plotted += 1;
            text.clear();
            let _ = write!(text, "{x}");
            let at = match index.get(text.as_str()) {
                Some(&at) => at,
                None => {
                    groups.push(Group::new(label(&text), Some(key), series));
                    index.insert(text.as_str().into(), groups.len() - 1);
                    groups.len() - 1
                }
            };
            add_row(&mut groups[at], ys, empty);
        })?;
        if !done {
            return Ok(None);
        }
        groups.sort_by(|a, b| {
            a.key
                .unwrap_or_default()
                .total_cmp(&b.key.unwrap_or_default())
        });
        let reduction = if groups.len() as u64 == plotted {
            ChartReduction::Rows
        } else {
            ChartReduction::Grouped
        };
        return Ok(Some(Points {
            groups,
            reduction,
            distinct,
            distinct_capped: false,
            skipped_rows: skipped,
            hidden_slices: 0,
        }));
    }
    // Equal ranges over [min, max]; each labelled with its smallest value.
    let width = (max - min) / MAX_CHART_POINTS as f64;
    let mut ranges: Vec<Option<Group>> = vec![None; MAX_CHART_POINTS];
    let done = reader.each(|x, ys| {
        let Some(key) = axis_key(&x) else {
            return;
        };
        let at = if width > 0.0 && width.is_finite() {
            (((key - min) / width) as usize).min(MAX_CHART_POINTS - 1)
        } else {
            0
        };
        let range = ranges[at].get_or_insert_with(|| {
            text.clear();
            let _ = write!(text, "{x}");
            Group::new(label(&text), Some(key), series)
        });
        if range.key.is_some_and(|k| key < k) {
            text.clear();
            let _ = write!(text, "{x}");
            range.label = label(&text);
            range.key = Some(key);
        }
        add_row(range, ys, empty);
    })?;
    if !done {
        return Ok(None);
    }
    Ok(Some(Points {
        groups: ranges.into_iter().flatten().collect(),
        reduction: ChartReduction::Ranges,
        distinct,
        distinct_capped: true,
        skipped_rows: skipped,
        hidden_slices: 0,
    }))
}

/// Text and other values, and every pie: one point per value in the order they first appear;
/// past the cap, the rest in "Other". A pie then keeps its largest slices.
fn categorical_points(
    reader: &Reader,
    request: &ChartRequest,
    empty: &mut u64,
) -> Result<Option<Points>, ChartError> {
    let pie = request.kind == ChartKind::Pie;
    let cap = if pie {
        MAX_PIE_GROUPS
    } else {
        MAX_CHART_POINTS
    };
    let series = reader.y.len();
    let mut text = String::new();
    let mut index: HashMap<Box<str>, usize> = HashMap::new();
    let mut null: Option<usize> = None;
    let mut groups: Vec<Group> = Vec::new();
    let mut other: Option<Group> = None;
    let done = reader.each(|x, ys| {
        let at = if matches!(x, Value::Null) {
            null.or_else(|| {
                (groups.len() < cap).then(|| {
                    groups.push(Group::new(ChartLabel::Null, None, series));
                    null = Some(groups.len() - 1);
                    groups.len() - 1
                })
            })
        } else {
            text.clear();
            let _ = write!(text, "{x}");
            index.get(text.as_str()).copied().or_else(|| {
                (groups.len() < cap).then(|| {
                    groups.push(Group::new(label(&text), None, series));
                    index.insert(text.as_str().into(), groups.len() - 1);
                    groups.len() - 1
                })
            })
        };
        let group = match at {
            Some(at) => &mut groups[at],
            None => other.get_or_insert_with(|| Group::new(ChartLabel::Other, None, series)),
        };
        add_row(group, ys, empty);
    })?;
    if !done {
        return Ok(None);
    }
    let capped = other.is_some();
    let distinct = groups.len() as u64 + u64::from(capped);
    let rows = reader.result.rows() as u64;
    if !pie {
        let reduction = if let Some(mut rest) = other {
            // The last kept value joins "Other", so the points stay within the cap.
            if let Some(last) = groups.pop() {
                rest.merge(&last);
            }
            groups.push(rest);
            ChartReduction::Other
        } else if groups.len() as u64 == rows {
            ChartReduction::Rows
        } else {
            ChartReduction::Grouped
        };
        return Ok(Some(Points {
            groups,
            reduction,
            distinct,
            distinct_capped: capped,
            skipped_rows: 0,
            hidden_slices: 0,
        }));
    }
    // A pie: the largest positive values, the rest in "Other".
    let value = |g: &Group| g.accs[0].value(request.aggregate);
    let total_groups = groups.len();
    let mut slices: Vec<(f64, Group)> = groups
        .into_iter()
        .filter_map(|g| value(&g).filter(|v| *v > 0.0).map(|v| (v, g)))
        .collect();
    let hidden = (total_groups - slices.len()) as u64;
    slices.sort_by(|a, b| b.0.total_cmp(&a.0));
    let folded = slices.len() > MAX_PIE_SLICES || other.is_some();
    let mut kept: Vec<Group> = Vec::with_capacity(MAX_PIE_SLICES);
    let mut rest = other.unwrap_or_else(|| Group::new(ChartLabel::Other, None, series));
    let keep = if folded {
        MAX_PIE_SLICES - 1
    } else {
        MAX_PIE_SLICES
    };
    for (i, (_, g)) in slices.into_iter().enumerate() {
        if i < keep {
            kept.push(g);
        } else {
            rest.merge(&g);
        }
    }
    let reduction = if folded {
        if value(&rest).is_some_and(|v| v > 0.0) {
            kept.push(rest);
        }
        ChartReduction::Other
    } else if total_groups as u64 == rows {
        ChartReduction::Rows
    } else {
        ChartReduction::Grouped
    };
    Ok(Some(Points {
        groups: kept,
        reduction,
        distinct,
        distinct_capped: capped,
        skipped_rows: 0,
        hidden_slices: hidden,
    }))
}

/// The format of a chart image the UI drew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartImageFormat {
    Png,
    Svg,
}

/// Writes a chart image the UI drew to `path`, replacing the file whole: an SVG as its text,
/// a PNG from base64 (optionally a `data:image/png;base64,` URL). Anything that is not such an
/// image is refused, so this never writes arbitrary bytes; an SVG with a script or embedded HTML
/// is refused too (the app's charts have neither).
pub fn save_chart_image(
    path: &Path,
    format: ChartImageFormat,
    data: &str,
) -> Result<(), ChartError> {
    let bytes = match format {
        ChartImageFormat::Svg => {
            let text = data.trim_start_matches('\u{feff}').trim_start();
            let starts = text.starts_with("<svg") || text.starts_with("<?xml");
            let lower = text.to_ascii_lowercase();
            let active = lower.contains("<script") || lower.contains("<foreignobject");
            if !starts || active || !lower.contains("<svg") || !text.trim_end().ends_with("</svg>")
            {
                return Err(ChartError::NotAnImage("SVG"));
            }
            data.as_bytes().to_vec()
        }
        ChartImageFormat::Png => {
            let encoded = data.strip_prefix("data:image/png;base64,").unwrap_or(data);
            if encoded.len() / 4 * 3 > MAX_CHART_IMAGE_BYTES {
                return Err(ChartError::ImageTooLarge {
                    bytes: encoded.len() / 4 * 3,
                });
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .map_err(|_| ChartError::NotAnImage("PNG"))?;
            if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                return Err(ChartError::NotAnImage("PNG"));
            }
            bytes
        }
    };
    if bytes.len() > MAX_CHART_IMAGE_BYTES {
        return Err(ChartError::ImageTooLarge { bytes: bytes.len() });
    }
    replace_atomically(path, &bytes).map_err(|e| ChartError::File {
        path: path.display().to_string(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Date32Array, Float64Array, Int64Array, RecordBatch, StringArray};
    use spelaion_driver_api::{ColumnMeta, ResultSchema};

    use super::*;
    use crate::ResultStore;
    use crate::filter::{ColumnFilter, FilterOp};
    use crate::results::ResultId;
    use crate::sort::SortKey;

    fn meta(name: &str, logical: LogicalType) -> ColumnMeta {
        ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
            origin: None,
        }
    }

    /// A result with these columns, appended in batches of `batch` rows built by `columns`.
    fn result(
        metas: Vec<ColumnMeta>,
        rows: usize,
        batch: usize,
        columns: impl Fn(std::ops::Range<usize>) -> Vec<ArrayRef>,
    ) -> (ResultStore, ResultId) {
        let schema = Arc::new(ResultSchema::new(metas));
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        let mut start = 0;
        while start < rows {
            let end = (start + batch).min(rows);
            store
                .append(
                    id,
                    RecordBatch::try_new(schema.arrow.clone(), columns(start..end)).unwrap(),
                )
                .unwrap();
            start = end;
        }
        store.complete(id);
        (store, id)
    }

    /// city (text), month (int), sales (decimal), units (float, one NULL), day (date).
    fn sales() -> (ResultStore, ResultId) {
        let cities = [
            Some("Lyon"),
            Some("Oslo"),
            Some("Lyon"),
            None,
            Some("Oslo"),
            Some("Rome"),
        ];
        let months = [1i64, 2, 3, 1, 2, 3];
        let sales = ["10.5", "20", "-3.5", "1", "4", "7"];
        let units = [Some(1.0), Some(2.0), None, Some(4.0), Some(5.0), Some(6.0)];
        let days = [19_000, 19_001, 19_002, 19_000, 19_001, 19_005];
        result(
            vec![
                meta("city", LogicalType::Text),
                meta("month", LogicalType::Int),
                meta("sales", LogicalType::Decimal),
                meta("units", LogicalType::Float),
                meta("day", LogicalType::Date),
            ],
            6,
            4,
            |r| {
                vec![
                    Arc::new(StringArray::from(cities[r.clone()].to_vec())),
                    Arc::new(Int64Array::from(months[r.clone()].to_vec())),
                    Arc::new(StringArray::from(sales[r.clone()].to_vec())),
                    Arc::new(Float64Array::from(units[r.clone()].to_vec())),
                    Arc::new(Date32Array::from(days[r].to_vec())),
                ]
            },
        )
    }

    fn chart(r: &StoredResult, kind: ChartKind, x: usize, y: &[usize]) -> ChartData {
        chart_with(r, kind, x, y, ChartAggregate::Sum)
    }

    fn chart_with(
        r: &StoredResult,
        kind: ChartKind,
        x: usize,
        y: &[usize],
        aggregate: ChartAggregate,
    ) -> ChartData {
        let request = ChartRequest {
            kind,
            x,
            y: y.to_vec(),
            aggregate,
        };
        r.chart(&request, &r.chart_ticket()).unwrap().unwrap()
    }

    fn texts(data: &ChartData) -> Vec<String> {
        data.labels
            .iter()
            .map(|l| match l {
                ChartLabel::Value(t) => t.clone(),
                ChartLabel::Null => "<null>".into(),
                ChartLabel::Other => "<other>".into(),
            })
            .collect()
    }

    #[test]
    fn text_x_groups_rows_in_the_order_values_first_appear() {
        let (store, id) = sales();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let data = chart(&r, ChartKind::Bar, 0, &[2, 3]);
        assert_eq!(texts(&data), ["Lyon", "Oslo", "<null>", "Rome"]);
        assert_eq!(data.reduction, ChartReduction::Grouped);
        assert_eq!(data.groups, 4);
        assert_eq!(data.rows, 6);
        assert!(data.positions.is_none());
        assert_eq!(data.series[0].name, "sales");
        assert_eq!(
            data.series[0].values,
            [Some(7.0), Some(24.0), Some(1.0), Some(7.0)]
        );
        // Lyon's second row has no units: the sum takes the other one, and it is counted.
        assert_eq!(
            data.series[1].values,
            [Some(1.0), Some(7.0), Some(4.0), Some(6.0)]
        );
        assert_eq!(data.empty_values, 1);
    }

    #[test]
    fn every_aggregate_combines_the_same_rows() {
        let (store, id) = sales();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let values = |aggregate| {
            chart_with(&r, ChartKind::Bar, 0, &[2], aggregate).series[0]
                .values
                .clone()
        };
        assert_eq!(values(ChartAggregate::Avg)[0], Some(3.5));
        assert_eq!(values(ChartAggregate::Min)[0], Some(-3.5));
        assert_eq!(values(ChartAggregate::Max)[0], Some(10.5));
        assert_eq!(
            values(ChartAggregate::Count),
            [Some(2.0), Some(2.0), Some(1.0), Some(1.0)]
        );
        // Count counts only the cells with a number; no number at all is 0, not empty.
        let units = chart_with(&r, ChartKind::Bar, 1, &[3], ChartAggregate::Count);
        assert_eq!(units.series[0].values, [Some(2.0), Some(2.0), Some(1.0)]);
        let avg = chart_with(&r, ChartKind::Line, 1, &[3], ChartAggregate::Avg);
        assert_eq!(avg.series[0].values, [Some(2.5), Some(3.5), Some(6.0)]);
    }

    #[test]
    fn a_numeric_or_date_x_is_drawn_in_order_with_its_positions() {
        let (store, id) = sales();
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        r.sort(Some(SortKey {
            column: 1,
            descending: true,
        }))
        .unwrap();
        let data = chart(&r, ChartKind::Line, 1, &[2]);
        assert_eq!(texts(&data), ["1", "2", "3"]);
        assert_eq!(data.positions.as_deref(), Some(&[1.0, 2.0, 3.0][..]));
        assert_eq!(data.series[0].values, [Some(11.5), Some(24.0), Some(3.5)]);
        assert_eq!(data.reduction, ChartReduction::Grouped);
        let days = chart(&r, ChartKind::Line, 4, &[3]);
        assert_eq!(
            texts(&days),
            ["2022-01-08", "2022-01-09", "2022-01-10", "2022-01-13"]
        );
        assert_eq!(
            days.positions.as_deref(),
            Some(&[19_000.0, 19_001.0, 19_002.0, 19_005.0][..])
        );
        // The third day's only row has no units: the point is there, empty.
        assert_eq!(days.series[0].values[2], None);
    }

    #[test]
    fn the_chart_reads_the_filtered_view() {
        let (store, id) = sales();
        let r = store.get(id).unwrap();
        let mut r = r.write().unwrap();
        r.filter_columns(vec![ColumnFilter {
            column: 0,
            op: FilterOp::Eq,
            value: "Lyon".into(),
        }])
        .unwrap();
        let data = chart(&r, ChartKind::Bar, 1, &[2]);
        assert_eq!(data.rows, 2);
        assert_eq!(data.reduction, ChartReduction::Rows);
        assert_eq!(data.series[0].values, [Some(10.5), Some(-3.5)]);
    }

    #[test]
    fn a_pie_keeps_positive_values_and_one_series() {
        let (store, id) = sales();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let data = chart(&r, ChartKind::Pie, 0, &[2]);
        // Largest first; NULL keeps its own slice.
        assert_eq!(texts(&data), ["Oslo", "Lyon", "Rome", "<null>"]);
        assert_eq!(
            data.series[0].values,
            [Some(24.0), Some(7.0), Some(7.0), Some(1.0)]
        );
        assert!(data.positions.is_none(), "a pie has no axis");
        let request = |y: Vec<usize>, aggregate| ChartRequest {
            kind: ChartKind::Pie,
            x: 0,
            y,
            aggregate,
        };
        let ticket = r.chart_ticket();
        assert_eq!(
            r.chart(&request(vec![2, 3], ChartAggregate::Sum), &ticket),
            Err(ChartError::PieSeries)
        );
        assert_eq!(
            r.chart(&request(vec![2], ChartAggregate::Avg), &ticket),
            Err(ChartError::PieAggregate)
        );
    }

    #[test]
    fn requests_that_cannot_be_drawn_are_refused() {
        let (store, id) = sales();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let request = |x, y: Vec<usize>| ChartRequest {
            kind: ChartKind::Bar,
            x,
            y,
            aggregate: ChartAggregate::Sum,
        };
        let ticket = r.chart_ticket();
        assert_eq!(
            r.chart(&request(1, vec![0]), &ticket),
            Err(ChartError::NotNumeric("city".into()))
        );
        assert_eq!(
            r.chart(&request(1, vec![4]), &ticket),
            Err(ChartError::NotNumeric("day".into()))
        );
        assert_eq!(
            r.chart(&request(1, vec![]), &ticket),
            Err(ChartError::NoSeries)
        );
        assert_eq!(
            r.chart(&request(9, vec![2]), &ticket),
            Err(ChartError::View(ViewError::NoSuchColumn(9)))
        );
        assert_eq!(
            r.chart(
                &request(0, vec![2; 9].into_iter().chain([3]).collect()),
                &ticket
            ),
            Ok(r.chart(&request(0, vec![2, 3]), &ticket).unwrap()),
            "repeated columns count once"
        );
    }

    /// The "Done when" of the item: a million rows never send more than the fixed number of
    /// points, whatever the X.
    #[test]
    fn a_million_rows_send_at_most_the_fixed_number_of_points() {
        const ROWS: usize = 1_000_000;
        let (store, id) = result(
            vec![
                meta("id", LogicalType::Int),
                meta("name", LogicalType::Text),
                meta("price", LogicalType::Float),
                meta("qty", LogicalType::Int),
            ],
            ROWS,
            65_536,
            |r| {
                vec![
                    Arc::new(Int64Array::from_iter_values(r.clone().map(|i| i as i64))),
                    Arc::new(StringArray::from_iter_values(
                        r.clone().map(|i| format!("item {i}")),
                    )),
                    Arc::new(Float64Array::from_iter_values(
                        r.clone().map(|i| i as f64 * 0.5),
                    )),
                    Arc::new(Int64Array::from_iter_values(r.map(|i| (i % 3) as i64))),
                ]
            },
        );
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();

        // A numeric X: equal ranges, in order, every row counted once.
        let line = chart(&r, ChartKind::Line, 0, &[3, 2]);
        assert_eq!(line.rows, ROWS as u64);
        assert_eq!(line.reduction, ChartReduction::Ranges);
        assert!(line.groups_capped);
        assert_eq!(line.labels.len(), MAX_CHART_POINTS);
        assert_eq!(line.series.len(), 2);
        assert!(
            line.series
                .iter()
                .all(|s| s.values.len() == MAX_CHART_POINTS)
        );
        let positions = line.positions.as_ref().unwrap();
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(texts(&line)[0], "0");
        assert_eq!(texts(&line)[1], "500");
        let counted = chart_with(&r, ChartKind::Bar, 0, &[3], ChartAggregate::Count);
        let total: f64 = counted.series[0].values.iter().flatten().sum();
        assert_eq!(total, ROWS as f64);
        assert!(counted.labels.len() <= MAX_CHART_POINTS);

        // A text X with a million values: the first ones, then "Other" with the rest.
        let bars = chart_with(&r, ChartKind::Bar, 1, &[3], ChartAggregate::Count);
        assert_eq!(bars.reduction, ChartReduction::Other);
        assert_eq!(bars.labels.len(), MAX_CHART_POINTS);
        assert_eq!(bars.labels.last(), Some(&ChartLabel::Other));
        assert_eq!(
            bars.series[0].values.last(),
            Some(&Some((ROWS - MAX_CHART_POINTS + 1) as f64))
        );

        // A pie over a million values: at most its fixed number of slices.
        let pie = chart(&r, ChartKind::Pie, 1, &[2]);
        assert!(pie.labels.len() <= MAX_PIE_SLICES);
        assert_eq!(pie.labels.last(), Some(&ChartLabel::Other));
        // Only the first values are ranked; the rest went straight to "Other".
        assert!(pie.groups_capped);
        assert_eq!(texts(&pie)[0], format!("item {}", MAX_PIE_GROUPS - 1));
        assert_eq!(pie.hidden_slices, 1, "item 0 is worth 0");

        // Few X values over a million rows: exact groups.
        let few = chart(&r, ChartKind::Bar, 3, &[3]);
        assert_eq!(texts(&few), ["0", "1", "2"]);
        assert_eq!(few.reduction, ChartReduction::Grouped);
    }

    #[test]
    fn a_newer_chart_stops_an_older_one() {
        let (store, id) = sales();
        let r = store.get(id).unwrap();
        let r = r.read().unwrap();
        let old = r.chart_ticket();
        let _newer = r.chart_ticket();
        let request = ChartRequest {
            kind: ChartKind::Bar,
            x: 0,
            y: vec![2],
            aggregate: ChartAggregate::Sum,
        };
        assert_eq!(r.chart(&request, &old), Ok(None));
        let current = r.chart_ticket();
        r.cancel_chart();
        assert_eq!(r.chart(&request, &current), Ok(None));
    }

    #[test]
    fn long_labels_are_cut() {
        assert_eq!(label("abc"), ChartLabel::Value("abc".into()));
        let long = "é".repeat(MAX_LABEL_CHARS + 5);
        let ChartLabel::Value(cut) = label(&long) else {
            panic!("a value label");
        };
        assert_eq!(cut.chars().count(), MAX_LABEL_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn only_png_and_svg_images_are_written() {
        let dir = std::env::temp_dir().join(format!("spelaion-chart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let svg = dir.join("chart.svg");
        save_chart_image(
            &svg,
            ChartImageFormat::Svg,
            "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        )
        .unwrap();
        assert!(std::fs::read_to_string(&svg).unwrap().starts_with("<svg"));
        let png = dir.join("chart.png");
        let bytes = b"\x89PNG\r\n\x1a\nrest";
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        save_chart_image(
            &png,
            ChartImageFormat::Png,
            &format!("data:image/png;base64,{encoded}"),
        )
        .unwrap();
        assert_eq!(std::fs::read(&png).unwrap(), bytes);

        let other = dir.join("other.bin");
        let not_png = base64::engine::general_purpose::STANDARD.encode(b"MZ\x90\x00");
        assert_eq!(
            save_chart_image(&other, ChartImageFormat::Png, &not_png),
            Err(ChartError::NotAnImage("PNG"))
        );
        assert_eq!(
            save_chart_image(&other, ChartImageFormat::Png, "not base64 at all!"),
            Err(ChartError::NotAnImage("PNG"))
        );
        assert_eq!(
            save_chart_image(&other, ChartImageFormat::Svg, "#!/bin/sh\necho <svg></svg>"),
            Err(ChartError::NotAnImage("SVG"))
        );
        assert_eq!(
            save_chart_image(&other, ChartImageFormat::Svg, "<svg></svg><script>"),
            Err(ChartError::NotAnImage("SVG"))
        );
        assert_eq!(
            save_chart_image(
                &other,
                ChartImageFormat::Svg,
                "<svg><SCRIPT>x()</SCRIPT></svg>"
            ),
            Err(ChartError::NotAnImage("SVG"))
        );
        assert!(!other.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
