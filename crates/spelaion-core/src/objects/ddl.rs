//! Reading and editing the `CREATE TABLE` text an engine keeps (ADR-047): SQLite's
//! `sqlite_master.sql`, as the user wrote it, and the column definitions of MySQL's and
//! MariaDB's `SHOW CREATE TABLE`. Only the parts a structure form changes are rewritten (a
//! column's type, nullability, default or comment, a table constraint that goes); everything
//! else (collations, generated columns, `AUTOINCREMENT`, `WITHOUT ROWID`, comments in the text)
//! stays exactly as the engine had it.

use std::ops::Range;

/// The lexical classes the editor needs: enough to find names, parentheses and commas without
/// being fooled by quotes, strings or comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tok {
    /// A keyword, a bare name or a number.
    Word,
    /// A quoted name: `"a"`, `` `a` `` or `[a]`.
    Quoted,
    /// A string literal: `'a'`.
    String,
    Open,
    Close,
    Comma,
    Dot,
    Semicolon,
    /// Any other character: operators, signs.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: Tok,
    pub start: usize,
    pub end: usize,
}

/// Which engine's text this is: MySQL strings take backslash escapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Sqlite,
    Mysql,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum DdlError {
    #[error("the table's definition could not be read: {0}")]
    Unreadable(&'static str),
}

/// The tokens of `sql`, without blanks and comments. An unterminated quote, string or comment
/// runs to the end of the text, which is then not a definition the editor accepts.
pub(crate) fn tokenize(sql: &str, flavor: Flavor) -> Result<Vec<Token>, DdlError> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        let kind = match c {
            b' ' | b'\t' | b'\r' | b'\n' | 0x0c => {
                i += 1;
                continue;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            // MySQL's `#` comments never appear in SHOW CREATE TABLE.
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = sql[i + 2..]
                    .find("*/")
                    .ok_or(DdlError::Unreadable("a comment is not closed"))?;
                i += 2 + end + 2;
                continue;
            }
            b'\'' => {
                i = closing(bytes, i, b'\'', flavor == Flavor::Mysql)
                    .ok_or(DdlError::Unreadable("a string is not closed"))?;
                Tok::String
            }
            b'"' | b'`' => {
                i = closing(bytes, i, c, false)
                    .ok_or(DdlError::Unreadable("a quoted name is not closed"))?;
                Tok::Quoted
            }
            b'[' if flavor == Flavor::Sqlite => {
                let end = sql[i..]
                    .find(']')
                    .ok_or(DdlError::Unreadable("a quoted name is not closed"))?;
                i += end + 1;
                Tok::Quoted
            }
            b'(' => {
                i += 1;
                Tok::Open
            }
            b')' => {
                i += 1;
                Tok::Close
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b'.' if !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                i += 1;
                Tok::Dot
            }
            b';' => {
                i += 1;
                Tok::Semicolon
            }
            c if is_word_byte(c) || c == b'.' => {
                while i < bytes.len() && (is_word_byte(bytes[i]) || bytes[i] == b'.') {
                    // A dot ends a name (`schema.table`) but not a number (`1.5`).
                    if bytes[i] == b'.' && !sql[start..i].bytes().all(|b| b.is_ascii_digit()) {
                        break;
                    }
                    i += 1;
                }
                Tok::Word
            }
            _ => {
                // One character, whole (names may hold any letter).
                i += sql[i..].chars().next().map_or(1, char::len_utf8);
                Tok::Other
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: i,
        });
    }
    Ok(tokens)
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// The position after the quote that closes the one at `open`; a doubled quote is part of the
/// text, and so is a backslash-escaped character where `backslash` holds.
fn closing(bytes: &[u8], open: usize, quote: u8, backslash: bool) -> Option<usize> {
    let mut i = open + 1;
    while i < bytes.len() {
        if backslash && bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Whether a type or a default typed in a form is one piece of SQL: closed quotes and
/// parentheses, and no `;` that would start another statement.
pub(crate) fn is_single_expression(text: &str, flavor: Flavor) -> bool {
    let Ok(tokens) = tokenize(text, flavor) else {
        return false;
    };
    let mut depth = 0i32;
    for token in &tokens {
        match token.kind {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            Tok::Semicolon => return false,
            Tok::Word | Tok::Quoted | Tok::String | Tok::Comma | Tok::Dot | Tok::Other => {}
        }
    }
    depth == 0 && !tokens.is_empty()
}

/// A name as written, without its quotes: `"a""b"` is `a"b`.
pub(crate) fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    match (chars.next(), text.chars().last()) {
        (Some(open @ ('"' | '`')), Some(close)) if close == open && text.len() >= 2 => {
            let inner = &text[1..text.len() - 1];
            let doubled = format!("{open}{open}");
            inner.replace(&doubled, &open.to_string())
        }
        (Some('['), Some(']')) if text.len() >= 2 => text[1..text.len() - 1].to_string(),
        _ => text.to_string(),
    }
}

fn word_is(sql: &str, token: &Token, word: &str) -> bool {
    token.kind == Tok::Word && sql[token.start..token.end].eq_ignore_ascii_case(word)
}

/// What a clause of a column definition does, as far as the forms are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClauseKind {
    NotNull,
    /// A bare `NULL`.
    Null,
    Default,
    Comment,
    Collate,
    CharacterSet,
    PrimaryKey,
    Unique,
    References,
    Generated,
    Other,
}

