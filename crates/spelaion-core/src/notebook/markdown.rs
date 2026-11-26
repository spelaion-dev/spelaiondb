//! Markdown cells read into a small block model (ADR-087, point 18), which the page draws as
//! React elements and [`super::html`] as escaped HTML: never the Markdown's own HTML.
//!
//! The subset: ATX headings (`#` to `######`), paragraphs, block quotes, bulleted and numbered
//! lists, fenced code (backticks or tildes), thematic breaks and pipe tables; inside them
//! strong, emphasis, code spans, links, images (as their text and address, never fetched),
//! backslash escapes and hard line breaks. Anything else is the text it is written with: raw
//! HTML stays text. Nesting is bounded, and a very long cell is shown as plain text, so no
//! input makes reading it slow or deep.

use serde::Serialize;

use super::format::unix_lines;

/// Deepest nesting of block quotes and of inline spans; deeper is read as text.
const MAX_DEPTH: usize = 8;
/// A cell longer than this, in bytes, is one plain paragraph: the inline reader looks ahead
/// for closing marks, and this keeps its cost bounded.
pub const MAX_MARKDOWN_BYTES: usize = 256 * 1024;

/// A block of a Markdown cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Block {
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Paragraph {
        content: Vec<Inline>,
    },
    Quote {
        blocks: Vec<Block>,
    },
    List {
        ordered: bool,
        /// The first item's number, for a numbered list.
        start: u32,
        items: Vec<Vec<Inline>>,
    },
    Code {
        language: Option<String>,
        text: String,
    },
    Rule,
    Table {
        align: Vec<Align>,
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
}

/// A table column's alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// A span of text inside a block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Inline {
    Text {
        text: String,
    },
    Strong {
        content: Vec<Inline>,
    },
    Emphasis {
        content: Vec<Inline>,
    },
    Code {
        text: String,
    },
    /// Shown as its text and its address; never followed.
    Link {
        content: Vec<Inline>,
        url: String,
    },
    /// Shown as its alternative text and address; never fetched.
    Image {
        alt: String,
        url: String,
    },
    LineBreak,
}

/// Reads a Markdown cell's text into blocks.
pub fn read(text: &str) -> Vec<Block> {
    let text = unix_lines(text);
    if text.len() > MAX_MARKDOWN_BYTES {
        return vec![Block::Paragraph {
            content: vec![Inline::Text { text }],
        }];
    }
    let lines: Vec<&str> = text.split('\n').collect();
    blocks(&lines, 0)
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// A fence's character and length, and the info string after it.
fn fence(line: &str) -> Option<(char, usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let c = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let count = trimmed.chars().take_while(|d| *d == c).count();
    let info = &trimmed[count..];
    (count >= 3 && !(c == '`' && info.contains('`'))).then_some((c, count, info.trim()))
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let level = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    // A closing run of `#` is not part of the heading.
    let rest = rest.trim();
    let rest = rest.trim_end_matches('#');
    let rest = if rest.is_empty() || rest.ends_with([' ', '\t']) {
        rest.trim_end()
    } else {
        rest.trim()
    };
    // `# #` leaves nothing; `#hash#tag` was not a heading above.
    Some((level as u8, rest))
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(c) = trimmed
        .chars()
        .next()
        .filter(|c| matches!(c, '-' | '*' | '_'))
    else {
        return false;
    };
    trimmed
        .chars()
        .filter(|d| !d.is_whitespace())
        .all(|d| d == c)
        && trimmed.chars().filter(|d| *d == c).count() >= 3
        && line.len() - line.trim_start().len() <= 3
}

/// A list item's kind (numbered with its number, or bulleted with its mark) and its text.
fn list_item(line: &str) -> Option<(Option<u32>, char, &str)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    if let Some(c) = trimmed
        .chars()
        .next()
        .filter(|c| matches!(c, '-' | '*' | '+'))
    {
        let rest = &trimmed[1..];
        return (rest.starts_with(' ') || rest.is_empty()).then_some((None, c, rest.trim()));
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=9).contains(&digits) {
        return None;
    }
    let mark = trimmed[digits..]
        .chars()
        .next()
        .filter(|c| matches!(c, '.' | ')'))?;
    let rest = &trimmed[digits + 1..];
    if !(rest.starts_with(' ') || rest.is_empty()) {
        return None;
    }
    Some((trimmed[..digits].parse().ok(), mark, rest.trim()))
}

