//! The rules of the user's changes to the app's keyboard shortcuts (SDB-447, ADR-064). Which
//! keys a command may take is decided here, not in the page: written as the app's registry
//! writes them (`Ctrl+Shift+Enter`), and never keys that type or edit text, that dialogs and
//! the editor keep (Escape, Enter, Tab), that AltGr produces, or that the system or the
//! webview keeps. The same check runs when they are read, so a store written by another
//! program cannot hand the app keys it would have refused to save.
//!
//! Which command already has some keys, and where keys act, is the registry's: the page
//! judges a clash between two commands before it saves (ADR-064).

use std::collections::HashSet;

use spelaion_store::{ShortcutOverride, Store, StoreError, is_command_id};

/// Most commands whose keys a user may change: more than the app has.
pub const MAX_SHORTCUT_OVERRIDES: usize = 300;
/// Longest shortcut, in characters.
pub const MAX_SHORTCUT_LENGTH: usize = 40;

/// Keys that are not a character, as the registry writes them.
const NAMED_KEYS: [&str; 15] = [
    "Enter",
    "Escape",
    "Tab",
    "Space",
    "Backspace",
    "Delete",
    "Insert",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
];

/// Keys that move the cursor or edit text where text is typed.
const EDITING_KEYS: [&str; 11] = [
    "Space",
    "Backspace",
    "Delete",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
];

/// Keys the system or the webview keeps for itself.
const KEPT_BY_SYSTEM: [&str; 8] = [
    "Alt+F4",
    "Alt+Tab",
    "Shift+Alt+Tab",
    "Alt+Escape",
    "Alt+Space",
    "Ctrl+Escape",
    "Ctrl+Shift+Escape",
    "Ctrl+Alt+Delete",
];

/// Why some keys are refused, said in the dialog that takes them.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShortcutError {
    #[error(
        "these keys cannot be read: a shortcut is Ctrl, Shift and Alt, in that order, then one key"
    )]
    Unreadable,
    #[error("these keys type or edit text: add Ctrl or Alt")]
    TypesText,
    #[error("Escape, Enter and Tab alone are kept for dialogs and the editor: add Ctrl or Alt")]
    KeptKey,
    #[error("AltGr types this character on some keyboards (Ctrl+Alt is AltGr on Windows)")]
    AltGr,
    #[error("the system keeps these keys for itself")]
    KeptBySystem,
    #[error("{0:?} is not one of the app's commands")]
    Command(String),
    #[error("{0} is given keys twice")]
    Twice(String),
    #[error("at most {MAX_SHORTCUT_OVERRIDES} shortcuts can be changed")]
    TooMany,
}

/// Whether `keys` may be a command's shortcut: written as the registry writes them, and none
/// of the keys the app, the editor or the system keeps.
pub fn check_shortcut(keys: &str) -> Result<(), ShortcutError> {
    if keys.len() > MAX_SHORTCUT_LENGTH {
        return Err(ShortcutError::Unreadable);
    }
    // "Ctrl+-" ends with its key: split on the "+" before the last character.
    let (modifiers, key) = match keys.get(..keys.len().saturating_sub(1)) {
        Some(head) => match head.rfind('+') {
            Some(at) => (&keys[..at], &keys[at + 1..]),
            None => ("", keys),
        },
        None => ("", keys),
    };
    let modifiers: Vec<&str> = if modifiers.is_empty() {
        Vec::new()
    } else {
        modifiers.split('+').collect()
    };
    let order = ["Ctrl", "Shift", "Alt"];
    let mut at = 0;
    for modifier in &modifiers {
        let Some(index) = order.iter().position(|m| m == modifier) else {
            return Err(ShortcutError::Unreadable);
        };
        if index < at {
            return Err(ShortcutError::Unreadable);
        }
        at = index + 1;
    }
    let character = {
        let mut chars = key.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    };
    let function_key = key
        .strip_prefix('F')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n));
    let readable = match character {
        // A letter is written in capitals, as the registry writes it.
        Some(c) => !c.is_whitespace() && !c.is_control() && !c.is_ascii_lowercase() && c != '+',
        None => function_key || NAMED_KEYS.contains(&key),
    };
    if !readable {
        return Err(ShortcutError::Unreadable);
    }
    if KEPT_BY_SYSTEM.contains(&keys) {
        return Err(ShortcutError::KeptBySystem);
    }
    let ctrl = modifiers.contains(&"Ctrl");
    let alt = modifiers.contains(&"Alt");
    if !ctrl && !alt {
        if character.is_some() || EDITING_KEYS.contains(&key) {
            return Err(ShortcutError::TypesText);
        }
        if matches!(key, "Escape" | "Enter" | "Tab") {
            return Err(ShortcutError::KeptKey);
        }
    }
    if ctrl && alt && character.is_some_and(|c| !c.is_ascii_alphanumeric()) {
        return Err(ShortcutError::AltGr);
    }
    Ok(())
}

/// Checks a whole set of changes: each command once, with keys [`check_shortcut`] accepts.
fn check_overrides(overrides: &[ShortcutOverride]) -> Result<(), ShortcutError> {
    if overrides.len() > MAX_SHORTCUT_OVERRIDES {
        return Err(ShortcutError::TooMany);
    }
    let mut seen = HashSet::new();
    for change in overrides {
        if !is_command_id(&change.command) {
            return Err(ShortcutError::Command(change.command.clone()));
        }
        if !seen.insert(change.command.as_str()) {
            return Err(ShortcutError::Twice(change.command.clone()));
        }
        if let Some(keys) = &change.keys {
            check_shortcut(keys)?;
        }
    }
    Ok(())
}

