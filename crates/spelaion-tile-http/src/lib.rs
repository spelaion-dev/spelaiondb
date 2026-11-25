//! The transport the desktop app gives the core to fetch map tiles (ADR-085).
//!
//! **It only moves bytes.** The core decides which tile is fetched, from which stored server,
//! whether a cached one is still fresh, how many requests a map may make and what comes back
//! (`spelaion_core::tiles`). Here, for each request the core makes:
//!
//! - **The schemes a tile server may have:** `https://`, or `http://` only to `localhost` or
//!   `127.0.0.1` (a tile server of one's own, as `check_tile_server` allows). A redirect is
//!   followed only to another `https://` address, a few times at most.
//! - **One stable User-Agent naming the app** ([`spelaion_core::tiles::TILE_USER_AGENT`]), as
//!   OpenStreetMap's tile policy asks, and nothing of the user's: no cookie store (that
//!   feature of the client is never compiled), no credentials, **no `Referer`**. The only other
//!   headers are the core's `If-None-Match` and `If-Modified-Since`, and an `Accept` for images.
//! - **The user's proxies** for servers elsewhere, as `spelaion-https` uses them; none for this
//!   computer.
//! - **Bounded:** a connect timeout, a timeout for the whole request, and a body read no
//!   further than one byte past the largest tile the core accepts.

use std::time::Duration;

use reqwest::header::{
    ACCEPT, CACHE_CONTROL, DATE, ETAG, EXPIRES, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    LAST_MODIFIED,
};
use reqwest::redirect::{Attempt, Policy};
use reqwest::{Client, ClientBuilder, Url};
use spelaion_core::tiles::{
    MAX_TILE_BYTES, TILE_USER_AGENT, TileRequest, TileResponse, TileTransport,
};

/// How long reaching a tile server may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one tile may take, start to end.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The most redirects one tile follows.
const MAX_REDIRECTS: usize = 5;

/// The transport for map tiles.
#[derive(Debug, Clone)]
pub struct TileHttp {
    /// For servers elsewhere: the system's proxies.
    remote: Client,
    /// For this computer: no proxy.
    local: Client,
}

impl TileHttp {
    pub fn new() -> Result<Self, String> {
        // What the updater and `spelaion-https` do: the first crypto provider installed in the
        // process is everyone's.
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        Ok(Self {
            remote: client(Client::builder())?,
            local: client(Client::builder().no_proxy())?,
        })
    }
}

fn client(builder: ClientBuilder) -> Result<Client, String> {
    builder
        .redirect(Policy::custom(follow_only_https))
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(TILE_USER_AGENT)
        .referer(false)
        .build()
        .map_err(|error| describe(&error))
}

/// Whether `address` is this computer, reached over plain `http://`.
fn is_local_http(address: &Url) -> bool {
    address.scheme() == "http"
        && matches!(address.host_str(), Some("localhost") | Some("127.0.0.1"))
}

/// Whether a tile may be asked of `address` first: `https://`, or `http://` to this computer,
/// with no user name or password.
fn acceptable(address: &Url) -> Result<(), String> {
    if address.scheme() != "https" && !is_local_http(address) {
        return Err(format!(
            "{} is neither https:// nor this computer",
            address.host_str().unwrap_or("the address")
        ));
    }
    if !address.username().is_empty() || address.password().is_some() {
        return Err("the address carries credentials, which are never sent".into());
    }
    Ok(())
}

/// The redirect policy: a hop only to an `https://` address with no credentials, and only so many.
fn follow_only_https(attempt: Attempt<'_>) -> reqwest::redirect::Action {
    let url = attempt.url();
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return attempt.error("a tile server may only redirect to an https:// address");
    }
    if attempt.previous().len() >= MAX_REDIRECTS {
        return attempt.error(format!("it redirected more than {MAX_REDIRECTS} times"));
    }
    attempt.follow()
}

/// An error with what caused it, the way a person reads it.
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let line = cause.to_string();
        if !text.contains(&line) {
            text.push_str(": ");
            text.push_str(&line);
        }
        source = cause.source();
    }
    text
}

fn header(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

#[async_trait::async_trait]
impl TileTransport for TileHttp {
    async fn get(&self, request: TileRequest) -> Result<TileResponse, String> {
        let address = Url::parse(&request.url).map_err(|error| error.to_string())?;
        acceptable(&address)?;
        let client = if is_local_http(&address) {
            &self.local
        } else {
            &self.remote
        };
        let mut builder = client
            .get(address)
            .header(ACCEPT, "image/png,image/jpeg,image/webp,image/*;q=0.8");
        if let Some(etag) = &request.if_none_match {
            builder = builder.header(IF_NONE_MATCH, etag);
        }
        if let Some(date) = &request.if_modified_since {
            builder = builder.header(IF_MODIFIED_SINCE, date);
        }
        let mut response = builder.send().await.map_err(|error| describe(&error))?;
        let headers = response.headers().clone();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| describe(&error))? {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_TILE_BYTES {
                // The core refuses it anyway: nothing more is read.
                break;
            }
        }
        Ok(TileResponse {
            status: response.status().as_u16(),
            body,
            cache_control: header(&headers, CACHE_CONTROL),
            expires: header(&headers, EXPIRES),
            date: header(&headers, DATE),
            etag: header(&headers, ETAG),
            last_modified: header(&headers, LAST_MODIFIED),
        })
    }
}
