//! Authentication of a hop with the configured methods, in order.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use spelaion_driver_api::Secret;

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

/// Public key of `identity_file`, to filter the agent's keys: from the `.pub` next to it, from
/// the key itself when it is not encrypted, or from a PuTTY key's own public lines, which are
/// never encrypted (Pageant holds `.ppk` keys).
fn public_key_of(identity: &Path) -> Option<PublicKey> {
    let mut pub_path = identity.as_os_str().to_owned();
    pub_path.push(".pub");
    russh::keys::load_public_key(PathBuf::from(pub_path))
        .ok()
        .or_else(|| {
            let text = std::fs::read_to_string(identity).ok()?;
            russh::keys::decode_secret_key(&text, None)
                .ok()
                .map(|k| k.public_key().clone())
                .or_else(|| ppk_public_key(&text))
        })
}

/// Whether `text` is a key in PuTTY's format (`.ppk`, versions 2 and 3).
fn is_ppk(text: &str) -> bool {
    text.trim_start().starts_with("PuTTY-User-Key-File-")
}

/// The value of a PuTTY key's header line `name: value`, among the lines before its public key.
fn ppk_header<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines()
        .take_while(|line| !line.starts_with("Public-Lines:"))
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

/// Whether `text` is a PuTTY key encrypted with a passphrase. Reading one without its passphrase
/// fails with an error of the PPK reader, not `KeyIsEncrypted`: its `Encryption` header is what
/// says a passphrase is needed (SDB-528).
fn is_encrypted_ppk(text: &str) -> bool {
    is_ppk(text) && ppk_header(text, "Encryption").is_some_and(|value| value != "none")
}