/// A clause of a column definition: its kind and its text range in the definition's SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Clause {
    pub kind: ClauseKind,
    pub range: Range<usize>,
    /// For `DEFAULT`: the value's range.
    pub value: Option<Range<usize>>,
}

/// A column definition split into its name, its type and its clauses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDef {
    pub name: String,
    pub name_range: Range<usize>,
    /// Empty where SQLite's column has no type.
    pub type_range: Range<usize>,
    pub clauses: Vec<Clause>,
}

impl ColumnDef {
    pub fn clause(&self, kind: ClauseKind) -> Option<&Clause> {
        self.clauses.iter().find(|c| c.kind == kind)
    }

    /// The default's SQL as written, `None` without one or for `DEFAULT NULL`.
    pub fn default<'a>(&self, sql: &'a str) -> Option<&'a str> {
        let value = self.clause(ClauseKind::Default)?.value.clone()?;
        let value = &sql[value];
        (!value.eq_ignore_ascii_case("null")).then_some(value)
    }

    pub fn generated(&self) -> bool {
        self.clause(ClauseKind::Generated).is_some()
    }
}

/// Words that start a clause after a column's type.
fn starts_clause(sql: &str, tokens: &[Token], i: usize, flavor: Flavor) -> bool {
    let token = &tokens[i];
    if token.kind != Tok::Word {
        return false;
    }
    let word = sql[token.start..token.end].to_ascii_uppercase();
    let common = matches!(
        word.as_str(),
        "CONSTRAINT"
            | "PRIMARY"
            | "NOT"
            | "NULL"
            | "UNIQUE"
            | "CHECK"
            | "DEFAULT"
            | "COLLATE"
            | "REFERENCES"
            | "GENERATED"
            | "AS"
    );
    common
        || flavor == Flavor::Mysql
            && (matches!(
                word.as_str(),
                "AUTO_INCREMENT"
                    | "COMMENT"
                    | "CHARSET"
                    | "ON"
                    | "VISIBLE"
                    | "INVISIBLE"
                    | "SRID"
                    | "STORAGE"
                    | "COLUMN_FORMAT"
                    | "VIRTUAL"
                    | "STORED"
                    | "PERSISTENT"
                    | "ENGINE_ATTRIBUTE"
                    | "SECONDARY_ENGINE_ATTRIBUTE"
            ) || word == "CHARACTER"
                && tokens
                    .get(i + 1)
                    .is_some_and(|next| word_is(sql, next, "SET")))
}

