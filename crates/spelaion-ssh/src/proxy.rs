//! SOCKS5 and HTTP CONNECT clients (ADR-027): a TCP connection to the proxy, then a tunnel
//! through it to `host:port`. The target name is sent as a name, so the proxy resolves it
//! (like `socks5h`): internal names only the proxy knows work.
//!
//! Small enough to keep here instead of a dependency: RFC 1928 (SOCKS5) with RFC 1929
//! (username and password), and `CONNECT` with `Proxy-Authorization: Basic`.

use std::sync::Arc;
use std::time::Duration;

use spelaion_driver_api::Secret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{Interaction, ProxyFailure, ProxyKind, ProxySpec, SecretRequest, SshError};

/// A proxy failure and what to say about it.
type Failure = (ProxyFailure, String);

/// Opens a connection to `host:port` through the proxy, within `timeout`. The proxy's
/// password is asked through `ui` when it has a user and no password.
pub(crate) async fn connect(
    proxy: &ProxySpec,
    host: &str,
    port: u16,
    timeout: Duration,
    ui: &Arc<dyn Interaction>,
) -> Result<TcpStream, SshError> {
    let password = match (&proxy.user, &proxy.password) {
        (Some(_), Some(password)) => Some(Secret::new(password.expose())),
        (Some(user), None) => Some(
            ui.ask_secret(&SecretRequest::ProxyPassword {
                user: user.clone(),
                host: proxy.host.clone(),
            })
            .await
            .ok_or(SshError::Cancelled)?,
        ),
        (None, _) => None,
    };
    let fail = |(failure, message): Failure| SshError::Proxy {
        proxy: format!("{}:{}", proxy.host, proxy.port),
        message,
        failure,
    };
    let attempt = async {
        let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
            .await
            .map_err(|e| {
                fail((
                    ProxyFailure::Unreachable,
                    format!("could not reach the proxy: {e}"),
                ))
            })?;
        let _ = stream.set_nodelay(true);
        let user = proxy.user.as_deref();
        match proxy.kind {
            ProxyKind::Socks5 => socks5(&mut stream, host, port, user, password.as_ref()).await,
            ProxyKind::Http => http_connect(&mut stream, host, port, user, password.as_ref()).await,
        }
        .map_err(fail)?;
        Ok(stream)
    };
    tokio::time::timeout(timeout, attempt).await.map_err(|_| {
        fail((
            ProxyFailure::Timeout,
            format!("no answer in {} s", timeout.as_secs()),
        ))
    })?
}

/// Checks a proxy before any target is known: it answers, and a SOCKS5 proxy accepts the
/// username and password. An HTTP proxy only checks them on a `CONNECT`, so for it this only
/// checks that it answers.
pub(crate) async fn check(proxy: &ProxySpec, timeout: Duration) -> Result<(), SshError> {
    let fail = |(failure, message): Failure| SshError::Proxy {
        proxy: format!("{}:{}", proxy.host, proxy.port),
        message,
        failure,
    };
    let attempt = async {
        let mut stream = TcpStream::connect((proxy.host.as_str(), proxy.port))
            .await
            .map_err(|e| {
                fail((
                    ProxyFailure::Unreachable,
                    format!("could not reach the proxy: {e}"),
                ))
            })?;
        if proxy.kind == ProxyKind::Socks5 {
            socks5_greeting(&mut stream, proxy.user.as_deref(), proxy.password.as_ref())
                .await
                .map_err(fail)?;
        }
        Ok(())
    };
    tokio::time::timeout(timeout, attempt).await.map_err(|_| {
        fail((
            ProxyFailure::Timeout,
            format!("no answer in {} s", timeout.as_secs()),
        ))
    })?
}

async fn socks5(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    user: Option<&str>,
    password: Option<&Secret>,
) -> Result<(), Failure> {
    let io = |e: std::io::Error| (ProxyFailure::Protocol, format!("the connection broke: {e}"));
    socks5_greeting(stream, user, password).await?;
    // CONNECT with the name, resolved by the proxy.
    if host.len() > 255 {
        return Err((
            ProxyFailure::Protocol,
            "the host name is too long for SOCKS5".into(),
        ));
    }
    let mut request = vec![0x05, 0x01, 0x00, 0x03, host.len() as u8];
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await.map_err(io)?;
    let mut reply = [0u8; 4];
    stream.read_exact(&mut reply).await.map_err(io)?;
    if reply[1] != 0x00 {
        return Err((
            ProxyFailure::Target,
            format!(
                "it could not reach {host}:{port}: {}",
                socks_reply(reply[1])
            ),
        ));
    }
    // The address the proxy bound: read and ignore it.
    let skip = match reply[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await.map_err(io)?;
            usize::from(len[0])
        }
        other => {
            return Err((
                ProxyFailure::Protocol,
                format!("unexpected address type {other} in its answer"),
            ));
        }
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).await.map_err(io)?;
    Ok(())
}

