//! Map tiles, fetched by the core under the tile servers' policy (ADR-085).
//!
//! The page asks for one tile of one map view (`view`, `z`, `x`, `y`) and gets the image; it never
//! names an address. Here:
//!
//! - **Nothing is asked with tiles off**, or with no valid server stored: the stored
//!   preferences are read for every tile and checked again (`MapPreferences::validate`).
//! - **The address is the stored server's**, with `{z}`, `{x}` and `{y}` filled in, for a tile
//!   that exists (zoom 0 to [`MAX_TILE_ZOOM`]).
//! - **A disk cache that honours the server's headers:** a tile is served without a request
//!   while it is fresh (`Cache-Control: max-age`, else `Expires`, else [`DEFAULT_LIFETIME`]);
//!   once stale it is asked for again with `If-None-Match` / `If-Modified-Since`, and a `304`
//!   keeps it with its new lifetime. `no-store` keeps nothing; `no-cache` revalidates every time.
//!   The cache is bounded ([`CACHE_LIMIT`]), the least recently used tiles leaving first.
//! - **Requests are bounded:** [`PARALLEL_REQUESTS`] at a time for the whole app, and at most
//!   [`MAX_VIEW_REQUESTS`] network requests per map view, after which a view gets only what is
//!   cached.
//! - **Only images come back**: PNG, JPEG or WebP, by their first bytes, at most
//!   [`MAX_TILE_BYTES`].
//!
//! The transport ([`TileTransport`], `spelaion-tile-http` in the app) only moves bytes: it sets
//! the User-Agent ([`TILE_USER_AGENT`]), sends no referrer and no cookies, and follows the
//! schemes the stored address may have.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spelaion_store::MapPreferences;
use tokio::sync::Semaphore;

/// What every tile request says it is: the product, its version and the project's address,
/// nothing of the user's (OpenStreetMap's policy asks for a distinct, stable one).
pub const TILE_USER_AGENT: &str = concat!(
    "SpelaionDB/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/spelaion-dev/spelaiondb)"
);

/// The deepest zoom a tile is asked for (the map's own limit, ADR-045).
pub const MAX_TILE_ZOOM: u8 = 19;
/// How long a tile is kept when its headers say nothing that can be read, in seconds: the
/// 7 days OpenStreetMap's policy asks for.
pub const DEFAULT_LIFETIME: i64 = 7 * 24 * 60 * 60;
/// Requests in flight at once, for the whole app.
pub const PARALLEL_REQUESTS: usize = 2;
/// Network requests one map view may make; past them it gets only what is cached.
pub const MAX_VIEW_REQUESTS: usize = 300;
/// The largest tile accepted.
pub const MAX_TILE_BYTES: usize = 2 * 1024 * 1024;
/// The most the cache keeps on disk.
pub const CACHE_LIMIT: u64 = 256 * 1024 * 1024;
/// Tiles written between two looks at the cache's size.
const EVICT_EVERY: usize = 64;

/// A tile server the app offers (ADR-085): only ever chosen by the user, never a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileServerPreset {
    pub name: &'static str,
    pub url: &'static str,
    pub attribution: &'static str,
    pub attribution_url: &'static str,
}

/// OpenStreetMap's standard tiles, under its tile policy.
pub const OPENSTREETMAP: TileServerPreset = TileServerPreset {
    name: "OpenStreetMap",
    url: "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
    attribution: "© OpenStreetMap contributors",
    attribution_url: "https://www.openstreetmap.org/copyright",
};

impl TileServerPreset {
    /// The preferences that choose this server, with tiles on: choosing it is the opt-in.
    pub fn preferences(&self) -> MapPreferences {
        MapPreferences {
            tiles: true,
            tile_name: self.name.into(),
            tile_url: self.url.into(),
            tile_attribution: self.attribution.into(),
            tile_attribution_url: self.attribution_url.into(),
        }
    }
}

/// One request the core asks a transport to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileRequest {
    pub url: String,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
}

/// What the server answered: its status, body and the headers the cache reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TileResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub cache_control: Option<String>,
    pub expires: Option<String>,
    pub date: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// What moves a tile's bytes (ADR-085): the app's is `spelaion-tile-http`.
#[async_trait::async_trait]
pub trait TileTransport: Send + Sync {
    async fn get(&self, request: TileRequest) -> Result<TileResponse, String>;
}