/// A PuTTY key's public key, from its `Public-Lines`, which are stored unencrypted.
fn ppk_public_key(text: &str) -> Option<PublicKey> {
    if !is_ppk(text) {
        return None;
    }
    let mut lines = text.lines();
    let count: usize = lines
        .find_map(|line| line.strip_prefix("Public-Lines:"))?
        .trim()
        .parse()
        .ok()?;
    let base64: String = lines.take(count).map(str::trim).collect();
    russh::keys::parse_public_key_base64(&base64).ok()
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

/// Loads a key, asking for the passphrase if it is encrypted (up to three attempts): an
/// OpenSSH or PEM key that says so when read, or a PuTTY key whose header says so. The file is
/// read once.
async fn load_key(
    path: &Path,
    hop: usize,
    spec: &HopSpec,
    ui: &Arc<dyn Interaction>,
) -> Result<PrivateKey, SshError> {
    let reading = |e: &dyn std::fmt::Display| {
        hop_error(hop, spec, format!("reading {}: {e}", path.display()))
    };
    let text = std::fs::read_to_string(path).map_err(|e| reading(&e))?;
    match russh::keys::decode_secret_key(&text, None) {
        Ok(key) => return Ok(key),
        Err(russh::keys::Error::KeyIsEncrypted) => {}
        Err(_) if is_encrypted_ppk(&text) => {}
        Err(e) => return Err(reading(&e)),
    }
    // A passphrase remembered for this file (ADR-090) is tried before anyone is asked; one
    // that no longer decrypts it (the key changed) is forgotten, and the person is asked.
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut retry = false;
    if let Some(remembered) = ui.remembered_passphrase(&canonical).await {
        if let Ok(key) = russh::keys::decode_secret_key(&text, Some(remembered.expose())) {
            return Ok(key);
        }
        ui.forget_passphrase(&canonical).await;
        retry = true;
    }
    let can_remember = ui.passphrases_kept();
    for attempt in 0..3 {
        let request = SecretRequest::KeyPassphrase {
            hop,
            path: path.to_path_buf(),
            retry: retry || attempt > 0,
            can_remember,
        };
        let answer = ui
            .ask_key_passphrase(&request)
            .await
            .ok_or(SshError::Cancelled)?;
        if let Ok(key) = russh::keys::decode_secret_key(&text, Some(answer.passphrase.expose())) {
            // Only a passphrase that decrypted the key is remembered, and only when asked.
            if answer.remember && can_remember {
                ui.remember_passphrase(&canonical, &answer.passphrase).await;
            }
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::{HostInfo, KeyPassphraseAnswer};

    /// PuTTY keys made with puttygen 0.82 (SDB-528): one RSA and one Ed25519 key, each as PPK
    /// version 2 and version 3 (Argon2id), without and with the passphrase `spelaion-test`, and
    /// two of those with Windows line endings, as PuTTYgen on Windows writes them.
    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ppk")
            .join(name)
    }

    const PLAIN: &[(&str, &str)] = &[
        ("ppk_rsa_v2.ppk", "rsa"),
        ("ppk_rsa_v3.ppk", "rsa"),
        ("ppk_ed25519_v2.ppk", "ed25519"),
        ("ppk_ed25519_v3.ppk", "ed25519"),
    ];

    const ENCRYPTED: &[(&str, &str)] = &[
        ("ppk_rsa_v2_pass.ppk", "rsa"),
        ("ppk_rsa_v3_pass.ppk", "rsa"),
        ("ppk_ed25519_v2_pass.ppk", "ed25519"),
        ("ppk_ed25519_v3_pass.ppk", "ed25519"),
        ("ppk_rsa_v2_pass_crlf.ppk", "rsa"),
        ("ppk_ed25519_v3_pass_crlf.ppk", "ed25519"),
    ];

    /// The OpenSSH public key puttygen exported for `algorithm`'s key.
    fn exported(algorithm: &str) -> PublicKey {
        russh::keys::load_public_key(fixture(&format!("ppk_{algorithm}.pub"))).unwrap()
    }

    /// Answers each passphrase question in turn (`None` cancels), and records the questions.
    struct Answers {
        answers: Mutex<VecDeque<Option<&'static str>>>,
        asked: Mutex<Vec<SecretRequest>>,
    }

    impl Answers {
        fn new(answers: &[Option<&'static str>]) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.iter().copied().collect()),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn retries(&self) -> Vec<bool> {
            self.asked
                .lock()
                .unwrap()
                .iter()
                .map(|request| match request {
                    SecretRequest::KeyPassphrase { retry, .. } => *retry,
                    other => panic!("only a key's passphrase is asked: {other:?}"),
                })
                .collect()
        }
    }

    #[async_trait]
    impl Interaction for Answers {
        async fn confirm_unknown_host(&self, _host: &HostInfo) -> bool {
            panic!("no host is checked here")
        }

        async fn ask_secret(&self, request: &SecretRequest) -> Option<Secret> {
            self.asked.lock().unwrap().push(request.clone());
            let answer = self.answers.lock().unwrap().pop_front().flatten();
            answer.map(Secret::new)
        }
    }

    fn spec() -> HopSpec {
        HopSpec {
            host: "bastion".into(),
            port: 22,
            user: "spelaion".into(),
            auth: vec![AuthMethod::Key],
            identity_file: None,
            password: None,
            command: None,
        }
    }

    async fn load(name: &str, ui: &Arc<Answers>) -> Result<PrivateKey, SshError> {
        let ui: Arc<dyn Interaction> = ui.clone();
        load_key(&fixture(name), 0, &spec(), &ui).await
    }

    /// Keeps passphrases by key file, as the desktop app's keychain does (ADR-090), and answers
    /// each question in turn with a passphrase and whether to remember it.
    struct Keeper {
        kept: Mutex<std::collections::HashMap<PathBuf, String>>,
        answers: Mutex<VecDeque<(&'static str, bool)>>,
        asked: Mutex<Vec<SecretRequest>>,
    }

    impl Keeper {
        fn new(kept: &[(PathBuf, &str)], answers: &[(&'static str, bool)]) -> Arc<Self> {
            Arc::new(Self {
                kept: Mutex::new(
                    kept.iter()
                        .map(|(path, value)| (path.clone(), value.to_string()))
                        .collect(),
                ),
                answers: Mutex::new(answers.iter().copied().collect()),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn kept(&self, path: &Path) -> Option<String> {
            self.kept.lock().unwrap().get(path).cloned()
        }
    }

    #[async_trait]
    impl Interaction for Keeper {
        async fn confirm_unknown_host(&self, _host: &HostInfo) -> bool {
            panic!("no host is checked here")
        }

        async fn ask_secret(&self, _request: &SecretRequest) -> Option<Secret> {
            panic!("a key's passphrase is asked through ask_key_passphrase")
        }

        async fn ask_key_passphrase(&self, request: &SecretRequest) -> Option<KeyPassphraseAnswer> {
            self.asked.lock().unwrap().push(request.clone());
            let (passphrase, remember) = self.answers.lock().unwrap().pop_front()?;
            Some(KeyPassphraseAnswer {
                passphrase: Secret::new(passphrase.to_string()),
                remember,
            })
        }

        fn passphrases_kept(&self) -> bool {
            true
        }

        async fn remembered_passphrase(&self, key: &Path) -> Option<Secret> {
            self.kept(key).map(Secret::new)
        }

        async fn remember_passphrase(&self, key: &Path, passphrase: &Secret) {
            self.kept
                .lock()
                .unwrap()
                .insert(key.to_path_buf(), passphrase.expose().to_string());
        }

        async fn forget_passphrase(&self, key: &Path) {
            self.kept.lock().unwrap().remove(key);
        }
    }

    fn canonical(name: &str) -> PathBuf {
        std::fs::canonicalize(fixture(name)).unwrap()
    }

    async fn load_with(name: &str, ui: &Arc<Keeper>) -> Result<PrivateKey, SshError> {
        let ui: Arc<dyn Interaction> = ui.clone();
        load_key(&fixture(name), 0, &spec(), &ui).await
    }

    #[tokio::test]
    async fn a_remembered_passphrase_opens_the_key_without_asking() {
        let name = "ppk_ed25519_v3_pass.ppk";
        let ui = Keeper::new(&[(canonical(name), "spelaion-test")], &[]);
        let key = load_with(name, &ui).await.unwrap();
        assert_eq!(key.public_key().key_data(), exported("ed25519").key_data());
        assert!(ui.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_remembered_passphrase_that_no_longer_works_is_forgotten_and_asked() {
        let name = "ppk_rsa_v2_pass.ppk";
        let ui = Keeper::new(
            &[(canonical(name), "an old one")],
            &[("spelaion-test", false)],
        );
        load_with(name, &ui).await.unwrap();
        let asked = ui.asked.lock().unwrap().clone();
        assert!(
            matches!(
                asked.as_slice(),
                [SecretRequest::KeyPassphrase {
                    retry: true,
                    can_remember: true,
                    ..
                }]
            ),
            "{asked:?}"
        );
        assert_eq!(ui.kept(&canonical(name)), None, "dropped, and not ticked");
    }

    #[tokio::test]
    async fn only_a_passphrase_that_worked_is_remembered_and_only_when_asked() {
        let name = "ppk_ed25519_v2_pass.ppk";
        // Ticked on a wrong one, then on the right one: only the right one is kept.
        let ui = Keeper::new(&[], &[("wrong", true), ("spelaion-test", true)]);
        load_with(name, &ui).await.unwrap();
        assert_eq!(ui.kept(&canonical(name)).as_deref(), Some("spelaion-test"));
        // Not ticked: nothing is kept.
        let ui = Keeper::new(&[], &[("spelaion-test", false)]);
        load_with(name, &ui).await.unwrap();
        assert_eq!(ui.kept(&canonical(name)), None);
        // Three wrong ones, each ticked: nothing is kept.
        let ui = Keeper::new(&[], &[("a", true), ("b", true), ("c", true)]);
        assert!(load_with(name, &ui).await.is_err());
        assert_eq!(ui.kept(&canonical(name)), None);
    }

    #[tokio::test]
    async fn an_interaction_that_keeps_nothing_is_never_offered_to_remember() {
        let ui = Answers::new(&[Some("spelaion-test")]);
        load("ppk_rsa_v3_pass.ppk", &ui).await.unwrap();
        let asked = ui.asked.lock().unwrap().clone();
        assert!(
            matches!(
                asked.as_slice(),
                [SecretRequest::KeyPassphrase {
                    can_remember: false,
                    ..
                }]
            ),
            "{asked:?}"
        );
    }

    #[tokio::test]
    async fn putty_keys_without_a_passphrase_load_without_asking() {
        for (name, algorithm) in PLAIN {
            let ui = Answers::new(&[]);
            let key = load(name, &ui).await.unwrap();
            assert_eq!(
                key.public_key().key_data(),
                exported(algorithm).key_data(),
                "{name}"
            );
            assert!(ui.retries().is_empty(), "{name}");
        }
    }

    #[tokio::test]
    async fn putty_keys_with_a_passphrase_ask_for_it_again_after_a_wrong_one() {
        for (name, algorithm) in ENCRYPTED {
            let ui = Answers::new(&[Some("wrong"), Some("spelaion-test")]);
            let key = load(name, &ui).await.unwrap();
            assert_eq!(
                key.public_key().key_data(),
                exported(algorithm).key_data(),
                "{name}"
            );
            assert_eq!(ui.retries(), [false, true], "{name}");
        }
    }

    #[tokio::test]
    async fn three_wrong_passphrases_end_with_wrong_passphrase() {
        let ui = Answers::new(&[Some("a"), Some("b"), Some("c"), Some("spelaion-test")]);
        match load("ppk_ed25519_v3_pass.ppk", &ui).await {
            Err(SshError::Hop { message, .. }) => {
                assert!(message.starts_with("wrong passphrase for "), "{message}");
            }
            other => panic!("three attempts at most: {:?}", other.map(|_| ())),
        }
        assert_eq!(ui.retries(), [false, true, true]);
    }

    #[tokio::test]
    async fn cancelling_the_question_cancels() {
        let ui = Answers::new(&[None]);
        let result = load("ppk_rsa_v2_pass.ppk", &ui).await;
        assert!(
            matches!(result, Err(SshError::Cancelled)),
            "{:?}",
            result.map(|_| ())
        );
    }

    #[tokio::test]
    async fn a_file_that_is_no_key_says_so_without_asking() {
        let ui = Answers::new(&[Some("spelaion-test")]);
        match load("ppk_rsa.pub", &ui).await {
            Err(SshError::Hop { message, .. }) => {
                assert!(message.starts_with("reading "), "{message}")
            }
            other => panic!("not a private key: {:?}", other.map(|_| ())),
        }
        assert!(ui.retries().is_empty());
        // A PuTTY key whose header says it is not encrypted, but which cannot be read, is not
        // asked for a passphrase either.
        assert!(!is_encrypted_ppk(
            "PuTTY-User-Key-File-3: ssh-rsa\nEncryption: none\n"
        ));
        assert!(is_encrypted_ppk(
            "PuTTY-User-Key-File-2: ssh-rsa\r\nEncryption: aes256-cbc\r\n"
        ));
        assert!(!is_encrypted_ppk(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nEncryption: x\n"
        ));
    }

    #[test]
    fn the_agent_filter_reads_an_encrypted_putty_keys_public_key() {
        for (name, algorithm) in PLAIN.iter().chain(ENCRYPTED) {
            let key = public_key_of(&fixture(name)).unwrap_or_else(|| panic!("{name}"));
            assert_eq!(key.key_data(), exported(algorithm).key_data(), "{name}");
        }
    }
}
