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
