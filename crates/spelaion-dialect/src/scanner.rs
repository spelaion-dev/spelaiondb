//! What a dialect's lexical scanner gives (ADR-065): the script as a lazy sequence of regions,
//! each a byte range with a kind. A dialect's splitter, its `code_ranges` and its other readers
//! all read the one scanner, so none of them can see code where another sees a string, a
//! comment or a client's line. It is an iterator over the text, never a token list: a dump of
//! millions of lines is read in one pass, holding no more than the region at hand.

use std::ops::Range;

use spelaion_driver_api::StatementSpan;

/// One region of a script, as its dialect's scanner reads it. Regions follow each other in the
/// script's order, never overlap, and leave nothing out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Region {
    pub(crate) kind: RegionKind,
    pub(crate) range: Range<usize>,
}

/// What a region of a script is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegionKind {
    /// SQL outside every other kind: words, operators, white space, a `;` that ends no
    /// statement.
    Code,
    /// A string literal, with its quotes.
    String,
    /// A quoted name, with its quotes.
    QuotedName,
    /// A comment, with its markers (and a line comment's newline).
    Comment,
    /// A comment whose body the server may run (MySQL's `/*! … */`, MariaDB's `/*M! … */`),
    /// with its markers. Not code for the readers that look inside code, but it makes its
    /// statement one: a statement written only as executable comments is a statement.
    ExecutableComment,
    /// A body quoted by the dialect's own markers (PostgreSQL's `$tag$ … $tag$`).
    QuotedBody,
    /// A client's line of its own, which is not SQL (psql's `\connect`), without its newline.
    /// Code as a whole: no string or comment in it opens. `statement`: the line as a statement
    /// of its own, which the session refuses; `None` for a line the splitter skips.
    MetaLine { statement: Option<StatementSpan> },
    /// The data after a `COPY … FROM STDIN;`, sent apart from its statement.
    CopyData,
    /// The line that ends that data (psql's `\.`).
    CopyEnd,
    /// Where a statement ends: its separator, or the end of the script (an empty range).
    /// `statement`: the statement it ends, `None` when that held no code.
    StatementEnd { statement: Option<StatementSpan> },
}

impl RegionKind {
    /// Whether the region is code for `Dialect::code_ranges`: where readers such as the
    /// placeholder scanner look.
    pub(crate) fn is_code(self) -> bool {
        match self {
            RegionKind::Code | RegionKind::MetaLine { .. } | RegionKind::StatementEnd { .. } => {
                true
            }
            RegionKind::String
            | RegionKind::QuotedName
            | RegionKind::Comment
            | RegionKind::ExecutableComment
            | RegionKind::QuotedBody
            | RegionKind::CopyData
            | RegionKind::CopyEnd => false,
        }
    }

    /// The statement the region gives the splitter, if any.
    pub(crate) fn statement(self) -> Option<StatementSpan> {
        match self {
            RegionKind::MetaLine { statement } | RegionKind::StatementEnd { statement } => {
                statement
            }
            RegionKind::Code
            | RegionKind::String
            | RegionKind::QuotedName
            | RegionKind::Comment
            | RegionKind::ExecutableComment
            | RegionKind::QuotedBody
            | RegionKind::CopyData
            | RegionKind::CopyEnd => None,
        }
    }
}

/// The statements a scanner's regions give: what a dialect's splitter answers.
pub(crate) fn statements(regions: impl Iterator<Item = Region>) -> Vec<StatementSpan> {
    regions
        .filter_map(|region| region.kind.statement())
        .collect()
}

/// The code a scanner's regions hold, adjacent regions joined: what a dialect's `code_ranges`
/// answers.
pub(crate) fn code_ranges(regions: impl Iterator<Item = Region>) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for region in regions {
        if !region.kind.is_code() || region.range.is_empty() {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end >= region.range.start => {
                last.end = last.end.max(region.range.end);
            }
            _ => ranges.push(region.range),
        }
    }
    ranges
}

/// A dialect's one reading of what opens a region other than code at a byte: the region's kind
/// and the byte after it, or `None` when only code starts there.
pub(crate) type Opening = fn(&str, usize) -> Option<(RegionKind, usize)>;

/// The scanner of a dialect whose statements end at a `;` outside the regions its [`Opening`]
/// reads (strings, names, comments, quoted bodies), and whose words decide nothing about where
/// a statement ends: DuckDB, ClickHouse, Trino, Hive, Spanner, Databend. A statement holds code
/// once it holds anything but white space (the shared rule) and comments.
pub(crate) struct SemicolonScanner<'a> {
    script: &'a str,
    opening: Opening,
    /// Where the next region starts.
    i: usize,
    /// Where the current statement starts.
    start: usize,
    /// Whether the current statement holds code yet.
    has_code: bool,
    /// The end of the script was reached.
    ended: bool,
}

impl<'a> SemicolonScanner<'a> {
    pub(crate) fn new(script: &'a str, opening: Opening) -> Self {
        Self {
            script,
            opening,
            i: 0,
            start: 0,
            has_code: false,
            ended: false,
        }
    }
}

impl Iterator for SemicolonScanner<'_> {
    type Item = Region;

    fn next(&mut self) -> Option<Region> {
        let script = self.script;
        let b = script.as_bytes();
        let len = b.len();
        let code = self.i;
        while self.i < len {
            let i = self.i;
            if let Some((kind, end)) = (self.opening)(script, i) {
                if code < i {
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..i,
                    });
                }
                if kind != RegionKind::Comment {
                    self.has_code = true;
                }
                self.i = end.max(i + 1);
                return Some(Region {
                    kind,
                    range: i..self.i,
                });
            }
            if b[i] == b';' {
                if code < i {
                    return Some(Region {
                        kind: RegionKind::Code,
                        range: code..i,
                    });
                }
                let statement = crate::trimmed(script, self.start, i).filter(|_| self.has_code);
                self.i = i + 1;
                self.start = i + 1;
                self.has_code = false;
                return Some(Region {
                    kind: RegionKind::StatementEnd { statement },
                    range: i..i + 1,
                });
            }
            if !crate::is_white_space_at(script, i) {
                self.has_code = true;
            }
            self.i += 1;
        }
        if code < len {
            return Some(Region {
                kind: RegionKind::Code,
                range: code..len,
            });
        }
        if self.ended {
            return None;
        }
        self.ended = true;
        // The end of the script ends the last statement.
        let statement = crate::trimmed(script, self.start, len).filter(|_| self.has_code)?;
        Some(Region {
            kind: RegionKind::StatementEnd {
                statement: Some(statement),
            },
            range: len..len,
        })
    }
}