/// A tile's image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tile {
    pub bytes: Vec<u8>,
    /// `image/png`, `image/jpeg` or `image/webp`, read from the bytes.
    pub content_type: &'static str,
}

impl Tile {
    /// The image as a `data:` address, which is how the page draws it: the window's policy lets
    /// no image come from the network (ADR-085).
    pub fn data_url(&self) -> String {
        use base64::Engine as _;
        format!(
            "data:{};base64,{}",
            self.content_type,
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TileError {
    #[error("map tiles are off")]
    TilesOff,
    #[error("there is no tile {z}/{x}/{y}")]
    OutOfRange { z: u8, x: u32, y: u32 },
    #[error("this map asked for its {MAX_VIEW_REQUESTS} tiles: open it again for more")]
    ViewLimit,
    #[error("the tile server answered {0}")]
    Status(u16),
    #[error("the tile server did not send an image")]
    NotImage,
    #[error("could not fetch the tile: {0}")]
    Transport(String),
}

/// What the cache keeps beside a tile's bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kept {
    url: String,
    /// Until when it is served without asking, in milliseconds since 1970.
    fresh_until_ms: i64,
    etag: Option<String>,
    last_modified: Option<String>,
    used_ms: i64,
}

/// How long a response may be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifetime {
    /// `no-store`: not kept at all.
    NoStore,
    /// Seconds it is fresh for (0: revalidated at every use).
    Seconds(i64),
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// An HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`) in milliseconds since 1970.
fn http_date_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(text.trim())
        .ok()
        .map(|date| date.timestamp_millis())
}

/// How long a response may be kept, from its headers (`Cache-Control` first, then `Expires`
/// against the server's `Date`), and [`DEFAULT_LIFETIME`] when they say nothing readable.
fn lifetime(response: &TileResponse, now_ms: i64) -> Lifetime {
    if let Some(control) = &response.cache_control {
        let mut max_age = None;
        for directive in control.split(',').map(|d| d.trim().to_ascii_lowercase()) {
            if directive == "no-store" {
                return Lifetime::NoStore;
            }
            if directive == "no-cache" {
                return Lifetime::Seconds(0);
            }
            if let Some(seconds) = directive.strip_prefix("max-age=") {
                max_age = seconds.trim_matches('"').parse::<i64>().ok();
            }
        }
        if let Some(seconds) = max_age {
            return Lifetime::Seconds(seconds.max(0));
        }
    }
    if let Some(expires) = response.expires.as_deref().and_then(http_date_ms) {
        let date = response
            .date
            .as_deref()
            .and_then(http_date_ms)
            .unwrap_or(now_ms);
        return Lifetime::Seconds(((expires - date) / 1000).max(0));
    }
    Lifetime::Seconds(DEFAULT_LIFETIME)
}

/// The image type of `bytes`, by their first bytes.
fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// The address of tile `z/x/y` of a server's template.
pub fn tile_address(template: &str, z: u8, x: u32, y: u32) -> String {
    template
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
}

/// The tiles of the app: the cache, the requests in flight and what each view has asked.
pub struct TileCache {
    dir: PathBuf,
    transport: Box<dyn TileTransport>,
    requests: Semaphore,
    views: Mutex<HashMap<u64, usize>>,
    written: AtomicUsize,
    limit: u64,
}

impl TileCache {
    /// A cache in `dir` (created when the first tile is kept), fetching through `transport`.
    pub fn new(dir: impl Into<PathBuf>, transport: Box<dyn TileTransport>) -> Self {
        Self {
            dir: dir.into(),
            transport,
            requests: Semaphore::new(PARALLEL_REQUESTS),
            views: Mutex::new(HashMap::new()),
            written: AtomicUsize::new(0),
            limit: CACHE_LIMIT,
        }
    }

    /// The same, keeping at most `limit` bytes (for tests).
    pub fn with_limit(mut self, limit: u64) -> Self {
        self.limit = limit;
        self
    }

    fn paths(&self, url: &str) -> (PathBuf, PathBuf) {
        let hash = format!("{:x}", Sha256::digest(url.as_bytes()));
        let folder = self.dir.join(&hash[..2]);
        (
            folder.join(format!("{hash}.tile")),
            folder.join(format!("{hash}.json")),
        )
    }

    fn read_kept(&self, url: &str) -> Option<(Kept, Vec<u8>)> {
        let (tile, meta) = self.paths(url);
        let kept: Kept = serde_json::from_slice(&std::fs::read(meta).ok()?).ok()?;
        let bytes = std::fs::read(tile).ok()?;
        // Another address with the same hash, or a file someone else wrote: not this tile.
        (kept.url == url && image_type(&bytes).is_some()).then_some((kept, bytes))
    }

