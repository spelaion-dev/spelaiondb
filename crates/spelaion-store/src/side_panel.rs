//! The side panel's layout (SDB-451): whether the user hid it, and its width. A personal
//! preference kept with the other personal settings, shared by every workspace of this user
//! directory, so the app opens as it was left.

use crate::{Store, StoreError};

/// The narrowest and the widest the side panel may be, in CSS pixels at the normal interface
/// size, and its width until the user changes it. The window may show it narrower than the
/// widest when there is not room for both it and the editor.
pub const SIDE_PANEL_MIN_WIDTH: u32 = 210;
pub const SIDE_PANEL_MAX_WIDTH: u32 = 700;
pub const SIDE_PANEL_DEFAULT_WIDTH: u32 = 290;

/// What the app restores of the side panel at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidePanelPreferences {
    pub hidden: bool,
    /// Between [`SIDE_PANEL_MIN_WIDTH`] and [`SIDE_PANEL_MAX_WIDTH`].
    pub width: u32,
}

impl Default for SidePanelPreferences {
    fn default() -> Self {
        Self {
            hidden: false,
            width: SIDE_PANEL_DEFAULT_WIDTH,
        }
    }
}

/// The width is outside [`SIDE_PANEL_MIN_WIDTH`]..=[`SIDE_PANEL_MAX_WIDTH`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "the side panel's width must be between {SIDE_PANEL_MIN_WIDTH} and {SIDE_PANEL_MAX_WIDTH} pixels, not {0}"
)]
pub struct SidePanelWidthError(pub u32);

impl SidePanelPreferences {
    pub fn validate(&self) -> Result<(), SidePanelWidthError> {
        if (SIDE_PANEL_MIN_WIDTH..=SIDE_PANEL_MAX_WIDTH).contains(&self.width) {
            Ok(())
        } else {
            Err(SidePanelWidthError(self.width))
        }
    }
}

impl Store {
    /// The side panel as it was left; a stored width that is not a number in range reads as
    /// the default width.
    pub fn get_side_panel_preferences(&self) -> Result<SidePanelPreferences, StoreError> {
        let hidden = self.get_flag("side_panel.hidden", false)?;
        let width = self
            .personal_setting("side_panel.width")?
            .and_then(|text| text.parse::<u32>().ok())
            .filter(|width| (SIDE_PANEL_MIN_WIDTH..=SIDE_PANEL_MAX_WIDTH).contains(width))
            .unwrap_or(SIDE_PANEL_DEFAULT_WIDTH);
        Ok(SidePanelPreferences { hidden, width })
    }

    /// Saves both; a width out of range is refused and nothing is saved.
    pub fn set_side_panel_preferences(
        &self,
        preferences: &SidePanelPreferences,
    ) -> Result<(), StoreError> {
        preferences
            .validate()
            .map_err(|e| StoreError::new(e.to_string()))?;
        let transaction = self.conn.unchecked_transaction()?;
        self.set_flag("side_panel.hidden", preferences.hidden)?;
        self.set_personal_setting("side_panel.width", &preferences.width.to_string())?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_side_panel_opens_as_it_was_left() {
        let path =
            std::env::temp_dir().join(format!("spelaion-side-panel-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.get_side_panel_preferences().unwrap(),
            SidePanelPreferences::default()
        );
        let left = SidePanelPreferences {
            hidden: true,
            width: 420,
        };
        store.set_side_panel_preferences(&left).unwrap();
        assert_eq!(store.get_side_panel_preferences().unwrap(), left);
        // A width out of range is refused, and what was saved stays.
        for width in [SIDE_PANEL_MIN_WIDTH - 1, SIDE_PANEL_MAX_WIDTH + 1, 0] {
            assert!(
                store
                    .set_side_panel_preferences(&SidePanelPreferences {
                        hidden: false,
                        width
                    })
                    .is_err()
            );
        }
        assert_eq!(store.get_side_panel_preferences().unwrap(), left);
        // A stored width that is not one reads as the default.
        store
            .set_personal_setting("side_panel.width", "wide")
            .unwrap();
        assert_eq!(
            store.get_side_panel_preferences().unwrap().width,
            SIDE_PANEL_DEFAULT_WIDTH
        );
        store
            .set_personal_setting("side_panel.width", "5000")
            .unwrap();
        assert_eq!(
            store.get_side_panel_preferences().unwrap().width,
            SIDE_PANEL_DEFAULT_WIDTH
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