/// Method negotiation and, if the proxy asks, username and password (RFC 1929).
async fn socks5_greeting(
    stream: &mut TcpStream,
    user: Option<&str>,
    password: Option<&Secret>,
) -> Result<(), Failure> {
    let io = |e: std::io::Error| (ProxyFailure::Protocol, format!("the connection broke: {e}"));
    // Greeting: no authentication, and username/password when there is a user.
    let methods: &[u8] = if user.is_some() {
        &[0x00, 0x02]
    } else {
        &[0x00]
    };
    let mut greeting = vec![0x05, methods.len() as u8];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting).await.map_err(io)?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await.map_err(io)?;
    if choice[0] != 0x05 {
        return Err((ProxyFailure::Protocol, "it is not a SOCKS5 proxy".into()));
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let (Some(user), Some(password)) = (user, password) else {
                return Err((
                    ProxyFailure::Rejected,
                    "the proxy asks for a username and password".into(),
                ));
            };
            let password = password.expose();
            if user.len() > 255 || password.len() > 255 {
                return Err((
                    ProxyFailure::Protocol,
                    "SOCKS5 usernames and passwords are at most 255 bytes".into(),
                ));
            }
            let mut auth = vec![0x01, user.len() as u8];
            auth.extend_from_slice(user.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await.map_err(io)?;
            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await.map_err(io)?;
            if status[1] != 0x00 {
                return Err((
                    ProxyFailure::Rejected,
                    "the proxy rejected the username or password".into(),
                ));
            }
        }
        0xff if user.is_none() => {
            return Err((
                ProxyFailure::Rejected,
                "the proxy asks for a username and password".into(),
            ));
        }
        _ => {
            return Err((
                ProxyFailure::Rejected,
                "the proxy accepts none of our authentication methods".into(),
            ));
        }
    }
    Ok(())
}

fn socks_reply(code: u8) -> &'static str {
    match code {
        0x01 => "general failure",
        0x02 => "not allowed by its rules",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "timed out",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown error",
    }
}

async fn http_connect(
    stream: &mut TcpStream,
    host: &str,
    port: u16,
    user: Option<&str>,
    password: Option<&Secret>,
) -> Result<(), Failure> {
    let io = |e: std::io::Error| (ProxyFailure::Protocol, format!("the connection broke: {e}"));
    if host.contains(['\r', '\n', ' ']) {
        return Err((
            ProxyFailure::Protocol,
            "the host name has spaces or line breaks".into(),
        ));
    }
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(user) = user {
        let password = password.map(Secret::expose).unwrap_or_default();
        let credentials = base64(format!("{user}:{password}").as_bytes());
        request.push_str(&format!("Proxy-Authorization: Basic {credentials}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await.map_err(io)?;
    // Read the answer's headers byte by byte: nothing after them may be consumed, it belongs
    // to the tunnel.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return Err((ProxyFailure::Protocol, "its answer is too long".into()));
        }
        let n = stream.read(&mut byte).await.map_err(io)?;
        if n == 0 {
            return Err((
                ProxyFailure::Protocol,
                "it closed the connection without answering".into(),
            ));
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head);
    let status_line = head.lines().next().unwrap_or_default();
    let status = status_line.split_whitespace().nth(1).unwrap_or_default();
    match status {
        "200" => Ok(()),
        // 407 is the standard answer; some proxies (tinyproxy) answer 401.
        "407" | "401" => Err((
            ProxyFailure::Rejected,
            if user.is_some() {
                "the proxy rejected the username or password".into()
            } else {
                "the proxy asks for a username and password".into()
            },
        )),
        _ => Err((
            ProxyFailure::Target,
            format!(
                "it refused to connect to {host}:{port}: {}",
                status_line.trim()
            ),
        )),
    }
}

/// Standard base64 with padding, for `Proxy-Authorization`.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(char::from(ALPHABET[(n >> 18) as usize & 63]));
        out.push(char::from(ALPHABET[(n >> 12) as usize & 63]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(super::base64(b""), "");
        assert_eq!(super::base64(b"f"), "Zg==");
        assert_eq!(super::base64(b"fo"), "Zm8=");
        assert_eq!(super::base64(b"foo"), "Zm9v");
        assert_eq!(
            super::base64(b"proxyuser:proxy-pass"),
            "cHJveHl1c2VyOnByb3h5LXBhc3M="
        );
    }
}
