//! Verification of each server's key against `known_hosts`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use data_encoding::BASE64;
use ring::hmac;
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};

use crate::{HostInfo, Interaction, SshError};

/// Where fingerprints are looked up and stored.
#[derive(Debug, Clone)]
pub struct KnownHosts {
    /// The team's `.spelaion/known_hosts`, reviewed in git (ADR-022): looked up first, so a
    /// newcomer's first connection is already verified. Read, never written. The caller
    /// passes it only from a trusted repository and only as the user trusted it.
    pub team_file: Option<PathBuf>,
    /// The user's `~/.ssh/known_hosts`: read, never written.
    pub user_file: Option<PathBuf>,
    /// SpelaionDB's own file, where accepted fingerprints are stored.
    pub app_file: PathBuf,
}

impl KnownHosts {
    /// `~/.ssh/known_hosts` + `<app_dir>/known_hosts`.
    pub fn for_user(app_dir: &Path) -> Self {
        let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let user_file =
            std::env::var_os(var).map(|home| PathBuf::from(home).join(".ssh").join("known_hosts"));
        Self {
            team_file: None,
            user_file,
            app_file: app_dir.join("known_hosts"),
        }
    }

    /// In order: the team's, the user's, SpelaionDB's. The first file that names the host
    /// decides: a matching key is accepted, a different one is a blocking error.
    fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.team_file
            .iter()
            .chain(self.user_file.iter())
            .chain(std::iter::once(&self.app_file))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HostFile {
    Unknown,
    Trusted,
    Changed(usize),
    Refused(usize, String),
}

/// Comments may contain arbitrary bytes; host patterns and keys must be valid UTF-8.
fn host_fields(bytes: &[u8]) -> Result<String, usize> {
    let mut content = String::new();
    for (index, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let mut fields = line
            .split(|b| b.is_ascii_whitespace())
            .filter(|v| !v.is_empty());
        if let Some(first) = fields.next().filter(|v| !v.starts_with(b"#")) {
            let first_text = std::str::from_utf8(first).map_err(|_| index + 1)?;
            content.push_str(first_text);
            let remaining = if first.starts_with(b"@") { 3 } else { 2 };
            for field in fields.take(remaining) {
                content.push(' ');
                content.push_str(std::str::from_utf8(field).map_err(|_| index + 1)?);
            }
        }
        content.push('\n');
    }
    Ok(content)
}

/// OpenSSH host patterns use `*` and `?`; brackets in a port-qualified host are literal.
fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t, mut star, mut matched) = (0, 0, None, 0);
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&text[t])) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            matched = t;
        } else if let Some(pos) = star {
            matched += 1;
            t = matched;
            p = pos + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn host_matches(patterns: &str, host: &str) -> Result<bool, &'static str> {
    if let Some(encoded) = patterns.strip_prefix("|1|") {
        let (salt, digest) = encoded.split_once('|').ok_or("invalid hashed hostname")?;
        let salt = BASE64
            .decode(salt.as_bytes())
            .map_err(|_| "invalid hashed hostname")?;
        let digest = BASE64
            .decode(digest.as_bytes())
            .map_err(|_| "invalid hashed hostname")?;
        if salt.is_empty() || digest.len() != 20 {
            return Err("invalid hashed hostname");
        }
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        return Ok(hmac::verify(&key, host.as_bytes(), &digest).is_ok()
            || hmac::verify(&key, host.to_ascii_lowercase().as_bytes(), &digest).is_ok());
    }
    let mut matched = false;
    for pattern in patterns.split(',') {
        if let Some(negated) = pattern.strip_prefix('!') {
            if wildcard_matches(negated, host) {
                return Ok(false);
            }
        } else if wildcard_matches(pattern, host) {
            matched = true;
        }
    }
    Ok(matched)
}

