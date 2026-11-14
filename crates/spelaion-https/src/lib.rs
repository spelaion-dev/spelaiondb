//! The HTTPS transport the desktop app gives the core (ADR-055): what brings the plugin
//! catalogue and a plugin's archive to this machine.
//!
//! **It only moves bytes.** The core decides what is fetched, from where, how much of it and
//! what is done with it (`spelaion_core::plugins::fetch`); nothing here takes an address from
//! anywhere but the core's call, and nothing fetched is written anywhere by this crate.
//!
//! What the core asks of a transport, and how it is kept here:
//!
//! - **HTTPS only**, where it starts and after every redirect: the client is `https_only`, and
//!   its redirect policy refuses a hop to anything else before following it. An address that
//!   carries a user name or a password is refused too, first or after a redirect.
//! - **The user's proxies, as the updater uses them.** The same client Tauri's updater plugin
//!   brings (`reqwest`, rustls over the system's certificate store), with the system's proxy
//!   settings and the usual `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` variables.
//! - **Nothing of the user's is sent:** no cookie store (that feature of the client is never
//!   compiled), no credentials, no headers but the client's own and a user agent.
//! - **A connect timeout.** How long a body may go without a byte is the core's to decide.
//! - **[`Body::address`] is where the bytes finally come from**, after any redirect.

use std::time::Duration;

use reqwest::redirect::{Attempt, Policy};
use reqwest::{Client, ClientBuilder, Response, Url};
use spelaion_core::plugins::fetch::{Body, Transport, TransportError};

/// How long reaching a server may take before the request is given up on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The most redirects one request follows (GitHub's releases take one or two).
const MAX_REDIRECTS: usize = 10;

/// What the requests say they are.
const USER_AGENT: &str = concat!("SpelaionDB/", env!("CARGO_PKG_VERSION"));

/// The transport over HTTPS.
#[derive(Debug, Clone)]
pub struct HttpsTransport {
    client: Client,
}

impl HttpsTransport {
    /// The transport the app uses: the system's certificates and proxies.
    pub fn new() -> Result<Self, TransportError> {
        Self::from_builder(Client::builder())
    }

    /// Everything but where certificates and proxies come from.
    fn from_builder(builder: ClientBuilder) -> Result<Self, TransportError> {
        // What the updater does: rustls needs a crypto provider, and the first one installed
        // in the process is everyone's.
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let client = builder
            .https_only(true)
            .redirect(Policy::custom(follow_only_https))
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(USER_AGENT)
            .referer(false)
            .build()
            .map_err(|error| TransportError(describe(&error)))?;
        Ok(Self { client })
    }
}

/// The redirect policy: a hop is followed only to a plain `https://` address, and only so many.
fn follow_only_https(attempt: Attempt<'_>) -> reqwest::redirect::Action {
    if let Err(why) = acceptable(attempt.url()) {
        return attempt.error(why);
    }
    if attempt.previous().len() >= MAX_REDIRECTS {
        return attempt.error(format!("it redirected more than {MAX_REDIRECTS} times"));
    }
    attempt.follow()
}

/// Whether an address may be fetched: `https://`, with no user name or password in it.
fn acceptable(address: &Url) -> Result<(), String> {
    if address.scheme() != "https" {
        return Err(format!(
            "it was sent on to {}, which is not an https:// address",
            shown(address)
        ));
    }
    if !address.username().is_empty() || address.password().is_some() {
        return Err("the address carries credentials, which are never sent".to_string());
    }
    Ok(())
}

/// An address as it may be shown: without whatever credentials it carried.
fn shown(address: &Url) -> String {
    let mut address = address.clone();
    let _ = address.set_username("");
    let _ = address.set_password(None);
    address.to_string()
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

#[async_trait::async_trait]
impl Transport for HttpsTransport {
    async fn get(&self, address: &str) -> Result<Box<dyn Body>, TransportError> {
        let parsed = Url::parse(address).map_err(|error| TransportError(error.to_string()))?;
        acceptable(&parsed).map_err(TransportError)?;
        let response = self
            .client
            .get(parsed)
            .send()
            .await
            .map_err(|error| TransportError(describe(&error)))?;
        // Where it ended is checked again: the policy saw every hop, this is the last one.
        acceptable(response.url()).map_err(TransportError)?;
        let status = response.status();
        if !status.is_success() {
            return Err(TransportError(format!("the server answered {status}")));
        }
        Ok(Box::new(HttpsBody {
            address: response.url().to_string(),
            response,
        }))
    }
}

/// A response that is arriving.
struct HttpsBody {
    address: String,
    response: Response,
}

#[async_trait::async_trait]
impl Body for HttpsBody {
    fn address(&self) -> String {
        self.address.clone()
    }

    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        self.response
            .chunk()
            .await
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
            .map_err(|error| TransportError(describe(&error)))
    }
}

#[cfg(test)]
mod tests;
