//! The map view's preferences (ADR-045): the tile server the user chose, and whether its
//! tiles are shown at all. Off until the user turns them on, because every tile asked for
//! tells that server which part of the world the map shows.

use crate::{Store, StoreError};

/// The map's background. With `tiles` off (the default) shapes are drawn on a plain background
/// and nothing leaves the computer; the server is kept so turning tiles on again needs no
/// typing. Empty strings mean no server.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MapPreferences {
    pub tiles: bool,
    pub tile_name: String,
    /// An image address with `{z}`, `{x}` and `{y}`.
    pub tile_url: String,
    /// The credit the server asks for, shown on the map.
    pub tile_attribution: String,
}

impl MapPreferences {
    /// No server at all, or a valid one; tiles on only with a server.
    pub fn validate(&self) -> Result<(), TileServerError> {
        let none = self.tile_name.trim().is_empty()
            && self.tile_url.is_empty()
            && self.tile_attribution.is_empty();
        if none {
            return if self.tiles {
                Err(TileServerError::Missing)
            } else {
                Ok(())
            };
        }
        check_tile_server(&self.tile_name, &self.tile_url, &self.tile_attribution)
    }
}

impl Store {
    /// The map's preferences; a stored server that no longer passes the checks reads as none,
    /// with tiles off.
    pub fn get_map_preferences(&self) -> Result<MapPreferences, StoreError> {
        let preferences = MapPreferences {
            tiles: self.get_flag("map.tiles", false)?,
            tile_name: self.personal_setting("map.tile_name")?.unwrap_or_default(),
            tile_url: self.personal_setting("map.tile_url")?.unwrap_or_default(),
            tile_attribution: self
                .personal_setting("map.tile_attribution")?
                .unwrap_or_default(),
        };
        Ok(if preferences.validate().is_ok() {
            preferences
        } else {
            MapPreferences::default()
        })
    }

    pub fn set_map_preferences(&self, preferences: &MapPreferences) -> Result<(), StoreError> {
        preferences
            .validate()
            .map_err(|e| StoreError(e.to_string()))?;
        let transaction = self.conn.unchecked_transaction()?;
        self.set_flag("map.tiles", preferences.tiles)?;
        self.set_personal_setting("map.tile_name", preferences.tile_name.trim())?;
        self.set_personal_setting("map.tile_url", &preferences.tile_url)?;
        self.set_personal_setting("map.tile_attribution", &preferences.tile_attribution)?;
        transaction.commit()?;
        Ok(())
    }
}

/// Longest name, URL and attribution of a tile server.
pub const TILE_NAME_MAX: usize = 80;
pub const TILE_URL_MAX: usize = 500;
pub const TILE_ATTRIBUTION_MAX: usize = 200;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TileServerError {
    #[error("the tile server needs a name, of at most {TILE_NAME_MAX} characters")]
    Name,
    #[error(
        "the tile address must start with https:// (http:// only for this computer) and be at most {TILE_URL_MAX} characters"
    )]
    Scheme,
    #[error("the tile address must contain {{z}}, {{x}} and {{y}}, and no other {{…}} field")]
    Placeholders,
    #[error("the tile address cannot contain spaces, quotes or control characters")]
    Characters,
    #[error("the attribution is at most {TILE_ATTRIBUTION_MAX} characters, on one line")]
    Attribution,
    #[error("tiles need a tile server: give its name and address first")]
    Missing,
}

