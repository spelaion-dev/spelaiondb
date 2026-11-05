//! SSH tunnel dialogs in the UI: the core asks (trait `Interaction`), the app emits an event
//! and waits for the answer from `answer_interaction`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spelaion_driver_api::Secret;
use spelaion_ssh::{HostInfo, Interaction, SecretRequest};
use tauri::AppHandle;
use tauri_specta::Event;
use tokio::sync::oneshot;

use crate::dto::{InteractionKind, InteractionRequest, SecretLabel};

/// Without an answer within this time, it is taken as "no".
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

type Answer = (bool, Option<Secret>);

/// Questions waiting for an answer from the UI.
#[derive(Clone, Default)]
pub struct Pending {
    next: Arc<AtomicU32>,
    waiting: Arc<Mutex<HashMap<u32, oneshot::Sender<Answer>>>>,
}

impl Pending {
    fn register(&self) -> (u32, oneshot::Receiver<Answer>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiting.lock().expect("lock poisoned").insert(id, tx);
        (id, rx)
    }

    pub fn answer(&self, id: u32, accept: bool, secret: Option<Secret>) {
        if let Some(tx) = self.waiting.lock().expect("lock poisoned").remove(&id) {
            let _ = tx.send((accept, secret));
        }
    }
}

pub struct AppInteraction {
    app: AppHandle,
    pending: Pending,
}

impl AppInteraction {
    pub fn new(app: AppHandle, pending: Pending) -> Self {
        Self { app, pending }
    }

    async fn ask(&self, request: InteractionKind) -> Answer {
        let (id, rx) = self.pending.register();
        let event = InteractionRequest { id, request };
        if event.emit(&self.app).is_err() {
            return (false, None);
        }
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Ok(Ok(answer)) => answer,
            _ => (false, None),
        }
    }
}

#[async_trait]
impl Interaction for AppInteraction {
    async fn confirm_unknown_host(&self, host: &HostInfo) -> bool {
        self.ask(InteractionKind::UnknownHost {
            hop: u32::try_from(host.hop).unwrap_or(u32::MAX) + 1,
            host: host.host.clone(),
            port: host.port.into(),
            algorithm: host.algorithm.clone(),
            fingerprint: host.fingerprint.clone(),
        })
        .await
        .0
    }

    async fn ask_secret(&self, request: &SecretRequest) -> Option<Secret> {
        let (title, prompt, echo, label) = match request {
            SecretRequest::KeyPassphrase { path, retry, .. } => (
                "SSH key passphrase".to_string(),
                format!(
                    "{}{}",
                    if *retry { "Wrong passphrase. " } else { "" },
                    path.display()
                ),
                false,
                SecretLabel::KeyPassphrase {
                    path: path.display().to_string(),
                    retry: *retry,
                },
            ),
            SecretRequest::Password { user, host, .. } => (
                "SSH password".to_string(),
                format!("{user}@{host}"),
                false,
                SecretLabel::Password {
                    user: user.clone(),
                    host: host.clone(),
                },
            ),
            SecretRequest::ProxyPassword { user, host } => (
                "Proxy password".to_string(),
                format!("{user}@{host}"),
                false,
                SecretLabel::ProxyPassword {
                    user: user.clone(),
                    host: host.clone(),
                },
            ),
            SecretRequest::Challenge {
                host,
                instructions,
                prompt,
                echo,
                ..
            } => {
                let title = if instructions.is_empty() {
                    format!("Verification for {host}")
                } else {
                    instructions.clone()
                };
                (
                    title,
                    prompt.clone(),
                    *echo,
                    SecretLabel::Challenge {
                        host: host.clone(),
                        instructions: instructions.clone(),
                    },
                )
            }
        };
        let (accept, secret) = self
            .ask(InteractionKind::Secret {
                title,
                prompt,
                echo,
                label,
            })
            .await;
        if accept { secret } else { None }
    }
}
