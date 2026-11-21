//! MongoDB's language, as the core reads it (ADR-056): a closed subset of the MongoDB shell's
//! method calls, parsed here and **never evaluated as JavaScript**. Each statement becomes one
//! command document, the canonical request the plugin is sent:
//! `{"database": "shop", "command": {"find": "users", "filter": {…}}}`, in canonical Extended
//! JSON so no type is lost. Anything this parser does not read is refused with its position,
//! not guessed.
//!
//! What is read:
//! - `db.<collection>.<method>(…)`, the collection also as `db["name"]` or
//!   `db.getCollection("name")`, and `db.getSiblingDB("other")` before it for another database.
//!   Methods: `find` (with `sort`, `limit`, `skip`, `projection`, `hint`, `collation`,
//!   `comment`, `maxTimeMS`, `batchSize` chained), `findOne`, `aggregate`, `countDocuments`,
//!   `estimatedDocumentCount`, `distinct`, `insertOne`, `insertMany`, `updateOne`,
//!   `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`, `createIndex`, `dropIndex`,
//!   `dropIndexes`, `getIndexes`, `drop`.
//! - `db.runCommand({…})`, `db.adminCommand({…})`, `db.createCollection(name, {…})`,
//!   `db.dropDatabase()`, `db.getCollectionNames()`, and `show dbs` / `show collections`.
//! - Values: objects (keys quoted or not), arrays, strings, numbers, `true`, `false`, `null`,
//!   regular expressions (`/a.*/i`) and the shell's constructors: `ObjectId("…")`,
//!   `ISODate("…")`, `new Date(…)`, `NumberInt`, `NumberLong`, `NumberDecimal` /
//!   `Decimal128`, `UUID("…")`, `BinData(…)`, `Timestamp(…)`, `MinKey()`, `MaxKey()`.
//!
//! The guard reads the command document, not the text ([`request_class`]), so every way of
//! writing a command is judged alike.

use serde_json::{Map, Value, json};
use spelaion_driver_api::{
    Dialect, RequestClass, RequestError, StatementSpan, code_ranges as ranges_between,
};

/// MongoDB's language.
#[derive(Debug, Clone, Copy, Default)]
pub struct MongodbDialect;

impl Dialect for MongodbDialect {
    fn name(&self) -> &'static str {
        "mongodb"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    /// A name as the shell writes a string: what `db.getCollection(…)` takes.
    fn quote_identifier(&self, ident: &str) -> String {
        Value::String(ident.to_string()).to_string()
    }

    fn code_ranges(&self, script: &str) -> Vec<std::ops::Range<usize>> {
        ranges_between(script, skip_non_code)
    }

    fn reads_requests(&self) -> bool {
        true
    }

    fn to_request(&self, statement: &str, database: &str) -> Option<Result<Value, RequestError>> {
        Some(to_request(statement, database))
    }

    fn request_class(&self, request: &Value) -> RequestClass {
        request_class(request)
    }

    fn browse_statement(&self, object: &spelaion_driver_api::ObjectInfo) -> Option<String> {
        Some(browse_statement(&object.schema, &object.name))
    }
}

/// How many documents opening a collection shows: never the whole of it.
pub const BROWSE_LIMIT: u32 = 200;

/// The statement that shows a collection's first documents, in its own database.
pub fn browse_statement(database: &str, collection: &str) -> String {
    let quoted = |text: &str| Value::String(text.to_string()).to_string();
    format!(
        "db.getSiblingDB({}).getCollection({}).find({{}}).limit({BROWSE_LIMIT})",
        quoted(database),
        quoted(collection)
    )
}

// ------------------------------------------------------------------------------ splitting

/// Where a string or a comment that starts at `i` ends, or `None` if none starts there.
fn skip_non_code(script: &str, i: usize) -> Option<usize> {
    let bytes = script.as_bytes();
    match bytes[i] {
        quote @ (b'"' | b'\'' | b'`') => {
            let mut j = i + 1;
            while j < bytes.len() {
                match bytes[j] {
                    b'\\' => j += 2,
                    c if c == quote => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(bytes.len())
        }
        b'/' if bytes.get(i + 1) == Some(&b'/') => Some(
            script[i..]
                .find('\n')
                .map(|n| i + n + 1)
                .unwrap_or(bytes.len()),
        ),
        b'/' if bytes.get(i + 1) == Some(&b'*') => Some(
            script[i + 2..]
                .find("*/")
                .map(|n| i + 2 + n + 2)
                .unwrap_or(bytes.len()),
        ),
        _ => None,
    }
}

/// Statements end at a `;` outside brackets, strings and comments, or at the end of a line
/// whose brackets are all closed when the next line does not continue it (a line that starts
/// with `.` chains onto the one before, as the shell reads it).
fn split(script: &str) -> Vec<StatementSpan> {
    let bytes = script.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut depth: i64 = 0;
    let mut i = 0;
    let push = |start: usize, end: usize, spans: &mut Vec<StatementSpan>| {
        if let Some(span) = trimmed(script, start, end) {
            spans.push(span);
        }
    };
    while i < bytes.len() {
        if let Some(next) = skip_non_code(script, i) {
            i = next;
            continue;
        }
        match bytes[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth <= 0 => {
                push(start, i, &mut spans);
                start = i + 1;
                depth = 0;
            }
            b'\n' if depth <= 0 && !continues(script, i + 1) => {
                push(start, i, &mut spans);
                start = i + 1;
                depth = 0;
            }
            _ => {}
        }
        i += 1;
    }
    push(start, bytes.len(), &mut spans);
    spans
}

/// Whether the text from `from` continues the statement before it: its first code is `.`.
fn continues(script: &str, from: usize) -> bool {
    let bytes = script.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'/' if matches!(bytes.get(i + 1), Some(b'/') | Some(b'*')) => {
                i = skip_non_code(script, i).unwrap_or(bytes.len());
            }
            b'.' => return true,
            _ => return false,
        }
    }
    false
}

/// The span between `start` and `end` without the space and comments before it and the space
/// after it, `None` if it holds no code.
fn trimmed(script: &str, start: usize, end: usize) -> Option<StatementSpan> {
    let bytes = script.as_bytes();
    let mut first = start;
    while first < end {
        match bytes[first] {
            // White space by the one rule of every splitter (SDB-434).
            _ if crate::is_white_space_at(script, first) => first += 1,
            b'/' if matches!(bytes.get(first + 1), Some(b'/') | Some(b'*')) => {
                first = skip_non_code(script, first).unwrap_or(end).min(end);
            }
            _ => break,
        }
    }
    let text = &script[first..end];
    let trail = text.len() - text.trim_end().len();
    (first < end - trail).then(|| StatementSpan {
        start: first,
        end: end - trail,
    })
}

// ------------------------------------------------------------------------------- reading

/// The canonical request of `statement`, run in `database` unless it names another.
pub fn to_request(statement: &str, database: &str) -> Result<Value, RequestError> {
    let mut parser = Parser {
        text: statement,
        pos: 0,
    };
    let (database, command) = parser.statement(database)?;
    parser.skip_space();
    if parser.peek() == Some(b';') {
        parser.pos += 1;
        parser.skip_space();
    }
    if parser.pos < statement.len() {
        return Err(parser.error("something follows the end of the statement"));
    }
    Ok(json!({"database": database, "command": Value::Object(command)}))
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
}

/// A collection method's arguments and what was chained after it.
struct Call {
    name: String,
    args: Vec<Value>,
    /// Where the call started, for errors.
    at: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> RequestError {
        self.error_at(self.pos, message)
    }

    fn error_at(&self, position: usize, message: &str) -> RequestError {
        RequestError {
            position,
            message: message.to_string(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.pos).copied()
    }

    fn skip_space(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.pos += 1;
            } else if c == b'/'
                && matches!(
                    self.text.as_bytes().get(self.pos + 1),
                    Some(b'/') | Some(b'*')
                )
            {
                self.pos = skip_non_code(self.text, self.pos).unwrap_or(self.text.len());
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_space();
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8, what: &str) -> Result<(), RequestError> {
        if self.eat(c) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {what}")))
        }
    }

