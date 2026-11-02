//! Authentication of a hop with the configured methods, in order.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use spelunk_driver_api::Secret;

use crate::tunnel::HopHandler;
use crate::{AuthMethod, HopSpec, Interaction, SecretRequest, SshError};

/// Keys tried with `AuthMethod::Key` when none is given, like OpenSSH.
const DEFAULT_KEYS: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa"];

pub(crate) async fn authenticate(
    session: &mut Handle<HopHandler>,
    hop: usize,
    spec: &HopSpec,
    ui: &Arc<dyn Interaction>,
) -> Result<(), SshError> {
    let mut tried = Vec::new();
    for method in &spec.auth {
        let ok = match method {
            AuthMethod::Agent => {
                tried.push("agent".to_string());
                with_agent(session, hop, spec).await?
            }
            AuthMethod::Key => {
                tried.push("key".to_string());
                with_key(session, hop, spec, ui).await?
            }
            AuthMethod::Password => {
                tried.push("password".to_string());
                with_password(session, hop, spec, ui).await?
            }
            AuthMethod::KeyboardInteractive => {
                tried.push("keyboard-interactive".to_string());
                with_challenge(session, hop, spec, ui).await?
            }
        };
        if ok {
            return Ok(());
        }
    }
    Err(SshError::Auth {
        hop,
        host: spec.host.clone(),
        user: spec.user.clone(),
        tried,
    })
}

fn hop_error(hop: usize, spec: &HopSpec, message: impl std::fmt::Display) -> SshError {
    SshError::Hop {
        hop,
        host: spec.host.clone(),
        message: message.to_string(),
    }
}

fn accepted(result: &AuthResult) -> bool {
    result.success()
}

// ----------------------------------------------------------------------------- agent

/// Public key of `identity_file`, to filter the agent's keys.
fn public_key_of(identity: &Path) -> Option<PublicKey> {
    let mut pub_path = identity.as_os_str().to_owned();
    pub_path.push(".pub");
    russh::keys::load_public_key(PathBuf::from(pub_path))
        .ok()
        .or_else(|| {
            russh::keys::load_secret_key(identity, None)
                .ok()
                .map(|k| k.public_key().clone())
        })
}

async fn with_agent(
    session: &mut Handle<HopHandler>,
    hop: usize,
    spec: &HopSpec,
) -> Result<bool, SshError> {
    let only = spec.identity_file.as_deref().and_then(public_key_of);

    #[cfg(unix)]
    {
        match AgentClient::connect_env().await {
            Ok(agent) => agent_identities(session, hop, spec, agent, only.as_ref()).await,
            // No agent is not an error: move on to the next method.
            Err(_) => Ok(false),
        }
    }
    #[cfg(windows)]
    {
        // First the Windows OpenSSH agent, then Pageant.
        if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await
            && agent_identities(session, hop, spec, agent, only.as_ref()).await?
        {
            return Ok(true);
        }
        match AgentClient::connect_pageant().await {
            Ok(agent) => agent_identities(session, hop, spec, agent, only.as_ref()).await,
            Err(_) => Ok(false),
        }
    }
}

async fn agent_identities<S>(
    session: &mut Handle<HopHandler>,
    hop: usize,
    spec: &HopSpec,
    mut agent: AgentClient<S>,
    only: Option<&PublicKey>,
) -> Result<bool, SshError>
where
    S: AgentStream + Unpin + Send + 'static,
{
    let Ok(identities) = agent.request_identities().await else {
        return Ok(false);
    };
    let hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|e| hop_error(hop, spec, e))?
        .flatten();
    for identity in identities {
        if only.is_some_and(|key| identity.public_key().key_data() != key.key_data()) {
            continue;
        }
        let result = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                session
                    .authenticate_publickey_with(&spec.user, key, hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                session
                    .authenticate_certificate_with(&spec.user, certificate, hash, &mut agent)
                    .await
            }
        };
        match result {
            Ok(r) if accepted(&r) => return Ok(true),
            Ok(_) => {}
            Err(e) => {
                return Err(hop_error(
                    hop,
                    spec,
                    format!("signing with the agent: {e:?}"),
                ));
            }
        }
    }
    Ok(false)
}

// ------------------------------------------------------------------------------- key

fn key_candidates(spec: &HopSpec) -> Vec<PathBuf> {
    if let Some(path) = &spec.identity_file {
        return vec![path.clone()];
    }
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let Some(home) = std::env::var_os(var) else {
        return Vec::new();
    };
    let ssh = PathBuf::from(home).join(".ssh");
    DEFAULT_KEYS
        .iter()
        .map(|k| ssh.join(k))
        .filter(|p| p.is_file())
        .collect()
}

