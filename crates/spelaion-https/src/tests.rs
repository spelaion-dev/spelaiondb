//! The transport against servers on this machine: an HTTPS one with a certificate made for the
//! test, and a plain one that only counts who reaches it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{CertificateParams, KeyPair, SanType};
use reqwest::Certificate;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;

/// What the HTTPS server answers at a path: a status, extra headers and a body.
#[derive(Clone)]
struct Answer {
    status: &'static str,
    headers: Vec<String>,
    body: Vec<u8>,
}

impl Answer {
    fn ok(body: &[u8]) -> Self {
        Self {
            status: "200 OK",
            headers: Vec::new(),
            body: body.to_vec(),
        }
    }

    fn redirect(to: String) -> Self {
        Self {
            status: "302 Found",
            headers: vec![
                format!("Location: {to}"),
                // A server that tries to give the client something to send back.
                "Set-Cookie: session=secret; Path=/".to_string(),
            ],
            body: Vec::new(),
        }
    }
}

/// An HTTPS server on 127.0.0.1 and the certificate a client must trust to reach it.
struct HttpsServer {
    port: u16,
    certificate: Vec<u8>,
    /// Every request that reached it, as it arrived (request line and headers).
    requests: Arc<Mutex<Vec<String>>>,
}

impl HttpsServer {
    async fn start(answers: impl Fn(u16) -> HashMap<&'static str, Answer>) -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.subject_alt_names = vec![SanType::IpAddress(IpAddr::V4(Ipv4Addr::LOCALHOST))];
        let certificate = params.self_signed(&key).unwrap();

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(certificate.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
            )
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let answers = Arc::new(answers(port));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                let answers = answers.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let request = read_request(&mut stream).await;
                    seen.lock().unwrap().push(request.clone());
                    let path = request.split_whitespace().nth(1).unwrap_or("").to_string();
                    let answer = answers.get(path.as_str()).cloned().unwrap_or(Answer {
                        status: "404 Not Found",
                        headers: Vec::new(),
                        body: Vec::new(),
                    });
                    let mut head = format!(
                        "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        answer.status,
                        answer.body.len()
                    );
                    for header in &answer.headers {
                        head.push_str(header);
                        head.push_str("\r\n");
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&answer.body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            port,
            certificate: certificate.pem().into_bytes(),
            requests,
        }
    }

    fn address(&self, path: &str) -> String {
        format!("https://127.0.0.1:{}{path}", self.port)
    }

    /// The transport the app builds, trusting only this server's certificate and going
    /// through no proxy (whatever this machine has configured).
    fn transport(&self) -> HttpsTransport {
        let certificate = Certificate::from_pem(&self.certificate).unwrap();
        HttpsTransport::from_builder(Client::builder().tls_certs_only([certificate]).no_proxy())
            .unwrap()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

async fn read_request<S: AsyncReadExt + Unpin>(stream: &mut S) -> String {
    let mut request = Vec::new();
    let mut buffer = [0u8; 1024];
    while !request.ends_with(b"\r\n\r\n") && request.len() < 64 * 1024 {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

/// A plain-HTTP server that counts the connections that reach it and answers each with a body.
struct PlainServer {
    port: u16,
    reached: Arc<AtomicUsize>,
}

impl PlainServer {
    async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let reached = Arc::new(AtomicUsize::new(0));
        let counter = reached.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let _ = read_request(&mut stream).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nevil")
                        .await;
                });
            }
        });
        Self { port, reached }
    }

    fn address(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Whether anything reached it, after giving a stray request time to arrive.
    async fn was_reached(&self) -> bool {
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.reached.load(Ordering::SeqCst) > 0
    }
}

async fn read_all(body: &mut Box<dyn Body>) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(chunk) = body.chunk().await.unwrap() {
        all.extend_from_slice(&chunk);
    }
    all
}

/// The error of a fetch that must fail.
async fn refused(transport: &HttpsTransport, address: &str) -> String {
    match transport.get(address).await {
        Ok(body) => panic!("{address} was fetched from {}", body.address()),
        Err(TransportError(why)) => why,
    }
}

#[tokio::test]
async fn an_https_address_is_fetched() {
    let server = HttpsServer::start(|_| HashMap::from([("/file", Answer::ok(b"the bytes"))])).await;
    let mut body = server
        .transport()
        .get(&server.address("/file"))
        .await
        .unwrap();
    assert_eq!(body.address(), server.address("/file"));
    assert_eq!(read_all(&mut body).await, b"the bytes");
}

#[tokio::test]
async fn plain_http_is_refused_before_anything_is_sent() {
    let plain = PlainServer::start().await;
    let server = HttpsServer::start(|_| HashMap::new()).await;
    let transport = server.transport();
    let why = refused(&transport, &plain.address("/catalogue")).await;
    assert!(why.contains("https://"), "{why}");
    assert!(!plain.was_reached().await, "the plain server was reached");
}

