//! Verification of each server's key against `known_hosts`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};

use crate::{HostInfo, Interaction, SshError};

/// Where fingerprints are looked up and stored.
#[derive(Debug, Clone)]
pub struct KnownHosts {
    /// The user's `~/.ssh/known_hosts`: read, never written.
    pub user_file: Option<PathBuf>,
    /// SpelunkDB's own file, where accepted fingerprints are stored.
    pub app_file: PathBuf,
}

impl KnownHosts {
    /// `~/.ssh/known_hosts` + `<app_dir>/known_hosts`.
    pub fn for_user(app_dir: &Path) -> Self {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let user_file =
            std::env::var_os(var).map(|home| PathBuf::from(home).join(".ssh").join("known_hosts"));
        Self {
            user_file,
            app_file: app_dir.join("known_hosts"),
        }
    }

    fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.user_file
            .iter()
            .chain(std::iter::once(&self.app_file))
            .filter(|p| p.is_file())
    }
}

/// Checks a hop's key. Returns `true` if it is accepted; otherwise leaves the reason in
/// `outcome` (russh only accepts a boolean).
pub(crate) async fn verify(
    known: &KnownHosts,
    ui: &Arc<dyn Interaction>,
    hop: usize,
    host: &str,
    port: u16,
    key: &PublicKeyOrCertificate,
    outcome: &Mutex<Option<SshError>>,
) -> bool {
    // With a host certificate, the public key it certifies is checked. Host authorities
    // (`@cert-authority`) are not supported yet.
    let key: PublicKey = match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
        PublicKeyOrCertificate::Certificate(cert) => PublicKey::new(cert.public_key().clone(), ""),
    };
    for file in known.files() {
        match russh::keys::check_known_hosts_path(host, port, &key, file) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(russh::keys::Error::KeyChanged { line }) => {
                *outcome.lock().expect("lock poisoned") = Some(SshError::HostKeyChanged {
                    hop,
                    host: host.to_string(),
                    file: file.clone(),
                    line,
                });
                return false;
            }
            // An unreadable known_hosts must not block: move on to the next one.
            Err(_) => {}
        }
    }

    let info = HostInfo {
        hop,
        host: host.to_string(),
        port,
        algorithm: key.algorithm().to_string(),
        fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
    };
    if !ui.confirm_unknown_host(&info).await {
        *outcome.lock().expect("lock poisoned") = Some(SshError::HostKeyRejected {
            hop,
            host: host.to_string(),
        });
        return false;
    }
    if let Some(parent) = known.app_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) =
        russh::keys::known_hosts::learn_known_hosts_path(host, port, &key, &known.app_file)
    {
        *outcome.lock().expect("lock poisoned") = Some(SshError::Hop {
            hop,
            host: host.to_string(),
            message: format!(
                "could not store the fingerprint in {}: {e}",
                known.app_file.display()
            ),
        });
        return false;
    }
    true
}
