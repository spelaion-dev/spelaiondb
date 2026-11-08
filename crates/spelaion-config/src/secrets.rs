//! Secret references: the configuration stores where the secret is, never its value.

use std::fmt;
use std::str::FromStr;

/// Service under which secrets are stored in the system keychain.
/// The brand's: a tunnel password saved once serves every product of the family (ADR-023).
pub const KEYCHAIN_SERVICE: &str = "Spelaion";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretRef {
    /// System keychain (Credential Manager, Keychain, Secret Service). Without a name, the
    /// entry is derived from the connection identifier.
    Keychain(Option<String>),
    /// Environment variable.
    Env(String),
    /// 1Password, through its CLI: `op read op://…`.
    OnePassword(String),
    /// Standard output of a shell command.
    Command(String),
    /// Ask the user when connecting.
    Prompt,
}

impl FromStr for SecretRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        Ok(match s {
            "keychain" => SecretRef::Keychain(None),
            "prompt" => SecretRef::Prompt,
            _ if s.starts_with("op://") => SecretRef::OnePassword(s.to_string()),
            _ => match s.split_once(':') {
                Some(("keychain", name)) if !name.is_empty() => {
                    SecretRef::Keychain(Some(name.to_string()))
                }
                Some(("env", var)) if !var.is_empty() => SecretRef::Env(var.to_string()),
                Some(("cmd", command)) if !command.trim().is_empty() => {
                    SecretRef::Command(command.trim().to_string())
                }
                Some(("vault", _)) => {
                    return Err("Vault is not supported yet; use env:, op:// or cmd:".into());
                }
                _ => {
                    return Err(format!(
                        "unknown reference {s:?}: use keychain, keychain:<name>, env:<VAR>, op://…, cmd:<command> or prompt"
                    ));
                }
            },
        })
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Keychain(None) => f.write_str("keychain"),
            SecretRef::Keychain(Some(name)) => write!(f, "keychain:{name}"),
            SecretRef::Env(var) => write!(f, "env:{var}"),
            SecretRef::OnePassword(uri) => f.write_str(uri),
            SecretRef::Command(cmd) => write!(f, "cmd:{cmd}"),
            SecretRef::Prompt => f.write_str("prompt"),
        }
    }
}

/// Access to the outside world to resolve secrets. In production it is [`SystemBackend`];
/// in tests, an in-memory one.
pub trait SecretBackend: Send + Sync {
    /// `Ok(None)` if the entry does not exist.
    fn keychain_get(&self, account: &str) -> Result<Option<String>, String>;
    fn keychain_set(&self, account: &str, secret: &str) -> Result<(), String>;
    /// Deletes an entry; does not fail if it does not exist.
    fn keychain_delete(&self, account: &str) -> Result<(), String>;
    fn env(&self, name: &str) -> Option<String>;
    /// Runs a program and returns its standard output.
    fn run(&self, program: &str, args: &[&str]) -> Result<String, String>;
    /// Runs a shell command (`sh -c` or `cmd /C`) and returns its standard output.
    fn run_shell(&self, command: &str) -> Result<String, String>;
    /// Whether there is a system keychain at all. Without one (Linux with no Secret Service
    /// running) a keychain password is asked when connecting instead of read, and nothing
    /// can be remembered. A keychain that exists but fails (locked, refused) is still an
    /// error: only its absence is.
    fn keychain_available(&self) -> bool {
        true
    }
    /// Reads an entry another application wrote in the system keychain (`service` and
    /// `account` as that application named them), to import its connections. Only for an
    /// import the user asked for, with passwords; `Ok(None)` when there is no such entry or
    /// no way to read it here. The default reads nothing.
    fn other_app_keychain_get(
        &self,
        _service: &str,
        _account: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// A secret another application stored as bytes: UTF-8, as the JetBrains IDEs write it, or
/// nothing. Trailing NULs are dropped.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
pub(crate) fn decode_other_app_secret(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    Some(text.trim_end_matches('\0').to_string())
}

/// Why the keychain cannot be used when there is none, for every message that mentions it.
pub const KEYCHAIN_UNAVAILABLE: &str = "no system keychain is available (on Linux a Secret Service such as GNOME Keyring, KWallet or KeePassXC must be running); until there is one, passwords are asked when connecting";

/// Forgets a keychain entry. Without a keychain there is nothing it could reach, so that is
/// not an error: deleting or editing a connection or a tunnel must not fail because of it.
pub(crate) fn forget(backend: &dyn SecretBackend, account: &str) -> Result<(), String> {
    if !backend.keychain_available() {
        return Ok(());
    }
    backend.keychain_delete(account)
}

/// The system's real keychain, environment and commands.
pub struct SystemBackend;

/// Picks the platform's keychain store. Only a success is kept: a Secret Service started
/// after the app (or unlocked later) is found on the next use, without restarting.
fn ensure_store() -> Result<(), String> {
    static STORE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if STORE.get().is_some() {
        return Ok(());
    }
    #[cfg(target_os = "windows")]
    let store = windows_native_keyring_store::Store::new();
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new();
    #[cfg(all(unix, not(target_os = "macos")))]
    let store = dbus_secret_service_keyring_store::Store::new();
    let store = store.map_err(|e| format!("{KEYCHAIN_UNAVAILABLE}: {e}"))?;
    // Two threads may both get here first: they set the same kind of store, harmlessly.
    keyring_core::set_default_store(store);
    let _ = STORE.set(());
    Ok(())
}

fn keychain_entry(account: &str) -> Result<keyring_core::Entry, String> {
    ensure_store()?;
    keyring_core::Entry::new(KEYCHAIN_SERVICE, account).map_err(|e| e.to_string())
}

/// An entry of a given service (the current one, or the one before ADR-023).
fn service_entry(service: &str, account: &str) -> Result<keyring_core::Entry, String> {
    // Picks the store first.
    keychain_entry(account)?;
    keyring_core::Entry::new(service, account).map_err(|e| e.to_string())
}

impl SecretBackend for SystemBackend {
    fn keychain_get(&self, account: &str) -> Result<Option<String>, String> {
        // An entry saved before the rename is found under the old service and copied.
        crate::migrate::keychain_get_or_migrate(
            account,
            KEYCHAIN_SERVICE,
            |service, account| match service_entry(service, account)?.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.to_string()),
            },
            |service, account, secret| {
                service_entry(service, account)?
                    .set_password(secret)
                    .map_err(|e| e.to_string())
            },
        )
    }