    /// A JavaScript name: letters, digits, `_` and `$`, not starting with a digit.
    fn ident(&mut self) -> Option<String> {
        self.skip_space();
        let start = self.pos;
        let bytes = self.text.as_bytes();
        while let Some(&c) = bytes.get(self.pos) {
            let ok = c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
            if !ok || (self.pos == start && c.is_ascii_digit()) {
                break;
            }
            self.pos += 1;
        }
        (self.pos > start).then(|| self.text[start..self.pos].to_string())
    }

    fn expect_ident(&mut self, what: &str) -> Result<String, RequestError> {
        self.ident()
            .ok_or_else(|| self.error(&format!("expected {what}")))
    }

    /// One statement: `show …` or `db…`.
    fn statement(&mut self, database: &str) -> Result<(String, Map<String, Value>), RequestError> {
        self.skip_space();
        let start = self.pos;
        match self.ident().as_deref() {
            Some("show") => self.show(database),
            Some("db") => self.db(database),
            Some("use") => Err(self.error_at(
                start,
                "`use` is not read here: choose the database in the explorer, or write \
                 db.getSiblingDB(\"name\")",
            )),
            _ => Err(self.error_at(
                start,
                "a statement starts with `db.` or `show` (the shell's JavaScript is not run)",
            )),
        }
    }

    fn show(&mut self, database: &str) -> Result<(String, Map<String, Value>), RequestError> {
        let at = self.pos;
        match self.ident().as_deref() {
            Some("dbs" | "databases") => Ok((
                "admin".into(),
                object(json!({"listDatabases": 1, "nameOnly": false})),
            )),
            Some("collections" | "tables") => Ok((
                database.into(),
                object(json!({"listCollections": 1, "nameOnly": true})),
            )),
            _ => Err(self.error_at(at, "`show` reads `dbs` or `collections`")),
        }
    }

    fn db(&mut self, database: &str) -> Result<(String, Map<String, Value>), RequestError> {
        let mut database = database.to_string();
        // `db.getSiblingDB("other")`, any number of times.
        let mut collection: Option<String> = None;
        loop {
            self.skip_space();
            if self.eat(b'[') {
                let at = self.pos;
                let name = self.string_literal()?;
                self.expect(b']', "`]`")?;
                collection = Some(self.collection_name(collection, &name, at)?);
                continue;
            }
            if !self.eat(b'.') {
                return Err(self.error("expected `.` and a collection or a method"));
            }
            let at = self.pos;
            let name = self.expect_ident("a collection or a method")?;
            self.skip_space();
            if self.peek() != Some(b'(') {
                // Part of a collection's name (`db.system.users`).
                collection = Some(match collection {
                    Some(before) => format!("{before}.{name}"),
                    None => name,
                });
                continue;
            }
            self.pos += 1;
            let args = self.arguments()?;
            if collection.is_none() {
                match name.as_str() {
                    "getSiblingDB" => {
                        database = one_string(&args, "getSiblingDB", at, self)?;
                        continue;
                    }
                    "getCollection" => {
                        collection = Some(one_string(&args, "getCollection", at, self)?);
                        continue;
                    }
                    _ => {
                        let command = self.db_method(&name, args, at)?;
                        return Ok(match command {
                            DbCommand::Here(command) => (database, command),
                            DbCommand::Admin(command) => ("admin".into(), command),
                        });
                    }
                }
            }
            let collection = collection.take().expect("a collection");
            let mut call = Call { name, args, at };
            let chained = self.chained()?;
            let command = collection_method(&collection, &mut call, chained, self)?;
            return Ok((database, command));
        }
    }

    fn collection_name(
        &self,
        before: Option<String>,
        name: &str,
        at: usize,
    ) -> Result<String, RequestError> {
        if before.is_some() {
            return Err(self.error_at(at, "a collection is named once"));
        }
        if name.is_empty() {
            return Err(self.error_at(at, "a collection's name is not empty"));
        }
        Ok(name.to_string())
    }

    /// `(…)` after a method's name, the `(` already read.
    fn arguments(&mut self) -> Result<Vec<Value>, RequestError> {
        let mut args = Vec::new();
        if self.eat(b')') {
            return Ok(args);
        }
        loop {
            args.push(self.value()?);
            if self.eat(b')') {
                return Ok(args);
            }
            self.expect(b',', "`,` or `)`")?;
            // A trailing comma.
            if self.eat(b')') {
                return Ok(args);
            }
        }
    }

    /// `.sort(…).limit(…)` after a method.
    fn chained(&mut self) -> Result<Vec<Call>, RequestError> {
        let mut calls = Vec::new();
        loop {
            self.skip_space();
            if self.peek() != Some(b'.') {
                return Ok(calls);
            }
            self.pos += 1;
            let at = self.pos;
            let name = self.expect_ident("a method")?;
            self.expect(b'(', "`(`")?;
            let args = self.arguments()?;
            calls.push(Call { name, args, at });
        }
    }

    fn db_method(
        &mut self,
        name: &str,
        mut args: Vec<Value>,
        at: usize,
    ) -> Result<DbCommand, RequestError> {
        let command = match name {
            "runCommand" | "adminCommand" => {
                let command = match (args.len(), args.pop()) {
                    (1, Some(Value::Object(command))) if !command.is_empty() => command,
                    _ => {
                        return Err(
                            self.error_at(at, &format!("{name} takes one command document"))
                        );
                    }
                };
                if name == "adminCommand" {
                    return Ok(DbCommand::Admin(command));
                }
                command
            }
            "createCollection" => {
                let mut args = args.into_iter();
                let name = match args.next() {
                    Some(Value::String(name)) if !name.is_empty() => name,
                    _ => return Err(self.error_at(at, "createCollection takes a name")),
                };
                let mut command = object(json!({"create": name}));
                if let Some(options) = args.next() {
                    merge(
                        &mut command,
                        options,
                        "createCollection's options",
                        at,
                        self,
                    )?;
                }
                no_more(args, "createCollection", at, self)?;
                command
            }
            "dropDatabase" => {
                no_more(args.into_iter(), "dropDatabase", at, self)?;
                object(json!({"dropDatabase": 1}))
            }
            "getCollectionNames" => {
                no_more(args.into_iter(), "getCollectionNames", at, self)?;
                object(json!({"listCollections": 1, "nameOnly": true}))
            }
            other => {
                return Err(self.error_at(
                    at,
                    &format!(
                        "db.{other}() is not read here: write db.runCommand({{…}}) with the \
                         command it runs"
                    ),
                ));
            }
        };
        Ok(DbCommand::Here(command))
    }

    // --------------------------------------------------------------------------- values

