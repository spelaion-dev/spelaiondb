//! Connections imported from MySQL Workbench 8.0: its `connections.xml`, in Workbench's
//! folder (`%APPDATA%\MySQL\Workbench` on Windows, `~/Library/Application Support/MySQL/Workbench`
//! on macOS, `~/.mysql/workbench` on Linux), the same file on every system. It is Workbench's
//! own serialisation (GRT XML): a list of `db.mgmt.Connection` objects, each with its driver
//! (Standard TCP/IP, over SSH, or a local socket / named pipe) and a dictionary of settings.
//! A connection named `Group/Name` is shown by Workbench inside the group `Group`: that is
//! its folder here.
//!
//! Workbench keeps passwords out of that file ("Store in Vault…"):
//! - on macOS in the keychain, under the connection's `hostIdentifier` as the service and its
//!   user as the account: only read when the user imports with passwords
//!   ([`crate::read_stored_passwords`]), where the system asks first;
//! - on Windows in `workbench_user_data.dat`, encrypted with the user's Windows key (DPAPI),
//!   which SpelaionDB does not read: the password is asked when connecting, and the preview
//!   says so;
//! - on Linux in the Secret Service, whose items of other applications this store does not
//!   search by: not read either, and the preview says so.
//!
//! Workbench 26 and later (a new application, built on MySQL Shell) keeps its connections in
//! a database of its own instead: this reader does not cover it.

use std::path::{Path, PathBuf};

use spelaion_driver_api::Secret;

use crate::datagrip::{Node, parse_text, shown};
use crate::import::{ImportedConnection, ImportedSsh, ImportedTls, KeychainItem};
use crate::model::{Engine, SshAuth, TlsModeConfig};

/// The largest `connections.xml` read: a few hundred bytes per connection.
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// The file Workbench keeps its connections in.
const CONNECTIONS_FILE: &str = "connections.xml";

/// Workbench's password vault on Windows (DPAPI), never read.
const VAULT_FILE: &str = "workbench_user_data.dat";

/// Workbench's connection methods (its drivers' ids).
const TCP: &str = "com.mysql.rdbms.mysql.driver.native";
const SSH: &str = "com.mysql.rdbms.mysql.driver.native_sshtun";
const SOCKET: &str = "com.mysql.rdbms.mysql.driver.native_socket";

#[derive(Debug, thiserror::Error)]
pub enum WorkbenchError {
    #[error("no MySQL Workbench connections under {0} (connections.xml in Workbench's folder)")]
    NotFound(PathBuf),
    #[error("could not read {path}: {message}")]
    Unreadable { path: PathBuf, message: String },
}

fn unreadable(path: &Path, e: impl std::fmt::Display) -> WorkbenchError {
    WorkbenchError::Unreadable {
        path: path.to_path_buf(),
        message: e.to_string(),
    }
}

/// Where MySQL Workbench keeps its connections on this system (it may not exist).
pub fn workbench_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("MySQL").join("Workbench"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support")
                .join("MySQL")
                .join("Workbench")
        })
    } else {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".mysql").join("workbench"))
    }
}

/// Which system wrote the file: it decides where Workbench keeps the passwords. Always this
/// system's in the app; a parameter so tests can read the fixture as each system would.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchPasswords {
    /// The macOS keychain, read on import with consent.
    Keychain,
    /// Windows' DPAPI vault (`workbench_user_data.dat`), not read.
    Vault,
    /// Linux's Secret Service, not read.
    SecretService,
}

impl WorkbenchPasswords {
    /// Where Workbench keeps passwords on this system.
    pub fn this_system() -> Self {
        if cfg!(windows) {
            WorkbenchPasswords::Vault
        } else if cfg!(target_os = "macos") {
            WorkbenchPasswords::Keychain
        } else {
            WorkbenchPasswords::SecretService
        }
    }
}