    fn keychain_set(&self, account: &str, secret: &str) -> Result<(), String> {
        keychain_entry(account)?
            .set_password(secret)
            .map_err(|e| e.to_string())
    }

    fn keychain_delete(&self, account: &str) -> Result<(), String> {
        match keychain_entry(account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn keychain_available(&self) -> bool {
        ensure_store().is_ok()
    }

    fn other_app_keychain_get(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<String>, String> {
        // Only the macOS keychain: it finds a generic password by service and account, the
        // system asks the user before handing over another application's entry, and it is the
        // one place a JetBrains IDE puts passwords there. On Windows the IDEs keep them in their
        // own KeePass database, and Secret Service items of other applications carry their own
        // attributes, which this store does not search by: nothing is read there.
        #[cfg(target_os = "macos")]
        {
            ensure_store()?;
            match keyring_core::Entry::new(service, account)
                .map_err(|e| e.to_string())?
                .get_secret()
            {
                Ok(bytes) => Ok(decode_other_app_secret(&bytes)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(e.to_string()),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (service, account);
            Ok(None)
        }
    }

    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let mut command = std::process::Command::new(program);
        command.args(args);
        capture(program, command)
    }

    fn run_shell(&self, command: &str) -> Result<String, String> {
        // On Windows the command is passed without re-escaping: Rust escapes quotes as `\"`,
        // which `cmd.exe` does not understand.
        #[cfg(windows)]
        let process = {
            use std::os::windows::process::CommandExt;
            let mut process = std::process::Command::new("cmd");
            process.arg("/C").raw_arg(command);
            process
        };
        #[cfg(not(windows))]
        let process = {
            let mut process = std::process::Command::new("sh");
            process.args(["-c", command]);
            process
        };
        capture(if cfg!(windows) { "cmd" } else { "sh" }, process)
    }
}

/// Runs and returns stdout; on failure, the exit code and stderr (never stdout, which could
/// contain the secret).
fn capture(program: &str, mut command: std::process::Command) -> Result<String, String> {
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run {program}: {e}"))?;
    if !output.status.success() {
        // The command's error may contain hints, but never the secret (stdout).
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "{program} exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| format!("{program} returned bytes that are not text"))?;
    Ok(stdout.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_reference_form() {
        assert_eq!("keychain".parse(), Ok(SecretRef::Keychain(None)));
        assert_eq!(
            "keychain:orders".parse(),
            Ok(SecretRef::Keychain(Some("orders".into())))
        );
        assert_eq!(
            "env:PGPASSWORD".parse(),
            Ok(SecretRef::Env("PGPASSWORD".into()))
        );
        assert_eq!(
            "op://Infra/orders/password".parse(),
            Ok(SecretRef::OnePassword("op://Infra/orders/password".into()))
        );
        assert_eq!(
            "cmd: pass show db".parse(),
            Ok(SecretRef::Command("pass show db".into()))
        );
        assert_eq!("prompt".parse(), Ok(SecretRef::Prompt));
        assert!(
            "vault://kv/x"
                .parse::<SecretRef>()
                .unwrap_err()
                .contains("Vault")
        );
        assert!(
            "hunter2"
                .parse::<SecretRef>()
                .unwrap_err()
                .contains("unknown")
        );
        assert!("env:".parse::<SecretRef>().is_err());
    }

    #[test]
    fn display_round_trips() {
        for s in [
            "keychain",
            "keychain:a",
            "env:X",
            "op://a/b/c",
            "cmd:echo hi",
            "prompt",
        ] {
            assert_eq!(s.parse::<SecretRef>().unwrap().to_string(), s);
        }
    }

    #[test]
    fn other_apps_secrets_are_read_as_utf8() {
        assert_eq!(
            decode_other_app_secret(b"fake-pass").as_deref(),
            Some("fake-pass")
        );
        assert_eq!(
            decode_other_app_secret("pässword".as_bytes()).as_deref(),
            Some("pässword")
        );
        assert_eq!(decode_other_app_secret(b"abcd\0").as_deref(), Some("abcd"));
        assert!(decode_other_app_secret(&[0xff, 0xfe, 0xfd]).is_none());
    }
}
