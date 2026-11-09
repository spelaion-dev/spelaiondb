//! What the connection tests share: temporary directories and a fake secret backend (the
//! real keychain is never touched). Each test file uses part of it.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use spelaion_config::{Config, ConnectionId, SecretBackend, Source, TrustStore, load};

pub struct Fixture {
    pub base: PathBuf,
}

impl Fixture {
    pub fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-copies-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    pub fn repo(&self) -> PathBuf {
        self.base.join("repo")
    }

    pub fn user(&self) -> PathBuf {
        self.base.join("user")
    }

    pub fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.base.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    pub fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.base.join(relative)).unwrap()
    }

    pub fn config(&self) -> Config {
        load(Some(&self.repo()), &self.user())
    }

    pub fn trusted(&self) -> TrustStore {
        let mut trust = TrustStore::load(&self.user()).unwrap();
        trust.trust(&self.config()).unwrap();
        trust
    }

    pub fn untrusted(&self) -> TrustStore {
        TrustStore::load(&self.user()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[derive(Default)]
pub struct FakeBackend {
    pub keychain: Mutex<BTreeMap<String, String>>,
    pub runs: Mutex<Vec<String>>,
    pub reads: Mutex<Vec<String>>,
    pub fail_set: bool,
    /// No system keychain at all (Linux without a Secret Service).
    pub no_keychain: bool,
    /// The keychain exists but refuses to read (locked).
    pub fail_get: bool,
    /// Entries other applications wrote, by service and account.
    pub other_apps: Mutex<BTreeMap<(String, String), String>>,
    /// Which other applications' entries were read.
    pub other_app_reads: Mutex<Vec<String>>,
}

impl SecretBackend for FakeBackend {
    fn keychain_available(&self) -> bool {
        !self.no_keychain
    }

    fn keychain_get(&self, account: &str) -> Result<Option<String>, String> {
        if self.no_keychain || self.fail_get {
            return Err("keychain unavailable".into());
        }
        self.reads.lock().unwrap().push(account.into());
        Ok(self.keychain.lock().unwrap().get(account).cloned())
    }

    fn keychain_set(&self, account: &str, secret: &str) -> Result<(), String> {
        if self.fail_set {
            return Err("keychain locked".into());
        }
        self.keychain
            .lock()
            .unwrap()
            .insert(account.into(), secret.into());
        Ok(())
    }

    fn keychain_delete(&self, account: &str) -> Result<(), String> {
        self.keychain.lock().unwrap().remove(account);
        Ok(())
    }

    fn env(&self, _name: &str) -> Option<String> {
        Some("from-the-environment".into())
    }

    fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        self.runs
            .lock()
            .unwrap()
            .push(format!("{program} {}", args.join(" ")));
        Ok(String::new())
    }

    fn run_shell(&self, command: &str) -> Result<String, String> {
        self.runs.lock().unwrap().push(command.into());
        Ok(String::new())
    }

    fn other_app_keychain_get(
        &self,
        service: &str,
        account: &str,
    ) -> Result<Option<String>, String> {
        if self.fail_get {
            return Err("keychain locked".into());
        }
        self.other_app_reads.lock().unwrap().push(service.into());
        Ok(self
            .other_apps
            .lock()
            .unwrap()
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }
}

pub fn personal(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::User,
        path: path.into(),
    }
}

pub fn team(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::Workspace,
        path: path.into(),
    }
}

pub const BASTION: &str = r#"schema_version = 1

[[hops]]
host = "jump.example.com"
user = "deploy"
"#;
