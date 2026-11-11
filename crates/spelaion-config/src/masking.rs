//! Column masking rules (ADR-051): which result columns a connection masks, from its own
//! `[masking]` table, the user's `user.toml` and its connection type. This module only checks
//! and combines the patterns; `spelaion-core::masking` matches them and masks the values.

use crate::load::is_hidden_char;
use crate::model::MaskingConfig;

/// A pattern that masks every column: what a connection gets while the user's own patterns
/// cannot be read (a missing rule must never unmask anything).
pub const MASK_EVERYTHING: &str = "*";

/// A connection's masking rules with every place applied (ADR-051). A column is masked when a
/// `required` pattern matches it, or when an `exemptible` one does and no `never` one does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaskingRules {
    /// Patterns nothing unmasks: the connection type's `mask_columns`, and the user's own
    /// patterns on a repository connection.
    pub required: Vec<String>,
    /// Patterns the connection's `never` may leave visible: its own `always`, and the user's
    /// patterns when the connection is the user's own.
    pub exemptible: Vec<String>,
    /// The connection's `never`.
    pub never: Vec<String>,
}

impl MaskingRules {
    /// No rule at all: nothing is masked.
    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.exemptible.is_empty()
    }

    /// The rules of a connection.
    /// - `user`: the user's `[masking] always`, or `None` when `user.toml` does not load
    ///   (then every column is masked until it is fixed).
    /// - `personal`: the connection is the user's own (not from a repository).
    /// - `type_patterns`: its connection type's `mask_columns`, every layer together.
    pub fn combine(
        user: Option<&[String]>,
        connection: &MaskingConfig,
        personal: bool,
        type_patterns: &[String],
    ) -> MaskingRules {
        let mut rules = MaskingRules {
            required: tidy(type_patterns),
            exemptible: tidy(&connection.always),
            never: tidy(&connection.never),
        };
        match user {
            None => rules.required.push(MASK_EVERYTHING.into()),
            Some(user) if personal => rules.exemptible.extend(tidy(user)),
            Some(user) => rules.required.extend(tidy(user)),
        }
        rules.required.dedup();
        rules.exemptible.dedup();
        rules
    }
}

fn tidy(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// One masking pattern: not empty, bounded, and without hidden or control characters.
pub fn check_pattern(pattern: &str) -> Result<(), String> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err("a masking pattern cannot be empty".into());
    }
    if trimmed.chars().count() > MaskingConfig::MAX_PATTERN {
        return Err(format!(
            "a masking pattern is at most {} characters",
            MaskingConfig::MAX_PATTERN
        ));
    }
    if pattern.chars().any(is_hidden_char) {
        return Err("a masking pattern cannot hold hidden characters".into());
    }
    Ok(())
}

/// A list of masking patterns (`label` names it in the message: `masking.always`).
pub fn check_patterns(label: &str, patterns: &[String]) -> Result<(), String> {
    if patterns.len() > MaskingConfig::MAX_PATTERNS {
        return Err(format!(
            "{label} holds at most {} patterns",
            MaskingConfig::MAX_PATTERNS
        ));
    }
    for pattern in patterns {
        check_pattern(pattern).map_err(|m| format!("{label}: {m} ({pattern:?})"))?;
    }
    Ok(())
}

/// A connection's `[masking]` table.
pub(crate) fn check_masking(masking: &MaskingConfig) -> Result<(), String> {
    check_patterns("masking.always", &masking.always)?;
    check_patterns("masking.never", &masking.never)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_repository_connection_cannot_exempt_the_users_patterns() {
        let connection = MaskingConfig {
            always: v(&["card"]),
            never: v(&["email"]),
        };
        let user = v(&["*email*"]);
        let team = MaskingRules::combine(Some(&user), &connection, false, &[]);
        assert_eq!(team.required, v(&["*email*"]));
        assert_eq!(team.exemptible, v(&["card"]));
        let mine = MaskingRules::combine(Some(&user), &connection, true, &[]);
        assert!(mine.required.is_empty());
        assert_eq!(mine.exemptible, v(&["card", "*email*"]));
    }

    #[test]
    fn type_patterns_are_always_required() {
        let connection = MaskingConfig {
            always: Vec::new(),
            never: v(&["*"]),
        };
        let rules = MaskingRules::combine(Some(&[]), &connection, true, &v(&["ssn"]));
        assert_eq!(rules.required, v(&["ssn"]));
    }

    #[test]
    fn unreadable_user_patterns_mask_everything() {
        let rules = MaskingRules::combine(None, &MaskingConfig::default(), true, &[]);
        assert_eq!(rules.required, v(&[MASK_EVERYTHING]));
        assert!(!rules.is_empty());
    }

    #[test]
    fn patterns_are_checked() {
        assert!(check_pattern("*password*").is_ok());
        assert!(check_pattern("  ").is_err());
        assert!(check_pattern("e\u{202E}mail").is_err());
        assert!(check_pattern("a\nb").is_err());
        assert!(check_pattern(&"x".repeat(129)).is_err());
        assert!(check_patterns("masking.always", &vec!["a".to_string(); 101]).is_err());
    }
}
