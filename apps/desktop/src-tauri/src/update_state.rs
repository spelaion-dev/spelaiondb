//! The app update's lifecycle (SDB-377, ADR-060), held by the adapter: one state, changed only
//! by the methods here, and shown to the page as one generated type. Checking, verifying and
//! installing stay in `update.rs` (ADR-021); this keeps what the last check found, so a
//! download that fails can be installed again without checking first, and how far a download
//! is.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::update::UpdateMethod;

/// What a check found, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
}

/// Where an update this copy installs itself stands.
#[derive(Debug, Clone, PartialEq)]
enum Install {
    Ready,
    Downloading {
        received: u64,
        total: Option<u64>,
    },
    /// The download or the install failed: it can be tried again.
    Failed {
        reason: String,
    },
}

/// The update's state. `U` is what installs it (the updater plugin's `Update`; a test's own).
#[derive(Debug, Clone, PartialEq)]
enum Phase<U> {
    /// Not checked in this run.
    Unknown,
    /// Updates are switched off in this run (`SPELAION_NO_UPDATES`).
    Unavailable,
    UpToDate,
    /// A new version this copy installs itself.
    Available {
        update: U,
        found: Found,
        install: Install,
    },
    /// A new version this copy does not install itself: how it is installed instead.
    NotInApp {
        found: Found,
        method: UpdateMethod,
    },
}

/// What the page shows about updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UpdateStatus {
    /// Not checked yet in this run.
    Unknown,
    /// Updates are switched off in this run (`SPELAION_NO_UPDATES`).
    Unavailable,
    UpToDate,
    #[serde(rename_all = "camelCase")]
    Available {
        version: String,
        current_version: String,
        notes: Option<String>,
        /// How this copy gets it; only `inApp` is installed by the app.
        method: UpdateMethod,
        /// Where installing it stands; always `ready` unless `method` is `inApp`.
        install: InstallStatus,
    },
}

/// Where installing an update stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum InstallStatus {
    /// Not started: "Install and restart" downloads it.
    Ready,
    /// Downloading: bytes received, of the total when the server said it.
    Downloading { received: f64, total: Option<f64> },
    /// The download or the install failed, and why; installing again retries it.
    Failed { reason: String },
}

/// The update's state changed outside a command's answer: a download started, moved, or
/// failed. Sent a few times a second at most.
#[derive(Debug, Clone, Serialize, Deserialize, Type, tauri_specta::Event)]
#[serde(rename_all = "camelCase")]
pub struct UpdateChanged {
    pub status: UpdateStatus,
}

/// The update's state, for every command and the download's progress.
pub struct UpdateState<U> {
    phase: Mutex<Phase<U>>,
    /// When the last progress was sent: a few times a second at most.
    last_progress: Mutex<Option<Instant>>,
}

impl<U> Default for UpdateState<U> {
    fn default() -> Self {
        Self {
            phase: Mutex::new(Phase::Unknown),
            last_progress: Mutex::new(None),
        }
    }
}

/// How often a download's progress reaches the page at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

impl<U: Clone> UpdateState<U> {
    fn phase(&self) -> std::sync::MutexGuard<'_, Phase<U>> {
        // A panic while the lock was held leaves the phase as it was set: still one state.
        self.phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What the page shows now.
    pub fn status(&self) -> UpdateStatus {
        status_of(&self.phase())
    }

    /// Updates are switched off in this run.
    pub fn switched_off(&self) -> UpdateStatus {
        self.set(Phase::Unavailable)
    }

    /// Whether a download is running: a check then answers what is known instead of starting.
    pub fn downloading(&self) -> bool {
        match &*self.phase() {
            Phase::Available {
                install: Install::Downloading { .. },
                ..
            } => true,
            Phase::Unknown
            | Phase::Unavailable
            | Phase::UpToDate
            | Phase::Available { .. }
            | Phase::NotInApp { .. } => false,
        }
    }

    /// What a check found: nothing, an update this copy installs (`InApp`), or one it does not.
    pub fn checked(&self, found: Option<(U, Found)>, method: UpdateMethod) -> UpdateStatus {
        self.set(match found {
            None => Phase::UpToDate,
            Some((update, found)) => match method {
                UpdateMethod::InApp => Phase::Available {
                    update,
                    found,
                    install: Install::Ready,
                },
                UpdateMethod::Deb | UpdateMethod::Rpm | UpdateMethod::ReleasesPage => {
                    Phase::NotInApp { found, method }
                }
            },
        })
    }

