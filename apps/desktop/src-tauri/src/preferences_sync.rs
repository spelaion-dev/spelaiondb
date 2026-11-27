//! Personal preferences reach every window (SDB-533, SDB-535). Each window (the main one, a
//! connection's own, a results window; ADR-083) reads them once when it opens. Whenever a
//! window saves one, or a settings file is imported, every window is told which ones changed
//! and reads them again from the core: one opened earlier would otherwise keep showing the
//! old value and write it back with its next change. The side panel is left out: it is each
//! window's own layout, saved by every window as it is moved.

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event as _;

/// A personal preference a window keeps once read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum PreferenceKind {
    Language,
    Theme,
    InterfaceSize,
    Explorer,
    Grid,
    Script,
    Editor,
    Map,
    ReconnectOnStart,
    DisplayFormat,
    Shortcuts,
    /// The editor snippets and the new script's template.
    Snippets,
    UpdateCheck,
}

impl PreferenceKind {
    /// Every one: what a settings import may change.
    pub const ALL: [PreferenceKind; 13] = [
        PreferenceKind::Language,
        PreferenceKind::Theme,
        PreferenceKind::InterfaceSize,
        PreferenceKind::Explorer,
        PreferenceKind::Grid,
        PreferenceKind::Script,
        PreferenceKind::Editor,
        PreferenceKind::Map,
        PreferenceKind::ReconnectOnStart,
        PreferenceKind::DisplayFormat,
        PreferenceKind::Shortcuts,
        PreferenceKind::Snippets,
        PreferenceKind::UpdateCheck,
    ];
}

/// The core stored new values of these preferences: every window reads them again.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct PreferencesChanged {
    pub settings: Vec<PreferenceKind>,
}

/// Tells every window, after the core stored them. A window that misses it keeps what it had
/// until it reads them again; nothing the core does depends on the windows' copies.
pub fn announce(app: &tauri::AppHandle, settings: &[PreferenceKind]) {
    let event = PreferencesChanged {
        settings: settings.to_vec(),
    };
    if let Err(e) = event.emit(app) {
        eprintln!("the preferences were saved but not sent to every window: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_kind_once() {
        for kind in PreferenceKind::ALL {
            // A new kind fails to compile here until it is placed, and then must be in ALL.
            let index = match kind {
                PreferenceKind::Language => 0,
                PreferenceKind::Theme => 1,
                PreferenceKind::InterfaceSize => 2,
                PreferenceKind::Explorer => 3,
                PreferenceKind::Grid => 4,
                PreferenceKind::Script => 5,
                PreferenceKind::Editor => 6,
                PreferenceKind::Map => 7,
                PreferenceKind::ReconnectOnStart => 8,
                PreferenceKind::DisplayFormat => 9,
                PreferenceKind::Shortcuts => 10,
                PreferenceKind::Snippets => 11,
                PreferenceKind::UpdateCheck => 12,
            };
            assert_eq!(PreferenceKind::ALL[index], kind);
        }
    }
}
