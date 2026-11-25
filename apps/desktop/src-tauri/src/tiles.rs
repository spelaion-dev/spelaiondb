//! Map tile commands (ADR-085). The page asks for a tile of a map view by its numbers and gets
//! the image as a `data:` address; the core reads the stored tile server, decides whether a
//! request is made at all and keeps the cache. The page never names an address: neither a
//! tile's nor the attribution's link.

use spelaion_core::tiles::{TileCache, TileRequest, TileResponse, TileTransport};
use spelaion_desktop_macros::command;
use tauri::State;

use crate::dto::MapPreferencesItem;
use crate::{AppState, CmdResult};

/// The app's tile cache: `tile-cache` next to `state.db`, fetching through `spelaion-tile-http`.
pub fn cache(user_dir: &std::path::Path) -> TileCache {
    let state_db = spelaion_config::state_db(user_dir);
    let dir = state_db
        .parent()
        .map_or_else(|| user_dir.join("tile-cache"), |p| p.join("tile-cache"));
    let transport: Box<dyn TileTransport> = match spelaion_tile_http::TileHttp::new() {
        Ok(transport) => Box::new(transport),
        Err(why) => Box::new(Unavailable(why)),
    };
    TileCache::new(dir, transport)
}

/// A transport that could not be built: every tile says why.
struct Unavailable(String);

#[async_trait::async_trait]
impl TileTransport for Unavailable {
    async fn get(&self, _request: TileRequest) -> Result<TileResponse, String> {
        Err(format!("no tile can be fetched: {}", self.0))
    }
}

/// Tile `z/x/y` of map view `view`, from the stored tile server, as a `data:` image. Refused with
/// tiles off, past the view's requests, or when the server sends no image.
#[command]
pub async fn map_tile(
    view: u32,
    z: u8,
    x: u32,
    y: u32,
    state: State<'_, AppState>,
) -> CmdResult<String> {
    let preferences = state
        .history
        .map_preferences()
        .await
        .map_err(|e| e.to_string())?;
    state
        .tiles
        .tile(&preferences, u64::from(view), z, x, y)
        .await
        .map(|tile| tile.data_url())
        .map_err(|e| e.to_string())
}

/// The preferences that choose OpenStreetMap's tiles, with tiles on (the user's opt-in): the
/// page saves them as they come, and the core checks them again then.
#[command]
pub fn openstreetmap_tiles() -> MapPreferencesItem {
    spelaion_core::tiles::OPENSTREETMAP.preferences().into()
}

/// Opens the stored tile server's attribution link in the system's browser, on the user's click:
/// an `https://` address checked when it was saved and again here.
#[command]
pub async fn open_tile_attribution(state: State<'_, AppState>) -> CmdResult<()> {
    let preferences = state
        .history
        .map_preferences()
        .await
        .map_err(|e| e.to_string())?;
    let link = preferences.tile_attribution_url;
    if link.is_empty() {
        return Err("this tile server's attribution has no link".into());
    }
    spelaion_core::check_attribution_link(&link).map_err(|e| e.to_string())?;
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(program)
        .arg(&link)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not open the browser: {e}; the page is {link}"))?;
    // The opener may stay with the browser it started: reap it without waiting here.
    std::thread::spawn(move || child.wait());
    Ok(())
}
