//! Bringing the plugin catalogue and a plugin's archive to this machine (ADR-055).
//!
//! The core has no HTTP client: the app's adapter brings one, as a [`Transport`]. **The
//! adapter only moves bytes.** Where they come from, how many there may be and what is done
//! with them is decided here:
//!
//! - **Only from where the core says.** The catalogue has one address, a constant
//!   ([`CATALOGUE_ADDRESS`]); an archive's is written by the core from the checked catalogue
//!   entry (`PinnedPlugin::archive_address`). No setting, variable, link, repository file or
//!   agent names an address: no function here takes one.
//! - **Only over HTTPS**, where it starts and where it ends after any redirect.
//! - **Bounded.** A catalogue stops at the largest a signed catalogue can be; an archive at
//!   exactly the size its entry gives, and one byte short is refused too. A body that stops
//!   arriving is given up on.
//! - **Nothing fetched is trusted.** A catalogue goes through
//!   [`PluginStore::accept_signed_catalogue`] (signature, serial); an archive through
//!   [`PluginStore::install_as`] (its hashes, what it holds). An archive is written to a file
//!   of its own that is removed whatever happens, also when the download is dropped half way.
//! - **Only when asked.** The catalogue is read when the app checks for its own updates, under
//!   the same personal setting, and when the Plugins page opens; `SPELAION_NO_UPDATES` switches
//!   that off and can do nothing else ([`updates_allowed`]). An archive is fetched on the
//!   user's click, never by itself. `spelaiondb mcp` never calls any of this.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::catalogue::{Catalogue, CatalogueError, CatalogueKey, MAX_CATALOGUE_BYTES};
use super::choice::states;
use super::installed::{AppBuild, InstallError, InstalledBuild, PluginStore};
use super::pins::{Pins, version_of};

/// Where the catalogue is published: one file, its signature and then its text, an asset of a
/// release of this repository that is never the latest one (the updater reads that one).
pub const CATALOGUE_ADDRESS: &str = "https://github.com/spelaion-dev/spelaiondb/releases/download/plugins-catalogue/plugins-catalogue.signed";

/// The variable that switches the app's update checks off, and with them the catalogue's.
pub const NO_UPDATES: &str = "SPELAION_NO_UPDATES";

/// How long a download may go without a byte before it is given up on.
const STALLED_AFTER: Duration = Duration::from_secs(60);

/// Whether the catalogue may be read by itself now: the user's "check for updates" setting
/// (ADR-021), unless `SPELAION_NO_UPDATES` (whose value is `no_updates`) is set to anything.
/// The variable can only switch it off.
pub fn updates_allowed(setting: bool, no_updates: Option<OsString>) -> bool {
    setting && no_updates.is_none()
}

/// Why a transport could not bring something, as the user reads it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(pub String);

/// What brings bytes over the network: the adapter's HTTP client.
///
/// What is asked of it: follow a redirect only to another `https://` address; go through the
/// proxies the user configured; send nothing of the user's (no cookies, no credentials); put
/// nothing of what it fetched anywhere itself.
#[async_trait::async_trait]
pub trait Transport: Send + Sync {
    /// Starts fetching `address`, which is always `https://`.
    async fn get(&self, address: &str) -> Result<Box<dyn Body>, TransportError>;
}

/// A response that is arriving.
#[async_trait::async_trait]
pub trait Body: Send {
    /// Where it finally came from, after any redirect.
    fn address(&self) -> String;
    /// Its next piece; `None` at its end.
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError>;
}