#[tokio::test]
async fn a_redirect_to_plain_http_is_refused_before_it_is_followed() {
    let plain = PlainServer::start().await;
    let to = plain.address("/archive.tar.gz");
    let server = HttpsServer::start(move |_| {
        HashMap::from([("/archive.tar.gz", Answer::redirect(to.clone()))])
    })
    .await;
    let why = refused(&server.transport(), &server.address("/archive.tar.gz")).await;
    assert!(why.contains("not an https:// address"), "{why}");
    assert_eq!(server.requests().len(), 1);
    assert!(!plain.was_reached().await, "the redirect was followed");
}

/// A redirect to another `https://` address is followed, the body is the last one's, and
/// `Body::address` says where it came from. What the first server set is not sent back.
#[tokio::test]
async fn a_redirect_to_https_is_followed_and_sends_nothing_back() {
    let server = HttpsServer::start(|port| {
        HashMap::from([
            (
                "/start",
                Answer::redirect(format!("https://127.0.0.1:{port}/middle")),
            ),
            ("/middle", Answer::redirect("/end".to_string())),
            ("/end", Answer::ok(b"arrived")),
        ])
    })
    .await;
    let mut body = server
        .transport()
        .get(&server.address("/start"))
        .await
        .unwrap();
    assert_eq!(body.address(), server.address("/end"));
    assert_eq!(read_all(&mut body).await, b"arrived");
    let requests = server.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    for request in &requests {
        let lower = request.to_ascii_lowercase();
        assert!(!lower.contains("\r\ncookie:"), "{request}");
        assert!(!lower.contains("\r\nauthorization:"), "{request}");
        assert!(!lower.contains("\r\nreferer:"), "{request}");
    }
}

#[tokio::test]
async fn credentials_are_never_sent() {
    let server = HttpsServer::start(|port| {
        HashMap::from([
            ("/file", Answer::ok(b"x")),
            (
                "/away",
                Answer::redirect(format!("https://user:password@127.0.0.1:{port}/file")),
            ),
        ])
    })
    .await;
    let transport = server.transport();
    let address = format!("https://user:password@127.0.0.1:{}/file", server.port);
    let why = refused(&transport, &address).await;
    assert!(why.contains("credentials"), "{why}");
    assert!(!why.contains("password@"), "{why}");
    assert!(server.requests().is_empty());

    let why = refused(&transport, &server.address("/away")).await;
    assert!(!why.contains("password@"), "{why}");
    assert_eq!(server.requests().len(), 1, "the redirect was followed");
}

#[tokio::test]
async fn an_error_status_is_not_a_body() {
    let server = HttpsServer::start(|_| HashMap::new()).await;
    let why = refused(&server.transport(), &server.address("/missing")).await;
    assert!(why.contains("404"), "{why}");
}

#[tokio::test]
async fn endless_redirects_are_given_up_on() {
    let server =
        HttpsServer::start(|_| HashMap::from([("/loop", Answer::redirect("/loop".into()))])).await;
    let why = refused(&server.transport(), &server.address("/loop")).await;
    assert!(why.contains("redirected more than"), "{why}");
    // The first request and every redirect followed.
    assert_eq!(server.requests().len(), MAX_REDIRECTS);
}

/// A server whose certificate the system does not trust is not reached: the app's own
/// transport has no way to trust another.
#[tokio::test]
async fn an_untrusted_certificate_is_refused() {
    let server = HttpsServer::start(|_| HashMap::from([("/file", Answer::ok(b"x"))])).await;
    let transport = HttpsTransport::from_builder(Client::builder().no_proxy()).unwrap();
    refused(&transport, &server.address("/file")).await;
}

#[test]
fn the_app_builds_its_transport() {
    HttpsTransport::new().unwrap();
}

/// The app's own transport against GitHub: a pinned plugin's archive, through the release's
/// redirect, the system's certificates and proxies, with the pinned size and SHA-256. It needs
/// the network, so it runs only when asked: `cargo test -p spelaion-https -- --ignored`.
#[tokio::test]
#[ignore = "needs the network"]
async fn a_pinned_archive_comes_from_github() {
    use spelaion_core::plugins::pins::{Pins, platform, sha256_of};
    let platform = platform().expect("a platform plugins are built for");
    let (plugin, build) = Pins::shipped()
        .plugins()
        .iter()
        .find_map(|plugin| Some((plugin, plugin.platforms.get(platform)?)))
        .expect("a pinned plugin for this platform");
    let address = plugin.archive_address(build);
    let transport = HttpsTransport::new().unwrap();
    let mut body = transport.get(&address).await.unwrap();
    assert!(body.address().starts_with("https://"), "{}", body.address());
    let bytes = read_all(&mut body).await;
    assert_eq!(bytes.len() as u64, build.archive_size);
    let file = std::env::temp_dir().join(format!("spelaion-https-{}", std::process::id()));
    std::fs::write(&file, &bytes).unwrap();
    let sha256 = sha256_of(&file).unwrap();
    let _ = std::fs::remove_file(&file);
    assert_eq!(sha256, build.archive_sha256, "{address}");
}