    fn write_kept(&self, kept: &Kept, bytes: Option<&[u8]>) {
        let (tile, meta) = self.paths(&kept.url);
        if let Some(parent) = meta.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }
        if let Some(bytes) = bytes
            && std::fs::write(&tile, bytes).is_err()
        {
            return;
        }
        if let Ok(json) = serde_json::to_vec(kept) {
            let _ = std::fs::write(meta, json);
        }
        if bytes.is_some()
            && self
                .written
                .fetch_add(1, Ordering::Relaxed)
                .is_multiple_of(EVICT_EVERY)
        {
            evict(&self.dir, self.limit);
        }
    }

    fn forget(&self, url: &str) {
        let (tile, meta) = self.paths(url);
        let _ = std::fs::remove_file(meta);
        let _ = std::fs::remove_file(tile);
    }

    /// Takes one of `view`'s network requests, if it has any left.
    fn take_request(&self, view: u64) -> bool {
        let mut views = self.views.lock().expect("lock poisoned");
        let used = views.entry(view).or_default();
        if *used >= MAX_VIEW_REQUESTS {
            return false;
        }
        *used += 1;
        true
    }

    /// Network requests `view` has made.
    pub fn requests_of(&self, view: u64) -> usize {
        self.views
            .lock()
            .expect("lock poisoned")
            .get(&view)
            .copied()
            .unwrap_or(0)
    }

    /// Tile `z/x/y` of map view `view`, from the server `preferences` name: from the cache while
    /// it is fresh, else from the server.
    pub async fn tile(
        &self,
        preferences: &MapPreferences,
        view: u64,
        z: u8,
        x: u32,
        y: u32,
    ) -> Result<Tile, TileError> {
        if !preferences.tiles || preferences.tile_url.is_empty() || preferences.validate().is_err()
        {
            return Err(TileError::TilesOff);
        }
        let side = 1u64 << z.min(MAX_TILE_ZOOM);
        if z > MAX_TILE_ZOOM || u64::from(x) >= side || u64::from(y) >= side {
            return Err(TileError::OutOfRange { z, x, y });
        }
        let url = tile_address(&preferences.tile_url, z, x, y);
        let now = now_ms();
        let kept = self.read_kept(&url);
        if let Some((mut meta, bytes)) = kept.clone()
            && now < meta.fresh_until_ms
        {
            meta.used_ms = now;
            self.write_kept(&meta, None);
            return tile_of(bytes);
        }
        if !self.take_request(view) {
            // Past the view's limit, a stale tile is better than none.
            return match kept {
                Some((_, bytes)) => tile_of(bytes),
                None => Err(TileError::ViewLimit),
            };
        }
        let request = TileRequest {
            url: url.clone(),
            if_none_match: kept.as_ref().and_then(|(m, _)| m.etag.clone()),
            if_modified_since: kept.as_ref().and_then(|(m, _)| m.last_modified.clone()),
        };
        let response = {
            let _turn = self.requests.acquire().await.expect("never closed");
            self.transport
                .get(request)
                .await
                .map_err(TileError::Transport)?
        };
        let now = now_ms();
        let fresh_until = |lifetime: Lifetime| match lifetime {
            Lifetime::NoStore => None,
            Lifetime::Seconds(seconds) => Some(now.saturating_add(seconds.saturating_mul(1000))),
        };
        match (response.status, kept) {
            (304, Some((meta, bytes))) => {
                match fresh_until(lifetime(&response, now)) {
                    Some(until) => self.write_kept(
                        &Kept {
                            fresh_until_ms: until,
                            etag: response.etag.clone().or(meta.etag),
                            last_modified: response.last_modified.clone().or(meta.last_modified),
                            used_ms: now,
                            url,
                        },
                        None,
                    ),
                    None => self.forget(&url),
                }
                tile_of(bytes)
            }
            (200, _) => {
                if response.body.len() > MAX_TILE_BYTES {
                    return Err(TileError::NotImage);
                }
                let kept_for = fresh_until(lifetime(&response, now));
                let tile = tile_of(response.body)?;
                match kept_for {
                    Some(until) => self.write_kept(
                        &Kept {
                            url,
                            fresh_until_ms: until,
                            etag: response.etag,
                            last_modified: response.last_modified,
                            used_ms: now,
                        },
                        Some(&tile.bytes),
                    ),
                    None => self.forget(&url),
                }
                Ok(tile)
            }
            (status, _) => Err(TileError::Status(status)),
        }
    }
}