    /// Starts downloading the update found: from `Ready` or after a failure, never twice at
    /// once. Returns what installs it.
    pub fn start_download(&self) -> Result<U, String> {
        let mut phase = self.phase();
        match &mut *phase {
            Phase::Available {
                install: Install::Downloading { .. },
                ..
            } => Err("the update is already downloading".into()),
            Phase::Available {
                update, install, ..
            } => {
                *install = Install::Downloading {
                    received: 0,
                    total: None,
                };
                Ok(update.clone())
            }
            Phase::Unknown | Phase::Unavailable | Phase::UpToDate | Phase::NotInApp { .. } => {
                Err("there is no update to install: check for updates first".into())
            }
        }
    }

    /// A chunk of the download arrived. Returns the status to send when it is time to tell the
    /// page (the first chunk, then a few times a second).
    pub fn received(&self, chunk: usize, total: Option<u64>) -> Option<UpdateStatus> {
        {
            let mut phase = self.phase();
            if let Phase::Available {
                install:
                    Install::Downloading {
                        received,
                        total: known,
                    },
                ..
            } = &mut *phase
            {
                *received = received.saturating_add(chunk as u64);
                *known = total.or(*known);
            } else {
                return None;
            }
        }
        let mut last = self
            .last_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        if last.is_some_and(|at| now.duration_since(at) < PROGRESS_EVERY) {
            return None;
        }
        *last = Some(now);
        Some(self.status())
    }

    /// The download or the install failed: the update stays, to be installed again.
    pub fn failed(&self, reason: String) -> UpdateStatus {
        let mut phase = self.phase();
        if let Phase::Available { install, .. } = &mut *phase {
            *install = Install::Failed { reason };
        }
        status_of(&phase)
    }

    fn set(&self, next: Phase<U>) -> UpdateStatus {
        let mut phase = self.phase();
        *phase = next;
        status_of(&phase)
    }
}

fn status_of<U>(phase: &Phase<U>) -> UpdateStatus {
    let available = |found: &Found, method, install| UpdateStatus::Available {
        version: found.version.clone(),
        current_version: found.current_version.clone(),
        notes: found.notes.clone(),
        method,
        install,
    };
    match phase {
        Phase::Unknown => UpdateStatus::Unknown,
        Phase::Unavailable => UpdateStatus::Unavailable,
        Phase::UpToDate => UpdateStatus::UpToDate,
        Phase::Available { found, install, .. } => available(
            found,
            UpdateMethod::InApp,
            match install {
                Install::Ready => InstallStatus::Ready,
                Install::Downloading { received, total } => InstallStatus::Downloading {
                    received: *received as f64,
                    total: total.map(|t| t as f64),
                },
                Install::Failed { reason } => InstallStatus::Failed {
                    reason: reason.clone(),
                },
            },
        ),
        Phase::NotInApp { found, method } => available(found, *method, InstallStatus::Ready),
    }
}

/// What a download calls with each chunk's size, and the total when the server said it.
pub type Progress<'a> = Box<dyn FnMut(usize, Option<u64>) + Send + 'a>;