fn quote_line(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let rest = trimmed.strip_prefix('>')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// The cells of a table row: split at `|` that is not escaped nor inside a code span.
fn table_cells(line: &str) -> Vec<String> {
    let mut trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix('|') {
        trimmed = rest;
    }
    if trimmed.ends_with('|') && !trimmed.ends_with("\\|") {
        trimmed = &trimmed[..trimmed.len() - 1];
    }
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut in_code = false;
    let mut chars = trimmed.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '`' => {
                in_code = !in_code;
                cell.push(c);
            }
            '|' if !in_code => cells.push(std::mem::take(&mut cell).trim().to_string()),
            _ => cell.push(c),
        }
    }
    cells.push(cell.trim().to_string());
    cells
}

/// The alignments of a table's delimiter row, or `None` when the line is not one.
fn delimiter_row(line: &str) -> Option<Vec<Align>> {
    if !line.contains('-') {
        return None;
    }
    table_cells(line)
        .iter()
        .map(|cell| {
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            let dashes = cell.trim_matches(':');
            (!dashes.is_empty() && dashes.bytes().all(|b| b == b'-')).then_some(
                match (left, right) {
                    (true, true) => Align::Center,
                    (true, false) => Align::Left,
                    (false, true) => Align::Right,
                    (false, false) => Align::None,
                },
            )
        })
        .collect()
}

/// Whether `line` starts a block other than a paragraph (so it ends a paragraph).
fn starts_block(line: &str) -> bool {
    fence(line).is_some()
        || heading(line).is_some()
        || is_rule(line)
        || quote_line(line).is_some()
        || list_item(line).is_some_and(|(_, _, text)| !text.is_empty())
}

fn blocks(lines: &[&str], depth: usize) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_blank(line) {
            i += 1;
        } else if let Some((c, count, info)) = fence(line) {
            let close = (i + 1..lines.len()).find(|&j| {
                fence(lines[j]).is_some_and(|(d, n, rest)| d == c && n >= count && rest.is_empty())
            });
            let end = close.unwrap_or(lines.len());
            let language = info.split_whitespace().next().map(str::to_string);
            out.push(Block::Code {
                language,
                text: lines[i + 1..end].join("\n"),
            });
            i = end + 1;
        } else if let Some((level, text)) = heading(line) {
            out.push(Block::Heading {
                level,
                content: inlines(text, depth),
            });
            i += 1;
        } else if is_rule(line) {
            out.push(Block::Rule);
            i += 1;
        } else if quote_line(line).is_some() {
            let mut inner = Vec::new();
            while let Some(rest) = lines.get(i).and_then(|l| quote_line(l)) {
                inner.push(rest);
                i += 1;
            }
            out.push(if depth + 1 >= MAX_DEPTH {
                Block::Paragraph {
                    content: vec![Inline::Text {
                        text: inner.join("\n"),
                    }],
                }
            } else {
                Block::Quote {
                    blocks: blocks(&inner, depth + 1),
                }
            });
        } else if let Some((number, mark, _)) = list_item(line) {
            let ordered = number.is_some();
            let mut items: Vec<String> = Vec::new();
            while let Some(current) = lines.get(i) {
                match list_item(current) {
                    Some((n, m, text)) if n.is_some() == ordered && m == mark => {
                        items.push(text.to_string());
                        i += 1;
                    }
                    Some(_) => break,
                    None if !is_blank(current)
                        && current.starts_with("  ")
                        && !items.is_empty() =>
                    {
                        // A continuation line of the item above.
                        if let Some(last) = items.last_mut() {
                            last.push('\n');
                            last.push_str(current.trim());
                        }
                        i += 1;
                    }
                    None if is_blank(current)
                        && lines
                            .get(i + 1)
                            .and_then(|next| list_item(next))
                            .is_some_and(|(n, m, _)| n.is_some() == ordered && m == mark) =>
                    {
                        i += 1;
                    }
                    None => break,
                }
            }
            out.push(Block::List {
                ordered,
                start: number.unwrap_or(1),
                items: items.iter().map(|item| inlines(item, depth)).collect(),
            });
        } else if line.contains('|')
            && let Some(align) = lines.get(i + 1).and_then(|next| delimiter_row(next))
            && table_cells(line).len() == align.len()
        {
            let header = table_cells(line)
                .iter()
                .map(|c| inlines(c, depth))
                .collect();
            i += 2;
            let mut rows = Vec::new();
            while let Some(row) = lines.get(i).filter(|l| !is_blank(l) && l.contains('|')) {
                let mut cells: Vec<Vec<Inline>> =
                    table_cells(row).iter().map(|c| inlines(c, depth)).collect();
                cells.resize(align.len(), Vec::new());
                rows.push(cells);
                i += 1;
            }
            out.push(Block::Table {
                align,
                header,
                rows,
            });
        } else {
            let start = i;
            i += 1;
            while let Some(next) = lines.get(i) {
                if is_blank(next) || starts_block(next) {
                    break;
                }
                i += 1;
            }
            let text = lines[start..i]
                .iter()
                .map(|l| l.trim_start())
                .collect::<Vec<_>>()
                .join("\n");
            out.push(Block::Paragraph {
                content: inlines(&text, depth),
            });
        }
    }
    out
}