/// The connections in Workbench's `connections.xml` under `path` (Workbench's folder, or the
/// file itself), with passwords where `passwords` says Workbench keeps them.
pub fn read_workbench(
    path: &Path,
    passwords: WorkbenchPasswords,
) -> Result<Vec<ImportedConnection>, WorkbenchError> {
    let file = if path.is_file() {
        path.to_path_buf()
    } else {
        path.join(CONNECTIONS_FILE)
    };
    if !file.is_file() {
        return Err(WorkbenchError::NotFound(path.to_path_buf()));
    }
    let content = read_bounded(&file)?;
    let root = parse_text(&content).map_err(|e| unreadable(&file, e))?;
    let data = root
        .children
        .iter()
        .find(|n| n.name == "data")
        .ok_or_else(|| unreadable(&file, "not a MySQL Workbench file"))?;
    let has_vault = file
        .parent()
        .is_some_and(|dir| dir.join(VAULT_FILE).is_file());
    let mut connections = Vec::new();
    for list in data.children.iter().filter(|n| n.name == "value") {
        for object in list
            .children
            .iter()
            .filter(|n| n.name == "value" && n.attr("struct-name") == Some("db.mgmt.Connection"))
        {
            connections.push(connection_of(object, passwords, has_vault));
        }
    }
    Ok(connections)
}

fn read_bounded(path: &Path) -> Result<String, WorkbenchError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| unreadable(path, e))?;
    let mut data = Vec::new();
    file.take(MAX_FILE + 1)
        .read_to_end(&mut data)
        .map_err(|e| unreadable(path, e))?;
    if data.len() as u64 > MAX_FILE {
        return Err(unreadable(path, "too large for a MySQL Workbench file"));
    }
    String::from_utf8(data).map_err(|e| unreadable(path, e))
}

/// A Workbench object's members by `key`: `<value key="…">` and `<link key="…">`.
fn member<'a>(object: &'a Node, key: &str) -> Option<&'a Node> {
    object
        .children
        .iter()
        .find(|n| (n.name == "value" || n.name == "link") && n.attr("key") == Some(key))
}

fn member_text(object: &Node, key: &str) -> String {
    member(object, key)
        .map(|n| n.text.trim().to_string())
        .unwrap_or_default()
}

/// A connection's settings (`parameterValues`), in the order Workbench wrote them.
struct Settings<'a>(Vec<(&'a str, &'a Node)>);

impl<'a> Settings<'a> {
    fn of(object: &'a Node) -> Self {
        let entries = member(object, "parameterValues")
            .map(|dict| {
                dict.children
                    .iter()
                    .filter(|n| n.name == "value")
                    .map(|n| (n.attr("key").unwrap_or_default(), n))
                    .collect()
            })
            .unwrap_or_default();
        Settings(entries)
    }

    /// A setting as trimmed text; empty when it is missing.
    fn text(&self, key: &str) -> String {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, n)| n.text.trim().to_string())
            .unwrap_or_default()
    }

    /// A password as Workbench wrote it in the file (old versions did): never trimmed, `None`
    /// when empty.
    fn secret(&self, key: &str) -> Option<Secret> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, n)| n.text.clone())
            .filter(|s| !s.is_empty())
            .map(Secret::new)
    }

    fn number(&self, key: &str) -> Option<i64> {
        self.text(key).parse().ok()
    }
}

/// The settings this reader maps, or that hold nothing to say: anything else with a value is
/// listed as not imported.
const KNOWN: &[&str] = &[
    "hostName",
    "port",
    "userName",
    "password",
    "schema",
    "socket",
    "useSSL",
    "sslCA",
    "sslCert",
    "sslKey",
    "sslCipher",
    "sshHost",
    "sshUserName",
    "sshKeyFile",
    "sshPassword",
    "sshCompressionLevel",
    "SQL_MODE",
    "timeout",
    // What Workbench learnt the last time it connected.
    "serverVersion",
    "lastConnected",
];

/// Workbench's "Use SSL": No, If available, Require, Require and Verify CA, Require and
/// Verify Identity. `None` keeps SpelaionDB's default (prefer TLS), as "If available" does.
fn tls_mode(use_ssl: Option<i64>, unmapped: &mut Vec<String>) -> Option<TlsModeConfig> {
    match use_ssl {
        None | Some(1) => None,
        Some(0) => Some(TlsModeConfig::Disable),
        Some(2) => Some(TlsModeConfig::Require),
        Some(3) => Some(TlsModeConfig::VerifyCa),
        Some(4) => Some(TlsModeConfig::VerifyFull),
        Some(other) => {
            unmapped.push(format!(
                "TLS: Use SSL {other} (SpelaionDB's default is kept)"
            ));
            None
        }
    }
}