/// The user's changes to the shortcuts. A stored change that no longer passes the checks is
/// left out: its command keeps its default.
pub fn shortcut_overrides(store: &Store) -> Result<Vec<ShortcutOverride>, StoreError> {
    Ok(store
        .shortcut_overrides()?
        .into_iter()
        .filter(|change| {
            change
                .keys
                .as_deref()
                .is_none_or(|keys| check_shortcut(keys).is_ok())
        })
        .take(MAX_SHORTCUT_OVERRIDES)
        .collect())
}

/// Replaces the user's changes to the shortcuts with `overrides`, all of them or none: the
/// store's own failure, or why the core refused them (`None` when they were saved).
pub fn set_shortcut_overrides(
    store: &Store,
    overrides: &[ShortcutOverride],
) -> Result<Option<ShortcutError>, StoreError> {
    if let Err(refused) = check_overrides(overrides) {
        return Ok(Some(refused));
    }
    store.set_shortcut_overrides(overrides)?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registrys_keys_are_accepted() {
        for keys in [
            "Ctrl+Enter",
            "Ctrl+Shift+Enter",
            "Shift+Alt+F",
            "Ctrl+Shift+Alt+U",
            "Ctrl+-",
            "Ctrl+=",
            "Ctrl+,",
            "Ctrl+/",
            "Ctrl+Shift+[",
            "Ctrl+Shift+\\",
            "Ctrl+Alt+1",
            "Ctrl+Alt+V",
            "Alt+PageDown",
            "Alt+Enter",
            "F12",
            "Ctrl+F2",
            "Ctrl+Shift+PageDown",
            "Alt+Insert",
            "Ctrl+Space",
            "Ctrl+Alt+F9",
        ] {
            assert_eq!(check_shortcut(keys), Ok(()), "{keys}");
        }
    }

    #[test]
    fn each_refused_kind_of_keys_says_why() {
        for (keys, why) in [
            ("", ShortcutError::Unreadable),
            ("Ctrl+", ShortcutError::Unreadable),
            ("Ctrl", ShortcutError::Unreadable),
            ("Alt+Ctrl+X", ShortcutError::Unreadable),
            ("Ctrl+Ctrl+X", ShortcutError::Unreadable),
            ("Meta+X", ShortcutError::Unreadable),
            ("Ctrl+x", ShortcutError::Unreadable),
            ("Ctrl+F25", ShortcutError::Unreadable),
            ("Ctrl+Nope", ShortcutError::Unreadable),
            ("Ctrl++", ShortcutError::Unreadable),
            ("A", ShortcutError::TypesText),
            ("Shift+A", ShortcutError::TypesText),
            ("7", ShortcutError::TypesText),
            ("Shift+!", ShortcutError::TypesText),
            ("Space", ShortcutError::TypesText),
            ("Shift+Delete", ShortcutError::TypesText),
            ("ArrowUp", ShortcutError::TypesText),
            // The keys a confirmation answers with (Enter, Escape, Tab between its buttons, and
            // 1–9 in the statement picker) are never a command's.
            ("Escape", ShortcutError::KeptKey),
            ("Shift+Escape", ShortcutError::KeptKey),
            ("Enter", ShortcutError::KeptKey),
            ("Shift+Enter", ShortcutError::KeptKey),
            ("Tab", ShortcutError::KeptKey),
            ("Shift+Tab", ShortcutError::KeptKey),
            ("1", ShortcutError::TypesText),
            ("9", ShortcutError::TypesText),
            ("Ctrl+Alt+@", ShortcutError::AltGr),
            ("Ctrl+Shift+Alt+{", ShortcutError::AltGr),
            ("Alt+F4", ShortcutError::KeptBySystem),
            ("Alt+Tab", ShortcutError::KeptBySystem),
            ("Ctrl+Shift+Escape", ShortcutError::KeptBySystem),
        ] {
            assert_eq!(check_shortcut(keys), Err(why), "{keys:?}");
        }
    }

    fn change(command: &str, keys: Option<&str>) -> ShortcutOverride {
        ShortcutOverride {
            command: command.into(),
            keys: keys.map(str::to_string),
        }
    }

    #[test]
    fn a_set_of_changes_is_refused_whole() {
        assert_eq!(
            check_overrides(&[
                change("editor.run", Some("Ctrl+F5")),
                change("tab.new", None)
            ]),
            Ok(())
        );
        assert_eq!(
            check_overrides(&[change("tab.new", None), change("tab.new", Some("Ctrl+N"))]),
            Err(ShortcutError::Twice("tab.new".into()))
        );
        assert_eq!(
            check_overrides(&[change("tab new", None)]),
            Err(ShortcutError::Command("tab new".into()))
        );
        assert_eq!(
            check_overrides(&[change("tab.new", Some("Alt+F4"))]),
            Err(ShortcutError::KeptBySystem)
        );
        let many: Vec<_> = (0..=MAX_SHORTCUT_OVERRIDES)
            .map(|n| change(&format!("c{n}"), None))
            .collect();
        assert_eq!(check_overrides(&many), Err(ShortcutError::TooMany));
    }
}