/// Why something was not fetched, or not kept.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("update checks are switched off, and the plugin catalogue is read with them")]
    SwitchedOff,
    #[error("{what} could not be reached at {address}: {why}")]
    Unreachable {
        what: &'static str,
        address: String,
        why: String,
    },
    #[error("{what} was not fetched: {address} is not an https:// address")]
    NotHttps { what: &'static str, address: String },
    #[error("{what} is larger than the {most} bytes it may have: the download was stopped")]
    TooLarge { what: &'static str, most: u64 },
    #[error("{what} ended after {found} bytes of {expected}")]
    Short {
        what: &'static str,
        expected: u64,
        found: u64,
    },
    #[error("{what} stopped arriving: the download was given up on")]
    Stalled { what: &'static str },
    #[error("the plugin catalogue offers no build of {0} for this computer and this SpelaionDB")]
    NotOffered(String),
    #[error(transparent)]
    Catalogue(#[from] CatalogueError),
    #[error(transparent)]
    Install(#[from] InstallError),
    #[error("{what}: {error}")]
    Io { what: String, error: String },
}

/// Fetches `address` into `sink`, at most `most` bytes of it. The number of bytes fetched.
async fn download(
    transport: &dyn Transport,
    what: &'static str,
    address: &str,
    most: u64,
    sink: &mut dyn Write,
) -> Result<u64, FetchError> {
    let not_https = |address: &str| FetchError::NotHttps {
        what,
        address: address.to_string(),
    };
    if !is_https(address) {
        return Err(not_https(address));
    }
    let unreachable = |error: TransportError| FetchError::Unreachable {
        what,
        address: address.to_string(),
        why: error.0,
    };
    let mut body = tokio::time::timeout(STALLED_AFTER, transport.get(address))
        .await
        .map_err(|_| FetchError::Stalled { what })?
        .map_err(unreachable)?;
    // Where the bytes really come from, before one of them is kept.
    let from = body.address();
    if !is_https(&from) {
        return Err(not_https(&from));
    }
    let mut fetched = 0u64;
    loop {
        let chunk = tokio::time::timeout(STALLED_AFTER, body.chunk())
            .await
            .map_err(|_| FetchError::Stalled { what })?
            .map_err(unreachable)?;
        let Some(chunk) = chunk else {
            return Ok(fetched);
        };
        fetched = fetched.saturating_add(chunk.len() as u64);
        if fetched > most {
            return Err(FetchError::TooLarge { what, most });
        }
        sink.write_all(&chunk).map_err(|error| FetchError::Io {
            what: format!("keeping {what}"),
            error: error.to_string(),
        })?;
    }
}

/// An address the core fetches from: `https://` and a host, nothing odd in it.
fn is_https(address: &str) -> bool {
    address
        .strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
        && !address.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Reads the published catalogue and accepts it into `store`, if `updates` allows reading it
/// ([`updates_allowed`]) and it is one this build's key signed, not older than the one kept.
pub async fn refresh_catalogue(
    store: &PluginStore,
    transport: &dyn Transport,
    updates: bool,
) -> Result<Catalogue, FetchError> {
    let key = CatalogueKey::shipped().ok_or(CatalogueError::NoKey)?;
    refresh_catalogue_with(&key, store, transport, updates).await
}

/// [`refresh_catalogue`] with `key` in place of the one compiled in (the tests').
pub async fn refresh_catalogue_with(
    key: &CatalogueKey,
    store: &PluginStore,
    transport: &dyn Transport,
    updates: bool,
) -> Result<Catalogue, FetchError> {
    if !updates {
        return Err(FetchError::SwitchedOff);
    }
    let most = (MAX_CATALOGUE_BYTES + 8192) as u64;
    let mut signed = Vec::new();
    download(
        transport,
        "the plugin catalogue",
        CATALOGUE_ADDRESS,
        most,
        &mut signed,
    )
    .await?;
    Ok(store.accept_signed_catalogue_with(key, &signed)?)
}

/// Fetches and installs what `catalogue` offers of the plugin `name` for this computer and
/// this SpelaionDB: its newest usable build. **The user's click**: nothing calls this by
/// itself. The archive comes from the address the core writes for that entry, must have
/// exactly the entry's size, and is installed only as that entry
/// ([`PluginStore::install_as`]). Dropping this future stops the download and leaves nothing.
pub async fn install_offered(
    store: &PluginStore,
    transport: &dyn Transport,
    catalogue: &Catalogue,
    app: &AppBuild,
    name: &str,
) -> Result<InstalledBuild, FetchError> {
    // What is offered is decided where it is decided for the page: one rule.
    let offered = states(&Pins::none(), None, None, Some(catalogue), app)
        .into_iter()
        .find(|state| state.name == name)
        .and_then(|state| state.offered)
        .ok_or_else(|| FetchError::NotOffered(name.to_string()))?;
    let version =
        version_of(&offered.version).ok_or_else(|| FetchError::NotOffered(name.to_string()))?;

    let incoming = Incoming::new(store).map_err(|error| FetchError::Io {
        what: "making room for the download".into(),
        error: error.to_string(),
    })?;
    let what = "the plugin's archive";
    let mut file = std::fs::File::create(&incoming.path).map_err(|error| FetchError::Io {
        what: format!("keeping {what}"),
        error: error.to_string(),
    })?;
    let fetched = download(
        transport,
        what,
        &offered.address,
        offered.archive_size,
        &mut file,
    )
    .await?;
    if fetched != offered.archive_size {
        return Err(FetchError::Short {
            what,
            expected: offered.archive_size,
            found: fetched,
        });
    }
    file.sync_all().map_err(|error| FetchError::Io {
        what: format!("keeping {what}"),
        error: error.to_string(),
    })?;
    drop(file);
    Ok(store.install_as(catalogue, app, name, version, &incoming.path)?)
}

/// A download's file in the store, removed when this value goes: checked or not, finished or
/// dropped half way.
struct Incoming {
    path: PathBuf,
}

impl Incoming {
    fn new(store: &PluginStore) -> std::io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let folder = store.folder().join("downloads");
        std::fs::create_dir_all(&folder)?;
        Ok(Self {
            path: folder.join(format!(
                "fetching-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )),
        })
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport that never answers, or a body that never continues.
    struct Silent {
        answers: bool,
    }

    struct SilentBody;

    #[async_trait::async_trait]
    impl Transport for Silent {
        async fn get(&self, _address: &str) -> Result<Box<dyn Body>, TransportError> {
            if !self.answers {
                std::future::pending::<()>().await;
            }
            Ok(Box::new(SilentBody))
        }
    }

    #[async_trait::async_trait]
    impl Body for SilentBody {
        fn address(&self) -> String {
            CATALOGUE_ADDRESS.to_string()
        }

        async fn chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            std::future::pending().await
        }
    }

    /// A download that stops arriving does not hang the page for ever.
    #[tokio::test(start_paused = true)]
    async fn a_download_that_stops_arriving_is_given_up_on() {
        for answers in [false, true] {
            let mut sink = Vec::new();
            let stalled = download(
                &Silent { answers },
                "the plugin catalogue",
                CATALOGUE_ADDRESS,
                10,
                &mut sink,
            )
            .await;
            assert!(
                matches!(stalled, Err(FetchError::Stalled { .. })),
                "{stalled:?}"
            );
            assert!(sink.is_empty());
        }
    }

    #[test]
    fn the_variable_can_only_switch_the_catalogue_off() {
        assert!(updates_allowed(true, None));
        // Set to anything, even to nothing or to "0": off, as the app's updates are.
        for value in ["1", "0", "", "false", "https://evil.example/catalogue"] {
            assert!(
                !updates_allowed(true, Some(OsString::from(value))),
                "{value:?}"
            );
            assert!(!updates_allowed(false, Some(OsString::from(value))));
        }
        // It never switches on what the user switched off.
        assert!(!updates_allowed(false, None));
    }

    #[test]
    fn the_core_fetches_only_from_https_addresses() {
        assert!(is_https(CATALOGUE_ADDRESS));
        for bad in [
            "http://github.com/spelaion-dev/spelaiondb/releases/download/x/y",
            "file:///etc/passwd",
            "https://",
            "https:///etc/passwd",
            "HTTPS://github.com/x",
            "ftp://github.com/x",
            "https://github.com/x y",
            "https://github.com/x\n",
            " https://github.com/x",
            "",
        ] {
            assert!(!is_https(bad), "{bad:?}");
        }
    }

    /// The catalogue's address is this repository's, and cannot be the latest release's (the
    /// updater's).
    #[test]
    fn the_catalogue_has_one_address() {
        assert!(CATALOGUE_ADDRESS.starts_with(
            "https://github.com/spelaion-dev/spelaiondb/releases/download/plugins-catalogue/"
        ));
        assert!(!CATALOGUE_ADDRESS.contains("/latest/"));
    }
}
