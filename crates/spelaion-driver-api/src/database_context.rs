//! Conservative local context for observers that must not execute metadata SQL.
//! In particular, SELECT DATABASE() would overwrite MySQL's ROW_COUNT().

#[derive(Debug, Clone, Copy)]
pub enum ContextDialect {
    Mysql,
    SqlServer,
    Duckdb,
}

/// Owned by a connection actor. Unknown is preferable to attributing SQL to a wrong database.
pub struct DatabaseContext {
    dialect: ContextDialect,
    database: Option<String>,
    uncertain: bool,
}

#[derive(Debug)]
enum Token {
    Word(String),
    Identifier(String),
    Dot,
    Semicolon,
    Other,
}
impl Token {
    fn identifier(&self) -> Option<&str> {
        match self {
            Self::Word(name) | Self::Identifier(name) => Some(name),
            Self::Dot | Self::Semicolon | Self::Other => None,
        }
    }
    fn keyword(&self, keyword: &str) -> bool {
        matches!(self,Self::Word(word) if word.eq_ignore_ascii_case(keyword))
    }
}

impl DatabaseContext {
    pub fn new(dialect: ContextDialect, database: Option<String>) -> Self {
        Self {
            dialect,
            database: database.filter(|name| !name.is_empty()),
            uncertain: false,
        }
    }
    pub fn snapshot(&self) -> Option<String> {
        self.database.clone()
    }
    /// An existing explicit metadata operation obtained this from the source server.
    pub fn refresh(&mut self, database: Option<String>) {
        self.database = database.filter(|name| !name.is_empty());
        self.uncertain = self.database.is_none();
    }

    /// Called after source SQL finishes, including failures (a batch may have partly run).
    /// This observes syntax; it never sends a statement or guesses the outcome of a branch.
    pub fn observe(&mut self, sql: &str, succeeded: bool) {
        if self.uncertain {
            return;
        }
        let Some(tokens) = tokens(sql, self.dialect) else {
            self.refresh(None);
            return;
        };
        let tokens = tokens.as_slice();
        let start = tokens
            .iter()
            .position(|token| !matches!(token, Token::Semicolon))
            .unwrap_or(tokens.len());
        let end = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Semicolon))
            .map_or(start, |index| index + 1);
        let tokens = &tokens[start..end];
        if tokens.is_empty() {
            return;
        }
        // A later USE may only parse/produce a plan. Success no longer proves execution.
        if matches!(self.dialect, ContextDialect::SqlServer)
            && tokens.iter().any(|token| {
                [
                    "PARSEONLY",
                    "NOEXEC",
                    "FMTONLY",
                    "SHOWPLAN_ALL",
                    "SHOWPLAN_TEXT",
                    "SHOWPLAN_XML",
                ]
                .iter()
                .any(|keyword| token.keyword(keyword))
            })
        {
            self.refresh(None);
            return;
        }
        if matches!(self.dialect, ContextDialect::Duckdb)
            && (tokens.iter().any(|token| token.keyword("SET_CONFIG"))
                || tokens.iter().any(|token| {
                    ["SET", "RESET", "PRAGMA"]
                        .iter()
                        .any(|keyword| token.keyword(keyword))
                }) && tokens.iter().any(|token| {
                    ["SCHEMA", "SEARCH_PATH", "DEFAULT_DATABASE", "ALL"]
                        .iter()
                        .any(|keyword| token.keyword(keyword))
                })
                || succeeded && tokens.first().is_some_and(|token| token.keyword("DETACH")))
        {
            self.refresh(None);
            return;
        }
        if tokens.windows(2).any(|pair| {
            ["ALTER", "DROP", "RENAME"]
                .iter()
                .any(|keyword| pair[0].keyword(keyword))
                && ["DATABASE", "SCHEMA"]
                    .iter()
                    .any(|keyword| pair[1].keyword(keyword))
        }) {
            self.refresh(None);
            return;
        }
        if tokens.first().is_some_and(|token| token.keyword("USE")) {
            let simple = match self.dialect {
                ContextDialect::Mysql | ContextDialect::SqlServer => match tokens {
                    [_, name] => name.identifier().map(str::to_owned),
                    _ => None,
                },
                // One part can be a schema in the current catalog or another catalog.
                ContextDialect::Duckdb => match tokens {
                    [_, catalog, Token::Dot, schema] if schema.identifier().is_some() => {
                        catalog.identifier().map(str::to_owned)
                    }
                    _ => None,
                },
            };
            if let Some(database) = simple {
                if succeeded {
                    self.database = Some(database);
                }
                return;
            }
            // A failed single-name USE is atomic, even when its successful meaning is ambiguous.
            if !succeeded && matches!(tokens,[_,name] if name.identifier().is_some()) {
                return;
            }
            self.refresh(None);
            return;
        }
        if tokens.iter().any(|token| {
            ["USE", "EXEC", "EXECUTE", "CALL"]
                .iter()
                .any(|keyword| token.keyword(keyword))
        }) {
            self.refresh(None);
            return;
        }
        // A bare procedure name is executable T-SQL at the start of a batch. This list
        // establishes only metadata certainty; it never authorizes or rewrites user SQL.
        if matches!(self.dialect, ContextDialect::SqlServer)
            && !tokens.first().is_some_and(|token| {
                [
                    "SELECT",
                    "INSERT",
                    "UPDATE",
                    "DELETE",
                    "MERGE",
                    "WITH",
                    "CREATE",
                    "ALTER",
                    "DROP",
                    "TRUNCATE",
                    "GRANT",
                    "DENY",
                    "REVOKE",
                    "BEGIN",
                    "COMMIT",
                    "ROLLBACK",
                    "SAVE",
                    "SET",
                    "PRINT",
                    "RAISERROR",
                    "THROW",
                    "DECLARE",
                    "FETCH",
                    "OPEN",
                    "CLOSE",
                    "DEALLOCATE",
                    "BREAK",
                    "CONTINUE",
                    "IF",
                    "WHILE",
                    "RETURN",
                    "WAITFOR",
                    "BACKUP",
                    "RESTORE",
                    "DBCC",
                    "CHECKPOINT",
                ]
                .iter()
                .any(|keyword| token.keyword(keyword))
            })
        {
            self.refresh(None);
        }
    }
}