/// Downloads and installs the update found, keeping it when that fails: `download` gets what
/// installs it and the [`Progress`] to call; `changed` is called with each status worth telling
/// the page.
pub async fn install<'a, U, C, D, F>(
    state: &'a UpdateState<U>,
    download: D,
    changed: &'a C,
) -> Result<(), String>
where
    U: Clone + Send + Sync + 'a,
    C: Fn(UpdateStatus) + Send + Sync,
    D: FnOnce(U, Progress<'a>) -> F,
    F: std::future::Future<Output = Result<(), String>>,
{
    let update = state.start_download()?;
    changed(state.status());
    let progress: Progress<'a> = Box::new(move |chunk: usize, total: Option<u64>| {
        if let Some(status) = state.received(chunk, total) {
            changed(status);
        }
    });
    match download(update, progress).await {
        Ok(()) => Ok(()),
        Err(reason) => {
            changed(state.failed(reason.clone()));
            Err(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn found() -> Found {
        Found {
            version: "0.13.0".into(),
            current_version: "0.12.0".into(),
            notes: Some("notes".into()),
        }
    }

    /// A local server whose first request fails and whose later ones send `body`.
    async fn fails_once(body: &'static [u8]) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                let response = if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n".to_vec()
                } else {
                    let mut ok =
                        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len())
                            .into_bytes();
                    ok.extend_from_slice(body);
                    ok
                };
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });
        (url, requests)
    }

    /// Downloads `address` over plain HTTP in chunks, as the updater plugin reports them.
    async fn download(address: String, mut progress: Progress<'_>) -> Result<(), String> {
        let mut socket = TcpStream::connect(&address)
            .await
            .map_err(|e| e.to_string())?;
        socket
            .write_all(b"GET /update HTTP/1.1\r\nhost: test\r\nconnection: close\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        let mut response = Vec::new();
        socket
            .read_to_end(&mut response)
            .await
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&response);
        let (head, body) = text.split_once("\r\n\r\n").ok_or("no response")?;
        if !head.starts_with("HTTP/1.1 200") {
            return Err(format!(
                "Download request failed with status: {}",
                head.lines().next().unwrap_or_default()
            ));
        }
        let total = body.len() as u64;
        for chunk in body.as_bytes().chunks(4) {
            progress(chunk.len(), Some(total));
        }
        Ok(())
    }

    /// A download that fails is installed again from the same update, with no new check: before,
    /// the update was taken out before downloading, and Install answered "check for updates
    /// first" (SDB-377).
    #[tokio::test]
    async fn a_failed_download_can_be_installed_again_without_a_new_check() {
        let (address, requests) = fails_once(b"the signed installer").await;
        let state = UpdateState::default();
        state.checked(Some((address, found())), UpdateMethod::InApp);
        let seen = Mutex::new(Vec::new());
        let changed = |status: UpdateStatus| seen.lock().unwrap().push(status);

        let first = install(&state, download, &changed).await;
        let reason = first.unwrap_err();
        assert!(reason.contains("503"), "{reason}");
        match state.status() {
            UpdateStatus::Available {
                install: InstallStatus::Failed { reason: shown },
                version,
                ..
            } => {
                assert_eq!(shown, reason);
                assert_eq!(version, "0.13.0");
            }
            other => panic!("{other:?}"),
        }

        install(&state, download, &changed).await.unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2, "no check in between");
        match state.status() {
            UpdateStatus::Available {
                install: InstallStatus::Downloading { received, total },
                ..
            } => {
                assert_eq!(received, 20.0);
                assert_eq!(total, Some(20.0));
            }
            other => panic!("{other:?}"),
        }
        let seen = seen.into_inner().unwrap();
        assert!(
            matches!(
                seen.first(),
                Some(UpdateStatus::Available {
                    install: InstallStatus::Downloading { received, .. },
                    ..
                }) if *received == 0.0
            ),
            "the page hears the download start: {seen:?}"
        );
        assert!(
            seen.iter().any(|s| matches!(
                s,
                UpdateStatus::Available {
                    install: InstallStatus::Failed { .. },
                    ..
                }
            )),
            "and the failure: {seen:?}"
        );
    }

    #[test]
    fn only_an_update_this_copy_installs_can_be_installed() {
        let state = UpdateState::<u8>::default();
        assert_eq!(state.status(), UpdateStatus::Unknown);
        assert!(
            state
                .start_download()
                .unwrap_err()
                .contains("check for updates first")
        );
        state.checked(None, UpdateMethod::InApp);
        assert_eq!(state.status(), UpdateStatus::UpToDate);
        assert!(state.start_download().is_err());
        let status = state.checked(Some((1, found())), UpdateMethod::Deb);
        assert!(matches!(
            status,
            UpdateStatus::Available {
                method: UpdateMethod::Deb,
                install: InstallStatus::Ready,
                ..
            }
        ));
        assert!(state.start_download().is_err());
        assert_eq!(state.switched_off(), UpdateStatus::Unavailable);
    }

    #[test]
    fn a_download_runs_once_at_a_time_and_its_progress_is_counted() {
        let state = UpdateState::<u8>::default();
        state.checked(Some((7, found())), UpdateMethod::InApp);
        assert!(!state.downloading());
        assert_eq!(state.start_download(), Ok(7));
        assert!(state.downloading());
        assert!(
            state
                .start_download()
                .unwrap_err()
                .contains("already downloading")
        );
        // The first chunk is told at once; the next ones a few times a second at most.
        assert!(state.received(10, None).is_some());
        assert!(state.received(10, Some(100)).is_none());
        match state.status() {
            UpdateStatus::Available {
                install: InstallStatus::Downloading { received, total },
                ..
            } => assert_eq!((received, total), (20.0, Some(100.0))),
            other => panic!("{other:?}"),
        }
        state.failed("the signature does not match".into());
        assert!(!state.downloading());
        assert_eq!(
            state.start_download(),
            Ok(7),
            "installed again after a failure"
        );
    }

    #[test]
    fn the_status_reaches_the_web_view_in_camel_case() {
        let state = UpdateState::<u8>::default();
        state.checked(Some((7, found())), UpdateMethod::InApp);
        state.start_download().unwrap();
        state.received(5, Some(10));
        let json = serde_json::to_value(state.status()).unwrap();
        assert_eq!(json["kind"], "available");
        assert_eq!(json["currentVersion"], "0.12.0");
        assert_eq!(json["method"], "inApp");
        assert_eq!(json["install"]["kind"], "downloading");
        assert_eq!(json["install"]["received"], 5.0);
        assert_eq!(json["install"]["total"], 10.0);
        assert_eq!(
            serde_json::to_value(UpdateStatus::Unknown).unwrap()["kind"],
            "unknown"
        );
    }
}
