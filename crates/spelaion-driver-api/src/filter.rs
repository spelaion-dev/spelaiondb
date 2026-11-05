//! Include and exclude name patterns for the explorer (ADR-025).
//!
//! `*` (or `%`) is any run of characters, `?` one character and `\` escapes the next one;
//! case is ignored and `_` is literal.

/// One character of a pattern: a literal (already lowercased) or a wildcard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Literal(char),
    One,
    Any,
}

fn tokens(pattern: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' | '%' => {
                if out.last() != Some(&Token::Any) {
                    out.push(Token::Any);
                }
            }
            '?' => out.push(Token::One),
            '\\' => {
                // A trailing backslash stands for itself.
                let escaped = chars.next().unwrap_or('\\');
                out.extend(escaped.to_lowercase().map(Token::Literal));
            }
            c => out.extend(c.to_lowercase().map(Token::Literal)),
        }
    }
    out
}

/// Wildcard matching with backtracking to the last `*` (linear in practice).
fn matches_tokens(tokens: &[Token], name: &[char]) -> bool {
    let (mut t, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match tokens.get(t) {
            Some(Token::Any) => {
                star = Some((t, n));
                t += 1;
            }
            Some(Token::One) => {
                t += 1;
                n += 1;
            }
            Some(Token::Literal(c)) if *c == name[n] => {
                t += 1;
                n += 1;
            }
            Some(Token::Literal(_)) | None => match star {
                Some((star_t, star_n)) => {
                    t = star_t + 1;
                    n = star_n + 1;
                    star = Some((star_t, star_n + 1));
                }
                None => return false,
            },
        }
    }
    tokens[t..].iter().all(|token| *token == Token::Any)
}

/// Whether `name` matches one pattern (ignoring case).
pub fn pattern_matches(pattern: &str, name: &str) -> bool {
    let name: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
    matches_tokens(&tokens(pattern), &name)
}

/// The names the explorer shows of one kind (schemas, objects or columns).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NameFilter {
    /// When not empty, only names matching one of these.
    pub include: Vec<String>,
    /// Names matching one of these are hidden, even if included.
    pub exclude: Vec<String>,
}

impl NameFilter {
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub fn matches(&self, name: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|p| pattern_matches(p, name)))
            && !self.exclude.iter().any(|p| pattern_matches(p, name))
    }
}

/// `LIKE` patterns for a catalog query, over the lowercased name with `!` as the escape
/// character (ADR-025): a backslash would need escaping in MySQL's string literals. They narrow what the server sends; [`NameFilter::matches`] still
/// decides. Whatever case folding the server does, they never drop a name the filter keeps:
/// an include pattern turns each non-ASCII character into `%`, and an exclude pattern is sent
/// only when it is plain ASCII without `?`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LikeFilter {
    /// A name must match one of these (none: no condition).
    pub include: Vec<String>,
    /// A name must match none of these.
    pub exclude: Vec<String>,
}

/// The escape character of [`LikeFilter`]'s patterns.
const ESCAPE: char = '!';

fn like_pattern(tokens: &[Token], exact_only: bool) -> Option<String> {
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Any => out.push('%'),
            Token::One if exact_only => return None,
            Token::One => out.push('_'),
            Token::Literal(c) if !c.is_ascii() => {
                if exact_only {
                    return None;
                }
                if !out.ends_with('%') {
                    out.push('%');
                }
            }
            Token::Literal(c) => {
                if matches!(*c, '%' | '_' | ESCAPE) {
                    out.push(ESCAPE);
                }
                out.push(*c);
            }
        }
    }
    Some(out)
}

impl NameFilter {
    /// What a driver may put into its catalog query (see [`LikeFilter`]).
    pub fn server_side(&self) -> LikeFilter {
        LikeFilter {
            include: self
                .include
                .iter()
                .filter_map(|p| like_pattern(&tokens(p), false))
                .collect(),
            exclude: self
                .exclude
                .iter()
                .filter_map(|p| like_pattern(&tokens(p), true))
                .collect(),
        }
    }
}