/// Reads the spans of a block's text. Line breaks are soft (a space) unless the line ends with
/// two spaces or a backslash.
fn inlines(text: &str, depth: usize) -> Vec<Inline> {
    let mut reader = Inlines {
        out: Vec::new(),
        text: String::new(),
    };
    reader.read(text, depth);
    reader.finish()
}

struct Inlines {
    out: Vec<Inline>,
    text: String,
}

impl Inlines {
    fn flush(&mut self) {
        if !self.text.is_empty() {
            self.out.push(Inline::Text {
                text: std::mem::take(&mut self.text),
            });
        }
    }

    fn push(&mut self, inline: Inline) {
        self.flush();
        self.out.push(inline);
    }

    fn finish(mut self) -> Vec<Inline> {
        self.flush();
        self.out
    }

    fn read(&mut self, text: &str, depth: usize) {
        if depth >= MAX_DEPTH {
            self.text.push_str(text);
            return;
        }
        let mut i = 0;
        let bytes = text.as_bytes();
        // Where a search for a closing mark already failed: a later opener of the same mark
        // cannot find one either, so it is not searched again (this keeps unclosed marks
        // from making the reading quadratic).
        let mut no_closer: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        while i < text.len() {
            let rest = &text[i..];
            let c = rest.chars().next().unwrap_or(' ');
            match c {
                '\\' => {
                    let next = rest[1..].chars().next();
                    match next {
                        Some(p) if p.is_ascii_punctuation() => {
                            self.text.push(p);
                            i += 2;
                        }
                        Some('\n') => {
                            self.push(Inline::LineBreak);
                            i += 2;
                        }
                        _ => {
                            self.text.push('\\');
                            i += 1;
                        }
                    }
                }
                '\n' => {
                    if self.text.ends_with("  ") {
                        let kept = self.text.trim_end().len();
                        self.text.truncate(kept);
                        self.push(Inline::LineBreak);
                    } else {
                        let kept = self.text.trim_end_matches(' ').len();
                        self.text.truncate(kept);
                        self.text.push(' ');
                    }
                    i += 1;
                    while text[i..].starts_with(' ') {
                        i += 1;
                    }
                }
                '`' => {
                    let run = rest.bytes().take_while(|&b| b == b'`').count();
                    let after = &rest[run..];
                    let key = "`".repeat(run.min(64));
                    let close = if no_closer.get(&key).is_some_and(|&from| i >= from) {
                        None
                    } else {
                        find_run(after, run)
                    };
                    if close.is_none() {
                        no_closer.insert(key, i);
                    }
                    match close {
                        Some(at) => {
                            let inner = after[..at].replace('\n', " ");
                            let inner = if inner.len() >= 2
                                && inner.starts_with(' ')
                                && inner.ends_with(' ')
                                && !inner.trim().is_empty()
                            {
                                inner[1..inner.len() - 1].to_string()
                            } else {
                                inner
                            };
                            self.push(Inline::Code { text: inner });
                            i += run + at + run;
                        }
                        None => {
                            self.text.push_str(&rest[..run]);
                            i += run;
                        }
                    }
                }
                '!' if rest[1..].starts_with('[') => match link(&rest[1..]) {
                    Some((label, url, used)) => {
                        self.push(Inline::Image {
                            alt: plain(&label),
                            url,
                        });
                        i += 1 + used;
                    }
                    None => {
                        self.text.push('!');
                        i += 1;
                    }
                },
                '[' => match link(rest) {
                    Some((label, url, used)) => {
                        let content = inlines(&label, depth + 1);
                        self.push(Inline::Link { content, url });
                        i += used;
                    }
                    None => {
                        self.text.push('[');
                        i += 1;
                    }
                },
                '*' | '_' => {
                    let run = rest.bytes().take_while(|&b| b == c as u8).count();
                    let strong = run >= 2;
                    let width = if strong { 2 } else { 1 };
                    let before = text[..i].chars().next_back();
                    let opens = rest[width..]
                        .chars()
                        .next()
                        .is_some_and(|n| !n.is_whitespace())
                        && !(c == '_' && before.is_some_and(char::is_alphanumeric));
                    let marker = &rest[..width];
                    let close = if opens && !no_closer.get(marker).is_some_and(|&from| i >= from) {
                        let found = find_closer(&rest[width..], marker, c);
                        if found.is_none() {
                            no_closer.insert(marker.to_string(), i);
                        }
                        found
                    } else {
                        None
                    };
                    match close {
                        Some(at) => {
                            let content = inlines(&rest[width..width + at], depth + 1);
                            self.push(if strong {
                                Inline::Strong { content }
                            } else {
                                Inline::Emphasis { content }
                            });
                            i += width + at + width;
                        }
                        None => {
                            self.text.push_str(&rest[..run]);
                            i += run;
                        }
                    }
                }
                _ => {
                    self.text.push(c);
                    i += c.len_utf8();
                }
            }
            debug_assert!(i <= bytes.len());
        }
    }
}