/// `host`, `host:port` or `[v6]:port`, as Workbench's SSH Hostname takes it.
fn host_and_port(text: &str) -> (String, Option<u16>) {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
        return (host.to_string(), port.filter(|p| *p > 0));
    }
    match text.rsplit_once(':') {
        // One colon: a port. More: an IPv6 address without one.
        Some((host, port)) if !host.contains(':') => match port.parse::<u16>() {
            Ok(p) if p > 0 => (host.to_string(), Some(p)),
            _ => (text.to_string(), None),
        },
        _ => (text.to_string(), None),
    }
}

/// Splits Workbench's `Group/Name` into its folder and name: Workbench groups by the text
/// before the first slash.
fn folder_and_name(full: &str) -> (String, String) {
    match full.split_once('/') {
        Some((group, name)) if !group.trim().is_empty() && !name.trim().is_empty() => {
            (group.trim().to_string(), name.trim().to_string())
        }
        _ => (String::new(), full.trim().to_string()),
    }
}

/// MySQL or MariaDB: Workbench connects to both with the same driver, and remembers the
/// server's version once it has connected.
fn engine_of(settings: &Settings<'_>) -> Engine {
    if settings
        .text("serverVersion")
        .to_lowercase()
        .contains("mariadb")
    {
        Engine::Mariadb
    } else {
        Engine::Mysql
    }
}

const VAULT_NOT_READ: &str = "Password: if Workbench saved one, it is in its vault (workbench_user_data.dat, protected with your Windows account), which SpelaionDB does not read; it is asked when connecting";
const SECRET_SERVICE_NOT_READ: &str = "Password: if Workbench saved one, it is in the system keychain under Workbench's own attributes, which SpelaionDB does not read; it is asked when connecting";