/// Check all host entries in one file, regardless of key algorithm or entry order.
fn check_contents(content: &str, host: &str, port: u16, presented: &PublicKey) -> HostFile {
    let host = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    let mut first_pin = None;
    let mut matched = false;
    let mut unsupported = None;
    for (index, line) in content.lines().enumerate() {
        let line_number = index + 1;
        let mut fields = line.split_whitespace();
        let Some(first) = fields.next() else { continue };
        if first.starts_with('#') {
            continue;
        }
        let (marker, patterns) = if first.starts_with('@') {
            (Some(first), fields.next())
        } else {
            (None, Some(first))
        };
        let Some(patterns) = patterns else { continue };
        match host_matches(patterns, &host) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(message) => return HostFile::Refused(line_number, message.into()),
        }
        if marker == Some("@cert-authority") {
            unsupported.get_or_insert(line_number);
            continue;
        }
        if marker.is_some() && marker != Some("@revoked") {
            return HostFile::Refused(line_number, "unsupported known_hosts marker".into());
        }
        let (Some(kind), Some(encoded)) = (fields.next(), fields.next()) else {
            return HostFile::Refused(line_number, "invalid host key entry".into());
        };
        let recorded = match russh::keys::parse_public_key_base64(encoded) {
            Ok(key) if key.algorithm().to_string() == kind => key,
            _ => return HostFile::Refused(line_number, "invalid or unsupported host key".into()),
        };
        if marker == Some("@revoked") {
            if recorded.key_data() == presented.key_data() {
                return HostFile::Refused(line_number, "the server key is revoked".into());
            }
            unsupported.get_or_insert(line_number);
        } else {
            first_pin.get_or_insert(line_number);
            matched |= recorded.key_data() == presented.key_data();
        }
    }
    if matched {
        HostFile::Trusted
    } else if let Some(line) = first_pin {
        HostFile::Changed(line)
    } else if let Some(line) = unsupported {
        HostFile::Refused(line, "no supported host key is pinned".into())
    } else {
        HostFile::Unknown
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
        let bytes = match std::fs::read(file) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                *outcome.lock().expect("lock poisoned") = Some(SshError::Hop {
                    hop,
                    host: host.to_string(),
                    message: format!("could not read {}: {e}", file.display()),
                });
                return false;
            }
        };
        let content = match host_fields(&bytes) {
            Ok(content) => content,
            Err(line) => {
                *outcome.lock().expect("lock poisoned") = Some(SshError::Hop {
                    hop,
                    host: host.to_string(),
                    message: format!("{}:{line}: invalid host key entry", file.display()),
                });
                return false;
            }
        };
        match check_contents(&content, host, port, &key) {
            HostFile::Trusted => return true,
            HostFile::Unknown => {}
            HostFile::Changed(line) => {
                *outcome.lock().expect("lock poisoned") = Some(SshError::HostKeyChanged {
                    hop,
                    host: host.to_string(),
                    file: file.clone(),
                    line,
                });
                return false;
            }
            HostFile::Refused(line, message) => {
                *outcome.lock().expect("lock poisoned") = Some(SshError::Hop {
                    hop,
                    host: host.to_string(),
                    message: format!("{}:{line}: {message}", file.display()),
                });
                return false;
            }
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

#[cfg(test)]
mod tests {
    use super::{HostFile, check_contents, host_fields};

    const ED: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const OTHER: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";
    const ECDSA: &str = "AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBHwf2HMM5TRXvo2SQJjsNkiDD5KqiiNjrGVv3UUh+mMT5RHxiRtOnlqvjhQtBq0VpmpCV/PwUdhOig4vkbqAcEc=";

    #[test]
    fn matches_all_pins_and_reports_the_real_line() {
        let key = russh::keys::parse_public_key_base64(ED).unwrap();
        let content = format!(
            "# comment\n[server]:2222 ssh-ed25519 {OTHER}\n[server]:2222\tssh-ed25519   {ED} a comment\n"
        );
        assert_eq!(
            check_contents(&content, "server", 2222, &key),
            HostFile::Trusted
        );
        let different = format!("# comment\n[server]:2222 ecdsa-sha2-nistp256 {ECDSA}\n");
        assert_eq!(
            check_contents(&different, "server", 2222, &key),
            HostFile::Changed(2)
        );
        assert_eq!(
            check_contents(&different, "server", 22, &key),
            HostFile::Unknown
        );
    }

    #[test]
    fn hashes_patterns_and_negations_follow_host_identity() {
        let key = russh::keys::parse_public_key_base64(
            "AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF",
        )
        .unwrap();
        let hashed = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|nuuC5vEqXlEZ/8BXQR7m619W6Ak= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF\n";
        assert_eq!(
            check_contents(hashed, "example.com", 22, &key),
            HostFile::Trusted
        );
        assert_eq!(
            check_contents(hashed, "EXAMPLE.COM", 22, &key),
            HostFile::Trusted
        );
        let port_hash = "|1|O33ESRMWPVkMYIwJ1Uw+n877jTo=|cWdlgozUJaXmY2T4RZ/Zx2hQhyE= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILIG2T/B0l0gaqj3puu510tu9N1OkQ4znY3LYuEm5zCF\n";
        assert_eq!(
            check_contents(port_hash, "EXAMPLE.COM", 2222, &key),
            HostFile::Trusted
        );
        assert_eq!(
            check_contents(port_hash, "example.com", 22, &key),
            HostFile::Unknown
        );
        assert_eq!(
            check_contents(hashed, "elsewhere.com", 22, &key),
            HostFile::Unknown
        );
        let patterns = format!("*.example.com,!bad.example.com ssh-ed25519 {OTHER}\n");
        assert_eq!(
            check_contents(&patterns, "GOOD.example.com", 22, &key),
            HostFile::Changed(1)
        );
        assert_eq!(
            check_contents(&patterns, "bad.example.com", 22, &key),
            HostFile::Unknown
        );
        let invalid_hash = "|1|YQ==|YQ== ssh-ed25519 AAAA\n";
        assert!(matches!(
            check_contents(invalid_hash, "example.com", 22, &key),
            HostFile::Refused(1, _)
        ));
    }

    #[test]
    fn invalid_revoked_and_unsupported_entries_never_authorize_a_host() {
        let key = russh::keys::parse_public_key_base64(ED).unwrap();
        let invalid = format!("server ssh-ed25519 {ED}\nserver ssh-ed25519 invalid\n");
        assert!(matches!(
            check_contents(&invalid, "server", 22, &key),
            HostFile::Refused(2, _)
        ));
        let revoked = format!("server ssh-ed25519 {ED}\n@revoked server ssh-ed25519 {ED}\n");
        assert!(matches!(
            check_contents(&revoked, "server", 22, &key),
            HostFile::Refused(2, _)
        ));
        let ca = format!("@cert-authority server ssh-ed25519 {ED}\n");
        assert!(matches!(
            check_contents(&ca, "server", 22, &key),
            HostFile::Refused(1, _)
        ));
        let with_pin = format!("{ca}server ssh-ed25519 {ED}\n");
        assert_eq!(
            check_contents(&with_pin, "server", 22, &key),
            HostFile::Trusted
        );
    }

    #[test]
    fn non_utf8_comments_are_ignored_but_invalid_fields_are_rejected() {
        let key = russh::keys::parse_public_key_base64(ED).unwrap();
        let mut bytes = b"# comment with non-utf8 \xff\nserver ssh-ed25519 ".to_vec();
        bytes.extend_from_slice(ED.as_bytes());
        bytes.extend_from_slice(b" comment with non-utf8 \xff\n");
        let content = host_fields(&bytes).unwrap();
        assert_eq!(
            check_contents(&content, "server", 22, &key),
            HostFile::Trusted
        );
        assert_eq!(host_fields(b"server ssh-ed25519 \xff\n"), Err(1));
    }
}