/// The end (exclusive token index) of the parenthesised group that starts at `open`.
fn group_end(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// The end of one value: an optional sign, then a token, with a parenthesised group after a
/// word (`now()`) or alone (`(expr)`).
fn value_end(sql: &str, tokens: &[Token], mut i: usize) -> usize {
    if tokens
        .get(i)
        .is_some_and(|t| t.kind == Tok::Other && matches!(&sql[t.start..t.end], "+" | "-"))
    {
        i += 1;
    }
    match tokens.get(i).map(|t| t.kind) {
        None => i,
        Some(Tok::Open) => group_end(tokens, i),
        Some(Tok::Word) if tokens.get(i + 1).is_some_and(|t| t.kind == Tok::Open) => {
            group_end(tokens, i + 1)
        }
        Some(_) => i + 1,
    }
}

/// Splits a column definition (`name type clauses…`, one element of a table's body).
pub(crate) fn parse_column(sql: &str, flavor: Flavor) -> Result<ColumnDef, DdlError> {
    let tokens = tokenize(sql, flavor)?;
    let name = tokens
        .first()
        .filter(|t| matches!(t.kind, Tok::Word | Tok::Quoted))
        .ok_or(DdlError::Unreadable("a column has no name"))?;
    let mut i = 1;
    let type_start = tokens.get(i).map_or(sql.len(), |t| t.start);
    let mut type_end = type_start;
    while i < tokens.len() && !starts_clause(sql, &tokens, i, flavor) {
        i = if tokens[i].kind == Tok::Open {
            group_end(&tokens, i)
        } else {
            i + 1
        };
        type_end = tokens[i - 1].end;
    }
    let mut clauses = Vec::new();
    while i < tokens.len() {
        let start = i;
        // `CONSTRAINT name` belongs to the clause after it.
        if word_is(sql, &tokens[i], "CONSTRAINT") {
            i = (i + 2).min(tokens.len());
        }
        let Some(head) = tokens.get(i) else {
            return Err(DdlError::Unreadable("a constraint has no kind"));
        };
        let word = sql[head.start..head.end].to_ascii_uppercase();
        let mut value = None;
        let (kind, end) = match word.as_str() {
            "NOT" if tokens.get(i + 1).is_some_and(|t| word_is(sql, t, "NULL")) => {
                (ClauseKind::NotNull, until_next(sql, &tokens, i + 2, flavor))
            }
            "NULL" => (ClauseKind::Null, until_next(sql, &tokens, i + 1, flavor)),
            "DEFAULT" => {
                let end = value_end(sql, &tokens, i + 1);
                if end > i + 1 {
                    value = Some(tokens[i + 1].start..tokens[end - 1].end);
                }
                (ClauseKind::Default, end)
            }
            "COMMENT" => (ClauseKind::Comment, (i + 2).min(tokens.len())),
            "COLLATE" => (ClauseKind::Collate, (i + 2).min(tokens.len())),
            "CHARSET" => (ClauseKind::CharacterSet, (i + 2).min(tokens.len())),
            "CHARACTER" => (ClauseKind::CharacterSet, (i + 3).min(tokens.len())),
            "PRIMARY" => (
                ClauseKind::PrimaryKey,
                until_next(sql, &tokens, i + 1, flavor),
            ),
            "UNIQUE" => (ClauseKind::Unique, until_next(sql, &tokens, i + 1, flavor)),
            "REFERENCES" => (
                ClauseKind::References,
                until_next(sql, &tokens, i + 1, flavor),
            ),
            "GENERATED" | "AS" => (
                ClauseKind::Generated,
                until_next(sql, &tokens, i + 1, flavor),
            ),
            _ => (ClauseKind::Other, until_next(sql, &tokens, i + 1, flavor)),
        };
        let end = end.max(i + 1).min(tokens.len());
        clauses.push(Clause {
            kind,
            range: tokens[start].start..tokens[end - 1].end,
            value,
        });
        i = end;
    }
    Ok(ColumnDef {
        name: unquote(&sql[name.start..name.end]),
        name_range: name.start..name.end,
        type_range: type_start..type_end,
        clauses,
    })
}

/// The index of the next clause's first token from `i`, skipping parenthesised groups.
fn until_next(sql: &str, tokens: &[Token], mut i: usize, flavor: Flavor) -> usize {
    while i < tokens.len() && !starts_clause(sql, tokens, i, flavor) {
        i = if tokens[i].kind == Tok::Open {
            group_end(tokens, i)
        } else {
            i + 1
        };
    }
    i
}

/// What a column definition becomes: each field is the new state, `None` to keep it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ColumnRewrite {
    pub data_type: Option<String>,
    pub nullable: Option<bool>,
    /// `Some(None)` drops the default.
    pub default: Option<Option<String>>,
    /// `Some(None)` drops the comment (MySQL).
    pub comment: Option<Option<String>>,
    /// The new type takes no character set or collation (MySQL: a number, a date…).
    pub drop_charset: bool,
}