fn connection_of(
    object: &Node,
    passwords: WorkbenchPasswords,
    has_vault: bool,
) -> ImportedConnection {
    let settings = Settings::of(object);
    let driver = member_text(object, "driver");
    let (folder, name) = folder_and_name(&member_text(object, "name"));
    let mut unmapped = Vec::new();

    let over_ssh = driver == SSH;
    let engine = match driver.as_str() {
        TCP | SSH => Ok(engine_of(&settings)),
        SOCKET => {
            let socket = settings.text("socket");
            Err(if socket.is_empty() {
                "a local socket or named pipe connection, which SpelaionDB does not open".into()
            } else {
                format!(
                    "a local socket or named pipe connection ({socket}), which SpelaionDB does not open"
                )
            })
        }
        "" => Err("a connection without a connection method".into()),
        other => Err(format!(
            "Workbench's connection method {other} is not supported"
        )),
    };

    let tls = if engine.is_ok() {
        let mode = tls_mode(settings.number("useSSL"), &mut unmapped);
        let (ca, cert, key) = (
            settings.text("sslCA"),
            settings.text("sslCert"),
            settings.text("sslKey"),
        );
        let has_files = !(ca.is_empty() && cert.is_empty() && key.is_empty());
        match (mode, has_files) {
            (None, false) => None,
            (mode, _) => Some(ImportedTls {
                mode: mode.unwrap_or_default(),
                ca,
                cert,
                key,
            }),
        }
    } else {
        None
    };
    let cipher = settings.text("sslCipher");
    if !cipher.is_empty() {
        unmapped.push(format!("TLS: ciphers {cipher}"));
    }

    let ssh = over_ssh.then(|| {
        let (host, port) = host_and_port(&settings.text("sshHost"));
        let key_path = settings.text("sshKeyFile");
        let key = !key_path.is_empty();
        if settings.number("sshCompressionLevel").unwrap_or(0) != 0 {
            unmapped.push(format!(
                "SSH: compression level {}",
                settings.text("sshCompressionLevel")
            ));
        }
        if key {
            unmapped.push(
                "SSH: if the private key has a passphrase, it is asked when connecting".into(),
            );
        } else if passwords != WorkbenchPasswords::Vault || !has_vault {
            // On Windows the vault note below covers it.
            unmapped.push(
                "SSH: if Workbench saved the SSH password, it is not imported; it is asked when connecting"
                    .into(),
            );
        }
        ImportedSsh {
            host,
            port,
            user: settings.text("sshUserName"),
            auth: if key { SshAuth::Key } else { SshAuth::Password },
            key_path,
            password: if key {
                None
            } else {
                settings.secret("sshPassword")
            },
            keepalive_interval: None,
            keepalive_count: None,
            connect_timeout: None,
            local_port: None,
        }
    });

    let user = settings.text("userName");
    let password = settings.secret("password");
    let host_identifier = member_text(object, "hostIdentifier");
    let mut stored_password = None;
    if engine.is_ok() && password.is_none() {
        match passwords {
            WorkbenchPasswords::Keychain => {
                if !host_identifier.is_empty() {
                    stored_password = Some(KeychainItem {
                        service: host_identifier,
                        account: user.clone(),
                    });
                }
            }
            WorkbenchPasswords::Vault => {
                if has_vault {
                    unmapped.push(VAULT_NOT_READ.into());
                }
            }
            WorkbenchPasswords::SecretService => unmapped.push(SECRET_SERVICE_NOT_READ.into()),
        }
    }

    let sql_mode = settings.text("SQL_MODE");
    if !sql_mode.is_empty() {
        unmapped.push(format!(
            "SQL mode: {sql_mode} (add SET SESSION sql_mode to the connection's session setup)"
        ));
    }
    let timeout = settings.text("timeout");
    if !timeout.is_empty() {
        unmapped.push(format!("Timeout: {timeout} s"));
    }
    for (key, node) in &settings.0 {
        if key.is_empty() || KNOWN.contains(key) {
            continue;
        }
        let value = node.text.trim();
        // Unticked boxes and empty fields say nothing.
        if value.is_empty() || (node.attr("type") == Some("int") && value == "0") {
            continue;
        }
        unmapped.push(format!("Option: {}", shown(key, value)));
    }

    let (host, port) = if over_ssh || driver == TCP {
        (
            settings.text("hostName"),
            settings
                .number("port")
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p > 0),
        )
    } else {
        (String::new(), None)
    };

    ImportedConnection {
        id: object.attr("id").unwrap_or_default().to_string(),
        name,
        description: String::new(),
        tags: Vec::new(),
        folder,
        engine,
        host,
        port,
        database: settings.text("schema"),
        user,
        password,
        stored_password,
        env: String::new(),
        read_only: false,
        ssh,
        proxy: None,
        tls,
        unmapped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_hosts_take_an_optional_port() {
        assert_eq!(
            host_and_port("bastion:2222"),
            ("bastion".into(), Some(2222))
        );
        assert_eq!(host_and_port("bastion"), ("bastion".into(), None));
        assert_eq!(host_and_port("[::1]:2200"), ("::1".into(), Some(2200)));
        assert_eq!(host_and_port("fe80::1"), ("fe80::1".into(), None));
        assert_eq!(
            host_and_port("host:notaport"),
            ("host:notaport".into(), None)
        );
    }

    #[test]
    fn groups_come_before_the_first_slash() {
        assert_eq!(
            folder_and_name("Prod/Orders"),
            ("Prod".into(), "Orders".into())
        );
        assert_eq!(
            folder_and_name("Prod/EU/Orders"),
            ("Prod".into(), "EU/Orders".into())
        );
        assert_eq!(
            folder_and_name("/Orders"),
            (String::new(), "/Orders".into())
        );
        assert_eq!(folder_and_name("Orders"), (String::new(), "Orders".into()));
    }

    #[test]
    fn use_ssl_follows_workbench_list() {
        let mut unmapped = Vec::new();
        assert_eq!(tls_mode(None, &mut unmapped), None);
        assert_eq!(tls_mode(Some(1), &mut unmapped), None);
        assert_eq!(
            tls_mode(Some(0), &mut unmapped),
            Some(TlsModeConfig::Disable)
        );
        assert_eq!(
            tls_mode(Some(2), &mut unmapped),
            Some(TlsModeConfig::Require)
        );
        assert_eq!(
            tls_mode(Some(3), &mut unmapped),
            Some(TlsModeConfig::VerifyCa)
        );
        assert_eq!(
            tls_mode(Some(4), &mut unmapped),
            Some(TlsModeConfig::VerifyFull)
        );
        assert!(unmapped.is_empty());
        assert_eq!(tls_mode(Some(9), &mut unmapped), None);
        assert_eq!(unmapped.len(), 1);
    }
}