/// Checks a tile server before it is saved: a name, an `https://` address (or `http://` to
/// this computer, for a tile server of one's own) with `{z}`, `{x}` and `{y}`, and an
/// attribution on one line. The address is only ever used as an image's address.
pub fn check_tile_server(name: &str, url: &str, attribution: &str) -> Result<(), TileServerError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > TILE_NAME_MAX || name.chars().any(char::is_control)
    {
        return Err(TileServerError::Name);
    }
    if url.chars().count() > TILE_URL_MAX {
        return Err(TileServerError::Scheme);
    }
    if url.chars().any(|c| {
        c.is_control() || c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '\\' | '`')
    }) {
        return Err(TileServerError::Characters);
    }
    let lower = url.to_ascii_lowercase();
    let local = ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|prefix| {
            lower
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with([':', '/']))
        });
    let host_start = if let Some(rest) = lower.strip_prefix("https://") {
        rest
    } else if local {
        &lower["http://".len()..]
    } else {
        return Err(TileServerError::Scheme);
    };
    // A host before the path, and no user name or password in it.
    let host = host_start.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains('@') || host.contains('{') {
        return Err(TileServerError::Scheme);
    }
    let mut fields = Vec::new();
    let mut rest = url;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after.find('}').ok_or(TileServerError::Placeholders)?;
        fields.push(&after[..close]);
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(TileServerError::Placeholders);
    }
    fields.sort_unstable();
    if fields != ["x", "y", "z"] {
        return Err(TileServerError::Placeholders);
    }
    if attribution.chars().count() > TILE_ATTRIBUTION_MAX
        || attribution.chars().any(char::is_control)
    {
        return Err(TileServerError::Attribution);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_servers_are_checked_before_they_are_saved() {
        let ok = |url: &str| check_tile_server("Tiles", url, "© Someone");
        assert_eq!(ok("https://tile.example.com/{z}/{x}/{y}.png"), Ok(()));
        assert_eq!(ok("https://t.example.com/{z}/{y}/{x}?key=abc"), Ok(()));
        assert_eq!(ok("http://localhost:8080/{z}/{x}/{y}.png"), Ok(()));
        assert_eq!(ok("http://127.0.0.1/{z}/{x}/{y}.png"), Ok(()));
        for url in [
            "http://tile.example.com/{z}/{x}/{y}.png",
            "http://localhost.example.com/{z}/{x}/{y}.png",
            "javascript:alert(1)//{z}{x}{y}",
            "file:///C:/{z}/{x}/{y}.png",
            "data:image/png;base64,{z}{x}{y}",
            "https:///{z}/{x}/{y}.png",
            "https://user:pass@tile.example.com/{z}/{x}/{y}.png",
            "https://{z}.example.com/{x}/{y}.png",
        ] {
            assert_eq!(ok(url), Err(TileServerError::Scheme), "{url}");
        }
        for url in [
            "https://tile.example.com/{z}/{x}.png",
            "https://tile.example.com/{z}/{x}/{y}/{s}.png",
            "https://tile.example.com/{z}/{x}/{y}/{x}.png",
            "https://tile.example.com/{z}/{x}/{y.png",
            "https://tile.example.com/{z}/{x}/{y}}.png",
        ] {
            assert_eq!(ok(url), Err(TileServerError::Placeholders), "{url}");
        }
        for url in [
            "https://tile.example.com/{z}/{x}/{y}.png\" onerror=\"x",
            "https://tile.example.com/{z}/ {x}/{y}.png",
            "https://tile.example.com/{z}/{x}/{y}.png\n",
        ] {
            assert_eq!(ok(url), Err(TileServerError::Characters), "{url}");
        }
        let url = "https://tile.example.com/{z}/{x}/{y}.png";
        assert_eq!(check_tile_server(" ", url, ""), Err(TileServerError::Name));
        assert_eq!(
            check_tile_server(&"n".repeat(TILE_NAME_MAX + 1), url, ""),
            Err(TileServerError::Name)
        );
        assert_eq!(
            check_tile_server("Tiles", url, "line\nbreak"),
            Err(TileServerError::Attribution)
        );
        assert_eq!(
            check_tile_server(
                "Tiles",
                &format!(
                    "https://e.com/{}{{z}}/{{x}}/{{y}}",
                    "a".repeat(TILE_URL_MAX)
                ),
                ""
            ),
            Err(TileServerError::Scheme)
        );
    }

    #[test]
    fn tiles_are_off_until_turned_on_and_need_a_server() {
        let path = std::env::temp_dir().join(format!("spelaion-map-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(&path).unwrap();
        let defaults = store.get_map_preferences().unwrap();
        assert_eq!(defaults, MapPreferences::default());
        assert!(!defaults.tiles, "tiles are opt-in");
        // Tiles with no server are refused.
        let no_server = MapPreferences {
            tiles: true,
            ..MapPreferences::default()
        };
        assert!(store.set_map_preferences(&no_server).is_err());
        let server = MapPreferences {
            tiles: true,
            tile_name: " Our tiles ".into(),
            tile_url: "https://tiles.example.com/{z}/{x}/{y}.png".into(),
            tile_attribution: "© Example".into(),
        };
        store.set_map_preferences(&server).unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let read = store.get_map_preferences().unwrap();
        assert_eq!(read.tile_name, "Our tiles");
        assert!(read.tiles);
        // Turned off, the server is kept.
        let off = MapPreferences {
            tiles: false,
            ..read.clone()
        };
        store.set_map_preferences(&off).unwrap();
        assert_eq!(store.get_map_preferences().unwrap(), off);
        // A bad address is refused and the saved one stays.
        let bad = MapPreferences {
            tile_url: "http://tiles.example.com/{z}/{x}/{y}.png".into(),
            ..read.clone()
        };
        assert!(store.set_map_preferences(&bad).is_err());
        assert_eq!(store.get_map_preferences().unwrap(), off);
        // A value written behind the store's back that fails the checks reads as no tiles.
        store
            .set_personal_setting("map.tile_url", "javascript:alert(1)//{z}{x}{y}")
            .unwrap();
        store.set_flag("map.tiles", true).unwrap();
        assert_eq!(
            store.get_map_preferences().unwrap(),
            MapPreferences::default()
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