/// Writes a column definition again with `change`: the clauses it does not touch keep their
/// text and order; a new `NOT NULL` goes after the type, a new default after the nullability,
/// a new comment last. `backslash_escapes`: the session reads `\` in a string as an escape
/// (see [`mysql_string`]).
pub(crate) fn rewrite_column(
    sql: &str,
    column: &ColumnDef,
    change: &ColumnRewrite,
    backslash_escapes: bool,
) -> String {
    let mut parts: Vec<String> = vec![sql[column.name_range.clone()].to_string()];
    let data_type = change
        .data_type
        .clone()
        .unwrap_or_else(|| sql[column.type_range.clone()].to_string());
    if !data_type.trim().is_empty() {
        parts.push(data_type.trim().to_string());
    }
    let mut kept: Vec<(ClauseKind, String)> = column
        .clauses
        .iter()
        .filter(|clause| {
            let nullability = matches!(clause.kind, ClauseKind::NotNull | ClauseKind::Null);
            let charset = matches!(clause.kind, ClauseKind::CharacterSet | ClauseKind::Collate);
            !(nullability && change.nullable.is_some()
                || clause.kind == ClauseKind::Default && change.default.is_some()
                || clause.kind == ClauseKind::Comment && change.comment.is_some()
                || charset && change.drop_charset)
        })
        .map(|clause| (clause.kind, sql[clause.range.clone()].to_string()))
        .collect();
    // A NOT NULL column keeps no `DEFAULT NULL` (MariaDB writes one for every nullable column).
    if change.nullable == Some(false) && change.default.is_none() {
        kept.retain(|(kind, text)| {
            !(*kind == ClauseKind::Default
                && text
                    .split_whitespace()
                    .nth(1)
                    .is_some_and(|v| v.eq_ignore_ascii_case("null")))
        });
    }
    let mut insert_at = 0;
    if change.nullable == Some(false) {
        kept.insert(0, (ClauseKind::NotNull, "NOT NULL".into()));
        insert_at = 1;
    } else if let Some(i) = kept
        .iter()
        .position(|(kind, _)| matches!(kind, ClauseKind::NotNull | ClauseKind::Null))
    {
        insert_at = i + 1;
    }
    if let Some(Some(default)) = &change.default {
        kept.insert(
            insert_at,
            (ClauseKind::Default, format!("DEFAULT {}", default.trim())),
        );
    }
    if let Some(Some(comment)) = &change.comment {
        kept.push((
            ClauseKind::Comment,
            format!("COMMENT {}", mysql_string(comment, backslash_escapes)),
        ));
    }
    parts.extend(kept.into_iter().map(|(_, text)| text));
    parts.join(" ")
}

/// A MySQL string literal where only a plain string may go (`COMMENT '…'`, `IDENTIFIED BY
/// '…'`): quotes doubled, and backslashes escaped when the session reads them as escapes,
/// which its `sql_mode` decides (`NO_BACKSLASH_ESCAPES`, SDB-262). Where an expression may go,
/// `export::sql_literal` writes a literal every mode reads alike.
pub(crate) fn mysql_string(text: &str, backslash_escapes: bool) -> String {
    let text = text.replace('\'', "''");
    if backslash_escapes {
        format!("'{}'", text.replace('\\', "\\\\"))
    } else {
        format!("'{text}'")
    }
}