fn tokens(sql: &str, dialect: ContextDialect) -> Option<Vec<Token>> {
    // Metadata work stays bounded even for a huge generated INSERT; the user SQL still runs.
    if sql.len() > 1_048_576 {
        return None;
    }
    let bytes = sql.as_bytes();
    let mut out = vec![];
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if bytes[at..].starts_with(b"--")
            && (!matches!(dialect, ContextDialect::Mysql)
                || bytes
                    .get(at + 2)
                    .is_none_or(|byte| byte.is_ascii_whitespace()))
            || matches!(dialect, ContextDialect::Mysql) && bytes[at] == b'#'
        {
            at = sql[at..].find('\n').map_or(bytes.len(), |end| at + end + 1);
            continue;
        }
        if bytes[at..].starts_with(b"/*") {
            if matches!(dialect, ContextDialect::Mysql)
                && (bytes[at..].starts_with(b"/*!") || bytes[at..].starts_with(b"/*M!"))
            {
                return None;
            }
            let mut depth = 1;
            at += 2;
            while at < bytes.len() && depth > 0 {
                if !matches!(dialect, ContextDialect::Mysql) && bytes[at..].starts_with(b"/*") {
                    depth += 1;
                    at += 2;
                } else if bytes[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
            if depth > 0 {
                return None;
            }
            continue;
        }
        match bytes[at] {
            b'.' => {
                out.push(Token::Dot);
                at += 1;
            }
            b';' => {
                out.push(Token::Semicolon);
                at += 1;
            }
            b'\'' | b'"' | b'`' | b'[' => {
                let opening = bytes[at];
                let closing = if opening == b'[' { b']' } else { opening };
                at += 1;
                let mut name = String::new();
                let mut closed = false;
                while at < bytes.len() {
                    if bytes[at] == closing {
                        if bytes.get(at + 1) == Some(&closing) {
                            name.push(closing as char);
                            at += 2;
                            continue;
                        }
                        at += 1;
                        closed = true;
                        break;
                    }
                    // MySQL's session SQL mode decides backslash escapes; do not guess it.
                    if bytes[at] == b'\\' && matches!(dialect, ContextDialect::Mysql) {
                        return None;
                    }
                    let character = sql[at..].chars().next()?;
                    name.push(character);
                    at += character.len_utf8();
                }
                if !closed {
                    return None;
                }
                out.push(if opening == b'\'' {
                    Token::Other
                } else {
                    Token::Identifier(name)
                });
            }
            _ => {
                let character = sql[at..].chars().next()?;
                if character.is_alphanumeric() || matches!(character, '_' | '$' | '@') {
                    let start = at;
                    at += character.len_utf8();
                    while at < bytes.len() {
                        let character = sql[at..].chars().next()?;
                        if !(character.is_alphanumeric() || matches!(character, '_' | '$' | '@')) {
                            break;
                        }
                        at += character.len_utf8();
                    }
                    out.push(Token::Word(sql[start..at].into()));
                } else {
                    out.push(Token::Other);
                    at += character.len_utf8();
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn context_observation_is_conservative_and_handles_quoted_names() {
        for (dialect, use_sql, expected) in [
            (
                ContextDialect::Mysql,
                "-- note\nUSE `café``name`;",
                "café`name",
            ),
            (
                ContextDialect::SqlServer,
                "/* a /* nested */ comment */ USE [other]]db]",
                "other]db",
            ),
            (
                ContextDialect::Duckdb,
                "USE \"another\".\"main\";",
                "another",
            ),
        ] {
            let mut context = DatabaseContext::new(dialect, Some("initial".into()));
            context.observe(use_sql, false);
            assert_eq!(context.snapshot().as_deref(), Some("initial"));
            context.observe(use_sql, true);
            assert_eq!(context.snapshot().as_deref(), Some(expected));
            context.observe("SELECT 'USE fake; EXEC fake'", true);
            assert_eq!(context.snapshot().as_deref(), Some(expected));
            context.observe("EXECUTE opaque", true);
            assert_eq!(context.snapshot(), None);
            context.observe(use_sql, true);
            assert_eq!(
                context.snapshot(),
                None,
                "opaque execution stays unknown until an authoritative refresh"
            );
            context.refresh(Some("observed".into()));
            assert_eq!(context.snapshot().as_deref(), Some("observed"));
            context.observe("USE another; SELECT missing", false);
            assert_eq!(context.snapshot(), None);
        }
        let mut context = DatabaseContext::new(ContextDialect::Mysql, Some("initial".into()));
        context.observe("/*! USE hidden */", true);
        assert_eq!(context.snapshot(), None);
        context.refresh(Some("initial".into()));
        context.observe("CALL opaque()", false);
        assert_eq!(context.snapshot(), None);
        let mut duck = DatabaseContext::new(ContextDialect::Duckdb, Some("memory".into()));
        duck.observe("USE possible_schema", true);
        assert_eq!(duck.snapshot(), None);
        duck.refresh(Some("memory".into()));
        duck.observe("USE missing", false);
        assert_eq!(duck.snapshot().as_deref(), Some("memory"));
        let mut bare = DatabaseContext::new(ContextDialect::SqlServer, Some("master".into()));
        bare.observe("dbo.opaque_procedure", true);
        bare.observe("USE tempdb", true);
        assert_eq!(bare.snapshot(), None);
        let mut sqlserver = DatabaseContext::new(ContextDialect::SqlServer, Some("master".into()));
        sqlserver.observe("SET PARSEONLY ON", true);
        sqlserver.observe("USE tempdb", true);
        sqlserver.observe("SET PARSEONLY OFF", true);
        assert_eq!(sqlserver.snapshot(), None);
        sqlserver.refresh(Some("master".into()));
        assert_eq!(sqlserver.snapshot().as_deref(), Some("master"));
        for sql in [
            "SET schema='other.main'",
            "RESET search_path",
            "PRAGMA search_path='other.main'",
            "DROP DATABASE current_db",
            "ALTER DATABASE current_db MODIFY NAME=new_db",
        ] {
            let mut context = DatabaseContext::new(ContextDialect::Duckdb, Some("initial".into()));
            context.observe(sql, true);
            assert_eq!(context.snapshot(), None, "{sql}");
        }
    }
}
