//! SSH tunnel dialogs in the UI: the core asks (trait `Interaction`), the app emits an event
//! and waits for the answer from `answer_interaction`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spelaion_config::SystemBackend;
use spelaion_driver_api::Secret;
use spelaion_ssh::{HostInfo, Interaction, KeyPassphraseAnswer, SecretRequest};
use tauri::AppHandle;
use tauri_specta::Event;
use tokio::sync::oneshot;

use crate::dto::{InteractionClosed, InteractionKind, InteractionRequest, SecretLabel};

/// Without an answer within this time, it is taken as "no".
const ANSWER_TIMEOUT: Duration = Duration::from_secs(300);

/// Whether the person accepted, the secret typed, and whether they asked to remember it (a key
/// passphrase, ADR-090).
type Answer = (bool, Option<Secret>, bool);

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

    pub fn answer(&self, id: u32, accept: bool, secret: Option<Secret>, remember: bool) {
        if let Some(tx) = self.waiting.lock().expect("lock poisoned").remove(&id) {
            let _ = tx.send((accept, secret, remember));
        }
    }

    /// Forgets a question nobody waits for; whether it was still unanswered.
    fn forget(&self, id: u32) -> bool {
        self.waiting
            .lock()
            .expect("lock poisoned")
            .remove(&id)
            .is_some()
    }
}

/// While a question is asked. Dropped before its answer (the test that asked was stopped, or
/// it waited too long), it tells the UI to close the dialog (ADR-032).
struct Asking<'a> {
    id: u32,
    pending: &'a Pending,
    interaction: &'a AppInteraction,
}

impl Drop for Asking<'_> {
    fn drop(&mut self) {
        if self.pending.forget(self.id) {
            let _ = self.interaction.send(&InteractionClosed { id: self.id });
        }
    }
}

pub struct AppInteraction {
    app: AppHandle,
    pending: Pending,
    /// The window whose page asked for what needs the answer (SDB-30, SDB-522): a connect or a
    /// test that page started, or a job started from one of its sessions. Only it shows the
    /// question; no question is ever sent to every window.
    window: String,
}

impl AppInteraction {
    /// Questions shown only in the window `label` (ADR-083).
    pub fn for_window(app: AppHandle, pending: Pending, label: &str) -> Self {
        Self {
            app,
            pending,
            window: label.to_owned(),
        }
    }

    fn send<E: Event + serde::Serialize + Clone>(&self, event: &E) -> tauri::Result<()> {
        event.emit_to(&self.app, self.window.as_str())
    }

    async fn ask(&self, request: InteractionKind) -> Answer {
        let (id, rx) = self.pending.register();
        let _asking = Asking {
            id,
            pending: &self.pending,
            interaction: self,
        };
        let event = InteractionRequest { id, request };
        if self.send(&event).is_err() {
            return (false, None, false);
        }
        match tokio::time::timeout(ANSWER_TIMEOUT, rx).await {
            Ok(Ok(answer)) => answer,
            _ => (false, None, false),
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
        self.ask_question(request).await.map(|(secret, _)| secret)
    }

    async fn ask_key_passphrase(&self, request: &SecretRequest) -> Option<KeyPassphraseAnswer> {
        self.ask_question(request)
            .await
            .map(|(passphrase, remember)| KeyPassphraseAnswer {
                passphrase,
                remember,
            })
    }

    /// A key's passphrase may be remembered where there is a system keychain (ADR-090); the
    /// core decides the entry's name and writes it (`spelaion-config`'s `secrets.rs`).
    fn passphrases_kept(&self) -> bool {
        use spelaion_config::SecretBackend as _;
        SystemBackend.keychain_available()
    }

    async fn remembered_passphrase(&self, key: &std::path::Path) -> Option<Secret> {
        spelaion_config::remembered_key_passphrase(&SystemBackend, key)
    }

    /// A keychain that refuses it only means it is asked again next time: the connection goes on.
    async fn remember_passphrase(&self, key: &std::path::Path, passphrase: &Secret) {
        let _ = spelaion_config::remember_key_passphrase(&SystemBackend, key, passphrase);
    }

    async fn forget_passphrase(&self, key: &std::path::Path) {
        let _ = spelaion_config::forget_key_passphrase(&SystemBackend, key);
    }
}

impl AppInteraction {
    /// Shows a question for a secret and waits for it: the secret, and whether the person asked
    /// to remember it (offered only for a key's passphrase that may be kept).
    async fn ask_question(&self, request: &SecretRequest) -> Option<(Secret, bool)> {
        let (title, prompt, echo, label) = match request {
            SecretRequest::KeyPassphrase {
                path,
                retry,
                can_remember,
                ..
            } => (
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
                    can_remember: *can_remember,
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
        let (accept, secret, remember) = self
            .ask(InteractionKind::Secret {
                title,
                prompt,
                echo,
                label,
            })
            .await;
        if accept {
            secret.map(|secret| (secret, remember))
        } else {
            None
        }
    }
}