/// A `CREATE TABLE` statement split into what comes before its body, the body's elements and
/// what follows (table options such as `WITHOUT ROWID`, `STRICT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CreateTable {
    /// The table's name token, to put another name in its place.
    pub name_range: Range<usize>,
    /// Each element of the body: a column or a table constraint.
    pub elements: Vec<Range<usize>>,
    /// From the body's `(` to its `)`, both included.
    pub body: Range<usize>,
}

pub(crate) fn parse_create_table(sql: &str, flavor: Flavor) -> Result<CreateTable, DdlError> {
    let tokens = tokenize(sql, flavor)?;
    let table = tokens
        .iter()
        .position(|t| word_is(sql, t, "TABLE"))
        .ok_or(DdlError::Unreadable("not a CREATE TABLE"))?;
    let mut i = table + 1;
    // IF NOT EXISTS
    if tokens.get(i).is_some_and(|t| word_is(sql, t, "IF")) {
        i += 3;
    }
    // `schema.name`: the name is the last part.
    while tokens.get(i + 1).is_some_and(|t| t.kind == Tok::Dot) {
        i += 2;
    }
    let name = tokens
        .get(i)
        .filter(|t| matches!(t.kind, Tok::Word | Tok::Quoted | Tok::String))
        .ok_or(DdlError::Unreadable("the table has no name"))?;
    let open = i + 1;
    if tokens.get(open).map(|t| t.kind) != Some(Tok::Open) {
        return Err(DdlError::Unreadable("the table has no column list"));
    }
    let close = group_end(&tokens, open);
    if close > tokens.len() || tokens[close - 1].kind != Tok::Close {
        return Err(DdlError::Unreadable("the column list is not closed"));
    }
    let mut elements = Vec::new();
    let mut depth = 0;
    let mut element_start: Option<usize> = None;
    for token in &tokens[open + 1..close - 1] {
        match token.kind {
            Tok::Open => depth += 1,
            Tok::Close => depth -= 1,
            Tok::Comma if depth == 0 => {
                let start = element_start
                    .take()
                    .ok_or(DdlError::Unreadable("an element of the table is empty"))?;
                elements.push(start..last_end(&tokens, token.start));
                continue;
            }
            _ => {}
        }
        element_start.get_or_insert(token.start);
    }
    let start = element_start.ok_or(DdlError::Unreadable("an element of the table is empty"))?;
    elements.push(start..tokens[close - 2].end);
    Ok(CreateTable {
        name_range: name.start..name.end,
        elements,
        body: tokens[open].start..tokens[close - 1].end,
    })
}

/// The end of the last token before `position`.
fn last_end(tokens: &[Token], position: usize) -> usize {
    tokens
        .iter()
        .take_while(|t| t.start < position)
        .last()
        .map_or(position, |t| t.end)
}

/// Whether an element of a table's body is a table constraint rather than a column.
pub(crate) fn is_table_constraint(element: &str, flavor: Flavor) -> bool {
    let Some(first) = element.split_whitespace().next() else {
        return false;
    };
    // A quoted name is always a column, even `"primary"`.
    let word = first
        .split('(')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(
        word.as_str(),
        "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
    ) || flavor == Flavor::Mysql
        && matches!(word.as_str(), "KEY" | "INDEX" | "FULLTEXT" | "SPATIAL")
}

/// The constraint an element of a table's body declares: its kind, its name if it has one,
/// and its columns (the local ones for a foreign key) as unquoted names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableConstraint {
    pub name: Option<String>,
    pub kind: ClauseKind,
    pub columns: Vec<String>,
    /// For a foreign key: the referenced table (its last name part).
    pub referenced: Option<String>,
}