/// Where a run of exactly `run` backticks starts in `text`.
fn find_run(text: &str, run: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if len == run {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// Where the emphasis that `marker` opened closes in `text`: the marker after a character that
/// is not white space, not part of a longer run of the same character for a single mark, and
/// for `_` not followed by a letter or digit.
fn find_closer(text: &str, marker: &str, c: char) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = text[from..].find(marker).map(|i| from + i) {
        let before = text[..at].chars().next_back();
        let after = text[at + marker.len()..].chars().next();
        let longer = marker.len() == 1 && (after == Some(c) || before == Some(c));
        let flanked = before.is_some_and(|b| !b.is_whitespace());
        let word_after = c == '_' && after.is_some_and(char::is_alphanumeric);
        if at > 0 && flanked && !longer && !word_after {
            return Some(at);
        }
        from = at + marker.len().max(1);
        if longer {
            from = at + text[at..].chars().take_while(|&d| d == c).count();
        }
    }
    None
}

/// A link `[label](url)` or `[label](url "title")` at the start of `text`: its label, its
/// address and how many bytes it took.
fn link(text: &str) -> Option<(String, String, usize)> {
    // A label is short: past this many bytes it is not looked for, which keeps text full of
    // `[` from making the reading quadratic.
    const MAX_LABEL: usize = 1_000;
    let bytes = &text.as_bytes()[..text.len().min(MAX_LABEL)];
    let mut level = 0usize;
    let mut close = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'[' => level += 1,
            b']' => {
                level -= 1;
                if level == 0 {
                    close = Some(i);
                    break;
                }
            }
            b'\n' if i + 1 < bytes.len() && bytes[i + 1] == b'\n' => return None,
            _ => {}
        }
        i += 1;
    }
    let close = close?;
    let rest = text[close + 1..].strip_prefix('(')?;
    // The address may hold balanced parentheses (`javascript:alert(1)`, a wiki page's name).
    let mut depth = 0usize;
    let mut end = None;
    for (at, c) in rest.char_indices().take(MAX_LABEL) {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => {
                end = Some(at);
                break;
            }
            ')' => depth -= 1,
            '\n' => return None,
            _ => {}
        }
    }
    let end = end?;
    let inside = rest[..end].trim();
    let url = match inside.split_once(char::is_whitespace) {
        Some((url, title)) if title.trim().starts_with(['"', '\'']) => url,
        Some(_) => return None,
        None => inside,
    };
    let url = url.trim_start_matches('<').trim_end_matches('>');
    Some((
        text[1..close].to_string(),
        url.to_string(),
        close + 1 + 1 + end + 1,
    ))
}