/// Loads a key, asking for the passphrase if it is encrypted (up to three attempts).
async fn load_key(
    path: &Path,
    hop: usize,
    spec: &HopSpec,
    ui: &Arc<dyn Interaction>,
) -> Result<PrivateKey, SshError> {
    match russh::keys::load_secret_key(path, None) {
        Ok(key) => return Ok(key),
        Err(russh::keys::Error::KeyIsEncrypted) => {}
        Err(e) => {
            return Err(hop_error(
                hop,
                spec,
                format!("reading {}: {e}", path.display()),
            ));
        }
    }
    for attempt in 0..3 {
        let request = SecretRequest::KeyPassphrase {
            hop,
            path: path.to_path_buf(),
            retry: attempt > 0,
        };
        let passphrase = ui.ask_secret(&request).await.ok_or(SshError::Cancelled)?;
        if let Ok(key) = russh::keys::load_secret_key(path, Some(passphrase.expose())) {
            return Ok(key);
        }
    }
    Err(hop_error(
        hop,
        spec,
        format!("wrong passphrase for {}", path.display()),
    ))
}

async fn with_key(
    session: &mut Handle<HopHandler>,
    hop: usize,
    spec: &HopSpec,
    ui: &Arc<dyn Interaction>,
) -> Result<bool, SshError> {
    for path in key_candidates(spec) {
        let key = Arc::new(load_key(&path, hop, spec, ui).await?);

        // User certificate next to the key (`id_ed25519-cert.pub`), like OpenSSH.
        let mut cert_path = path.as_os_str().to_owned();
        cert_path.push("-cert.pub");
        if let Ok(cert) = russh::keys::load_openssh_certificate(PathBuf::from(cert_path)) {
            let result = session
                .authenticate_openssh_cert(&spec.user, key.clone(), cert)
                .await
                .map_err(|e| hop_error(hop, spec, e))?;
            if accepted(&result) {
                return Ok(true);
            }
        }

        let hash = session
            .best_supported_rsa_hash()
            .await
            .map_err(|e| hop_error(hop, spec, e))?
            .flatten();
        let result = session
            .authenticate_publickey(&spec.user, PrivateKeyWithHashAlg::new(key, hash))
            .await
            .map_err(|e| hop_error(hop, spec, e))?;
        if accepted(&result) {
            return Ok(true);
        }
    }
    Ok(false)
}

// -------------------------------------------------------------------------- password

async fn with_password(
    session: &mut Handle<HopHandler>,
    hop: usize,
    spec: &HopSpec,
    ui: &Arc<dyn Interaction>,
) -> Result<bool, SshError> {
    let password: Secret = match &spec.password {
        Some(p) => p.clone(),
        None => ui
            .ask_secret(&SecretRequest::Password {
                hop,
                user: spec.user.clone(),
                host: spec.host.clone(),
            })
            .await
            .ok_or(SshError::Cancelled)?,
    };
    let result = session
        .authenticate_password(&spec.user, password.expose())
        .await
        .map_err(|e| hop_error(hop, spec, e))?;
    Ok(accepted(&result))
}

// -------------------------------------------------------------- keyboard-interactive

async fn with_challenge(
    session: &mut Handle<HopHandler>,
    hop: usize,
    spec: &HopSpec,
    ui: &Arc<dyn Interaction>,
) -> Result<bool, SshError> {
    let mut response = session
        .authenticate_keyboard_interactive_start(&spec.user, None::<String>)
        .await
        .map_err(|e| hop_error(hop, spec, e))?;
    // A server may chain several rounds (password and then a 2FA code).
    for _ in 0..10 {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest {
                instructions,
                prompts,
                ..
            } => {
                let mut answers = Vec::with_capacity(prompts.len());
                for prompt in prompts {
                    let request = SecretRequest::Challenge {
                        hop,
                        host: spec.host.clone(),
                        instructions: instructions.clone(),
                        prompt: prompt.prompt,
                        echo: prompt.echo,
                    };
                    answers.push(
                        ui.ask_secret(&request)
                            .await
                            .ok_or(SshError::Cancelled)?
                            .expose()
                            .to_string(),
                    );
                }
                response = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(|e| hop_error(hop, spec, e))?;
            }
        }
    }
    Ok(false)
}