pub(crate) fn table_constraint(element: &str, flavor: Flavor) -> Option<TableConstraint> {
    let tokens = tokenize(element, flavor).ok()?;
    let mut i = 0;
    let mut name = None;
    if word_is(element, tokens.first()?, "CONSTRAINT") {
        let token = tokens.get(1)?;
        name = Some(unquote(&element[token.start..token.end]));
        i = 2;
    }
    let head = tokens.get(i)?;
    let kind = if word_is(element, head, "PRIMARY") {
        ClauseKind::PrimaryKey
    } else if word_is(element, head, "UNIQUE") {
        ClauseKind::Unique
    } else if word_is(element, head, "FOREIGN") {
        ClauseKind::References
    } else {
        ClauseKind::Other
    };
    let open = tokens.iter().skip(i).position(|t| t.kind == Tok::Open)? + i;
    let close = group_end(&tokens, open);
    let columns = names_in(element, &tokens[open + 1..close.saturating_sub(1)]);
    let referenced = (kind == ClauseKind::References)
        .then(|| {
            let at = tokens
                .iter()
                .position(|t| word_is(element, t, "REFERENCES"))?;
            let mut j = at + 1;
            while tokens.get(j + 1).is_some_and(|t| t.kind == Tok::Dot) {
                j += 2;
            }
            let t = tokens.get(j)?;
            Some(unquote(&element[t.start..t.end]))
        })
        .flatten();
    Some(TableConstraint {
        name,
        kind,
        columns,
        referenced,
    })
}

/// The names in a list of `name [COLLATE x] [ASC|DESC]` items.
fn names_in(sql: &str, tokens: &[Token]) -> Vec<String> {
    let mut names = Vec::new();
    let mut expect = true;
    for token in tokens {
        match token.kind {
            Tok::Comma => expect = true,
            Tok::Word | Tok::Quoted if expect => {
                names.push(unquote(&sql[token.start..token.end]));
                expect = false;
            }
            _ => {}
        }
    }
    names
}

/// The referenced table of a column's `REFERENCES t (…)` clause.
pub(crate) fn references_table(clause: &str, flavor: Flavor) -> Option<String> {
    let tokens = tokenize(clause, flavor).ok()?;
    let mut j = 1;
    while tokens.get(j + 1).is_some_and(|t| t.kind == Tok::Dot) {
        j += 2;
    }
    let t = tokens.get(j)?;
    Some(unquote(&clause[t.start..t.end]))
}

/// Whether `sql` names `name` anywhere (as a word or a quoted name, ignoring case).
pub(crate) fn mentions(sql: &str, name: &str, flavor: Flavor) -> bool {
    tokenize(sql, flavor).is_ok_and(|tokens| {
        tokens.iter().any(|t| {
            matches!(t.kind, Tok::Word | Tok::Quoted | Tok::String)
                && unquote_any(&sql[t.start..t.end]).eq_ignore_ascii_case(name)
        })
    })
}

fn unquote_any(text: &str) -> String {
    if text.starts_with('\'') && text.ends_with('\'') && text.len() >= 2 {
        text[1..text.len() - 1].replace("''", "'")
    } else {
        unquote(text)
    }
}