    fn value(&mut self) -> Result<Value, RequestError> {
        self.skip_space();
        let at = self.pos;
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                self.object_rest()
            }
            Some(b'[') => {
                self.pos += 1;
                let mut items = Vec::new();
                if self.eat(b']') {
                    return Ok(Value::Array(items));
                }
                loop {
                    items.push(self.value()?);
                    if self.eat(b']') {
                        return Ok(Value::Array(items));
                    }
                    self.expect(b',', "`,` or `]`")?;
                    if self.eat(b']') {
                        return Ok(Value::Array(items));
                    }
                }
            }
            Some(b'"' | b'\'') => Ok(Value::String(self.string_literal()?)),
            Some(b'/') => self.regex(),
            Some(b'-' | b'+' | b'.' | b'0'..=b'9') => self.number(),
            Some(_) => {
                let Some(word) = self.ident() else {
                    return Err(self.error("expected a value"));
                };
                match word.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    "Infinity" => Ok(json!({"$numberDouble": "Infinity"})),
                    "NaN" => Ok(json!({"$numberDouble": "NaN"})),
                    "MinKey" | "MaxKey" => {
                        let before = self.pos;
                        if self.eat(b'(') {
                            self.pos = before;
                            self.constructor(&word, at)
                        } else {
                            Ok(min_max(&word))
                        }
                    }
                    // `Date()` without `new` is the date as text in JavaScript.
                    "Date" => Err(self.error_at(at, "write `new Date(…)` or `ISODate(…)`")),
                    "new" => {
                        let at = self.pos;
                        let name = self.expect_ident("a constructor")?;
                        self.constructor(&name, at)
                    }
                    "undefined" => Err(self.error_at(at, "`undefined` is not stored: use null")),
                    _ => self.constructor(&word, at),
                }
            }
            None => Err(self.error("expected a value")),
        }
    }

    fn object_rest(&mut self) -> Result<Value, RequestError> {
        let mut map = Map::new();
        if self.eat(b'}') {
            return Ok(Value::Object(map));
        }
        loop {
            self.skip_space();
            let at = self.pos;
            let key = match self.peek() {
                Some(b'"' | b'\'') => self.string_literal()?,
                Some(b'0'..=b'9') => {
                    let start = self.pos;
                    while matches!(self.peek(), Some(b'0'..=b'9')) {
                        self.pos += 1;
                    }
                    self.text[start..self.pos].to_string()
                }
                _ => self.expect_ident("a key")?,
            };
            self.expect(b':', "`:`")?;
            let value = self.value()?;
            if map.insert(key.clone(), value).is_some() {
                return Err(self.error_at(at, &format!("the key `{key}` is written twice")));
            }
            if self.eat(b'}') {
                return Ok(Value::Object(map));
            }
            self.expect(b',', "`,` or `}`")?;
            if self.eat(b'}') {
                return Ok(Value::Object(map));
            }
        }
    }

    fn string_literal(&mut self) -> Result<String, RequestError> {
        self.skip_space();
        let at = self.pos;
        let quote = match self.peek() {
            Some(q @ (b'"' | b'\'')) => q,
            _ => return Err(self.error("expected a string")),
        };
        self.pos += 1;
        let mut out = String::new();
        loop {
            let rest = &self.text[self.pos..];
            let Some(c) = rest.chars().next() else {
                return Err(self.error_at(at, "this string is never closed"));
            };
            self.pos += c.len_utf8();
            match c {
                c if c as u32 == u32::from(quote) => return Ok(out),
                '\\' => {
                    let Some(e) = self.text[self.pos..].chars().next() else {
                        return Err(self.error_at(at, "this string is never closed"));
                    };
                    self.pos += e.len_utf8();
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'v' => out.push('\u{b}'),
                        '0' => out.push('\0'),
                        'u' => out.push(self.unicode_escape()?),
                        'x' => {
                            let hex = self.text.get(self.pos..self.pos + 2).unwrap_or("");
                            let code = u32::from_str_radix(hex, 16)
                                .map_err(|_| self.error("expected two hexadecimal digits"))?;
                            self.pos += 2;
                            out.push(char::from_u32(code).expect("below 256"));
                        }
                        '\n' => {}
                        other => out.push(other),
                    }
                }
                '\n' => return Err(self.error_at(at, "a string does not cross lines")),
                c => out.push(c),
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, RequestError> {
        let hex4 = |p: &mut Self| -> Result<u32, RequestError> {
            let hex = p.text.get(p.pos..p.pos + 4).unwrap_or("");
            let code = u32::from_str_radix(hex, 16)
                .map_err(|_| p.error("expected four hexadecimal digits"))?;
            p.pos += 4;
            Ok(code)
        };
        if self.peek() == Some(b'{') {
            self.pos += 1;
            let end = self.text[self.pos..]
                .find('}')
                .ok_or_else(|| self.error("expected `}`"))?;
            let code = u32::from_str_radix(&self.text[self.pos..self.pos + end], 16)
                .map_err(|_| self.error("expected hexadecimal digits"))?;
            self.pos += end + 1;
            return char::from_u32(code).ok_or_else(|| self.error("not a character"));
        }
        let high = hex4(self)?;
        if (0xD800..0xDC00).contains(&high) && self.text[self.pos..].starts_with("\\u") {
            self.pos += 2;
            let low = hex4(self)?;
            let code = 0x10000 + ((high - 0xD800) << 10) + (low.wrapping_sub(0xDC00) & 0x3FF);
            return char::from_u32(code).ok_or_else(|| self.error("not a character"));
        }
        char::from_u32(high).ok_or_else(|| self.error("not a character"))
    }

    fn number(&mut self) -> Result<Value, RequestError> {
        let at = self.pos;
        let text = self.number_text()?;
        number_value(&text).ok_or_else(|| self.error_at(at, &format!("`{text}` is not a number")))
    }

    fn number_text(&mut self) -> Result<String, RequestError> {
        self.skip_space();
        let start = self.pos;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
            self.skip_space();
        }
        let sign = &self.text[start..self.pos];
        let sign = sign.trim().trim_start_matches('+');
        let digits_start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'_'))
            || (matches!(self.peek(), Some(b'-' | b'+'))
                && matches!(self.text.as_bytes().get(self.pos - 1), Some(b'e' | b'E')))
        {
            self.pos += 1;
        }
        let digits = self.text[digits_start..self.pos].replace('_', "");
        if digits == "Infinity" || self.text[digits_start..].starts_with("Infinity") {
            self.pos = digits_start + "Infinity".len();
            return Ok(format!("{sign}Infinity"));
        }
        if digits.is_empty() {
            return Err(self.error_at(start, "expected a number"));
        }
        Ok(format!("{sign}{digits}"))
    }

    fn regex(&mut self) -> Result<Value, RequestError> {
        let at = self.pos;
        self.pos += 1;
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            let Some(c) = self.text[self.pos..].chars().next() else {
                return Err(self.error_at(at, "this regular expression is never closed"));
            };
            self.pos += c.len_utf8();
            match c {
                '\\' => {
                    let Some(e) = self.text[self.pos..].chars().next() else {
                        return Err(self.error_at(at, "this regular expression is never closed"));
                    };
                    self.pos += e.len_utf8();
                    pattern.push('\\');
                    pattern.push(e);
                }
                '[' => {
                    in_class = true;
                    pattern.push(c);
                }
                ']' => {
                    in_class = false;
                    pattern.push(c);
                }
                '/' if !in_class => break,
                '\n' => return Err(self.error_at(at, "a regular expression does not cross lines")),
                c => pattern.push(c),
            }
        }
        let start = self.pos;
        while matches!(self.peek(), Some(b'a'..=b'z')) {
            self.pos += 1;
        }
        let mut flags: Vec<char> = self.text[start..self.pos].chars().collect();
        for flag in &flags {
            if !"imsxlu".contains(*flag) {
                return Err(self.error_at(start, &format!("`{flag}` is not a flag MongoDB reads")));
            }
        }
        flags.sort_unstable();
        flags.dedup();
        Ok(json!({"$regularExpression": {
            "pattern": pattern,
            "options": flags.into_iter().collect::<String>(),
        }}))
    }

    fn constructor(&mut self, name: &str, at: usize) -> Result<Value, RequestError> {
        if !self.eat(b'(') {
            return Err(self.error_at(
                at,
                &format!("`{name}` is not read here: the shell's JavaScript is not run"),
            ));
        }
        // Numbers inside a constructor keep their text: NumberLong(9007199254740993).
        let args = self.constructor_arguments()?;
        constructed(name, args).map_err(|message| self.error_at(at, &message))
    }

    fn constructor_arguments(&mut self) -> Result<Vec<Arg>, RequestError> {
        let mut args = Vec::new();
        if self.eat(b')') {
            return Ok(args);
        }
        loop {
            self.skip_space();
            let arg = match self.peek() {
                Some(b'-' | b'+' | b'.' | b'0'..=b'9') => Arg::Number(self.number_text()?),
                Some(b'"' | b'\'') => Arg::Text(self.string_literal()?),
                _ => Arg::Value(self.value()?),
            };
            args.push(arg);
            if self.eat(b')') {
                return Ok(args);
            }
            self.expect(b',', "`,` or `)`")?;
        }
    }
}

enum DbCommand {
    Here(Map<String, Value>),
    Admin(Map<String, Value>),
}