fn tile_of(bytes: Vec<u8>) -> Result<Tile, TileError> {
    match image_type(&bytes) {
        Some(content_type) => Ok(Tile {
            bytes,
            content_type,
        }),
        None => Err(TileError::NotImage),
    }
}

/// Removes the least recently used tiles of `dir` until it holds at most nine tenths of
/// `limit`, when it holds more than `limit`.
fn evict(dir: &Path, limit: u64) {
    let mut entries: Vec<(i64, u64, PathBuf, PathBuf)> = Vec::new();
    let mut total = 0u64;
    let Ok(folders) = std::fs::read_dir(dir) else {
        return;
    };
    for folder in folders.flatten() {
        let Ok(files) = std::fs::read_dir(folder.path()) else {
            continue;
        };
        for file in files.flatten() {
            let meta = file.path();
            if meta.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let tile = meta.with_extension("tile");
            let size = std::fs::metadata(&tile).map(|m| m.len()).unwrap_or(0)
                + file.metadata().map(|m| m.len()).unwrap_or(0);
            let used = std::fs::read(&meta)
                .ok()
                .and_then(|json| serde_json::from_slice::<Kept>(&json).ok())
                .map_or(0, |kept| kept.used_ms);
            total += size;
            entries.push((used, size, meta, tile));
        }
    }
    if total <= limit {
        return;
    }
    entries.sort_by_key(|(used, ..)| *used);
    let target = limit / 10 * 9;
    for (_, size, meta, tile) in entries {
        if total <= target {
            break;
        }
        let _ = std::fs::remove_file(meta);
        let _ = std::fs::remove_file(tile);
        total = total.saturating_sub(size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(headers: &[(&str, &str)]) -> TileResponse {
        let mut r = TileResponse {
            status: 200,
            ..TileResponse::default()
        };
        for (name, value) in headers {
            let value = Some((*value).to_string());
            match *name {
                "cache-control" => r.cache_control = value,
                "expires" => r.expires = value,
                "date" => r.date = value,
                _ => {}
            }
        }
        r
    }

    #[test]
    fn lifetimes_follow_the_headers_and_default_to_seven_days() {
        let now = http_date_ms("Wed, 25 Nov 2026 10:00:00 GMT").unwrap();
        let of = |headers: &[(&str, &str)]| lifetime(&response(headers), now);
        assert_eq!(of(&[]), Lifetime::Seconds(DEFAULT_LIFETIME));
        assert_eq!(
            of(&[("cache-control", "public, max-age=3600")]),
            Lifetime::Seconds(3600)
        );
        assert_eq!(
            of(&[("cache-control", "no-store, max-age=60")]),
            Lifetime::NoStore
        );
        assert_eq!(of(&[("cache-control", "no-cache")]), Lifetime::Seconds(0));
        // Cache-Control wins over Expires.
        assert_eq!(
            of(&[
                ("cache-control", "max-age=10"),
                ("expires", "Thu, 26 Nov 2026 10:00:00 GMT")
            ]),
            Lifetime::Seconds(10)
        );
        // Expires against the server's own Date, then against now.
        assert_eq!(
            of(&[
                ("expires", "Wed, 25 Nov 2026 12:00:00 GMT"),
                ("date", "Wed, 25 Nov 2026 11:00:00 GMT")
            ]),
            Lifetime::Seconds(3600)
        );
        assert_eq!(
            of(&[("expires", "Wed, 25 Nov 2026 10:30:00 GMT")]),
            Lifetime::Seconds(1800)
        );
        assert_eq!(
            of(&[("expires", "Mon, 23 Nov 2026 10:00:00 GMT")]),
            Lifetime::Seconds(0)
        );
        // What cannot be read is the default.
        assert_eq!(of(&[("expires", "0")]), Lifetime::Seconds(DEFAULT_LIFETIME));
        assert_eq!(
            of(&[("cache-control", "public")]),
            Lifetime::Seconds(DEFAULT_LIFETIME)
        );
    }

    #[test]
    fn only_images_are_tiles() {
        assert_eq!(
            image_type(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0]),
            Some("image/png")
        );
        assert_eq!(image_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(image_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_type(b"<html>"), None);
        assert_eq!(
            tile_address("https://t.example.com/{z}/{x}/{y}.png?k=1", 3, 5, 7),
            "https://t.example.com/3/5/7.png?k=1"
        );
    }

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];

    /// A server in memory: answers each request with the next of `answers` (the last one again
    /// when they run out) and remembers every request.
    struct Scripted {
        answers: Mutex<Vec<TileResponse>>,
        seen: std::sync::Arc<Mutex<Vec<TileRequest>>>,
    }

    #[async_trait::async_trait]
    impl TileTransport for Scripted {
        async fn get(&self, request: TileRequest) -> Result<TileResponse, String> {
            self.seen.lock().unwrap().push(request);
            let mut answers = self.answers.lock().unwrap();
            Ok(if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            })
        }
    }

    fn cache_with(
        name: &str,
        answers: Vec<TileResponse>,
    ) -> (TileCache, std::sync::Arc<Mutex<Vec<TileRequest>>>) {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-tiles-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let transport = Scripted {
            answers: Mutex::new(answers),
            seen: seen.clone(),
        };
        (TileCache::new(dir, Box::new(transport)), seen)
    }

    fn ok(headers: &[(&str, &str)]) -> TileResponse {
        let mut r = response(headers);
        r.body = PNG.to_vec();
        for (name, value) in headers {
            match *name {
                "etag" => r.etag = Some((*value).into()),
                "last-modified" => r.last_modified = Some((*value).into()),
                _ => {}
            }
        }
        r
    }

    fn server() -> MapPreferences {
        MapPreferences {
            tiles: true,
            tile_name: "Local".into(),
            tile_url: "http://127.0.0.1:1/{z}/{x}/{y}.png".into(),
            tile_attribution: String::new(),
            tile_attribution_url: String::new(),
        }
    }

    #[tokio::test]
    async fn nothing_is_asked_with_tiles_off_or_out_of_range() {
        let (cache, seen) = cache_with("off", vec![ok(&[])]);
        let off = MapPreferences {
            tiles: false,
            ..server()
        };
        assert_eq!(cache.tile(&off, 1, 0, 0, 0).await, Err(TileError::TilesOff));
        // A server written behind the store's back is no server.
        let bad = MapPreferences {
            tile_url: "http://tiles.example.com/{z}/{x}/{y}.png".into(),
            ..server()
        };
        assert_eq!(cache.tile(&bad, 1, 0, 0, 0).await, Err(TileError::TilesOff));
        for (z, x, y) in [(20, 0, 0), (2, 4, 0), (2, 0, 4)] {
            assert_eq!(
                cache.tile(&server(), 1, z, x, y).await,
                Err(TileError::OutOfRange { z, x, y })
            );
        }
        assert!(seen.lock().unwrap().is_empty(), "nothing was requested");
    }

    #[tokio::test]
    async fn a_fresh_tile_is_not_asked_again_and_a_stale_one_is_revalidated() {
        let not_modified = TileResponse {
            status: 304,
            cache_control: Some("max-age=3600".into()),
            ..TileResponse::default()
        };
        let (cache, seen) = cache_with(
            "revalidate",
            vec![
                ok(&[
                    ("cache-control", "max-age=0"),
                    ("etag", "\"v1\""),
                    ("last-modified", "Wed, 25 Nov 2026 10:00:00 GMT"),
                ]),
                not_modified,
            ],
        );
        let first = cache.tile(&server(), 1, 3, 2, 1).await.unwrap();
        assert_eq!(first.content_type, "image/png");
        // Stale at once: asked again, conditionally, and the 304 keeps the bytes.
        let second = cache.tile(&server(), 1, 3, 2, 1).await.unwrap();
        assert_eq!(second, first);
        // Now fresh for an hour: not asked again, from another view too.
        let third = cache.tile(&server(), 2, 3, 2, 1).await.unwrap();
        assert_eq!(third, first);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].url, "http://127.0.0.1:1/3/2/1.png");
        assert_eq!(
            (
                seen[0].if_none_match.clone(),
                seen[0].if_modified_since.clone()
            ),
            (None, None)
        );
        assert_eq!(seen[1].if_none_match.as_deref(), Some("\"v1\""));
        assert_eq!(
            seen[1].if_modified_since.as_deref(),
            Some("Wed, 25 Nov 2026 10:00:00 GMT")
        );
        assert_eq!(cache.requests_of(2), 0);
    }

    #[tokio::test]
    async fn no_store_keeps_nothing_and_errors_are_not_kept() {
        let (cache, seen) = cache_with("nostore", vec![ok(&[("cache-control", "no-store")])]);
        cache.tile(&server(), 1, 1, 0, 0).await.unwrap();
        cache.tile(&server(), 1, 1, 0, 0).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);

        let html = TileResponse {
            status: 200,
            body: b"<html>blocked</html>".to_vec(),
            ..TileResponse::default()
        };
        let (cache, _) = cache_with("html", vec![html]);
        assert_eq!(
            cache.tile(&server(), 1, 1, 0, 0).await,
            Err(TileError::NotImage)
        );
        let (cache, _) = cache_with(
            "status",
            vec![TileResponse {
                status: 429,
                ..TileResponse::default()
            }],
        );
        assert_eq!(
            cache.tile(&server(), 1, 1, 0, 0).await,
            Err(TileError::Status(429))
        );
    }

    #[tokio::test]
    async fn a_view_makes_at_most_its_requests() {
        let (cache, seen) = cache_with("limit", vec![ok(&[("cache-control", "no-store")])]);
        for _ in 0..MAX_VIEW_REQUESTS {
            cache.tile(&server(), 7, 0, 0, 0).await.unwrap();
        }
        assert_eq!(
            cache.tile(&server(), 7, 0, 0, 0).await,
            Err(TileError::ViewLimit)
        );
        assert_eq!(seen.lock().unwrap().len(), MAX_VIEW_REQUESTS);
        // Another view has its own.
        cache.tile(&server(), 8, 0, 0, 0).await.unwrap();
    }

    #[tokio::test]
    async fn the_cache_stays_within_its_limit_oldest_first() {
        let (cache, _) = cache_with("evict", vec![ok(&[("cache-control", "max-age=3600")])]);
        let cache = cache.with_limit(1);
        // Every write past the first looks at the size only every EVICT_EVERY tiles: the first
        // write's look finds one tile over the limit and removes it.
        cache.tile(&server(), 1, 1, 0, 0).await.unwrap();
        let kept = std::fs::read_dir(&cache.dir)
            .map(|folders| {
                folders
                    .flatten()
                    .flat_map(|f| std::fs::read_dir(f.path()).into_iter().flatten().flatten())
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(kept, 0, "the tile past the limit was removed");
    }

    /// SDB-533: OpenStreetMap, once chosen, is turned off by unticking tiles and taken away by
    /// Remove, each read back from the store as saved, and then no tile is asked of it.
    #[tokio::test]
    async fn openstreetmap_turns_off_and_is_removed() {
        let path = std::env::temp_dir().join(format!("spelaion-osm-off-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let (cache, seen) = cache_with("osm-off", vec![ok(&[])]);
        let store = spelaion_store::Store::open(&path).unwrap();
        let chosen = OPENSTREETMAP.preferences();
        store.set_map_preferences(&chosen).unwrap();
        assert_eq!(store.get_map_preferences().unwrap(), chosen);

        // Unticked, as the settings page saves it: the server stays, tiles are off, also once
        // the store is opened again.
        let off = MapPreferences {
            tiles: false,
            ..chosen.clone()
        };
        store.set_map_preferences(&off).unwrap();
        drop(store);
        let store = spelaion_store::Store::open(&path).unwrap();
        let read = store.get_map_preferences().unwrap();
        assert_eq!(read, off);
        assert_eq!(
            cache.tile(&read, 1, 0, 0, 0).await,
            Err(TileError::TilesOff)
        );

        // Chosen again, then removed: every field empty and tiles off.
        store.set_map_preferences(&chosen).unwrap();
        store
            .set_map_preferences(&MapPreferences::default())
            .unwrap();
        drop(store);
        let store = spelaion_store::Store::open(&path).unwrap();
        let read = store.get_map_preferences().unwrap();
        assert_eq!(read, MapPreferences::default());
        assert_eq!(
            cache.tile(&read, 1, 0, 0, 0).await,
            Err(TileError::TilesOff)
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing was asked of OpenStreetMap"
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_preset_is_a_valid_server_and_opts_in() {
        let preferences = OPENSTREETMAP.preferences();
        assert!(preferences.tiles);
        assert_eq!(preferences.validate(), Ok(()));
        assert!(TILE_USER_AGENT.starts_with("SpelaionDB/"));
        assert!(TILE_USER_AGENT.contains("https://github.com/spelaion-dev/spelaiondb"));
    }
}