/// The text of a label without its marks, for an image's alternative text.
fn plain(label: &str) -> String {
    fn walk(inlines: &[Inline], out: &mut String) {
        for inline in inlines {
            match inline {
                Inline::Text { text } | Inline::Code { text } => out.push_str(text),
                Inline::Strong { content }
                | Inline::Emphasis { content }
                | Inline::Link { content, .. } => walk(content, out),
                Inline::Image { alt, .. } => out.push_str(alt),
                Inline::LineBreak => out.push(' '),
            }
        }
    }
    let mut out = String::new();
    walk(&inlines(label, 0), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str) -> Inline {
        Inline::Text { text: text.into() }
    }

    fn para(content: Vec<Inline>) -> Block {
        Block::Paragraph { content }
    }

    #[test]
    fn headings_paragraphs_and_rules() {
        assert_eq!(
            read("# Title #\n\nSome text\non two lines\n\n---\n###### Six\n####### seven"),
            vec![
                Block::Heading {
                    level: 1,
                    content: vec![t("Title")]
                },
                para(vec![t("Some text on two lines")]),
                Block::Rule,
                Block::Heading {
                    level: 6,
                    content: vec![t("Six")]
                },
                para(vec![t("####### seven")]),
            ]
        );
        assert_eq!(read("#hashtag"), vec![para(vec![t("#hashtag")])]);
    }

    #[test]
    fn inline_marks() {
        assert_eq!(
            read("a **strong** and *em* and _em_ and `co*de` and snake_case_name"),
            vec![para(vec![
                t("a "),
                Inline::Strong {
                    content: vec![t("strong")]
                },
                t(" and "),
                Inline::Emphasis {
                    content: vec![t("em")]
                },
                t(" and "),
                Inline::Emphasis {
                    content: vec![t("em")]
                },
                t(" and "),
                Inline::Code {
                    text: "co*de".into()
                },
                t(" and snake_case_name"),
            ])]
        );
        assert_eq!(read("2 * 3 * 4"), vec![para(vec![t("2 * 3 * 4")])]);
        assert_eq!(read("\\*not\\*"), vec![para(vec![t("*not*")])]);
        assert_eq!(
            read("``a ` b``"),
            vec![para(vec![Inline::Code {
                text: "a ` b".into()
            }])]
        );
        assert_eq!(
            read("unclosed `tick"),
            vec![para(vec![t("unclosed `tick")])]
        );
    }

    #[test]
    fn links_and_images_keep_their_address_as_text() {
        assert_eq!(
            read("see [the **docs**](https://x.test/a \"t\") ![chart](c.png)"),
            vec![para(vec![
                t("see "),
                Inline::Link {
                    content: vec![
                        t("the "),
                        Inline::Strong {
                            content: vec![t("docs")]
                        }
                    ],
                    url: "https://x.test/a".into()
                },
                t(" "),
                Inline::Image {
                    alt: "chart".into(),
                    url: "c.png".into()
                },
            ])]
        );
        assert_eq!(read("[not a link]"), vec![para(vec![t("[not a link]")])]);
    }

    #[test]
    fn raw_html_stays_text() {
        assert_eq!(
            read("<script>alert(1)</script>\n<b>x</b>"),
            vec![para(vec![t("<script>alert(1)</script> <b>x</b>")])]
        );
    }

    #[test]
    fn hard_line_breaks() {
        assert_eq!(
            read("one  \ntwo\\\nthree"),
            vec![para(vec![
                t("one"),
                Inline::LineBreak,
                t("two"),
                Inline::LineBreak,
                t("three")
            ])]
        );
    }

    #[test]
    fn lists_quotes_code_and_tables() {
        assert_eq!(
            read(
                "- a\n  more\n- b\n\n3. c\n4. d\n\n> quoted\n> # in\n\n```sql x\nselect 1\n```\n\n| a | b |\n|:--|--:|\n| 1 | `x|y` |\n| 2 |"
            ),
            vec![
                Block::List {
                    ordered: false,
                    start: 1,
                    items: vec![vec![t("a more")], vec![t("b")]]
                },
                Block::List {
                    ordered: true,
                    start: 3,
                    items: vec![vec![t("c")], vec![t("d")]]
                },
                Block::Quote {
                    blocks: vec![
                        para(vec![t("quoted")]),
                        Block::Heading {
                            level: 1,
                            content: vec![t("in")]
                        }
                    ]
                },
                Block::Code {
                    language: Some("sql".into()),
                    text: "select 1".into()
                },
                Block::Table {
                    align: vec![Align::Left, Align::Right],
                    header: vec![vec![t("a")], vec![t("b")]],
                    rows: vec![
                        vec![vec![t("1")], vec![Inline::Code { text: "x|y".into() }]],
                        vec![vec![t("2")], vec![]],
                    ]
                },
            ]
        );
        assert_eq!(
            read("```\nunclosed\n# not a heading"),
            vec![Block::Code {
                language: None,
                text: "unclosed\n# not a heading".into()
            }]
        );
    }

    #[test]
    fn deep_or_huge_input_stays_bounded() {
        let deep = ">".repeat(500) + " x";
        assert!(!read(&deep).is_empty());
        let stars = "*a ".repeat(20_000);
        assert!(!read(&stars).is_empty());
        let nested = "[".repeat(5_000) + &"]".repeat(5_000);
        assert!(!read(&nested).is_empty());
        let huge = "x".repeat(MAX_MARKDOWN_BYTES + 1);
        assert_eq!(read(&huge), vec![para(vec![t(&huge)])]);
    }
}