/// A constructor's argument as it was written.
enum Arg {
    Number(String),
    Text(String),
    Value(Value),
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("an object literal"),
    }
}

fn min_max(word: &str) -> Value {
    if word == "MinKey" {
        json!({"$minKey": 1})
    } else {
        json!({"$maxKey": 1})
    }
}

/// A number literal as canonical Extended JSON: an integer is an `int` when it fits, else a
/// `long` (never a `double` that would lose digits); anything with a point or an exponent is a
/// `double`.
fn number_value(text: &str) -> Option<Value> {
    if text.ends_with("Infinity") {
        return Some(json!({"$numberDouble": text.trim_start_matches('+')}));
    }
    let integer = !text.contains(['.', 'e', 'E']);
    if integer {
        let n: i64 = text.parse().ok()?;
        return Some(match i32::try_from(n) {
            Ok(small) => json!({"$numberInt": small.to_string()}),
            Err(_) => json!({"$numberLong": n.to_string()}),
        });
    }
    let n: f64 = text.parse().ok()?;
    if !n.is_finite() {
        return None;
    }
    Some(json!({"$numberDouble": double_text(n)}))
}

/// A double as canonical Extended JSON writes it: with a point when it is whole.
fn double_text(n: f64) -> String {
    let text = format!("{n:?}");
    if text.contains(['.', 'e', 'E']) {
        text
    } else {
        format!("{text}.0")
    }
}

/// An integer argument of a constructor or a method.
fn integer_of(arg: &Arg) -> Option<i64> {
    match arg {
        Arg::Number(text) => text.parse().ok(),
        Arg::Text(text) => text.trim().parse().ok(),
        Arg::Value(value) => integer_in(value),
    }
}

/// The integer a canonical Extended JSON number holds.
fn integer_in(value: &Value) -> Option<i64> {
    let object = value.as_object()?;
    if let Some(Value::String(text)) = object.get("$numberInt").or(object.get("$numberLong")) {
        return text.parse().ok();
    }
    match object.get("$numberDouble") {
        Some(Value::String(text)) => {
            let n: f64 = text.parse().ok()?;
            (n.fract() == 0.0 && n.abs() < 9.0e15).then_some(n as i64)
        }
        _ => None,
    }
}

fn int_value(n: i64) -> Value {
    match i32::try_from(n) {
        Ok(small) => json!({"$numberInt": small.to_string()}),
        Err(_) => json!({"$numberLong": n.to_string()}),
    }
}

fn constructed(name: &str, args: Vec<Arg>) -> Result<Value, String> {
    let one_text = |args: &[Arg]| -> Option<String> {
        match args {
            [Arg::Text(text)] => Some(text.clone()),
            _ => None,
        }
    };
    match name {
        "ObjectId" => match one_text(&args) {
            Some(hex) if hex.len() == 24 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Ok(json!({"$oid": hex.to_ascii_lowercase()}))
            }
            Some(_) => Err("ObjectId takes 24 hexadecimal digits".into()),
            None if args.is_empty() => Err(
                "ObjectId() makes a new id in the shell, which is not run here: leave `_id` out \
                 and the server makes one, or write its 24 hexadecimal digits"
                    .into(),
            ),
            None => Err("ObjectId takes one string of 24 hexadecimal digits".into()),
        },
        "ISODate" | "Date" => match args.as_slice() {
            [] => Ok(date_value(now_millis())),
            [Arg::Text(text)] => parse_date(text).map(date_value).ok_or_else(|| {
                format!("`{text}` is not a date MongoDB reads (2024-05-01T12:00:00Z)")
            }),
            [arg] if integer_of(arg).is_some() => Ok(date_value(integer_of(arg).expect("checked"))),
            _ => Err(format!(
                "{name} takes a date as text, or milliseconds since 1970"
            )),
        },
        "NumberInt" => match args.as_slice() {
            [arg] => integer_of(arg)
                .and_then(|n| i32::try_from(n).ok())
                .map(|n| json!({"$numberInt": n.to_string()}))
                .ok_or_else(|| "NumberInt takes a whole number of 32 bits".into()),
            _ => Err("NumberInt takes one number".into()),
        },
        "NumberLong" | "Long" => match args.as_slice() {
            [arg] => integer_of(arg)
                .map(|n| json!({"$numberLong": n.to_string()}))
                .ok_or_else(|| "NumberLong takes a whole number of 64 bits".into()),
            _ => Err("NumberLong takes one number".into()),
        },
        "NumberDecimal" | "Decimal128" => match args.as_slice() {
            [Arg::Text(text)] | [Arg::Number(text)] if decimal_ok(text) => {
                Ok(json!({"$numberDecimal": text.trim()}))
            }
            _ => Err(format!("{name} takes a decimal number, as text")),
        },
        "UUID" => match one_text(&args) {
            Some(text) => {
                let hex: String = text.chars().filter(|c| *c != '-').collect();
                if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("UUID takes 32 hexadecimal digits".into());
                }
                let bytes: Vec<u8> = (0..16)
                    .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("checked"))
                    .collect();
                Ok(json!({"$binary": {"base64": base64(&bytes), "subType": "04"}}))
            }
            None => Err(
                "UUID() makes a new one in the shell, which is not run here: write its digits"
                    .into(),
            ),
        },
        "BinData" => match args.as_slice() {
            [subtype, Arg::Text(data)] => {
                let subtype = integer_of(subtype)
                    .and_then(|n| u8::try_from(n).ok())
                    .ok_or("BinData's subtype is a number from 0 to 255")?;
                if !data
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
                {
                    return Err("BinData's data is base64".into());
                }
                Ok(json!({"$binary": {"base64": data, "subType": format!("{subtype:02x}")}}))
            }
            _ => Err("BinData takes a subtype and base64 text".into()),
        },
        "Timestamp" => match args.as_slice() {
            [t, i] => match (integer_of(t), integer_of(i)) {
                (Some(t), Some(i))
                    if (0..=u32::MAX as i64).contains(&t) && (0..=u32::MAX as i64).contains(&i) =>
                {
                    Ok(json!({"$timestamp": {"t": t, "i": i}}))
                }
                _ => Err("Timestamp takes two whole numbers of 32 bits".into()),
            },
            _ => Err("Timestamp takes a time and an increment".into()),
        },
        "MinKey" | "MaxKey" if args.is_empty() => Ok(min_max(name)),
        other => Err(format!(
            "`{other}` is not read here: the shell's JavaScript is not run"
        )),
    }
}

fn decimal_ok(text: &str) -> bool {
    let t = text.trim();
    let body = t.trim_start_matches(['-', '+']);
    matches!(body, "Infinity" | "NaN")
        || (!body.is_empty()
            && body.chars().any(|c| c.is_ascii_digit())
            && body
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '-' | '+')))
}

