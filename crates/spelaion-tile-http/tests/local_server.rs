//! Map tiles against a tile server on this computer (ADR-085): never tile.openstreetmap.org or
//! any other outside host. The core's cache and this transport together: the User-Agent and
//! the headers that go out, a fresh tile not asked again, a stale one asked conditionally, and
//! nothing asked with tiles off.

use std::sync::{Arc, Mutex};

use spelaion_core::MapPreferences;
use spelaion_core::tiles::{TILE_USER_AGENT, TileCache, TileError};
use spelaion_tile_http::TileHttp;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];

/// One request as the server read it: its path and its headers, names in lower case.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A tile server on 127.0.0.1 that answers each request with `answer(request number, request)`
/// (status line and headers, then the body) and remembers what it was sent.
async fn server(
    answer: impl Fn(usize, &Seen) -> (String, Vec<u8>) + Send + Sync + 'static,
) -> (u16, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let answer = Arc::new(answer);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
            let answer = answer.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request).to_string();
                let mut lines = text.split("\r\n");
                let path = lines
                    .next()
                    .and_then(|l| l.split(' ').nth(1))
                    .unwrap_or("")
                    .to_string();
                let headers = lines
                    .take_while(|l| !l.is_empty())
                    .filter_map(|l| l.split_once(':'))
                    .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let seen = Seen { path, headers };
                let number = {
                    let mut log = log.lock().unwrap();
                    log.push(seen.clone());
                    log.len()
                };
                let (head, body) = answer(number, &seen);
                let response = format!(
                    "{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (port, seen)
}

fn preferences(port: u16, tiles: bool) -> MapPreferences {
    MapPreferences {
        tiles,
        tile_name: "Local tiles".into(),
        tile_url: format!("http://127.0.0.1:{port}/{{z}}/{{x}}/{{y}}.png"),
        tile_attribution: "Test tiles".into(),
        tile_attribution_url: String::new(),
    }
}

fn cache(name: &str) -> TileCache {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "spelaion-tile-http-{name}-{}-{nonce}",
        std::process::id()
    ));
    TileCache::new(dir, Box::new(TileHttp::new().unwrap()))
}

#[tokio::test]
async fn tiles_go_out_with_our_user_agent_and_nothing_of_the_users() {
    let (port, seen) = server(|_, _| {
        (
            "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nCache-Control: max-age=3600".into(),
            PNG.to_vec(),
        )
    })
    .await;
    let cache = cache("agent");
    let tile = cache
        .tile(&preferences(port, true), 1, 4, 3, 2)
        .await
        .unwrap();
    assert_eq!(tile.content_type, "image/png");
    assert_eq!(tile.bytes, PNG);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let request = &seen[0];
    assert_eq!(request.path, "/4/3/2.png");
    assert_eq!(request.header("user-agent"), Some(TILE_USER_AGENT));
    assert!(
        TILE_USER_AGENT.starts_with("SpelaionDB/")
            && TILE_USER_AGENT.contains("https://github.com/spelaion-dev/spelaiondb"),
        "{TILE_USER_AGENT}"
    );
    for absent in [
        "referer",
        "cookie",
        "authorization",
        "if-none-match",
        "if-modified-since",
    ] {
        assert_eq!(request.header(absent), None, "{absent}");
    }
}

#[tokio::test]
async fn a_fresh_tile_is_not_asked_again_and_a_stale_one_is_asked_conditionally() {
    // The first answer is stale at once with an ETag; the second, a 304 fresh for an hour.
    let (port, seen) = server(|number, request| {
        if number == 1 {
            (
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nCache-Control: max-age=0\r\n\
                 ETag: \"tile-v1\"\r\nLast-Modified: Wed, 25 Nov 2026 10:00:00 GMT"
                    .into(),
                PNG.to_vec(),
            )
        } else if request.header("if-none-match") == Some("\"tile-v1\"") {
            (
                "HTTP/1.1 304 Not Modified\r\nCache-Control: max-age=3600".into(),
                Vec::new(),
            )
        } else {
            ("HTTP/1.1 500 Unexpected".into(), Vec::new())
        }
    })
    .await;
    let cache = cache("conditional");
    let server = preferences(port, true);
    let first = cache.tile(&server, 1, 2, 1, 1).await.unwrap();
    let second = cache.tile(&server, 1, 2, 1, 1).await.unwrap();
    let third = cache.tile(&server, 1, 2, 1, 1).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(second, third);
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "the tile fresh after the 304 was not asked again"
    );
    assert_eq!(seen[1].header("if-none-match"), Some("\"tile-v1\""));
    assert_eq!(
        seen[1].header("if-modified-since"),
        Some("Wed, 25 Nov 2026 10:00:00 GMT")
    );
    assert_eq!(seen[1].header("user-agent"), Some(TILE_USER_AGENT));
}

#[tokio::test]
async fn a_tile_without_cache_headers_is_kept_seven_days() {
    let (port, seen) = server(|_, _| {
        (
            "HTTP/1.1 200 OK\r\nContent-Type: image/png".into(),
            PNG.to_vec(),
        )
    })
    .await;
    let cache = cache("default");
    for view in 1..=3 {
        cache
            .tile(&preferences(port, true), view, 1, 0, 1)
            .await
            .unwrap();
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn nothing_is_asked_with_tiles_off() {
    let (port, seen) = server(|_, _| ("HTTP/1.1 200 OK".into(), PNG.to_vec())).await;
    let cache = cache("off");
    assert_eq!(
        cache.tile(&preferences(port, false), 1, 0, 0, 0).await,
        Err(TileError::TilesOff)
    );
    // Give a request that should not exist the time to arrive.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
}

#[tokio::test]
async fn what_is_not_an_image_or_comes_from_elsewhere_is_refused() {
    let (port, _) = server(|number, _| match number {
        1 => (
            "HTTP/1.1 200 OK\r\nContent-Type: text/html".into(),
            b"<html>blocked</html>".to_vec(),
        ),
        // A redirect away from https:// is not followed.
        _ => (
            "HTTP/1.1 302 Found\r\nLocation: http://tiles.example.com/0/0/0.png".into(),
            Vec::new(),
        ),
    })
    .await;
    let cache = cache("refused");
    let server = preferences(port, true);
    assert_eq!(
        cache.tile(&server, 1, 0, 0, 0).await,
        Err(TileError::NotImage)
    );
    let redirected = cache.tile(&server, 1, 1, 0, 0).await;
    assert!(
        matches!(&redirected, Err(TileError::Transport(why)) if why.contains("https://")),
        "{redirected:?}"
    );
}
