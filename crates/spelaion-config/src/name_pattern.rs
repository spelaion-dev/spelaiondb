//! The name the connection form suggests for a new connection (ADR-030), from a personal
//! pattern like DBeaver's `DEFAULT_CONNECTION_NAME_PATTERN`: text with variables in braces.
//!
//! - `{host}`, `{port}`, `{database}`, `{user}`, `{engine}`, `{env}` and `{file}` (the file
//!   name of a SQLite or DuckDB database; `{database}` is then the name without extension).
//! - `{a|b}` is the first of them that has a value: `{database|host}`.
//! - `{{` and `}}` are literal braces.
//!
//! An unknown variable is an error that names it, never silently dropped.

use crate::draft::ConnectionDraft;

/// Close to how the form named connections before: the database (or file), else the host.
pub const DEFAULT_NAME_PATTERN: &str = "{database|host}";

/// Every variable a pattern can use.
pub const NAME_VARIABLES: [&str; 7] = ["host", "port", "database", "user", "engine", "env", "file"];

/// Longest pattern accepted.
pub const MAX_NAME_PATTERN: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NamePatternError {
    #[error("unknown variables in the name pattern: {}", .0.iter().map(|v| format!("{{{v}}}")).collect::<Vec<_>>().join(", "))]
    Unknown(Vec<String>),
    #[error("a \"{{\" in the name pattern is not closed (write {{{{ for a brace)")]
    Unclosed,
    #[error("a \"}}\" in the name pattern has no \"{{\" (write }}}} for a brace)")]
    Unopened,
    #[error("the name pattern has an empty variable {{}}")]
    Empty,
    #[error("the name pattern is longer than {MAX_NAME_PATTERN} characters")]
    TooLong,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    /// Alternatives, the first with a value wins.
    Var(Vec<String>),
}

/// A checked pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePattern {
    parts: Vec<Part>,
}

/// The values the variables take for one connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameValues {
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    /// The engine's display name (`PostgreSQL`).
    pub engine: String,
    pub env: String,
    pub file: String,
}

impl NameValues {
    /// The values of what the user filled in so far. `engine` is the engine's display name.
    pub fn of_draft(draft: &ConnectionDraft, engine: &str) -> Self {
        let trim = |s: &str| s.trim().to_string();
        let mut values = Self {
            engine: engine.to_string(),
            env: trim(&draft.env),
            ..Self::default()
        };
        if draft.engine.is_file() {
            let path = draft.database.trim();
            let file = path.rsplit(['/', '\\']).next().unwrap_or_default();
            let stem = match file.rsplit_once('.') {
                Some((stem, _)) if !stem.is_empty() => stem,
                _ => file,
            };
            values.file = file.to_string();
            values.database = stem.to_string();
        } else {
            values.host = trim(&draft.host);
            values.port = draft
                .port
                .or(draft.engine.default_port())
                .map(|p| p.to_string())
                .unwrap_or_default();
            values.database = trim(&draft.database);
            values.user = trim(&draft.user);
        }
        values
    }

    fn get(&self, name: &str) -> &str {
        match name {
            "host" => &self.host,
            "port" => &self.port,
            "database" => &self.database,
            "user" => &self.user,
            "engine" => &self.engine,
            "env" => &self.env,
            "file" => &self.file,
            _ => "",
        }
    }
}

impl NamePattern {
    pub fn parse(pattern: &str) -> Result<Self, NamePatternError> {
        if pattern.chars().count() > MAX_NAME_PATTERN {
            return Err(NamePatternError::TooLong);
        }
        let mut parts = Vec::new();
        let mut text = String::new();
        let mut unknown: Vec<String> = Vec::new();
        let mut chars = pattern.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    text.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    text.push('}');
                }
                '}' => return Err(NamePatternError::Unopened),
                '{' => {
                    let mut inner = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some('{') | None => return Err(NamePatternError::Unclosed),
                            Some(c) => inner.push(c),
                        }
                    }
                    let names: Vec<String> =
                        inner.split('|').map(|n| n.trim().to_lowercase()).collect();
                    if names.iter().any(String::is_empty) {
                        return Err(NamePatternError::Empty);
                    }
                    for name in &names {
                        if !NAME_VARIABLES.contains(&name.as_str()) && !unknown.contains(name) {
                            unknown.push(name.clone());
                        }
                    }
                    if !text.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut text)));
                    }
                    parts.push(Part::Var(names));
                }
                c => text.push(c),
            }
        }
        if !unknown.is_empty() {
            return Err(NamePatternError::Unknown(unknown));
        }
        if !text.is_empty() {
            parts.push(Part::Text(text));
        }
        Ok(Self { parts })
    }

    /// The name for these values: separators left around empty variables are trimmed, so
    /// `{engine} - {database}` without a database is just the engine.
    pub fn render(&self, values: &NameValues) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Var(names) => {
                    if let Some(value) = names
                        .iter()
                        .map(|n| values.get(n).trim())
                        .find(|v| !v.is_empty())
                    {
                        out.push_str(value);
                    }
                }
            }
        }
        tidy(&out)
    }
}