impl LikeFilter {
    /// ` and (lower(col) like … or …) and not (lower(col) like … or …)`, each pattern written
    /// by `bind` (a placeholder for a parameter it records, or a quoted literal). Empty when
    /// there is nothing to narrow.
    pub fn sql(&self, column: &str, mut bind: impl FnMut(&str) -> String) -> String {
        let mut any = |patterns: &[String]| {
            patterns
                .iter()
                .map(|p| format!("lower({column}) like {} escape '{ESCAPE}'", bind(p)))
                .collect::<Vec<_>>()
                .join(" or ")
        };
        let mut out = String::new();
        if !self.include.is_empty() {
            out.push_str(&format!(" and ({})", any(&self.include)));
        }
        if !self.exclude.is_empty() {
            out.push_str(&format!(" and not ({})", any(&self.exclude)));
        }
        out
    }
}

/// A pattern that matches exactly one name: its wildcard characters escaped ("Hide" and
/// "Show only" in the explorer add these).
pub fn literal_pattern(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '*' | '%' | '?' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_match_runs_and_single_characters() {
        assert!(pattern_matches("tenant_*", "tenant_042"));
        assert!(pattern_matches("tenant_%", "tenant_042"));
        assert!(pattern_matches("*_log", "orders_log"));
        assert!(pattern_matches("*", ""));
        assert!(pattern_matches("t?", "t1"));
        assert!(!pattern_matches("t?", "t10"));
        assert!(pattern_matches("a*b*c", "axxbyyc"));
        assert!(!pattern_matches("a*b*c", "axxbyy"));
        assert!(pattern_matches("**x", "x"));
    }

    #[test]
    fn case_is_ignored_and_underscore_is_literal() {
        assert!(pattern_matches("Orders", "ORDERS"));
        assert!(pattern_matches("ñandú*", "ÑANDÚ_2"));
        assert!(pattern_matches("order_items", "order_items"));
        assert!(!pattern_matches("order_items", "orderXitems"));
    }

    #[test]
    fn a_backslash_escapes_wildcards() {
        assert!(pattern_matches(r"100\%", "100%"));
        assert!(!pattern_matches(r"100\%", "1000"));
        assert!(pattern_matches(r"what\?", "what?"));
        assert!(!pattern_matches(r"what\?", "whats"));
        assert!(pattern_matches(r"a\\b", r"a\b"));
        assert!(pattern_matches("end\\", "end\\"));
    }

    #[test]
    fn literal_patterns_match_only_their_name() {
        for name in ["plain", "50%_off", "who?", r"back\slash", "star*"] {
            let pattern = literal_pattern(name);
            assert!(pattern_matches(&pattern, name), "{name}");
        }
        assert!(!pattern_matches(&literal_pattern("50%"), "500"));
        assert!(!pattern_matches(&literal_pattern("who?"), "whom"));
    }

    #[test]
    fn server_patterns_escape_and_never_narrow_too_much() {
        let filter = NameFilter {
            include: vec![
                "Tenant_*".into(),
                "50%".into(),
                r"what\?".into(),
                "a?c".into(),
                "ÑAME*".into(),
            ],
            exclude: vec!["*_OLD".into(), "x?".into(), "ü*".into()],
        };
        let like = filter.server_side();
        let include = ["tenant!_%", "50%", "what?", "a_c", "%ame%"];
        assert_eq!(like.include, include);
        // `?` and non-ASCII exclude patterns stay in Rust only.
        assert_eq!(like.exclude, ["%!_old"]);
        assert_eq!(
            like.sql("name", |p| format!("'{p}'")),
            format!(
                " and ({}) and not (lower(name) like '%!_old' escape '!')",
                include
                    .map(|p| format!("lower(name) like '{p}' escape '!'"))
                    .join(" or "),
            )
        );
        // The escape character itself, a literal `%` and a backslash.
        let odd = NameFilter {
            include: vec!["wow!\\%\\\\".into()],
            exclude: vec![],
        };
        assert_eq!(odd.server_side().include, ["wow!!!%\\"]);
        assert_eq!(LikeFilter::default().sql("name", |p| p.to_string()), "");
    }

    #[test]
    fn include_then_exclude() {
        let filter = NameFilter {
            include: vec!["tenant_*".into(), "public".into()],
            exclude: vec!["*_old".into()],
        };
        assert!(filter.matches("public"));
        assert!(filter.matches("tenant_1"));
        assert!(!filter.matches("tenant_1_old"));
        assert!(!filter.matches("audit"));
        assert!(NameFilter::default().matches("anything"));
        assert!(NameFilter::default().is_empty());
        let only_exclude = NameFilter {
            include: vec![],
            exclude: vec!["pg_*".into()],
        };
        assert!(only_exclude.matches("public") && !only_exclude.matches("pg_toast"));
    }
}