/// A `CREATE INDEX`, `CREATE TRIGGER` or `CREATE VIEW` statement with its object put in
/// `schema` (`quoted`): SQLite keeps them unqualified, and outside `main` they must name the
/// schema to be created there again. A statement already qualified is left as it is.
pub(crate) fn qualify_create(sql: &str, quoted_schema: &str) -> String {
    let Ok(tokens) = tokenize(sql, Flavor::Sqlite) else {
        return sql.to_string();
    };
    let Some(kind) = tokens.iter().position(|t| {
        ["INDEX", "TRIGGER", "VIEW"]
            .iter()
            .any(|w| word_is(sql, t, w))
    }) else {
        return sql.to_string();
    };
    let mut i = kind + 1;
    if tokens.get(i).is_some_and(|t| word_is(sql, t, "IF")) {
        i += 3;
    }
    match (tokens.get(i), tokens.get(i + 1)) {
        (Some(_), Some(next)) if next.kind == Tok::Dot => sql.to_string(),
        (Some(name), _) => format!(
            "{}{quoted_schema}.{}",
            &sql[..name.start],
            &sql[name.start..]
        ),
        _ => sql.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(sql: &str, flavor: Flavor) -> (ColumnDef, Vec<(ClauseKind, String)>) {
        let def = parse_column(sql, flavor).unwrap();
        let clauses = def
            .clauses
            .iter()
            .map(|c| (c.kind, sql[c.range.clone()].to_string()))
            .collect();
        (def, clauses)
    }

    #[test]
    fn columns_are_split_into_type_and_clauses() {
        let sql = r#""Total" NUMERIC(10, 2) CONSTRAINT nn NOT NULL ON CONFLICT ABORT DEFAULT -1.5 CHECK ("Total" > 0) COLLATE NOCASE"#;
        let (def, clauses) = column(sql, Flavor::Sqlite);
        assert_eq!(def.name, "Total");
        assert_eq!(&sql[def.type_range.clone()], "NUMERIC(10, 2)");
        assert_eq!(
            clauses,
            [
                (
                    ClauseKind::NotNull,
                    "CONSTRAINT nn NOT NULL ON CONFLICT ABORT".into()
                ),
                (ClauseKind::Default, "DEFAULT -1.5".into()),
                (ClauseKind::Other, "CHECK (\"Total\" > 0)".into()),
                (ClauseKind::Collate, "COLLATE NOCASE".into()),
            ]
        );
        assert_eq!(def.default(sql), Some("-1.5"));
        // No type at all, and a function call as the default.
        let (def, clauses) = column("x DEFAULT (datetime('now')) NOT NULL", Flavor::Sqlite);
        assert!(def.type_range.is_empty());
        assert_eq!(clauses[0].1, "DEFAULT (datetime('now'))");
        assert_eq!(clauses[1].0, ClauseKind::NotNull);
    }

    #[test]
    fn mysql_columns_keep_their_attributes() {
        let sql = "`note` varchar(80) CHARACTER SET latin1 COLLATE latin1_bin NOT NULL DEFAULT 'it''s \\' x' COMMENT 'a\\\\b'";
        let (def, clauses) = column(sql, Flavor::Mysql);
        assert_eq!(&sql[def.type_range.clone()], "varchar(80)");
        let kinds: Vec<_> = clauses.iter().map(|c| c.0).collect();
        assert_eq!(
            kinds,
            [
                ClauseKind::CharacterSet,
                ClauseKind::Collate,
                ClauseKind::NotNull,
                ClauseKind::Default,
                ClauseKind::Comment
            ]
        );
        assert_eq!(def.default(sql), Some("'it''s \\' x'"));
        let (def, _) = column("`n` int unsigned NOT NULL AUTO_INCREMENT", Flavor::Mysql);
        assert_eq!(def.type_range, 4..16);
        let (def, _) = column(
            "`d` datetime DEFAULT current_timestamp() ON UPDATE current_timestamp()",
            Flavor::Mysql,
        );
        assert_eq!(def.clauses.len(), 2);
        assert_eq!(
            def.default("`d` datetime DEFAULT current_timestamp() ON UPDATE current_timestamp()"),
            Some("current_timestamp()")
        );
    }

    #[test]
    fn a_rewrite_touches_only_what_changes() {
        let sql = "qty INTEGER CONSTRAINT nn NOT NULL DEFAULT 0 CHECK (qty >= 0)";
        let def = parse_column(sql, Flavor::Sqlite).unwrap();
        let rewrite = |change: ColumnRewrite| rewrite_column(sql, &def, &change, true);
        assert_eq!(
            rewrite(ColumnRewrite {
                data_type: Some("REAL".into()),
                ..Default::default()
            }),
            "qty REAL CONSTRAINT nn NOT NULL DEFAULT 0 CHECK (qty >= 0)"
        );
        assert_eq!(
            rewrite(ColumnRewrite {
                nullable: Some(true),
                default: Some(None),
                ..Default::default()
            }),
            "qty INTEGER CHECK (qty >= 0)"
        );
        assert_eq!(
            rewrite(ColumnRewrite {
                default: Some(Some("'7'".into())),
                ..Default::default()
            }),
            "qty INTEGER CONSTRAINT nn NOT NULL DEFAULT '7' CHECK (qty >= 0)"
        );
        let sql = "`a` varchar(5) COLLATE utf8mb4_bin DEFAULT NULL COMMENT 'old'";
        let def = parse_column(sql, Flavor::Mysql).unwrap();
        assert_eq!(
            rewrite_column(
                sql,
                &def,
                &ColumnRewrite {
                    data_type: Some("int".into()),
                    nullable: Some(false),
                    comment: Some(Some("it's".into())),
                    drop_charset: true,
                    ..Default::default()
                },
                true
            ),
            "`a` int NOT NULL COMMENT 'it''s'"
        );
        // A backslash is escaped only where the session reads it as an escape (SDB-262).
        let comment = ColumnRewrite {
            comment: Some(Some(r"C:\dir".into())),
            ..Default::default()
        };
        assert_eq!(
            rewrite_column(sql, &def, &comment, true),
            r"`a` varchar(5) COLLATE utf8mb4_bin DEFAULT NULL COMMENT 'C:\\dir'"
        );
        assert_eq!(
            rewrite_column(sql, &def, &comment, false),
            r"`a` varchar(5) COLLATE utf8mb4_bin DEFAULT NULL COMMENT 'C:\dir'"
        );
    }

    #[test]
    fn create_table_bodies_split_at_top_level_commas() {
        let sql = "CREATE TABLE IF NOT EXISTS main.\"t, u\" (a INT DEFAULT (1, 2), b TEXT, -- note, here\n CONSTRAINT k UNIQUE (a, b), FOREIGN KEY (b) REFERENCES other.p(x)) WITHOUT ROWID";
        let create = parse_create_table(sql, Flavor::Sqlite).unwrap();
        assert_eq!(&sql[create.name_range.clone()], "\"t, u\"");
        let elements: Vec<&str> = create.elements.iter().map(|r| &sql[r.clone()]).collect();
        assert_eq!(
            elements,
            [
                "a INT DEFAULT (1, 2)",
                "b TEXT",
                "CONSTRAINT k UNIQUE (a, b)",
                "FOREIGN KEY (b) REFERENCES other.p(x)"
            ]
        );
        assert!(sql[create.body.end..].trim() == "WITHOUT ROWID");
        assert!(is_table_constraint(elements[2], Flavor::Sqlite));
        assert!(!is_table_constraint("\"primary\" INT", Flavor::Sqlite));
        assert_eq!(
            table_constraint(elements[2], Flavor::Sqlite),
            Some(TableConstraint {
                name: Some("k".into()),
                kind: ClauseKind::Unique,
                columns: vec!["a".into(), "b".into()],
                referenced: None,
            })
        );
        assert_eq!(
            table_constraint(elements[3], Flavor::Sqlite)
                .unwrap()
                .referenced,
            Some("p".into())
        );
    }

    #[test]
    fn single_expressions_and_names() {
        assert!(is_single_expression("numeric(10, 2)", Flavor::Sqlite));
        assert!(is_single_expression("'a;b'", Flavor::Sqlite));
        assert!(!is_single_expression("1; DROP TABLE t", Flavor::Sqlite));
        assert!(!is_single_expression("(1", Flavor::Sqlite));
        assert!(!is_single_expression("'open", Flavor::Sqlite));
        assert!(!is_single_expression("  ", Flavor::Sqlite));
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote("[x y]"), "x y");
        assert!(mentions(
            "CREATE VIEW v AS SELECT * FROM \"Orders\"",
            "orders",
            Flavor::Sqlite
        ));
        assert!(!mentions(
            "CREATE VIEW v AS SELECT 'x' -- orders",
            "orders",
            Flavor::Sqlite
        ));
        assert_eq!(
            qualify_create("CREATE UNIQUE INDEX IF NOT EXISTS i ON t (a)", "\"aux\""),
            "CREATE UNIQUE INDEX IF NOT EXISTS \"aux\".i ON t (a)"
        );
        assert_eq!(
            qualify_create(
                "CREATE TRIGGER aux.tr AFTER INSERT ON t BEGIN SELECT 1; END",
                "aux"
            ),
            "CREATE TRIGGER aux.tr AFTER INSERT ON t BEGIN SELECT 1; END"
        );
    }
}