fn date_value(millis: i64) -> Value {
    json!({"$date": {"$numberLong": millis.to_string()}})
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// An ISO 8601 date as milliseconds since 1970: `YYYY-MM-DD`, then optionally `THH:MM`,
/// `:SS`, `.fraction` and `Z` or an offset (no offset is UTC, as the shell reads it).
fn parse_date(text: &str) -> Option<i64> {
    let t = text.trim();
    let num = |s: &str| -> Option<i64> {
        (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let (date, rest) = t.split_at(t.find(['T', ' ']).unwrap_or(t.len()));
    let mut parts = date.split('-');
    let year = num(parts.next()?)?;
    let month = num(parts.next()?)?;
    let day = num(parts.next()?)?;
    if parts.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=days_in(year, month)).contains(&day)
    {
        return None;
    }
    let mut millis = days_from_civil(year, month, day) * 86_400_000;
    let rest = rest.strip_prefix(['T', ' ']).unwrap_or(rest);
    if rest.is_empty() {
        return Some(millis);
    }
    // The offset, at the end.
    let (clock, offset) = if let Some(clock) = rest.strip_suffix('Z') {
        (clock, 0)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (clock, zone) = rest.split_at(i);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let zone = &zone[1..];
        let (h, m) = match zone.split_once(':') {
            Some((h, m)) => (num(h)?, num(m)?),
            None if zone.len() == 4 => (num(&zone[..2])?, num(&zone[2..])?),
            None => (num(zone)?, 0),
        };
        if h > 23 || m > 59 {
            return None;
        }
        (clock, sign * (h * 60 + m) * 60_000)
    } else {
        (rest, 0)
    };
    let (hms, fraction) = match clock.split_once('.') {
        Some((hms, fraction)) => (hms, Some(fraction)),
        None => (clock, None),
    };
    let mut fields = hms.split(':');
    let hour = num(fields.next()?)?;
    let minute = num(fields.next()?)?;
    let second = match fields.next() {
        Some(s) => num(s)?,
        None => 0,
    };
    if fields.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let fraction_ms = match fraction {
        None => 0,
        Some(f) => {
            if f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut digits: String = f.chars().take(3).collect();
            while digits.len() < 3 {
                digits.push('0');
            }
            digits.parse::<i64>().ok()?
        }
    };
    millis += ((hour * 60 + minute) * 60 + second) * 1000 + fraction_ms;
    Some(millis - offset)
}

fn days_in(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a date of the proleptic Gregorian calendar.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn one_string(args: &[Value], method: &str, at: usize, p: &Parser) -> Result<String, RequestError> {
    match args {
        [Value::String(name)] if !name.is_empty() => Ok(name.clone()),
        _ => Err(p.error_at(at, &format!("{method} takes one name"))),
    }
}

fn no_more(
    mut args: impl Iterator<Item = Value>,
    method: &str,
    at: usize,
    p: &Parser,
) -> Result<(), RequestError> {
    match args.next() {
        None => Ok(()),
        Some(_) => Err(p.error_at(at, &format!("{method} takes fewer arguments"))),
    }
}

/// Adds an options document's fields to a command.
fn merge(
    command: &mut Map<String, Value>,
    options: Value,
    what: &str,
    at: usize,
    p: &Parser,
) -> Result<(), RequestError> {
    let Value::Object(options) = options else {
        return Err(p.error_at(at, &format!("{what} are a document")));
    };
    for (key, value) in options {
        if command.contains_key(&key) {
            return Err(p.error_at(at, &format!("{what} cannot set `{key}`")));
        }
        command.insert(key, value);
    }
    Ok(())
}

fn document(
    value: Option<Value>,
    what: &str,
    at: usize,
    p: &Parser,
) -> Result<Value, RequestError> {
    match value {
        None => Ok(json!({})),
        Some(value @ Value::Object(_)) => Ok(value),
        Some(_) => Err(p.error_at(at, &format!("{what} is a document"))),
    }
}

/// A collection method and what was chained after it, as one command document.
fn collection_method(
    collection: &str,
    call: &mut Call,
    chained: Vec<Call>,
    p: &Parser,
) -> Result<Map<String, Value>, RequestError> {
    let at = call.at;
    let name = call.name.clone();
    let mut args = std::mem::take(&mut call.args).into_iter();
    let mut command = Map::new();
    let chain_allowed = matches!(name.as_str(), "find");
    if !chain_allowed && let Some(first) = chained.first() {
        return Err(p.error_at(
            first.at,
            &format!(".{}() does not follow {name}()", first.name),
        ));
    }
    match name.as_str() {
        "find" | "findOne" => {
            command.insert("find".into(), collection.into());
            command.insert(
                "filter".into(),
                document(args.next(), "find's filter", at, p)?,
            );
            if let Some(projection) = args.next() {
                command.insert(
                    "projection".into(),
                    document(Some(projection), "find's projection", at, p)?,
                );
            }
            no_more(args, &name, at, p)?;
            if name == "findOne" {
                command.insert("limit".into(), int_value(1));
                command.insert("singleBatch".into(), Value::Bool(true));
            }
            for link in chained {
                let mut link_args = link.args.into_iter();
                let (key, value) = match link.name.as_str() {
                    "sort" | "projection" | "collation" => (
                        link.name.clone(),
                        document(
                            link_args.next(),
                            &format!("{}'s argument", link.name),
                            link.at,
                            p,
                        )?,
                    ),
                    "hint" => match link_args.next() {
                        Some(hint @ (Value::Object(_) | Value::String(_))) => ("hint".into(), hint),
                        _ => {
                            return Err(
                                p.error_at(link.at, "hint takes an index's keys or its name")
                            );
                        }
                    },
                    "comment" => match link_args.next() {
                        Some(comment) => ("comment".into(), comment),
                        None => return Err(p.error_at(link.at, "comment takes a value")),
                    },
                    "limit" | "skip" | "batchSize" | "maxTimeMS" => {
                        let n =
                            link_args
                                .next()
                                .as_ref()
                                .and_then(integer_in)
                                .ok_or_else(|| {
                                    p.error_at(
                                        link.at,
                                        &format!("{} takes a whole number", link.name),
                                    )
                                })?;
                        if link.name == "limit" && n < 0 {
                            command.insert("singleBatch".into(), Value::Bool(true));
                            ("limit".into(), int_value(-n))
                        } else if n < 0 {
                            return Err(
                                p.error_at(link.at, &format!("{} is not negative", link.name))
                            );
                        } else {
                            (link.name.clone(), int_value(n))
                        }
                    }
                    other => {
                        return Err(
                            p.error_at(link.at, &format!(".{other}() is not read after find()"))
                        );
                    }
                };
                no_more(link_args, &link.name, link.at, p)?;
                if command.insert(key.clone(), value).is_some() && key != "limit" {
                    return Err(p.error_at(link.at, &format!(".{key}() is written twice")));
                }
            }
        }
        "aggregate" => {
            let pipeline = match args.next() {
                Some(pipeline @ Value::Array(_)) => pipeline,
                _ => return Err(p.error_at(at, "aggregate takes a pipeline: [ {…}, … ]")),
            };
            command.insert("aggregate".into(), collection.into());
            command.insert("pipeline".into(), pipeline);
            let options = args.next();
            no_more(args, &name, at, p)?;
            let has_cursor = options
                .as_ref()
                .and_then(|o| o.as_object())
                .is_some_and(|o| o.contains_key("cursor"));
            if let Some(options) = options {
                merge(&mut command, options, "aggregate's options", at, p)?;
            }
            if !has_cursor {
                command.insert("cursor".into(), json!({}));
            }
        }
        "countDocuments" => {
            command.insert("count".into(), collection.into());
            command.insert(
                "query".into(),
                document(args.next(), "countDocuments' filter", at, p)?,
            );
            if let Some(options) = args.next() {
                merge(&mut command, options, "countDocuments' options", at, p)?;
            }
            no_more(args, &name, at, p)?;
        }
        "estimatedDocumentCount" => {
            command.insert("count".into(), collection.into());
            if let Some(options) = args.next() {
                merge(
                    &mut command,
                    options,
                    "estimatedDocumentCount's options",
                    at,
                    p,
                )?;
            }
            no_more(args, &name, at, p)?;
        }
        "distinct" => {
            let key = match args.next() {
                Some(Value::String(key)) if !key.is_empty() => key,
                _ => return Err(p.error_at(at, "distinct takes a field's name")),
            };
            command.insert("distinct".into(), collection.into());
            command.insert("key".into(), key.into());
            command.insert(
                "query".into(),
                document(args.next(), "distinct's filter", at, p)?,
            );
            no_more(args, &name, at, p)?;
        }
        "insertOne" | "insertMany" => {
            let documents = match (name.as_str(), args.next()) {
                ("insertOne", Some(doc @ Value::Object(_))) => vec![doc],
                ("insertMany", Some(Value::Array(docs)))
                    if !docs.is_empty() && docs.iter().all(Value::is_object) =>
                {
                    docs
                }
                ("insertOne", _) => return Err(p.error_at(at, "insertOne takes a document")),
                _ => return Err(p.error_at(at, "insertMany takes a list of documents")),
            };
            command.insert("insert".into(), collection.into());
            command.insert("documents".into(), Value::Array(documents));
            if let Some(options) = args.next() {
                let ordered = options.get("ordered").cloned();
                if let Some(ordered) = ordered {
                    command.insert("ordered".into(), ordered);
                }
            }
            no_more(args, &name, at, p)?;
        }
        "updateOne" | "updateMany" | "replaceOne" => {
            let filter = match args.next() {
                Some(filter @ Value::Object(_)) => filter,
                _ => return Err(p.error_at(at, &format!("{name} takes a filter first"))),
            };
            let update = match args.next() {
                Some(update @ (Value::Object(_) | Value::Array(_))) => update,
                _ => return Err(p.error_at(at, &format!("{name} takes the change second"))),
            };
            let mut statement = Map::new();
            statement.insert("q".into(), filter);
            statement.insert("u".into(), update);
            statement.insert("multi".into(), Value::Bool(name == "updateMany"));
            if let Some(options) = args.next() {
                let Value::Object(options) = options else {
                    return Err(p.error_at(at, &format!("{name}'s options are a document")));
                };
                for (key, value) in options {
                    match key.as_str() {
                        "upsert" | "arrayFilters" | "hint" | "collation" => {
                            statement.insert(key, value);
                        }
                        other => {
                            return Err(p.error_at(
                                at,
                                &format!("{name} does not read the option `{other}`"),
                            ));
                        }
                    }
                }
            }
            no_more(args, &name, at, p)?;
            command.insert("update".into(), collection.into());
            command.insert("updates".into(), json!([Value::Object(statement)]));
        }
        "deleteOne" | "deleteMany" => {
            let filter = match args.next() {
                Some(filter @ Value::Object(_)) => filter,
                _ => {
                    return Err(p.error_at(
                        at,
                        &format!("{name} takes a filter ({{}} for every document)"),
                    ));
                }
            };
            no_more(args, &name, at, p)?;
            let limit = if name == "deleteOne" { 1 } else { 0 };
            command.insert("delete".into(), collection.into());
            command.insert(
                "deletes".into(),
                json!([{"q": filter, "limit": int_value(limit)}]),
            );
        }
        "createIndex" => {
            let keys = match args.next() {
                Some(Value::Object(keys)) if !keys.is_empty() => keys,
                _ => return Err(p.error_at(at, "createIndex takes the index's keys")),
            };
            let mut index = Map::new();
            index.insert("key".into(), Value::Object(keys.clone()));
            let options = match args.next() {
                None => Map::new(),
                Some(Value::Object(options)) => options,
                Some(_) => return Err(p.error_at(at, "createIndex's options are a document")),
            };
            no_more(args, &name, at, p)?;
            if !options.contains_key("name") {
                index.insert("name".into(), index_name(&keys).into());
            }
            for (key, value) in options {
                if key == "key" {
                    return Err(p.error_at(at, "createIndex's options cannot set `key`"));
                }
                index.insert(key, value);
            }
            command.insert("createIndexes".into(), collection.into());
            command.insert("indexes".into(), json!([Value::Object(index)]));
        }
        "dropIndex" => {
            let index = match args.next() {
                Some(index @ (Value::String(_) | Value::Object(_))) => index,
                _ => return Err(p.error_at(at, "dropIndex takes an index's name or keys")),
            };
            no_more(args, &name, at, p)?;
            command.insert("dropIndexes".into(), collection.into());
            command.insert("index".into(), index);
        }
        "dropIndexes" => {
            no_more(args, &name, at, p)?;
            command.insert("dropIndexes".into(), collection.into());
            command.insert("index".into(), "*".into());
        }
        "getIndexes" => {
            no_more(args, &name, at, p)?;
            command.insert("listIndexes".into(), collection.into());
        }
        "drop" => {
            no_more(args, &name, at, p)?;
            command.insert("drop".into(), collection.into());
        }
        other => {
            return Err(p.error_at(
                at,
                &format!(
                    ".{other}() is not read here: write db.runCommand({{…}}) with the command \
                     it runs"
                ),
            ));
        }
    }
    Ok(command)
}

/// The name MongoDB gives an index: each key and its direction, joined by `_`.
fn index_name(keys: &Map<String, Value>) -> String {
    keys.iter()
        .map(|(key, value)| {
            let direction = match value {
                Value::String(text) => text.clone(),
                other => integer_in(other)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "1".into()),
            };
            format!("{key}_{direction}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

// -------------------------------------------------------------------------------- guard

/// What a request is to the guard (ADR-056, point 4), read from its command document.
pub fn request_class(request: &Value) -> RequestClass {
    let Some(command) = request.get("command").and_then(Value::as_object) else {
        return RequestClass::Unknown;
    };
    let Some((name, _)) = command.iter().next() else {
        return RequestClass::Unknown;
    };
    // Code that runs on the server: the core cannot see what it does.
    if mentions_code(&Value::Object(command.clone())) {
        return RequestClass::Unknown;
    }
    match name.as_str() {
        "find" | "count" | "distinct" | "listCollections" | "listIndexes" | "listDatabases"
        | "ping" | "buildInfo" | "hello" | "isMaster" | "serverStatus" | "dbStats"
        | "collStats" | "connectionStatus" | "hostInfo" | "getParameter" => RequestClass::Read,
        "aggregate" => match command.get("pipeline").and_then(Value::as_array) {
            Some(pipeline)
                if pipeline.iter().any(|stage| {
                    stage
                        .as_object()
                        .is_some_and(|s| s.contains_key("$out") || s.contains_key("$merge"))
                }) =>
            {
                RequestClass::Drop
            }
            Some(_) => RequestClass::Read,
            None => RequestClass::Unknown,
        },
        "explain" => match command.get("explain") {
            // An explain that executes runs what it explains.
            Some(inner)
                if command.get("verbosity").and_then(Value::as_str) == Some("queryPlanner") =>
            {
                request_class(&json!({"command": inner}))
            }
            _ => RequestClass::Unknown,
        },
        "insert" | "createIndexes" | "create" => RequestClass::Write,
        "delete" => match command.get("deletes").and_then(Value::as_array) {
            Some(deletes) if !deletes.is_empty() => {
                // `limit: 0` deletes every match; `limit: 1` only one.
                let every = |d: &Value| {
                    empty_filter(d.get("q")) && d.get("limit").and_then(integer_in) != Some(1)
                };
                if deletes.iter().any(every) {
                    RequestClass::DeleteAll
                } else {
                    RequestClass::Write
                }
            }
            _ => RequestClass::Unknown,
        },
        "update" => match command.get("updates").and_then(Value::as_array) {
            Some(updates) if !updates.is_empty() => {
                let all = updates.iter().any(|u| {
                    empty_filter(u.get("q"))
                        && u.get("multi").and_then(Value::as_bool) == Some(true)
                });
                if all {
                    RequestClass::UpdateAll
                } else {
                    RequestClass::Write
                }
            }
            _ => RequestClass::Unknown,
        },
        "findAndModify" | "findandmodify" => {
            if command.get("remove").and_then(Value::as_bool) == Some(true)
                || command.contains_key("update")
            {
                RequestClass::Write
            } else {
                RequestClass::Unknown
            }
        }
        "drop" | "dropDatabase" | "dropIndexes" | "deleteIndexes" | "renameCollection" => {
            RequestClass::Drop
        }
        _ => RequestClass::Unknown,
    }
}

/// A filter that matches every document: missing or `{}`.
fn empty_filter(filter: Option<&Value>) -> bool {
    match filter {
        None | Some(Value::Null) => true,
        Some(Value::Object(map)) => map.is_empty(),
        Some(_) => false,
    }
}

/// Whether a value holds server-side code anywhere: `$where`, `$function`, `$accumulator`,
/// `mapReduce`'s functions, JavaScript values.
fn mentions_code(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "$where" | "$function" | "$accumulator" | "$code" | "mapReduce"
            ) || mentions_code(value)
        }),
        Value::Array(items) => items.iter().any(mentions_code),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(statement: &str) -> Value {
        to_request(statement, "shop")
            .unwrap_or_else(|e| panic!("{statement}: {e} at {}", e.position))
    }

    fn command(statement: &str) -> Value {
        request(statement)["command"].clone()
    }

    fn refused(statement: &str) -> RequestError {
        match to_request(statement, "shop") {
            Ok(request) => panic!("{statement} was read as {request}"),
            Err(error) => error,
        }
    }

    fn keys(value: &Value) -> Vec<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn find_with_its_chain_is_one_find_command_named_by_its_first_key() {
        let request = request(
            r#"db.users.find({age: {$gt: 30}}, {name: 1}).sort({name: -1}).skip(10).limit(5)"#,
        );
        assert_eq!(request["database"], "shop");
        let command = &request["command"];
        assert_eq!(
            keys(command),
            ["find", "filter", "projection", "sort", "skip", "limit"]
        );
        assert_eq!(
            *command,
            json!({"find": "users", "filter": {"age": {"$gt": {"$numberInt": "30"}}},
                "projection": {"name": {"$numberInt": "1"}}, "sort": {"name": {"$numberInt": "-1"}},
                "skip": {"$numberInt": "10"}, "limit": {"$numberInt": "5"}})
        );
        assert_eq!(
            command_of("db.users.find()"),
            json!({"find": "users", "filter": {}})
        );
        assert_eq!(
            command_of("db.users.find().limit(-3)"),
            json!({"find": "users", "filter": {}, "singleBatch": true, "limit": {"$numberInt": "3"}})
        );
    }

    fn command_of(statement: &str) -> Value {
        command(statement)
    }

    #[test]
    fn collections_are_named_every_way_the_shell_names_them() {
        for statement in [
            r#"db.users.find()"#,
            r#"db["users"].find()"#,
            r#"db['users'].find()"#,
            r#"db.getCollection("users").find()"#,
            "db .users\n  .find()",
        ] {
            assert_eq!(command(statement)["find"], "users", "{statement}");
        }
        assert_eq!(command("db.system.users.find()")["find"], "system.users");
        assert_eq!(
            command(r#"db.getCollection("a b-c").find()"#)["find"],
            "a b-c"
        );
        let other = request(r#"db.getSiblingDB("logs").events.find()"#);
        assert_eq!(other["database"], "logs");
        assert_eq!(other["command"]["find"], "events");
    }

    #[test]
    fn every_method_writes_the_command_the_shell_sends() {
        let cases: Vec<(&str, Value)> = vec![
            (
                "db.c.findOne({a: 1})",
                json!({"find": "c", "filter": {"a": {"$numberInt": "1"}}, "limit": {"$numberInt": "1"}, "singleBatch": true}),
            ),
            (
                "db.c.aggregate([{$match: {}}, {$count: 'n'}])",
                json!({"aggregate": "c", "pipeline": [{"$match": {}}, {"$count": "n"}], "cursor": {}}),
            ),
            (
                "db.c.aggregate([], {allowDiskUse: true})",
                json!({"aggregate": "c", "pipeline": [], "allowDiskUse": true, "cursor": {}}),
            ),
            (
                "db.c.countDocuments({a: 1})",
                json!({"count": "c", "query": {"a": {"$numberInt": "1"}}}),
            ),
            ("db.c.estimatedDocumentCount()", json!({"count": "c"})),
            (
                "db.c.distinct('city', {a: 1})",
                json!({"distinct": "c", "key": "city", "query": {"a": {"$numberInt": "1"}}}),
            ),
            (
                "db.c.insertOne({_id: 1})",
                json!({"insert": "c", "documents": [{"_id": {"$numberInt": "1"}}]}),
            ),
            (
                "db.c.insertMany([{a: 1}, {a: 2}], {ordered: false})",
                json!({"insert": "c", "documents": [{"a": {"$numberInt": "1"}}, {"a": {"$numberInt": "2"}}], "ordered": false}),
            ),
            (
                "db.c.updateOne({_id: 1}, {$set: {a: 2}}, {upsert: true})",
                json!({"update": "c", "updates": [{"q": {"_id": {"$numberInt": "1"}}, "u": {"$set": {"a": {"$numberInt": "2"}}}, "multi": false, "upsert": true}]}),
            ),
            (
                "db.c.updateMany({a: 1}, [{$set: {b: 1}}])",
                json!({"update": "c", "updates": [{"q": {"a": {"$numberInt": "1"}}, "u": [{"$set": {"b": {"$numberInt": "1"}}}], "multi": true}]}),
            ),
            (
                "db.c.replaceOne({_id: 1}, {a: 1})",
                json!({"update": "c", "updates": [{"q": {"_id": {"$numberInt": "1"}}, "u": {"a": {"$numberInt": "1"}}, "multi": false}]}),
            ),
            (
                "db.c.deleteOne({_id: 1})",
                json!({"delete": "c", "deletes": [{"q": {"_id": {"$numberInt": "1"}}, "limit": {"$numberInt": "1"}}]}),
            ),
            (
                "db.c.deleteMany({})",
                json!({"delete": "c", "deletes": [{"q": {}, "limit": {"$numberInt": "0"}}]}),
            ),
            (
                "db.c.createIndex({email: 1, age: -1}, {unique: true})",
                json!({"createIndexes": "c", "indexes": [{"key": {"email": {"$numberInt": "1"}, "age": {"$numberInt": "-1"}}, "name": "email_1_age_-1", "unique": true}]}),
            ),
            (
                "db.c.createIndex({body: 'text'}, {name: 'search'})",
                json!({"createIndexes": "c", "indexes": [{"key": {"body": "text"}, "name": "search"}]}),
            ),
            (
                "db.c.dropIndex('email_1')",
                json!({"dropIndexes": "c", "index": "email_1"}),
            ),
            (
                "db.c.dropIndexes()",
                json!({"dropIndexes": "c", "index": "*"}),
            ),
            ("db.c.getIndexes()", json!({"listIndexes": "c"})),
            ("db.c.drop()", json!({"drop": "c"})),
            (
                "db.runCommand({ping: 1})",
                json!({"ping": {"$numberInt": "1"}}),
            ),
            (
                "db.createCollection('logs', {capped: true, size: 1024})",
                json!({"create": "logs", "capped": true, "size": {"$numberInt": "1024"}}),
            ),
            ("db.dropDatabase()", json!({"dropDatabase": 1})),
            (
                "db.getCollectionNames()",
                json!({"listCollections": 1, "nameOnly": true}),
            ),
            (
                "show collections",
                json!({"listCollections": 1, "nameOnly": true}),
            ),
        ];
        for (statement, expected) in cases {
            assert_eq!(command(statement), expected, "{statement}");
        }
        let admin = request("db.adminCommand({listDatabases: 1})");
        assert_eq!(admin["database"], "admin");
        assert_eq!(request("show dbs")["database"], "admin");
    }

    #[test]
    fn values_keep_every_type_as_extended_json() {
        let c = command(concat!(
            r#"db.c.insertOne({s: "a\"b\nñ", t: 'it\'s', i: 7, big: 9007199254740993, f: 1.5, w: 2.0, e: 1e3, "#,
            r#"neg: -4, inf: -Infinity, yes: true, nil: null, re: /^a.*b/mi, "#,
            r#"oid: ObjectId("65A1B2C3D4E5F60718293A4B"), d: ISODate("2024-05-01T12:30:00.250+02:00"), "#,
            r#"d2: new Date("2024-05-01"), d3: new Date(0), l: NumberLong("9007199254740993"), n: NumberInt(5), "#,
            r#"dec: NumberDecimal("1.10"), u: UUID("0123456789ab-cdef-0123-456789abcdef"), b: BinData(0, "AAE="), "#,
            r#"ts: Timestamp(1700000000, 7), lo: MinKey(), hi: MaxKey, nested: {a: [1, {b: 'x'}], }, })"#
        ));
        let d = &c["documents"][0];
        assert_eq!(d["s"], "a\"b\nñ");
        assert_eq!(d["i"], json!({"$numberInt": "7"}));
        assert_eq!(d["big"], json!({"$numberLong": "9007199254740993"}));
        assert_eq!(d["f"], json!({"$numberDouble": "1.5"}));
        assert_eq!(d["w"], json!({"$numberDouble": "2.0"}));
        assert_eq!(d["e"], json!({"$numberDouble": "1000.0"}));
        assert_eq!(d["neg"], json!({"$numberInt": "-4"}));
        assert_eq!(d["inf"], json!({"$numberDouble": "-Infinity"}));
        assert_eq!(d["yes"], true);
        assert_eq!(d["nil"], Value::Null);
        assert_eq!(d["t"], "it's");
        assert_eq!(
            d["re"],
            json!({"$regularExpression": {"pattern": "^a.*b", "options": "im"}})
        );
        assert_eq!(d["oid"], json!({"$oid": "65a1b2c3d4e5f60718293a4b"}));
        assert_eq!(d["d"], json!({"$date": {"$numberLong": "1714559400250"}}));
        assert_eq!(d["d2"], json!({"$date": {"$numberLong": "1714521600000"}}));
        assert_eq!(d["d3"], json!({"$date": {"$numberLong": "0"}}));
        assert_eq!(d["l"], json!({"$numberLong": "9007199254740993"}));
        assert_eq!(d["n"], json!({"$numberInt": "5"}));
        assert_eq!(d["dec"], json!({"$numberDecimal": "1.10"}));
        assert_eq!(
            d["u"],
            json!({"$binary": {"base64": "ASNFZ4mrze8BI0VniavN7w==", "subType": "04"}})
        );
        assert_eq!(
            d["b"],
            json!({"$binary": {"base64": "AAE=", "subType": "00"}})
        );
        assert_eq!(d["ts"], json!({"$timestamp": {"t": 1700000000, "i": 7}}));
        assert_eq!(d["lo"], json!({"$minKey": 1}));
        assert_eq!(d["hi"], json!({"$maxKey": 1}));
        assert_eq!(d["nested"], json!({"a": [{"$numberInt": "1"}, {"b": "x"}]}));
    }

    #[test]
    fn javascript_is_never_read_as_a_statement() {
        for statement in [
            "var x = 1",
            "x = db.users.find()",
            "db.users.find().forEach(printjson)",
            "db.users.find({a: Math.random()})",
            "db.users.find({a: x})",
            "db.users.find({$where: function() { return true }})",
            "for (let i = 0; i < 3; i++) db.c.insertOne({i})",
            "load('script.js')",
            "db.eval('1')",
            "db.users.find().toArray()",
            "db.users.find({}).map(d => d)",
            "use shop",
            "db.users.find({a: undefined})",
            "db.users.insertOne({_id: ObjectId()})",
            "db.users.find({a: 1}) db.users.drop()",
            "db.users.deleteMany()",
            "db.users.aggregate({$match: {}})",
            "db.users.find({a: 1, a: 2})",
            "db.users.find({d: Date()})",
            "db.users.find({r: /a/g})",
        ] {
            let error = refused(statement);
            assert!(!error.message.is_empty(), "{statement}");
        }
        let error = refused("db.users.find({a: Math.random()})");
        assert_eq!(error.position, "db.users.find({a: ".len());
    }

    #[test]
    fn scripts_split_at_semicolons_and_at_lines_that_do_not_continue() {
        let script = "db.a.find()\n// a comment; with a semicolon\ndb.b.find({x: ';'});db.c\n  .find()\n  .limit(2)\n\ndb.d.insertOne({\n  a: 1\n})\n/* only a comment */";
        let spans = MongodbDialect.split_statements(script);
        let texts: Vec<&str> = spans.iter().map(|s| s.text(script)).collect();
        assert_eq!(
            texts,
            [
                "db.a.find()",
                "db.b.find({x: ';'})",
                "db.c\n  .find()\n  .limit(2)",
                "db.d.insertOne({\n  a: 1\n})",
            ]
        );
        for text in texts {
            to_request(text, "shop").unwrap_or_else(|e| panic!("{text}: {e}"));
        }
        assert!(
            MongodbDialect
                .split_statements("  // nothing\n /* at all */ ")
                .is_empty()
        );
    }

    #[test]
    fn the_guard_reads_the_command_not_the_text() {
        let class = |statement: &str| request_class(&request(statement));
        for read in [
            "db.c.find({a: 1})",
            "db.c.aggregate([{$group: {_id: '$a'}}])",
            "db.c.countDocuments({})",
            "db.c.distinct('a')",
            "show dbs",
            "db.runCommand({find: 'c', filter: {}})",
            "db.c.getIndexes()",
        ] {
            assert_eq!(class(read), RequestClass::Read, "{read}");
        }
        for write in [
            "db.c.insertOne({a: 1})",
            "db.c.updateOne({}, {$set: {a: 1}})",
            "db.c.updateMany({a: 1}, {$set: {b: 1}})",
            "db.c.deleteOne({})",
            "db.c.deleteMany({a: 1})",
            "db.c.createIndex({a: 1})",
            "db.runCommand({insert: 'c', documents: [{}]})",
        ] {
            assert_eq!(class(write), RequestClass::Write, "{write}");
        }
        assert_eq!(class("db.c.deleteMany({})"), RequestClass::DeleteAll);
        assert_eq!(
            class(
                "db.runCommand({delete: 'c', deletes: [{q: {_id: 1}, limit: 1}, {q: {}, limit: 0}]})"
            ),
            RequestClass::DeleteAll
        );
        assert_eq!(
            class("db.c.updateMany({}, {$set: {a: 1}})"),
            RequestClass::UpdateAll
        );
        for drop in [
            "db.c.drop()",
            "db.dropDatabase()",
            "db.c.dropIndex('a_1')",
            "db.c.aggregate([{$match: {}}, {$out: 'other'}])",
            "db.c.aggregate([{$merge: {into: 'other'}}])",
            "db.runCommand({drop: 'c'})",
            "db.adminCommand({renameCollection: 'shop.a', to: 'shop.b'})",
        ] {
            assert_eq!(class(drop), RequestClass::Drop, "{drop}");
        }
        for unknown in [
            "db.c.find({$where: 'sleep(1000)'})",
            "db.c.aggregate([{$project: {x: {$function: {body: 'f', args: [], lang: 'js'}}}}])",
            "db.runCommand({shutdown: 1})",
            "db.runCommand({setParameter: 1, x: 1})",
            "db.runCommand({explain: {delete: 'c', deletes: [{q: {}, limit: 0}]}})",
            "db.adminCommand({createUser: 'u', pwd: 'p', roles: []})",
        ] {
            assert_eq!(class(unknown), RequestClass::Unknown, "{unknown}");
        }
        assert_eq!(
            class("db.runCommand({explain: {find: 'c', filter: {}}, verbosity: 'queryPlanner'})"),
            RequestClass::Read
        );
        assert_eq!(request_class(&json!({"nothing": 1})), RequestClass::Unknown);
    }

    #[test]
    fn opening_a_collection_reads_a_bounded_page_of_it_wherever_it_is() {
        let statement = MongodbDialect
            .browse_statement(&spelaion_driver_api::ObjectInfo {
                schema: "logs \"2024\"".into(),
                name: r"a.b\c".into(),
                kind: spelaion_driver_api::ObjectKind::Collection,
                estimated_rows: None,
                comment: None,
            })
            .unwrap();
        let request = to_request(&statement, "elsewhere").unwrap();
        assert_eq!(request["database"], "logs \"2024\"");
        assert_eq!(
            request["command"],
            json!({"find": r"a.b\c", "filter": {}, "limit": {"$numberInt": "200"}})
        );
        assert_eq!(request_class(&request), RequestClass::Read);
    }

    #[test]
    fn dates_are_read_as_the_shell_reads_them() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(
            parse_date("2000-02-29T23:59:59.999Z"),
            Some(951_868_799_999)
        );
        assert_eq!(parse_date("1969-12-31T23:00:00-01:00"), Some(0));
        assert_eq!(parse_date("2024-05-01 12:00"), Some(1_714_564_800_000));
        for bad in [
            "2023-02-29",
            "2024-13-01",
            "2024-05-01T24:00",
            "yesterday",
            "2024-05-01T12:00:00.Z",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }
}