/// Collapses spaces, removes brackets left empty and trims separators at both ends.
fn tidy(name: &str) -> String {
    let mut name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    loop {
        let before = name.len();
        for empty in ["()", "[]", "( )", "[ ]"] {
            name = name.replace(empty, "");
        }
        name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        if name.len() == before {
            break;
        }
    }
    const SEPARATORS: &[char] = &[' ', '-', '–', '—', '_', '@', ':', '/', ',', ';', '|', '·'];
    name.trim_matches(SEPARATORS).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::Source;
    use crate::model::Engine;

    fn values() -> NameValues {
        NameValues {
            host: "db.example.com".into(),
            port: "5432".into(),
            database: "orders".into(),
            user: "app".into(),
            engine: "PostgreSQL".into(),
            env: "prod".into(),
            file: String::new(),
        }
    }

    fn render(pattern: &str, values: &NameValues) -> String {
        NamePattern::parse(pattern).unwrap().render(values)
    }

    #[test]
    fn variables_are_replaced() {
        assert_eq!(
            render(
                "{engine} {database}@{host}:{port} ({user}, {env})",
                &values()
            ),
            "PostgreSQL orders@db.example.com:5432 (app, prod)"
        );
        assert_eq!(render("{DATABASE}", &values()), "orders");
        assert_eq!(
            render("{{literal}} {database}", &values()),
            "{literal} orders"
        );
    }

    #[test]
    fn alternatives_take_the_first_with_a_value() {
        let mut v = values();
        assert_eq!(render(DEFAULT_NAME_PATTERN, &v), "orders");
        v.database.clear();
        assert_eq!(render(DEFAULT_NAME_PATTERN, &v), "db.example.com");
        assert_eq!(render("{ file | database | host }", &v), "db.example.com");
    }

    #[test]
    fn separators_around_empty_variables_are_trimmed() {
        let mut v = values();
        v.database.clear();
        v.env.clear();
        assert_eq!(render("{engine} - {database}", &v), "PostgreSQL");
        assert_eq!(render("{database} ({env})", &v), "");
        assert_eq!(render("{host} [{env}]", &v), "db.example.com");
        assert_eq!(
            render("  {user}   @   {host}  ", &v),
            "app @ db.example.com"
        );
    }

    #[test]
    fn unknown_and_malformed_patterns_are_errors() {
        assert_eq!(
            NamePattern::parse("{hots} {database} {schema} {hots}"),
            Err(NamePatternError::Unknown(vec![
                "hots".into(),
                "schema".into()
            ]))
        );
        assert_eq!(
            NamePattern::parse("{database|nope}"),
            Err(NamePatternError::Unknown(vec!["nope".into()]))
        );
        assert_eq!(
            NamePattern::parse("{database"),
            Err(NamePatternError::Unclosed)
        );
        assert_eq!(
            NamePattern::parse("{data{base}"),
            Err(NamePatternError::Unclosed)
        );
        assert_eq!(NamePattern::parse("db}"), Err(NamePatternError::Unopened));
        assert_eq!(NamePattern::parse("{}"), Err(NamePatternError::Empty));
        assert_eq!(NamePattern::parse("{host|}"), Err(NamePatternError::Empty));
        assert_eq!(
            NamePattern::parse(&"x".repeat(MAX_NAME_PATTERN + 1)),
            Err(NamePatternError::TooLong)
        );
        assert!(
            NamePatternError::Unknown(vec!["hots".into()])
                .to_string()
                .contains("{hots}")
        );
        // Empty means no suggestion.
        assert_eq!(render("", &values()), "");
    }

    /// A form as it would be filled, built from a file so new settings need no change here.
    fn draft(engine: Engine) -> ConnectionDraft {
        let file: crate::model::ConnectionFile = toml::from_str(&format!(
            "schema_version = 1\nengine = \"{}\"\nenv = \" dev \"\nhost = \" db.internal \"\n\
             database = \"sales\"\nuser = \"reader\"\n",
            engine.as_str()
        ))
        .unwrap();
        ConnectionDraft::from_entry(&crate::load::ConnectionEntry {
            id: crate::load::ConnectionId {
                source: Source::User,
                path: "x".into(),
            },
            file_path: "x.toml".into(),
            file,
        })
    }

    #[test]
    fn values_come_from_the_form() {
        let v = NameValues::of_draft(&draft(Engine::Mysql), "MySQL");
        assert_eq!(
            (
                v.host.as_str(),
                v.port.as_str(),
                v.database.as_str(),
                v.user.as_str()
            ),
            ("db.internal", "3306", "sales", "reader")
        );
        assert_eq!(
            (v.engine.as_str(), v.env.as_str(), v.file.as_str()),
            ("MySQL", "dev", "")
        );

        let mut sqlite = draft(Engine::Sqlite);
        sqlite.database = r"C:\data\shop.v2.sqlite".into();
        let v = NameValues::of_draft(&sqlite, "SQLite");
        assert_eq!(
            (v.file.as_str(), v.database.as_str()),
            ("shop.v2.sqlite", "shop.v2")
        );
        assert_eq!(
            (v.host.as_str(), v.port.as_str(), v.user.as_str()),
            ("", "", "")
        );
        sqlite.database = "/srv/.hidden".into();
        let v = NameValues::of_draft(&sqlite, "SQLite");
        assert_eq!(
            (v.file.as_str(), v.database.as_str()),
            (".hidden", ".hidden")
        );
        let pattern = NamePattern::parse(DEFAULT_NAME_PATTERN).unwrap();
        sqlite.database = "data/scratch.sqlite".into();
        assert_eq!(
            pattern.render(&NameValues::of_draft(&sqlite, "SQLite")),
            "scratch"
        );
    }
}
